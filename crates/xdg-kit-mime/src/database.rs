use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use xdg_kit_basedir::BaseDirs;

use crate::fnmatch::Pattern;
use crate::merge::{self, Merged, Ranked};
use crate::model::{Folder, Glob, MagicRule};
use crate::read::{cache, text};
use crate::xml::TypeInfo;
use crate::{Diagnostic, MimeType};

/// The most [`MimeDatabase::sniff_len`] asks for.
const MAX_SNIFF_LEN: usize = 64 * 1024;

/// The files of a folder whose changes `refresh` notices, besides the
/// folder itself.
const SOURCE_FILES: &[&str] = &[
    "mime.cache",
    "globs2",
    "globs",
    "magic",
    "aliases",
    "subclasses",
    "icons",
    "generic-icons",
    "XMLnamespaces",
    "treemagic",
    "types",
];

/// What a path looked like when it was read; `None` when it did not exist.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp(Option<(Option<SystemTime>, u64, u64)>);

impl Stamp {
    fn of(path: &Path) -> Self {
        Self(
            fs::metadata(path)
                .ok()
                .map(|metadata| (metadata.modified().ok(), metadata.len(), metadata.ino())),
        )
    }
}

/// One database folder as read.
#[derive(Debug)]
struct Loaded {
    dir: PathBuf,
    stamps: Vec<(PathBuf, Stamp)>,
    folder: Arc<Folder>,
    diagnostics: Vec<Diagnostic>,
}

impl Loaded {
    fn read(dir: &Path) -> Self {
        // Stamps first: a change during the read is then seen by `refresh`.
        let stamps = std::iter::once(dir.to_owned())
            .chain(SOURCE_FILES.iter().map(|name| dir.join(name)))
            .map(|path| {
                let stamp = Stamp::of(&path);
                (path, stamp)
            })
            .collect();
        let mut folder = Folder::default();
        let mut diagnostics = Vec::new();
        let cache_path = dir.join("mime.cache");
        let parts = match fs::read(&cache_path) {
            Ok(bytes) => match cache::read(&cache_path, &bytes, &mut folder, &mut diagnostics) {
                Ok(()) => text::Parts::NotInCache,
                Err(unusable) => {
                    diagnostics.push(Diagnostic::new(
                        &cache_path,
                        None,
                        format!("{unusable}; the text files are read instead"),
                    ));
                    folder = Folder::default();
                    text::Parts::All
                }
            },
            Err(err) if err.kind() == io::ErrorKind::NotFound => text::Parts::All,
            Err(err) => {
                diagnostics.push(Diagnostic::new(
                    &cache_path,
                    None,
                    format!("cannot be read: {err}; the text files are read instead"),
                ));
                text::Parts::All
            }
        };
        text::read(dir, parts, &mut folder, &mut diagnostics);
        Self {
            dir: dir.to_owned(),
            stamps,
            folder: Arc::new(folder),
            diagnostics,
        }
    }

    fn unchanged(&self) -> bool {
        self.stamps
            .iter()
            .all(|(path, stamp)| Stamp::of(path) == *stamp)
    }
}

/// A glob, ready for matching.
#[derive(Debug)]
pub(crate) struct IndexedGlob {
    pub(crate) mime: MimeType,
    pub(crate) weight: u32,
    pub(crate) pattern: String,
    /// The pattern as compared: lowercased unless case-sensitive.
    pub(crate) compared: String,
    /// Characters in `compared`: longer patterns rank first.
    pub(crate) compared_len: usize,
    pub(crate) case_sensitive: bool,
    pub(crate) folder: usize,
    pub(crate) order: usize,
}

/// A magic rule with host-order values already swapped. Kept in priority
/// order (then folder, then file order), so the first match wins.
#[derive(Debug)]
pub(crate) struct IndexedMagic {
    pub(crate) mime: MimeType,
    pub(crate) matchlets: Vec<crate::model::Matchlet>,
}

/// The globs, sorted into the spec's three kinds.
#[derive(Debug, Default)]
pub(crate) struct GlobIndex {
    pub(crate) all: Vec<IndexedGlob>,
    /// Patterns without wildcards, by compared text.
    pub(crate) literal: HashMap<String, Vec<usize>>,
    /// `*` and a literal tail: by the tail.
    pub(crate) suffix: HashMap<String, Vec<usize>>,
    /// Every other pattern, split for matching.
    pub(crate) full: Vec<(usize, Pattern)>,
    /// Each canonical type's globs, highest weight first, then the more
    /// important folder, then file order.
    pub(crate) by_type: HashMap<MimeType, Vec<usize>>,
}

/// How `update-mime-database` sorts a pattern: a literal (no `*?[\`),
/// simple (`*` then no `*?[\`), or full.
pub(crate) fn pattern_kind(pattern: &str) -> PatternKind {
    let special = |c: char| matches!(c, '*' | '?' | '[' | '\\');
    match pattern.strip_prefix('*') {
        Some(tail) if !tail.contains(special) => PatternKind::Suffix,
        _ if !pattern.contains(special) => PatternKind::Literal,
        _ => PatternKind::Full,
    }
}

/// See [`pattern_kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PatternKind {
    Literal,
    Suffix,
    Full,
}

/// Lowercasing for case-insensitive matching: Unicode-aware for UTF-8,
/// ASCII-only otherwise.
pub(crate) fn lowercase(text: &[u8]) -> Vec<u8> {
    match std::str::from_utf8(text) {
        Ok(text) => text.to_lowercase().into_bytes(),
        Err(_) => text.to_ascii_lowercase(),
    }
}

impl GlobIndex {
    fn new(globs: &[Ranked<Glob>], merged: &Merged) -> Self {
        let mut index = Self::default();
        for ranked in globs {
            let glob = &ranked.rule;
            let compared = if glob.case_sensitive {
                glob.pattern.clone()
            } else {
                String::from_utf8(lowercase(glob.pattern.as_bytes()))
                    .unwrap_or_else(|_| glob.pattern.clone())
            };
            let at = index.all.len();
            match pattern_kind(&compared) {
                PatternKind::Literal => index.literal.entry(compared.clone()).or_default().push(at),
                PatternKind::Suffix => index
                    .suffix
                    .entry(compared[1..].to_owned())
                    .or_default()
                    .push(at),
                PatternKind::Full => index.full.push((at, Pattern::new(&compared))),
            }
            index.all.push(IndexedGlob {
                mime: glob.mime.clone(),
                weight: glob.weight,
                pattern: glob.pattern.clone(),
                compared_len: compared.chars().count(),
                compared,
                case_sensitive: glob.case_sensitive,
                folder: ranked.folder,
                order: ranked.order,
            });
            index
                .by_type
                .entry(merged.canonical(&glob.mime))
                .or_default()
                .push(at);
        }
        let all = &index.all;
        for list in index.by_type.values_mut() {
            list.sort_by(|&a, &b| {
                let (a, b) = (&all[a], &all[b]);
                b.weight
                    .cmp(&a.weight)
                    .then(a.folder.cmp(&b.folder))
                    .then(a.order.cmp(&b.order))
            });
        }
        index
    }
}

/// Swaps groups of `word_size` bytes on little-endian machines, as the spec
/// says for `host16` and `host32` values and masks (stored big-endian).
fn host_order(bytes: &[u8], word_size: u32) -> Vec<u8> {
    let size = word_size as usize;
    if cfg!(target_endian = "big") || size <= 1 || !bytes.len().is_multiple_of(size) {
        return bytes.to_vec();
    }
    bytes
        .chunks(size)
        .flat_map(|group| group.iter().rev().copied())
        .collect()
}

pub(crate) fn to_host(matchlets: &[crate::model::Matchlet]) -> Vec<crate::model::Matchlet> {
    matchlets
        .iter()
        .map(|matchlet| crate::model::Matchlet {
            value: host_order(&matchlet.value, matchlet.word_size),
            mask: matchlet
                .mask
                .as_ref()
                .map(|mask| host_order(mask, matchlet.word_size)),
            children: to_host(&matchlet.children),
            ..matchlet.clone()
        })
        .collect()
}

/// The candidates that are folders, least important first. A folder reached
/// again under another path (a symbolic link, a bind mount) is read once,
/// at its most important place.
fn present(candidates: &[(PathBuf, Stamp)]) -> Vec<&Path> {
    let mut seen = HashSet::new();
    let mut dirs: Vec<&Path> = candidates
        .iter()
        .filter_map(|(dir, _)| {
            let metadata = fs::metadata(dir).ok().filter(fs::Metadata::is_dir)?;
            seen.insert((metadata.dev(), metadata.ino()))
                .then_some(dir.as_path())
        })
        .collect();
    dirs.reverse();
    dirs
}

/// A type's per-type XML, with the stamps of every place its file may be.
type Described = (Arc<TypeInfo>, Vec<(PathBuf, Stamp)>);

/// The Shared MIME-info database: an immutable snapshot of every database
/// folder, merged by the spec's rules.
///
/// It is `Send + Sync`; share it through an `Arc`. [`MimeDatabase::refresh`]
/// gives a new snapshot when the files have changed.
pub struct MimeDatabase {
    /// Every folder that was or may be a database folder, with its stamp,
    /// so that one appearing or disappearing is noticed.
    candidates: Vec<(PathBuf, Stamp)>,
    /// The folders read, least important first.
    folders: Vec<Loaded>,
    pub(crate) merged: Merged,
    pub(crate) globs: GlobIndex,
    pub(crate) magic: Vec<IndexedMagic>,
    /// Cached per-type XML, keyed by canonical type. Stamps for every
    /// possible file location let `refresh` detect changes.
    descriptions: Mutex<HashMap<MimeType, Described>>,
    diagnostics: Vec<Diagnostic>,
}

/// Shows the folders and rule counts.
impl std::fmt::Debug for MimeDatabase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MimeDatabase")
            .field("dirs", &self.dirs())
            .field("globs", &self.globs.all.len())
            .field("magic", &self.magic.len())
            .field("aliases", &self.merged.aliases.len())
            .field("diagnostics", &self.diagnostics.len())
            .finish_non_exhaustive()
    }
}

impl MimeDatabase {
    /// Loads the database from every `mime` folder under the data
    /// directories: `$XDG_DATA_HOME/mime`, then each of `$XDG_DATA_DIRS`.
    #[must_use]
    pub fn load(base_dirs: &BaseDirs) -> Self {
        // Most important first, as base dirs list them; reversed below.
        // A folder listed again (the data home in `$XDG_DATA_DIRS`) is read
        // once, at its first place.
        let mut candidates: Vec<PathBuf> = Vec::new();
        for dir in std::iter::once(base_dirs.data_home())
            .chain(base_dirs.data_dirs().iter().map(PathBuf::as_path))
        {
            let mime = dir.join("mime");
            if !candidates.contains(&mime) {
                candidates.push(mime);
            }
        }
        Self::from_candidates(candidates)
    }

    /// Loads the database from exactly `dirs`, each a `mime` folder, most
    /// important first.
    #[must_use]
    pub fn from_dirs(dirs: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
        Self::from_candidates(dirs.into_iter().map(Into::into).collect())
    }

    fn from_candidates(candidates: Vec<PathBuf>) -> Self {
        let stamped: Vec<(PathBuf, Stamp)> = candidates
            .into_iter()
            .map(|dir| {
                let stamp = Stamp::of(&dir);
                (dir, stamp)
            })
            .collect();
        let folders = present(&stamped).into_iter().map(Loaded::read).collect();
        Self::assemble(stamped, folders)
    }

    fn assemble(candidates: Vec<(PathBuf, Stamp)>, folders: Vec<Loaded>) -> Self {
        let merged = merge::merge(folders.iter().map(|loaded| &*loaded.folder));
        let globs = GlobIndex::new(&merged.globs, &merged);
        let magic = merged
            .magic
            .iter()
            .map(|ranked: &Ranked<MagicRule>| IndexedMagic {
                mime: ranked.rule.mime.clone(),
                matchlets: to_host(&ranked.rule.matchlets),
            })
            .collect();
        let diagnostics = folders
            .iter()
            .flat_map(|loaded| loaded.diagnostics.iter().cloned())
            .collect();
        Self {
            candidates,
            folders,
            merged,
            globs,
            magic,
            descriptions: Mutex::new(HashMap::new()),
            diagnostics,
        }
    }

    /// Returns a new snapshot if any database file changed since this one
    /// was read, or `None` if nothing changed. Uses `stat` to check each
    /// file's modification time, size and inode, then re-reads the changed
    /// folders. Also checks per-type XML files for cached descriptions.
    /// The new snapshot reads descriptions afresh.
    ///
    /// Two changes within the filesystem's time resolution that keep the
    /// size and inode can go unnoticed.
    #[must_use]
    pub fn refresh(&self) -> Option<Self> {
        let candidates_changed = self
            .candidates
            .iter()
            .any(|(dir, stamp)| Stamp::of(dir) != *stamp);
        let folders_changed = self.folders.iter().any(|loaded| !loaded.unchanged());
        let descriptions_changed = self
            .descriptions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .flat_map(|(_, stamps)| stamps)
            .any(|(path, stamp)| Stamp::of(path) != *stamp);
        if !candidates_changed && !folders_changed && !descriptions_changed {
            return None;
        }
        let stamped: Vec<(PathBuf, Stamp)> = self
            .candidates
            .iter()
            .map(|(dir, _)| (dir.clone(), Stamp::of(dir)))
            .collect();
        let folders = present(&stamped)
            .into_iter()
            .map(
                |dir| match self.folders.iter().find(|loaded| loaded.dir == *dir) {
                    Some(loaded) if loaded.unchanged() => Loaded {
                        dir: loaded.dir.clone(),
                        stamps: loaded.stamps.clone(),
                        folder: Arc::clone(&loaded.folder),
                        diagnostics: loaded.diagnostics.clone(),
                    },
                    _ => Loaded::read(dir),
                },
            )
            .collect();
        Some(Self::assemble(stamped, folders))
    }

    /// The `mime` folders read, most important first.
    #[must_use]
    pub fn dirs(&self) -> Vec<&Path> {
        self.folders
            .iter()
            .rev()
            .map(|loaded| loaded.dir.as_path())
            .collect()
    }

    /// What was skipped or read only in part.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// How many bytes from the start of a file to pass to
    /// [`MimeDatabase::type_for_data`]: the furthest any magic rule looks,
    /// at least the 128 bytes the text-or-binary check uses, and at most
    /// 64 KiB however far a rule claims to look. An XML document's root
    /// element can lie further in; it is then not found.
    #[must_use]
    pub fn sniff_len(&self) -> usize {
        (self.merged.max_extent as usize).clamp(crate::detect::FALLBACK_LEN, MAX_SNIFF_LEN)
    }

    /// The type `mime` is an alias of, or `mime` itself.
    #[must_use]
    pub fn canonical(&self, mime: &MimeType) -> MimeType {
        self.merged.canonical(mime)
    }

    /// The aliases of `mime`'s canonical type.
    #[must_use]
    pub fn aliases(&self, mime: &MimeType) -> Vec<MimeType> {
        let canonical = self.canonical(mime);
        let mut aliases: Vec<MimeType> = self
            .merged
            .aliases
            .keys()
            .filter(|alias| self.canonical(alias) == canonical && **alias != canonical)
            .cloned()
            .collect();
        aliases.sort();
        aliases
    }

    /// The explicit parents of `mime` (`sub-class-of`), each canonical.
    #[must_use]
    pub fn parents(&self, mime: &MimeType) -> Vec<MimeType> {
        let canonical = self.canonical(mime);
        let mut parents: Vec<MimeType> = Vec::new();
        for parent in self.merged.parents.get(&canonical).into_iter().flatten() {
            let parent = self.canonical(parent);
            if !parents.contains(&parent) {
                parents.push(parent);
            }
        }
        parents
    }

    /// Every type `mime` is, breadth-first, without `mime` itself: explicit
    /// parents first, then the implicit `text/plain` (when `mime` or one of
    /// its ancestors is `text/*`) and `application/octet-stream` (for
    /// everything but `inode/*`). Each type appears once; loops in the
    /// database are harmless.
    #[must_use]
    pub fn ancestors(&self, mime: &MimeType) -> Vec<MimeType> {
        let start = self.canonical(mime);
        let mut seen: HashSet<MimeType> = HashSet::from([start.clone()]);
        let mut found = Vec::new();
        let mut queue = VecDeque::from([start.clone()]);
        while let Some(current) = queue.pop_front() {
            for parent in self.parents(&current) {
                if seen.insert(parent.clone()) {
                    found.push(parent.clone());
                    queue.push_back(parent);
                }
            }
        }
        // Subclassing is transitive: a type with a `text/*` ancestor is
        // `text/plain` too.
        let mut implicit = Vec::new();
        if start.is_media("text") || found.iter().any(|mime| mime.is_media("text")) {
            implicit.push(MimeType::known("text/plain"));
        }
        if !start.is_media("inode") {
            implicit.push(MimeType::known("application/octet-stream"));
        }
        for mime in implicit {
            if seen.insert(mime.clone()) {
                found.push(mime);
            }
        }
        found
    }

    /// Whether every `a` is also a `b`: the same type, a subclass through
    /// explicit parents, or by the implicit rules (spec "Subclassing").
    #[must_use]
    pub fn is_a(&self, a: &MimeType, b: &MimeType) -> bool {
        let b = self.canonical(b);
        self.canonical(a) == b || self.ancestors(a).contains(&b)
    }

    /// The icon for `mime`: the database's `icon` for it as written, else the
    /// type lowercased with `/` turned into `-` (`IMAGE/PNG` → `image-png`).
    /// MIME types are case-insensitive; icon names are lowercase.
    #[must_use]
    pub fn icon_name(&self, mime: &MimeType) -> String {
        let canonical = self.canonical(mime);
        self.merged
            .icons
            .get(&canonical)
            .cloned()
            .unwrap_or_else(|| canonical.as_str().to_ascii_lowercase().replace('/', "-"))
    }

    /// The generic icon for `mime`: the database's `generic-icon` for it as
    /// written, else the media type lowercased and `-x-generic`
    /// (`IMAGE/PNG` → `image-x-generic`).
    #[must_use]
    pub fn generic_icon_name(&self, mime: &MimeType) -> String {
        let canonical = self.canonical(mime);
        self.merged
            .generic_icons
            .get(&canonical)
            .cloned()
            .unwrap_or_else(|| format!("{}-x-generic", canonical.media().to_ascii_lowercase()))
    }

    /// `mime`'s file name patterns, highest weight first (then the more
    /// important folder, then file order).
    ///
    /// The spec calls a type's first `<glob>` its main extension, but the
    /// generated files order globs by weight, so that cannot be recovered;
    /// the first pattern here is only one of the highest weight.
    #[must_use]
    pub fn patterns_for(&self, mime: &MimeType) -> Vec<String> {
        let mut patterns: Vec<String> = Vec::new();
        let found = self.globs.by_type.get(&self.canonical(mime));
        for &at in found.into_iter().flatten() {
            let pattern = &self.globs.all[at].pattern;
            if !patterns.contains(pattern) {
                patterns.push(pattern.clone());
            }
        }
        patterns
    }

    /// Lists every type the database defines, sorted. Reads each folder's
    /// `types` file, which `update-mime-database` writes but the spec does
    /// not define. For folders without that file, lists the
    /// `MEDIA/SUBTYPE.xml` files on each call.
    #[must_use]
    pub fn all_types(&self) -> Vec<MimeType> {
        let mut all: Vec<MimeType> = Vec::new();
        for loaded in &self.folders {
            match &loaded.folder.types {
                Some(types) => all.extend(types.iter().cloned()),
                None => all.extend(crate::xml::types_in(&loaded.dir)),
            }
        }
        all.sort();
        all.dedup();
        all
    }

    /// The folders' paths, least important first, for the per-type XML.
    pub(crate) fn folder_dirs(&self) -> impl Iterator<Item = &Path> {
        self.folders.iter().map(|loaded| loaded.dir.as_path())
    }

    /// The per-type XML of `mime`, read on first use. The files are read
    /// without holding the lock; only types that have a file are kept, so
    /// asking about made-up types does not grow the cache.
    pub(crate) fn type_info(&self, mime: &MimeType) -> Arc<TypeInfo> {
        let canonical = self.canonical(mime);
        let cache = || {
            self.descriptions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        if let Some((info, _)) = cache().get(&canonical) {
            return Arc::clone(info);
        }
        // Stamps first: a change during the read is then seen by `refresh`.
        let stamps = self
            .folder_dirs()
            .flat_map(|dir| crate::xml::type_file_paths(dir, &canonical))
            .map(|path| {
                let stamp = Stamp::of(&path);
                (path, stamp)
            })
            .collect();
        let info = Arc::new(TypeInfo::read(&canonical, self.folder_dirs()));
        if !info.found() {
            return info;
        }
        Arc::clone(&cache().entry(canonical).or_insert((info, stamps)).0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_are_sorted_as_update_mime_database_sorts_them() {
        assert_eq!(pattern_kind("Makefile"), PatternKind::Literal);
        assert_eq!(pattern_kind("*.tar.gz"), PatternKind::Suffix);
        assert_eq!(pattern_kind("*~"), PatternKind::Suffix);
        assert_eq!(pattern_kind("readme*"), PatternKind::Full);
        assert_eq!(pattern_kind("*.[ch]"), PatternKind::Full);
        assert_eq!(pattern_kind("a\\b"), PatternKind::Full);
        assert_eq!(pattern_kind("*.*"), PatternKind::Full);
    }

    #[test]
    fn host_values_swap_on_little_endian_only() {
        let swapped = host_order(&[1, 2, 3, 4], 2);
        if cfg!(target_endian = "little") {
            assert_eq!(swapped, [2, 1, 4, 3]);
        } else {
            assert_eq!(swapped, [1, 2, 3, 4]);
        }
        assert_eq!(
            host_order(&[1, 2, 3], 2),
            [1, 2, 3],
            "a value that is not whole words stays"
        );
        assert_eq!(host_order(&[1, 2], 1), [1, 2]);
    }

    #[test]
    fn only_types_with_an_xml_file_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("text")).unwrap();
        fs::write(
            dir.path().join("text/plain.xml"),
            "<mime-type><comment>Text</comment></mime-type>",
        )
        .unwrap();
        let db = MimeDatabase::from_dirs(&[dir.path().to_owned()]);
        let kept = || db.descriptions.lock().unwrap().len();
        for made_up in 0..100 {
            let mime = MimeType::new(&format!("x-made-up/n{made_up}")).unwrap();
            assert!(!db.type_info(&mime).found());
        }
        assert_eq!(kept(), 0);
        let plain = MimeType::new("text/plain").unwrap();
        assert!(db.type_info(&plain).found());
        assert!(Arc::ptr_eq(&db.type_info(&plain), &db.type_info(&plain)));
        assert_eq!(kept(), 1);
    }

    #[test]
    fn database_is_send_and_sync() {
        fn shareable<T: Send + Sync>() {}
        shareable::<MimeDatabase>();
    }
}
