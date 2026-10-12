use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use xdg_kit_basedir::{BaseDirs, FileStamp};

use crate::fnmatch::Pattern;
use crate::merge::{self, Merged, Ranked};
use crate::model::{Folder, Glob, MagicRule};
use crate::read::{cache, text};
use crate::work;
use crate::xml::TypeInfo;
use crate::{Diagnostic, Limits, MimeType};

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

/// One database folder as read.
#[derive(Debug)]
struct Loaded {
    dir: PathBuf,
    stamps: Vec<(PathBuf, FileStamp)>,
    folder: Arc<Folder>,
    diagnostics: Vec<Diagnostic>,
}

impl Loaded {
    fn read(dir: &Path, limits: &Limits) -> Self {
        // Stamps first: a change during the read is then seen by `refresh`.
        let stamps = std::iter::once(dir.to_owned())
            .chain(SOURCE_FILES.iter().map(|name| dir.join(name)))
            .map(|path| {
                // The path, each byte copied by the join.
                work::count(1 + path.as_os_str().len());
                let stamp = FileStamp::of(&path);
                (path, stamp)
            })
            .collect();
        let mut folder = Folder::default();
        let mut diagnostics = Vec::new();
        let cache_path = dir.join("mime.cache");
        let parts = match xdg_kit_basedir::read_regular_file_with(&cache_path, limits.max_file_len)
        {
            Ok(bytes) => {
                match cache::read(&cache_path, &bytes, &mut folder, &mut diagnostics, limits) {
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
                }
            }
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
        text::read(dir, parts, &mut folder, &mut diagnostics, limits);
        Self {
            dir: dir.to_owned(),
            stamps,
            folder: Arc::new(folder),
            diagnostics,
        }
    }

    /// The folders of [`MimeDatabase::from_files`], parsed, least
    /// important first.
    #[cfg(any(test, fuzzing))]
    fn in_memory(folders: &[Vec<(&str, &[u8])>]) -> Vec<Self> {
        let mut loaded: Vec<Self> = folders
            .iter()
            .enumerate()
            .map(|(at, files)| {
                let dir = PathBuf::from(format!("/fuzz/{at}/mime"));
                let mut folder = Folder::default();
                let mut diagnostics = Vec::new();
                for (name, bytes) in files {
                    text::parse(
                        name,
                        &dir.join(name),
                        bytes,
                        &mut folder,
                        &mut diagnostics,
                        &Limits::default(),
                    );
                }
                // As `text::read` finishes a folder.
                folder.max_extent = folder.magic_extent();
                Self {
                    dir,
                    stamps: Vec::new(),
                    folder: Arc::new(folder),
                    diagnostics,
                }
            })
            .collect();
        // `assemble` takes the least important first.
        loaded.reverse();
        loaded
    }

    /// This folder again, as read before.
    fn kept(&self) -> Self {
        // Each byte copied.
        work::count_by(|| {
            self.dir.as_os_str().len()
                + self
                    .stamps
                    .iter()
                    .map(|(path, _)| 1 + path.as_os_str().len())
                    .sum::<usize>()
                + self
                    .diagnostics
                    .iter()
                    .map(|diagnostic| {
                        1 + diagnostic.path.as_os_str().len() + diagnostic.message.len()
                    })
                    .sum::<usize>()
        });
        Self {
            dir: self.dir.clone(),
            stamps: self.stamps.clone(),
            folder: Arc::clone(&self.folder),
            diagnostics: self.diagnostics.clone(),
        }
    }

    fn unchanged(&self) -> bool {
        self.stamps.iter().all(|(path, stamp)| {
            work::count(1);
            FileStamp::of(path) == *stamp
        })
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
    /// The lengths in bytes of the `suffix` tails, each once, shortest
    /// first.
    pub(crate) suffix_lens: Vec<usize>,
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

/// Lowercasing for case-insensitive matching: each character on its own,
/// without context (`Σ` is always `σ`), for UTF-8; ASCII-only otherwise.
pub(crate) fn lowercase(text: &[u8]) -> Vec<u8> {
    match std::str::from_utf8(text) {
        Ok(text) => text
            .chars()
            .flat_map(char::to_lowercase)
            .collect::<String>()
            .into_bytes(),
        Err(_) => text.to_ascii_lowercase(),
    }
}

impl GlobIndex {
    fn new(globs: &[Ranked<Glob>], merged: &Merged) -> Self {
        let mut index = Self::default();
        for ranked in globs {
            let glob = &ranked.rule;
            // The glob, and per byte of its pattern: lowercasing it (a scan
            // and a copy) and checking the result, or copying it; finding
            // its kind (two scans); copying it again, and counting its
            // characters.
            work::count(1 + 6 * glob.pattern.len());
            let compared = if glob.case_sensitive {
                glob.pattern.clone()
            } else {
                String::from_utf8(lowercase(glob.pattern.as_bytes()))
                    .unwrap_or_else(|_| glob.pattern.clone())
            };
            let at = index.all.len();
            match pattern_kind(&compared) {
                PatternKind::Literal => {
                    // Each byte copied, hashed and compared.
                    work::count(3 * compared.len());
                    index.literal.entry(compared.clone()).or_default().push(at);
                }
                PatternKind::Suffix => {
                    work::count(3 * compared.len());
                    index
                        .suffix
                        .entry(compared[1..].to_owned())
                        .or_default()
                        .push(at);
                }
                // Folded by the matcher, which keeps sets as written.
                PatternKind::Full => index
                    .full
                    .push((at, Pattern::new(&glob.pattern, !glob.case_sensitive))),
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
            // The type hashed and compared.
            work::count(2 * glob.mime.as_str().len());
            index
                .by_type
                .entry(merged.canonical(&glob.mime))
                .or_default()
                .push(at);
        }
        let all = &index.all;
        for list in index.by_type.values_mut() {
            list.sort_by(|&a, &b| {
                // Each comparison.
                work::count(1);
                let (a, b) = (&all[a], &all[b]);
                b.weight
                    .cmp(&a.weight)
                    .then(a.folder.cmp(&b.folder))
                    .then(a.order.cmp(&b.order))
            });
        }
        index.suffix_lens = index
            .suffix
            .keys()
            .map(|suffix| {
                // Each key's length collected.
                work::count(1);
                suffix.len()
            })
            .collect();
        index.suffix_lens.sort_unstable_by(|a, b| {
            // Each comparison.
            work::count(1);
            a.cmp(b)
        });
        // The pass that drops repeated lengths, a step per length.
        work::count(index.suffix_lens.len());
        index.suffix_lens.dedup();
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

/// `matchlets` in host order, each copied once.
pub(crate) fn to_host(matchlets: &[crate::model::Matchlet]) -> Vec<crate::model::Matchlet> {
    matchlets
        .iter()
        .map(|matchlet| {
            // Every field named, so a new one is not left out.
            let crate::model::Matchlet {
                start,
                range,
                word_size,
                value,
                mask,
                children,
            } = matchlet;
            // The matchlet, and each byte of its value and mask copied.
            work::count(1 + value.len() + mask.as_ref().map_or(0, Vec::len));
            crate::model::Matchlet {
                start: *start,
                range: *range,
                word_size: *word_size,
                value: host_order(value, *word_size),
                mask: mask.as_ref().map(|mask| host_order(mask, *word_size)),
                children: to_host(children),
            }
        })
        .collect()
}

/// The candidates that are folders, least important first. A folder reached
/// again under another path (a symbolic link, a bind mount) is read once,
/// at its most important place.
fn present(candidates: &[(PathBuf, FileStamp)]) -> Vec<&Path> {
    let mut seen = HashSet::new();
    let mut dirs: Vec<&Path> = candidates
        .iter()
        .filter_map(|(dir, _)| {
            work::count(1);
            let metadata = fs::metadata(dir).ok().filter(fs::Metadata::is_dir)?;
            seen.insert((metadata.dev(), metadata.ino()))
                .then_some(dir.as_path())
        })
        .collect();
    // Each folder moved by the reversal.
    work::count(dirs.len());
    dirs.reverse();
    dirs
}

/// A type's per-type XML, with the stamps of every place its file may be
/// and of the media folders listed to find it.
type Described = (Arc<TypeInfo>, Vec<(PathBuf, FileStamp)>);

/// The Shared MIME-info database: an immutable snapshot of every database
/// folder, merged by the spec's rules.
///
/// It is `Send + Sync`; share it through an `Arc`. [`MimeDatabase::refresh`]
/// gives a new snapshot when the files have changed.
pub struct MimeDatabase {
    /// Every folder that was or may be a database folder, with its stamp,
    /// so that one appearing or disappearing is noticed.
    candidates: Vec<(PathBuf, FileStamp)>,
    /// The folders read, least important first.
    folders: Vec<Loaded>,
    pub(crate) merged: Merged,
    pub(crate) globs: GlobIndex,
    pub(crate) magic: Vec<IndexedMagic>,
    /// Cached per-type XML, keyed by canonical type. Stamps for every
    /// possible file location, and every media folder listed, let
    /// `refresh` detect changes.
    descriptions: Mutex<HashMap<MimeType, Described>>,
    diagnostics: Vec<Diagnostic>,
    /// The limits used to read this database and its descriptions.
    limits: Limits,
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
        Self::load_with(base_dirs, &Limits::default())
    }

    /// Loads the database as [`MimeDatabase::load`] does, using `limits`.
    /// [`MimeDatabase::refresh`] and reads of per-type XML files use the
    /// same limits.
    #[must_use]
    pub fn load_with(base_dirs: &BaseDirs, limits: &Limits) -> Self {
        // Most important first, as base dirs list them; reversed below.
        // A folder listed again (the data home in `$XDG_DATA_DIRS`) is read
        // once, at its first place.
        let mut seen = HashSet::new();
        let candidates = std::iter::once(base_dirs.data_home())
            .chain(base_dirs.data_dirs().iter().map(PathBuf::as_path))
            .map(|dir| dir.join("mime"))
            .filter(|mime| {
                // Each byte copied by the join, then copied, hashed and
                // compared.
                work::count(1 + 4 * mime.as_os_str().len());
                seen.insert(mime.clone())
            })
            .collect();
        Self::from_candidates(candidates, limits)
    }

    /// Loads the database from exactly `dirs`, each a `mime` folder, in
    /// priority order, highest first. Converts relative paths to absolute paths now, without
    /// following links. Later changes to the working directory do not change
    /// their location, including during [`MimeDatabase::refresh`] or when
    /// reading a description later. Keeps the supplied path if conversion
    /// fails because it is empty or the current directory is gone.
    #[must_use]
    pub fn from_dirs(dirs: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
        Self::from_dirs_with(dirs, &Limits::default())
    }

    /// Reads folders as [`MimeDatabase::from_dirs`] does, using `limits`.
    /// [`MimeDatabase::refresh`] and reads of per-type XML files use the
    /// same limits.
    #[must_use]
    pub fn from_dirs_with(
        dirs: impl IntoIterator<Item = impl Into<PathBuf>>,
        limits: &Limits,
    ) -> Self {
        let dirs = dirs
            .into_iter()
            .map(|dir| {
                // Absolute now, so that a later change of folder does not
                // move it; as given when it cannot be. Each byte copied.
                let dir = dir.into();
                let dir = std::path::absolute(&dir).unwrap_or(dir);
                work::count(dir.as_os_str().len());
                dir
            })
            .collect();
        Self::from_candidates(dirs, limits)
    }

    /// The limits it was read within.
    #[must_use]
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Builds a database from folders held in memory, most important first.
    /// Each folder is a list of (file name, contents) pairs for text files.
    /// Parses and merges them as [`MimeDatabase::from_dirs`] does, but reads
    /// every supplied file without the `globs` fallback. Lookups do not read
    /// from disk; descriptions still look for per-type XML files under
    /// made-up `/fuzz/…` folders. For fuzz targets and tests only.
    #[cfg(any(test, fuzzing))]
    #[doc(hidden)]
    #[must_use]
    pub fn from_files(folders: &[Vec<(&str, &[u8])>]) -> Self {
        Self::assemble(Vec::new(), Loaded::in_memory(folders), Limits::default())
    }

    fn from_candidates(candidates: Vec<PathBuf>, limits: &Limits) -> Self {
        let stamped: Vec<(PathBuf, FileStamp)> = candidates
            .into_iter()
            .map(|dir| {
                work::count(1);
                let stamp = FileStamp::of(&dir);
                (dir, stamp)
            })
            .collect();
        let folders = present(&stamped)
            .into_iter()
            .map(|dir| Loaded::read(dir, limits))
            .collect();
        Self::assemble(stamped, folders, *limits)
    }

    fn assemble(
        candidates: Vec<(PathBuf, FileStamp)>,
        folders: Vec<Loaded>,
        limits: Limits,
    ) -> Self {
        let merged = merge::merge(folders.iter().map(|loaded| &*loaded.folder));
        let globs = GlobIndex::new(&merged.globs, &merged);
        let magic = merged
            .magic
            .iter()
            .map(|ranked: &Ranked<MagicRule>| {
                // The type copied (shared: one step); `to_host` counts
                // the matchlets.
                work::count(1);
                IndexedMagic {
                    mime: ranked.rule.mime.clone(),
                    matchlets: to_host(&ranked.rule.matchlets),
                }
            })
            .collect();
        let diagnostics = folders
            .iter()
            .flat_map(|loaded| loaded.diagnostics.iter())
            .map(|diagnostic| {
                // Each byte copied.
                work::count(1 + diagnostic.path.as_os_str().len() + diagnostic.message.len());
                diagnostic.clone()
            })
            .collect();
        Self {
            candidates,
            folders,
            merged,
            globs,
            magic,
            descriptions: Mutex::new(HashMap::new()),
            diagnostics,
            limits,
        }
    }

    /// Returns a new snapshot if any database file changed since this one
    /// was read, or `None` if nothing changed. Compares each file's
    /// [`FileStamp`] (modification and status change times, size, device
    /// and inode, so a `chmod` counts), then re-reads the changed folders.
    /// Also checks the per-type XML files of cached descriptions, and the
    /// media folders listed to find them in a folder without `types`.
    /// The new snapshot reads descriptions afresh, and keeps to the same
    /// [`MimeDatabase::limits`].
    ///
    /// Two changes within the filesystem's time resolution that keep the
    /// size and inode can go unnoticed.
    #[must_use]
    pub fn refresh(&self) -> Option<Self> {
        let candidates_changed = self.candidates.iter().any(|(dir, stamp)| {
            work::count(1);
            FileStamp::of(dir) != *stamp
        });
        let folders_changed = self.folders.iter().any(|loaded| !loaded.unchanged());
        let descriptions_changed = self
            .descriptions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .flat_map(|(_, stamps)| stamps)
            .any(|(path, stamp)| {
                work::count(1);
                FileStamp::of(path) != *stamp
            });
        if !candidates_changed && !folders_changed && !descriptions_changed {
            return None;
        }
        let stamped: Vec<(PathBuf, FileStamp)> = self
            .candidates
            .iter()
            .map(|(dir, _)| {
                // Each byte copied.
                work::count(1 + dir.as_os_str().len());
                (dir.clone(), FileStamp::of(dir))
            })
            .collect();
        let earlier: HashMap<&Path, &Loaded> = self
            .folders
            .iter()
            .map(|loaded| {
                // The folder hashed and compared.
                work::count(1 + 2 * loaded.dir.as_os_str().len());
                (loaded.dir.as_path(), loaded)
            })
            .collect();
        let folders = present(&stamped)
            .into_iter()
            .map(|dir| {
                // The folder looked up: hashed and compared.
                work::count(1 + 2 * dir.as_os_str().len());
                match earlier.get(dir) {
                    Some(loaded) if loaded.unchanged() => loaded.kept(),
                    _ => Loaded::read(dir, &self.limits),
                }
            })
            .collect();
        Some(Self::assemble(stamped, folders, self.limits))
    }

    /// The `mime` folders read, most important first, made absolute when
    /// given.
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
        let aliases = self
            .merged
            .aliases_of
            .get(&canonical)
            .cloned()
            .unwrap_or_default();
        // The lookup, then each alias.
        work::count(2 * canonical.as_str().len() + aliases.len());
        aliases
    }

    /// The explicit parents of `mime` (`sub-class-of`), each canonical.
    #[must_use]
    pub fn parents(&self, mime: &MimeType) -> Vec<MimeType> {
        let canonical = self.canonical(mime);
        let parents = self
            .merged
            .parents
            .get(&canonical)
            .cloned()
            .unwrap_or_default();
        // The lookup, then each parent.
        work::count(2 * canonical.as_str().len() + parents.len());
        parents
    }

    /// Every type `mime` is, without `mime` itself: its explicit parents
    /// breadth-first, then the implicit `text/plain` (when `mime` or one of
    /// those ancestors is `text/*`) with its own ancestors breadth-first,
    /// then `application/octet-stream` with its own ancestors, last. That
    /// type is last wherever it is met: as an explicit parent, through
    /// `text/plain`'s parents, or implicitly (for everything but an
    /// `inode/*` type whose ancestors are all `inode/*`). Only the
    /// ancestors found before `application/octet-stream` bring in
    /// `text/plain`: a `text/*` type reachable only through
    /// `application/octet-stream` does not. Each type appears once; loops
    /// in the database are harmless.
    #[must_use]
    pub fn ancestors(&self, mime: &MimeType) -> Vec<MimeType> {
        let start = self.canonical(mime);
        let mut seen: HashSet<MimeType> = HashSet::from([start.clone()]);
        let mut found = Vec::new();
        // Resolve the implicit types through the database, which may
        // define them as aliases of other types.
        let any_data = self.canonical(&MimeType::known("application/octet-stream"));
        let mut any_data_met = self.add_ancestors(&start, Some(&any_data), &mut seen, &mut found);
        // Each type visited; `is_media` counts its own scan.
        let media = |mime: &MimeType, media: &str| {
            work::count(1);
            mime.is_media(media)
        };
        // Subclassing is transitive: a type with a `text/*` ancestor is
        // `text/plain` too, and so whatever `text/plain` is.
        if start.is_media("text") || found.iter().any(|mime| media(mime, "text")) {
            let text = self.canonical(&MimeType::known("text/plain"));
            if text == any_data {
                any_data_met = true;
            } else if seen.insert(text.clone()) {
                found.push(text.clone());
                any_data_met |= self.add_ancestors(&text, Some(&any_data), &mut seen, &mut found);
            }
        }
        // Only a type that is `inode/*` through and through is not data.
        let data = any_data_met
            || !start.is_media("inode")
            || found.iter().any(|mime| !media(mime, "inode"));
        if data && seen.insert(any_data.clone()) {
            found.push(any_data.clone());
            self.add_ancestors(&any_data, None, &mut seen, &mut found);
        }
        found
    }

    /// Adds unseen explicit ancestors of `start` to `found` breadth-first,
    /// stopping at `held` without adding it. Returns whether `held` was
    /// encountered.
    fn add_ancestors(
        &self,
        start: &MimeType,
        held: Option<&MimeType>,
        seen: &mut HashSet<MimeType>,
        found: &mut Vec<MimeType>,
    ) -> bool {
        let mut met = false;
        let mut queue = VecDeque::from([start.clone()]);
        while let Some(current) = queue.pop_front() {
            for parent in self.parents(&current) {
                // The parent compared with `held`, then hashed and compared.
                work::count(1 + 3 * parent.as_str().len());
                if held == Some(&parent) {
                    met = true;
                } else if seen.insert(parent.clone()) {
                    found.push(parent.clone());
                    queue.push_back(parent);
                }
            }
        }
        met
    }

    /// Whether every `a` is also a `b`: the same type, a subclass through
    /// explicit parents, or by the implicit rules (spec "Subclassing").
    #[must_use]
    pub fn is_a(&self, a: &MimeType, b: &MimeType) -> bool {
        let b = self.canonical(b);
        self.canonical(a) == b
            || self.ancestors(a).iter().any(|mime| {
                // Each ancestor compared.
                work::count(1 + mime.as_str().len());
                *mime == b
            })
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
    ///
    /// From text files, a bracket that can never match keeps the flags in
    /// its pattern, so `*[a:b[.x` written case-sensitive lists as
    /// `*[a:b[.x:cs` and `*[a:b[.x`, where `mime.cache` gives only
    /// `*[a:b[.x`; none of them matches a name.
    #[must_use]
    pub fn patterns_for(&self, mime: &MimeType) -> Vec<String> {
        let mut patterns: Vec<String> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        let found = self.globs.by_type.get(&self.canonical(mime));
        for &at in found.into_iter().flatten() {
            let pattern = &self.globs.all[at].pattern;
            // Hashed and compared; copied when new.
            crate::work::count(1 + 2 * pattern.len());
            if seen.insert(pattern) {
                crate::work::count(pattern.len());
                patterns.push(pattern.clone());
            }
        }
        patterns
    }

    /// Lists every type the database defines, sorted. Reads each folder's
    /// `types` file, which `update-mime-database` writes but the spec does
    /// not define. For folders without that file, lists the
    /// `MEDIA/SUBTYPE.xml` files on each call. Errors listing them are left
    /// out; [`MimeDatabase::all_types_with_diagnostics`] gives them.
    #[must_use]
    pub fn all_types(&self) -> Vec<MimeType> {
        self.all_types_with_diagnostics().0
    }

    /// Lists types as [`MimeDatabase::all_types`] does, with diagnostics for
    /// failures to open or fully list folders of `MEDIA/SUBTYPE.xml` files,
    /// and for a folder or link in them that cannot be checked (an error
    /// other than its absence). Missing folders are not reported. Entries
    /// read before a listing error are kept.
    #[must_use]
    pub fn all_types_with_diagnostics(&self) -> (Vec<MimeType>, Vec<Diagnostic>) {
        let mut all: Vec<MimeType> = Vec::new();
        let mut diagnostics = Vec::new();
        for loaded in &self.folders {
            match &loaded.folder.types {
                Some(types) => all.extend(types.iter().cloned()),
                None => all.extend(crate::xml::types_in(&loaded.dir, &mut diagnostics)),
            }
        }
        all.sort();
        all.dedup();
        (all, diagnostics)
    }

    /// The per-type XML of `mime`, read on first use. The files are read
    /// without holding the lock; only types that have a file are kept, so
    /// asking about made-up types does not grow the cache. An answer from
    /// a folder listing that failed, or from a path that could not be
    /// checked, is not kept either, so the next lookup looks again.
    ///
    /// Each folder's file is looked up by the spellings the folder
    /// registers for the type ([`registered`]), then by the lowercase
    /// name, whatever spelling `mime` has, so the answer does not depend
    /// on which spelling was asked first.
    pub(crate) fn type_info(&self, mime: &MimeType) -> Arc<TypeInfo> {
        let canonical = self.canonical(mime);
        let cache = || {
            self.descriptions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        // Looked up: hashed and compared.
        work::count(2 * canonical.as_str().len());
        if let Some((info, _)) = cache().get(&canonical) {
            return Arc::clone(info);
        }
        let mut listing = Vec::new();
        // The media folders listed, stamped before.
        let mut stamps = Vec::new();
        let paths: Vec<Vec<PathBuf>> = self
            .folders
            .iter()
            .map(|loaded| {
                let spellings = registered(loaded, &canonical, &mut listing, &mut stamps);
                crate::xml::type_file_paths(&loaded.dir, &spellings, &canonical)
            })
            .collect();
        // Stamps first: a change during the read is then seen by `refresh`.
        stamps.extend(paths.iter().flatten().map(|path| {
            // Copied.
            work::count(path.as_os_str().len());
            (path.clone(), FileStamp::of(path))
        }));
        // Not kept after a listing error: the next lookup lists again.
        let listed = listing.is_empty();
        let info = Arc::new(TypeInfo::read(paths, listing, &self.limits));
        if !info.found() || !listed || info.unchecked() {
            return info;
        }
        // Hashed, compared and kept.
        work::count(3 * canonical.as_str().len());
        Arc::clone(&cache().entry(canonical).or_insert((info, stamps)).0)
    }
}

/// Returns the spellings `loaded` registers for `mime` ([`spellings_in`]): from
/// its `types` file, or from per-type filenames if it has no `types` file. Only
/// lists media folders whose names match `mime`'s media part, ignoring ASCII
/// case ([`crate::xml::types_in_media`]). Adds listing errors to `diagnostics`
/// and stamped folders to `stamps`.
fn registered(
    loaded: &Loaded,
    mime: &MimeType,
    diagnostics: &mut Vec<Diagnostic>,
    stamps: &mut Vec<(PathBuf, FileStamp)>,
) -> Vec<MimeType> {
    if let Some(types) = &loaded.folder.types {
        spellings_in(types, mime, false)
    } else {
        let types = crate::xml::types_in_media(&loaded.dir, mime, diagnostics, stamps);
        spellings_in(&types, mime, true)
    }
}

/// Returns each spelling of `mime` in `types` once. Preserves their order
/// unless they were `listed` from a folder; then sorts by byte order so lookup
/// results do not depend on directory listing order.
fn spellings_in(types: &[MimeType], mime: &MimeType, listed: bool) -> Vec<MimeType> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut spellings: Vec<MimeType> = Vec::new();
    for spelling in types {
        // Compared.
        work::count(1 + spelling.as_str().len());
        if spelling != mime {
            continue;
        }
        // Hashed and compared; copied when new.
        work::count(1 + 2 * spelling.as_str().len());
        if seen.insert(spelling.as_str()) {
            work::count(spelling.as_str().len());
            spellings.push(spelling.clone());
        }
    }
    if listed {
        spellings.sort_unstable_by(|a, b| {
            // Each comparison a sort makes.
            work::count(1 + a.as_str().len().min(b.as_str().len()));
            a.as_str().cmp(b.as_str())
        });
    }
    spellings
}

#[cfg(test)]
mod tests {
    /// Files of two folders: globs, aliases, parents (with a loop),
    /// magic and XML namespaces, the second folder deleting a glob of the
    /// first.
    fn two_folders() -> [Vec<(&'static str, &'static [u8])>; 2] {
        [
            vec![
                (
                    "globs2",
                    b"0:text/x-a:__NOGLOBS__\n50:text/x-a:*.top\n".as_slice(),
                ),
                ("aliases", b"text/x-old text/x-a\n"),
                ("subclasses", b"text/x-a text/x-b\ntext/x-b text/x-a\n"),
                (
                    "magic",
                    b"MIME-Magic\0\n[50:application/xml]\n>0=\0\x05<?xml\n",
                ),
                ("XMLnamespaces", b""),
            ],
            vec![
                ("globs2", b"50:text/x-a:*.a\n50:text/x-b:*.b\n".as_slice()),
                ("aliases", b""),
                ("subclasses", b"application/x-a+xml application/xml\n"),
                ("magic", b"MIME-Magic\0\n[60:text/x-b]\n>0=\0\x01B\n"),
                ("XMLnamespaces", b"urn:a a application/x-a+xml\n"),
            ],
        ]
    }

    #[test]
    fn files_in_memory_build_what_folders_build() {
        let root = tempfile::tempdir().unwrap();
        let folders = two_folders();
        let dirs: Vec<PathBuf> = folders
            .iter()
            .enumerate()
            .map(|(at, files)| {
                let dir = root.path().join(format!("{at}/mime"));
                fs::create_dir_all(&dir).unwrap();
                for (name, bytes) in files {
                    fs::write(dir.join(name), bytes).unwrap();
                }
                dir
            })
            .collect();
        let read = MimeDatabase::from_dirs(&dirs);
        let given = MimeDatabase::from_files(&folders);
        for (a, b) in read.folders.iter().zip(&given.folders) {
            assert_eq!(a.folder, b.folder);
            assert_eq!(a.diagnostics.len(), b.diagnostics.len());
        }
        // Lists compare by their printed form (in order); maps by value,
        // since a map prints in no fixed order.
        let (merged_read, merged_given) = (&read.merged, &given.merged);
        assert_eq!(
            format!("{:?}", merged_read.globs),
            format!("{:?}", merged_given.globs)
        );
        assert_eq!(
            format!("{:?}", merged_read.magic),
            format!("{:?}", merged_given.magic)
        );
        assert_eq!(
            format!("{:?}", merged_read.tree),
            format!("{:?}", merged_given.tree)
        );
        assert_eq!(merged_read.aliases, merged_given.aliases);
        assert_eq!(merged_read.parents, merged_given.parents);
        assert_eq!(merged_read.icons, merged_given.icons);
        assert_eq!(merged_read.generic_icons, merged_given.generic_icons);
        assert_eq!(merged_read.namespaces, merged_given.namespaces);
        assert_eq!(merged_read.max_extent, merged_given.max_extent);
        let (globs_read, globs_given) = (&read.globs, &given.globs);
        assert_eq!(
            format!("{:?}", globs_read.all),
            format!("{:?}", globs_given.all)
        );
        assert_eq!(
            format!("{:?}", globs_read.full),
            format!("{:?}", globs_given.full)
        );
        assert_eq!(globs_read.literal, globs_given.literal);
        assert_eq!(globs_read.suffix, globs_given.suffix);
        assert_eq!(globs_read.by_type, globs_given.by_type);
        let name = |db: &MimeDatabase, name: &str| -> Vec<String> {
            db.type_for_name(name)
                .matches()
                .iter()
                .map(|found| found.mime.to_string())
                .collect()
        };
        for file in ["x.top", "x.a", "x.b", "x.none"] {
            assert_eq!(name(&read, file), name(&given, file), "{file}");
        }
        for data in [
            b"B data".as_slice(),
            b"<?xml version=\"1.0\"?><a xmlns=\"urn:a\"/>",
            b"text",
            b"",
        ] {
            assert_eq!(read.type_for_data(data), given.type_for_data(data));
        }
        let a = MimeType::new("text/x-a").unwrap();
        assert_eq!(read.ancestors(&a), given.ancestors(&a));
        assert_eq!(
            given
                .type_for_data(b"<?xml version=\"1.0\"?><a xmlns=\"urn:a\"/>")
                .mime
                .as_str(),
            "application/x-a+xml"
        );
    }

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
    fn to_host_counts_each_matchlet_and_byte_copied() {
        use crate::model::Matchlet;
        // A chain 200 deep; values of 1 to 7 bytes, every third unmasked.
        let mut chain = Vec::new();
        let mut expected = 0;
        for depth in (0..200_usize).rev() {
            let value = vec![0; 1 + depth % 7];
            let mask = (depth % 3 != 0).then(|| vec![0xff; value.len()]);
            expected += 1 + value.len() + mask.as_ref().map_or(0, Vec::len);
            chain = vec![Matchlet {
                start: 0,
                range: 1,
                word_size: 1,
                value,
                mask,
                children: chain,
            }];
        }
        let (copied, steps) = work::steps(|| to_host(&chain));
        assert_eq!(copied, chain);
        assert_eq!(steps, expected);
    }

    #[test]
    fn listing_errors_reach_the_type_list_and_the_lookup() {
        use crate::xml::LISTING_FAULT;
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path().to_owned();
        let text = top.join("text");
        fs::create_dir_all(&text).unwrap();
        fs::create_dir_all(top.join("image")).unwrap();
        for name in ["text/plain.xml", "text/x-a.xml", "image/png.xml"] {
            fs::write(
                top.join(name),
                r#"<mime-type xmlns="http://www.freedesktop.org/standards/shared-mime-info"><comment>A</comment></mime-type>"#,
            )
            .unwrap();
        }
        let db = MimeDatabase::from_dirs([&top]);
        let mime = |name: &str| MimeType::new(name).unwrap();
        let png = mime("image/png");
        let first = fs::read_dir(&text).unwrap().next().unwrap().unwrap();
        let first = first.file_name();
        let first = mime(&format!(
            "text/{}",
            first.to_str().unwrap().strip_suffix(".xml").unwrap()
        ));
        let denied = io::ErrorKind::PermissionDenied;
        let err = io::Error::from(denied);
        let unopened = format!("cannot be listed: {err}");
        let cut = format!("cannot be listed in full: {err}");
        for (at, message, kept) in [
            (None, &unopened, vec![png.clone()]),
            (Some(1), &cut, vec![png.clone(), first]),
        ] {
            LISTING_FAULT.set(Some((text.clone(), at, denied)));
            let (types, diagnostics) = db.all_types_with_diagnostics();
            assert_eq!(types, kept, "{at:?}");
            assert_eq!(
                diagnostics,
                [Diagnostic::new(&text, None, message.as_str())],
                "{at:?}"
            );
        }
        // Left out by `all_types`.
        LISTING_FAULT.set(Some((text.clone(), None, denied)));
        assert_eq!(db.all_types(), std::slice::from_ref(&png));
        assert!(LISTING_FAULT.with_borrow(Option::is_none));
        // A lookup's listing error is that type's, though the file is
        // still found by its lowercase name. That answer is not cached:
        // the next lookup lists again, and its answer is.
        let plain = mime("text/plain");
        LISTING_FAULT.set(Some((top.clone(), None, denied)));
        let reported = [Diagnostic::new(&top, None, unopened.as_str())];
        let faulty = db.type_info(&plain);
        assert!(faulty.found());
        assert_eq!(faulty.diagnostics(), reported);
        assert!(LISTING_FAULT.with_borrow(Option::is_none));
        assert!(db.descriptions.lock().unwrap().is_empty());
        let fresh = db.type_info(&plain);
        assert!(fresh.diagnostics().is_empty(), "{:?}", fresh.diagnostics());
        assert!(Arc::ptr_eq(&fresh, &db.type_info(&plain)));
        assert!(db.type_diagnostics(&png).is_empty());
    }

    /// A per-type file whose comment is `comment`.
    fn per_type(comment: &str) -> String {
        format!(
            r#"<mime-type xmlns="http://www.freedesktop.org/standards/shared-mime-info"><comment>{comment}</comment></mime-type>"#
        )
    }

    /// Sets `path`'s mode.
    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Asks `db` about `text/plain` while `locked` is at mode 000: its
    /// description, its diagnostics and whether the answer was kept. `None`
    /// when `probe` can be looked at anyway (root).
    fn asked_while_locked(
        db: &MimeDatabase,
        locked: &Path,
        probe: &Path,
    ) -> Option<(Option<String>, Vec<Diagnostic>, bool)> {
        asked_about(db, &MimeType::new("text/plain").unwrap(), locked, probe)
    }

    /// Asks `db` about `mime` as [`asked_while_locked`] does.
    fn asked_about(
        db: &MimeDatabase,
        mime: &MimeType,
        locked: &Path,
        probe: &Path,
    ) -> Option<(Option<String>, Vec<Diagnostic>, bool)> {
        chmod(locked, 0o000);
        if fs::metadata(probe).is_ok() {
            chmod(locked, 0o755);
            eprintln!("a locked folder can be looked into (root?): not checked");
            return None;
        }
        let description = db.description(mime, &crate::Locales::untranslated());
        let diagnostics = db.type_diagnostics(mime);
        let kept = !db.descriptions.lock().unwrap().is_empty();
        chmod(locked, 0o755);
        Some((description, diagnostics, kept))
    }

    /// "cannot be read: " and the error for a refused lookup.
    fn refused() -> String {
        let err = io::Error::from_raw_os_error(rustix::io::Errno::ACCESS.raw_os_error());
        format!("cannot be read: {err}")
    }

    #[test]
    fn a_per_type_file_that_cannot_be_checked_is_reported_and_not_kept() {
        let dir = tempfile::tempdir().unwrap();
        let text = dir.path().join("text");
        fs::create_dir(&text).unwrap();
        fs::write(text.join("plain.xml"), per_type("Text")).unwrap();
        fs::write(dir.path().join("types"), "text/plain\n").unwrap();
        let db = MimeDatabase::from_dirs([dir.path()]);
        let file = text.join("plain.xml");
        let Some(asked) = asked_while_locked(&db, &text, &file) else {
            return;
        };
        assert_eq!(
            asked,
            (None, vec![Diagnostic::new(&file, None, refused())], false)
        );
        let plain = m("text/plain");
        let un = crate::Locales::untranslated();
        assert_eq!(db.description(&plain, &un).as_deref(), Some("Text"));
        // With a less important folder that has the file, its answer is not
        // kept either.
        let low = tempfile::tempdir().unwrap();
        fs::create_dir(low.path().join("text")).unwrap();
        fs::write(low.path().join("text/plain.xml"), per_type("Low")).unwrap();
        let db = MimeDatabase::from_dirs([dir.path(), low.path()]);
        let Some(asked) = asked_while_locked(&db, &text, &file) else {
            return;
        };
        assert_eq!(
            asked,
            (
                Some("Low".to_owned()),
                vec![Diagnostic::new(&file, None, refused())],
                false
            )
        );
        assert_eq!(db.description(&plain, &un).as_deref(), Some("Text"));
    }

    #[test]
    fn a_media_folder_that_cannot_be_checked_is_reported_and_not_kept() {
        // No `types` file: the folder's names are listed. `text` is a link
        // into a locked folder.
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        fs::create_dir_all(locked.join("text")).unwrap();
        fs::write(locked.join("text/plain.xml"), per_type("Text")).unwrap();
        let text = dir.path().join("text");
        std::os::unix::fs::symlink("locked/text", &text).unwrap();
        let db = MimeDatabase::from_dirs([dir.path()]);
        let Some(asked) = asked_while_locked(&db, &locked, &text) else {
            return;
        };
        assert_eq!(
            asked,
            (
                None,
                vec![
                    Diagnostic::new(&text, None, refused()),
                    Diagnostic::new(text.join("plain.xml"), None, refused()),
                ],
                false
            )
        );
        let un = crate::Locales::untranslated();
        assert_eq!(
            db.description(&m("text/plain"), &un).as_deref(),
            Some("Text")
        );
    }

    #[test]
    fn a_linked_per_type_file_that_cannot_be_checked_is_reported_once_and_not_kept() {
        // No `types` file: the folder's names are listed. `text/plain.xml`
        // is a link into a locked folder; looked at by the listing, then
        // by the lookup, it is reported once.
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let locked = outside.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("plain.xml"), per_type("Text")).unwrap();
        let text = dir.path().join("text");
        fs::create_dir(&text).unwrap();
        let file = text.join("plain.xml");
        std::os::unix::fs::symlink(locked.join("plain.xml"), &file).unwrap();
        let db = MimeDatabase::from_dirs([dir.path()]);
        let Some(asked) = asked_while_locked(&db, &locked, &file) else {
            return;
        };
        assert_eq!(
            asked,
            (None, vec![Diagnostic::new(&file, None, refused())], false)
        );
        let un = crate::Locales::untranslated();
        assert_eq!(
            db.description(&m("text/plain"), &un).as_deref(),
            Some("Text")
        );
        // The type list reports it too.
        chmod(&locked, 0o000);
        let (types, diagnostics) = db.all_types_with_diagnostics();
        chmod(&locked, 0o755);
        assert!(!types.contains(&m("text/plain")), "{types:?}");
        assert_eq!(diagnostics, [Diagnostic::new(&file, None, refused())]);
    }

    #[test]
    fn an_entry_that_cannot_be_checked_concerns_only_its_own_type() {
        // No `types` file: the media folder is listed for each lookup.
        // `text/other.xml` is a link into a locked folder: only a lookup of
        // `text/other` gets its diagnostic and is not kept.
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let locked = outside.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("other.xml"), per_type("Other")).unwrap();
        let text = dir.path().join("text");
        fs::create_dir(&text).unwrap();
        fs::write(text.join("plain.xml"), per_type("Text")).unwrap();
        let other = text.join("other.xml");
        std::os::unix::fs::symlink(locked.join("other.xml"), &other).unwrap();
        let db = MimeDatabase::from_dirs([dir.path()]);
        let un = crate::Locales::untranslated();
        let kept = |mime: &MimeType| db.descriptions.lock().unwrap().contains_key(mime);
        let (plain, made_up) = (m("text/plain"), m("text/other"));
        chmod(&locked, 0o000);
        if fs::metadata(&other).is_ok() {
            chmod(&locked, 0o755);
            eprintln!("a locked folder can be looked into (root?): not checked");
            return;
        }
        let asked = (
            db.description(&plain, &un),
            db.type_diagnostics(&plain),
            kept(&plain),
            db.description(&made_up, &un),
            db.type_diagnostics(&made_up),
            kept(&made_up),
        );
        let (_, listed) = db.all_types_with_diagnostics();
        chmod(&locked, 0o755);
        assert_eq!(
            asked,
            (
                Some("Text".to_owned()),
                Vec::new(),
                true,
                None,
                vec![Diagnostic::new(&other, None, refused())],
                false
            )
        );
        assert_eq!(listed, [Diagnostic::new(&other, None, refused())]);
        // Kept: an edit is not seen without `refresh`.
        fs::write(text.join("plain.xml"), per_type("Changed")).unwrap();
        assert_eq!(db.description(&plain, &un).as_deref(), Some("Text"));
    }

    #[test]
    fn an_entry_in_another_case_that_cannot_be_checked_is_the_types() {
        // `text/Plain.xml` names `text/plain` too, but is not a candidate
        // `TypeInfo::read` looks at: only the listing sees it. Its error is
        // the lookup's, which is not kept: once it can be read, it gives the
        // answer (registered spellings come first, in byte order), and
        // nothing a stamp sees has changed.
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let locked = outside.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("t.xml"), per_type("Linked")).unwrap();
        let text = dir.path().join("text");
        fs::create_dir(&text).unwrap();
        fs::write(text.join("plain.xml"), per_type("Hi")).unwrap();
        let entry = text.join("Plain.xml");
        std::os::unix::fs::symlink(locked.join("t.xml"), &entry).unwrap();
        let db = MimeDatabase::from_dirs([dir.path()]);
        let Some(asked) = asked_while_locked(&db, &locked, &entry) else {
            return;
        };
        assert_eq!(
            asked,
            (
                Some("Hi".to_owned()),
                vec![Diagnostic::new(&entry, None, refused())],
                false
            )
        );
        let un = crate::Locales::untranslated();
        assert_eq!(
            db.description(&m("text/plain"), &un).as_deref(),
            Some("Linked")
        );
    }

    #[test]
    fn an_entry_in_a_media_folder_in_another_case_that_cannot_be_checked_is_the_types() {
        // `Text/plain.xml` names `text/plain` too; only the listing sees it.
        // Its error is the lookup's, which is not kept (as above).
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let locked = outside.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("t.xml"), per_type("Linked")).unwrap();
        let text = dir.path().join("text");
        fs::create_dir(&text).unwrap();
        fs::write(text.join("plain.xml"), per_type("Hi")).unwrap();
        let un = crate::Locales::untranslated();
        let high = tempfile::tempdir().unwrap();
        let media = high.path().join("Text");
        fs::create_dir(&media).unwrap();
        let entry = media.join("plain.xml");
        std::os::unix::fs::symlink(locked.join("t.xml"), &entry).unwrap();
        let db = MimeDatabase::from_dirs([high.path(), dir.path()]);
        let Some(asked) = asked_while_locked(&db, &locked, &entry) else {
            return;
        };
        assert_eq!(
            asked,
            (
                Some("Hi".to_owned()),
                vec![Diagnostic::new(&entry, None, refused())],
                false
            )
        );
        assert_eq!(
            db.description(&m("text/plain"), &un).as_deref(),
            Some("Linked")
        );
    }

    #[test]
    fn checking_whose_an_entry_error_is_is_counted() {
        // Two folders alike but for `text/x-other.xml`, a link into a locked
        // folder in one (an error) and to nothing in the other (absent).
        // Looking up `text/plain` costs one more step, and the bytes of
        // `text/x-other` compared, for the error.
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let locked = outside.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("t.xml"), per_type("Other")).unwrap();
        let folder = |name: &str, target: &Path| {
            let text = dir.path().join(name).join("text");
            fs::create_dir_all(&text).unwrap();
            fs::write(text.join("plain.xml"), per_type("Hi")).unwrap();
            std::os::unix::fs::symlink(target, text.join("x-other.xml")).unwrap();
            MimeDatabase::from_dirs([dir.path().join(name)])
        };
        let erring = folder("e", &locked.join("t.xml"));
        let absent = folder("a", &outside.path().join("gone"));
        let plain = m("text/plain");
        chmod(&locked, 0o000);
        if fs::metadata(locked.join("t.xml")).is_ok() {
            chmod(&locked, 0o755);
            eprintln!("a locked folder can be looked into (root?): not checked");
            return;
        }
        let (erring, more) = work::steps(|| erring.type_info(&plain));
        let (absent, fewer) = work::steps(|| absent.type_info(&plain));
        chmod(&locked, 0o755);
        assert!(erring.diagnostics().is_empty() && absent.diagnostics().is_empty());
        assert_eq!(more - fewer, 1 + "text/x-other".len());
    }

    #[test]
    fn a_candidate_that_cannot_be_checked_ends_its_folders_search() {
        // As a file that cannot be read does: the folder's next candidate,
        // the lowercase name, is not tried.
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let locked = outside.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("x-Mixed.xml"), per_type("Linked")).unwrap();
        let application = dir.path().join("application");
        fs::create_dir(&application).unwrap();
        let first = application.join("x-Mixed.xml");
        std::os::unix::fs::symlink(locked.join("x-Mixed.xml"), &first).unwrap();
        fs::write(application.join("x-mixed.xml"), per_type("Lowercase")).unwrap();
        fs::write(dir.path().join("types"), "application/x-Mixed\n").unwrap();
        let db = MimeDatabase::from_dirs([dir.path()]);
        let Some((description, diagnostics, kept)) =
            asked_about(&db, &m("application/x-mixed"), &locked, &first)
        else {
            return;
        };
        assert_eq!(description, None);
        assert_eq!(diagnostics, [Diagnostic::new(&first, None, refused())]);
        assert!(!kept);
    }

    #[test]
    fn a_media_folder_that_is_a_file_holds_no_type() {
        // `text` is a file: `text/plain.xml` is absent (`NotADirectory`),
        // not an error, so the less important folder's answer is kept.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("types"), "text/plain\n").unwrap();
        fs::write(dir.path().join("text"), "").unwrap();
        let low = tempfile::tempdir().unwrap();
        fs::create_dir(low.path().join("text")).unwrap();
        fs::write(low.path().join("text/plain.xml"), per_type("Low")).unwrap();
        let db = MimeDatabase::from_dirs([dir.path(), low.path()]);
        let plain = m("text/plain");
        let un = crate::Locales::untranslated();
        assert_eq!(db.description(&plain, &un).as_deref(), Some("Low"));
        assert!(db.type_diagnostics(&plain).is_empty());
        assert!(db.descriptions.lock().unwrap().contains_key(&plain));
    }

    #[test]
    fn only_types_with_an_xml_file_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("text")).unwrap();
        fs::write(
            dir.path().join("text/plain.xml"),
            r#"<mime-type xmlns="http://www.freedesktop.org/standards/shared-mime-info"><comment>Text</comment></mime-type>"#,
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

    /// A per-type file under the spelling a folder registers, by its
    /// `types` file or else by its file names: the same answer for every
    /// spelling asked, in any order, on any database.
    #[test]
    fn a_per_type_file_is_found_by_the_registered_spelling() {
        use crate::Locales;
        let body = |comment: &str| {
            format!(
                r#"<mime-type xmlns="http://www.freedesktop.org/standards/shared-mime-info"><comment>{comment}</comment></mime-type>"#
            )
        };
        let asked = [
            m("application/x-mixed"),
            m("application/x-Mixed"),
            m("application/X-MIXED"),
        ];
        let un = Locales::untranslated();
        for (file, types, found) in [
            ("x-Mixed", None, true),
            ("x-Mixed", Some("application/x-Mixed\n"), true),
            // As `update-mime-database` names it.
            ("x-mixed", Some("application/x-Mixed\n"), true),
            // Neither the registered spelling nor lowercase.
            ("x-MIXED", Some("application/x-Mixed\n"), false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir_all(dir.path().join("application")).unwrap();
            let xml = dir.path().join(format!("application/{file}.xml"));
            fs::write(&xml, body("Mixed")).unwrap();
            if let Some(types) = types {
                fs::write(dir.path().join("types"), types).unwrap();
            }
            for first in &asked {
                let db = MimeDatabase::from_dirs([dir.path()]);
                for mime in std::iter::once(first).chain(&asked) {
                    assert_eq!(
                        db.description(mime, &un).as_deref(),
                        found.then_some("Mixed"),
                        "{file} {types:?}: {first}, then {mime}"
                    );
                }
            }
            if found {
                // `refresh` sees the file found, whatever spelling asked.
                let db = MimeDatabase::from_dirs([dir.path()]);
                assert!(db.type_info(&asked[0]).found());
                fs::write(&xml, body("Changed")).unwrap();
                let db = db.refresh().expect("the rewritten file was noticed");
                assert_eq!(db.description(&asked[2], &un).as_deref(), Some("Changed"));
            }
        }
    }

    #[test]
    fn database_is_send_and_sync() {
        fn shareable<T: Send + Sync>() {}
        shareable::<MimeDatabase>();
    }

    fn m(text: &str) -> MimeType {
        MimeType::new(text).unwrap()
    }

    /// Assembles one folder of `files`, parsed first, and counts the steps
    /// assembling takes.
    fn assembled(files: &[(&str, &str)]) -> (MimeDatabase, usize) {
        let folder: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(name, text)| (*name, text.as_bytes()))
            .collect();
        let loaded = Loaded::in_memory(&[folder]);
        work::steps(|| MimeDatabase::assemble(Vec::new(), loaded, Limits::default()))
    }

    /// The aliases of an alias chain `text/x-a0` → `text/x-a1` → … →
    /// `text/x-a{n}`.
    fn chain(n: usize) -> String {
        lines(n, |at| format!("text/x-a{at} text/x-a{}", at + 1))
    }

    /// `n` lines, each made by `line` from its number.
    fn lines(n: usize, line: impl Fn(usize) -> String) -> String {
        let mut text = String::new();
        for at in 0..n {
            text.push_str(&line(at));
            text.push('\n');
        }
        text
    }

    #[test]
    fn globs_of_a_long_alias_chain_are_indexed_in_linear_time() {
        work::assert_linear(
            1000,
            |n| {
                let aliases = chain(n);
                let globs = lines(n, |at| format!("50:text/x-a0:*.g{at}"));
                let (db, steps) = assembled(&[("aliases", &aliases), ("globs2", &globs)]);
                assert_eq!(db.globs.by_type[&m(&format!("text/x-a{n}"))].len(), n);
                (steps, aliases.len() + globs.len())
            },
            // Twice the analysed cost: per alias and per glob a few steps,
            // the sort of the chain's aliases (about ten comparisons each)
            // and the suffix lengths collected, sorted (about five
            // comparisons each) and deduplicated, and per byte about
            // twelve (each type looked up, hashed and compared a few
            // times; each pattern lowercased, sorted, copied and indexed).
            |items, bytes| 2 * (15 * items + 12 * bytes),
        );
    }

    #[test]
    fn the_aliases_of_a_long_alias_chain_are_listed_in_linear_time() {
        work::assert_linear(
            1000,
            |n| {
                let aliases = chain(n);
                let (db, _) = assembled(&[("aliases", &aliases)]);
                let end = m(&format!("text/x-a{n}"));
                let (listed, steps) = work::steps(|| db.aliases(&end));
                assert_eq!(listed.len(), n);
                let (canonical, more) = work::steps(|| db.canonical(&m("text/x-a0")));
                assert_eq!(canonical, end);
                (steps + more, aliases.len())
            },
            // Twice the analysed cost: a lookup, then a step per alias.
            |items, _| 2 * (items + 100),
        );
    }

    #[test]
    fn a_type_with_many_parents_is_handled_in_linear_time() {
        work::assert_linear(
            1000,
            |n| {
                let subclasses = lines(n, |at| format!("text/x-a text/x-p{at}"));
                let (db, assembling) = assembled(&[("subclasses", &subclasses)]);
                let a = m("text/x-a");
                let (parents, listing) = work::steps(|| db.parents(&a));
                assert_eq!(parents.len(), n);
                let (ancestors, walking) = work::steps(|| db.ancestors(&a));
                // The parents, `text/plain` and `application/octet-stream`.
                assert_eq!(ancestors.len(), n + 2);
                let last = m(&format!("text/x-p{}", n - 1));
                let (is_a, checking) = work::steps(|| db.is_a(&a, &last));
                assert!(is_a);
                (assembling + listing + walking + checking, subclasses.len())
            },
            // Twice the analysed cost: per parent a few steps each time it
            // is merged, listed, walked and compared, and per byte about
            // twelve (hashing and comparing it, copying it).
            |items, bytes| 2 * (8 * items + 12 * bytes),
        );
    }

    #[test]
    fn a_long_unclosed_bracket_glob_is_indexed_in_linear_time() {
        work::assert_linear(
            20_000,
            |n| {
                let globs = format!("50:text/x-a:{}\n", "[".repeat(n));
                let (db, steps) = assembled(&[("globs2", &globs)]);
                assert_eq!(db.globs.full.len(), 1);
                (steps, globs.len())
            },
            // Twice the analysed cost: per byte about twenty, eleven to
            // merge and index the pattern (copying, lowercasing, sorting,
            // hashing) and about four for each of its two parses (as
            // characters and as bytes).
            |_, bytes| 2 * (100 + 20 * bytes),
        );
    }

    #[test]
    fn many_data_folders_are_loaded_in_linear_time() {
        let home = tempfile::tempdir().unwrap();
        work::assert_linear(
            2000,
            |n| {
                // The data home listed again among the data folders, last.
                let data_home = home.path().join(".local/share");
                let list: Vec<String> = (0..n)
                    .map(|at| format!("/nonexistent/d{at}"))
                    .chain(std::iter::once(data_home.to_str().unwrap().to_owned()))
                    .collect();
                let list = list.join(":");
                let base_dirs = BaseDirs::from_vars(|name| match name {
                    "HOME" => Some(home.path().as_os_str().to_owned()),
                    "XDG_DATA_DIRS" => Some(list.clone().into()),
                    _ => None,
                })
                .unwrap();
                let (db, steps) = work::steps(|| MimeDatabase::load(&base_dirs));
                let candidates: Vec<PathBuf> =
                    db.candidates.iter().map(|(dir, _)| dir.clone()).collect();
                // The data home first and once, then the rest in order.
                let expected: Vec<PathBuf> = std::iter::once(data_home.join("mime"))
                    .chain((0..n).map(|at| PathBuf::from(format!("/nonexistent/d{at}/mime"))))
                    .collect();
                assert_eq!(candidates, expected);
                (steps, list.len())
            },
            // Twice the analysed cost: per folder a few steps (joined,
            // checked for a repeat, stamped, looked for), and per byte four
            // (copied by the join, then copied, hashed and compared).
            |items, bytes| 2 * (4 * items + 4 * (bytes + 5 * items)),
        );
    }

    #[test]
    fn many_folders_are_refreshed_in_linear_time() {
        let root = tempfile::tempdir().unwrap();
        work::assert_linear(
            300,
            |n| {
                let dirs: Vec<PathBuf> = (0..n)
                    .map(|at| {
                        let dir = root.path().join(format!("{n}/d{at}"));
                        fs::create_dir_all(&dir).unwrap();
                        dir
                    })
                    .collect();
                let db = MimeDatabase::from_dirs(&dirs);
                fs::write(dirs[0].join("aliases"), b"text/x-a text/x-b\n").unwrap();
                let (refreshed, steps) = work::steps(|| db.refresh());
                let refreshed = refreshed.expect("a folder changed");
                assert_eq!(refreshed.dirs().len(), n);
                assert_eq!(refreshed.canonical(&m("text/x-a")), m("text/x-b"));
                let bytes = dirs.iter().map(|dir| dir.as_os_str().len()).sum();
                (steps, bytes)
            },
            // Twice the analysed cost: per folder about 150 steps (its
            // twelve stamps checked twice and copied with the file names
            // in their paths), and per byte of its path eighteen (copied
            // for each of its thirteen paths and as a candidate, hashed
            // and compared twice).
            |items, bytes| 2 * (150 * items + 18 * bytes),
        );
    }

    /// A `magic` file with one rule for `text/x-a`: a line per depth in
    /// `depths`, each an eight-byte value with a mask.
    fn magic_of(depths: impl IntoIterator<Item = usize>) -> Vec<u8> {
        let mut file = b"MIME-Magic\0\n[50:text/x-a]\n".to_vec();
        for depth in depths {
            file.extend_from_slice(format!("{depth}>0=").as_bytes());
            file.extend_from_slice(b"\0\x08abcdefgh&\xff\xff\xff\xff\xff\xff\xff\xff\n");
        }
        file
    }

    /// Loads a folder holding only `magic` within [`Limits::unlimited`],
    /// counting the steps, and returns its one rule's matchlets.
    fn magic_loaded(dir: &Path, magic: &[u8]) -> (Vec<crate::model::Matchlet>, usize) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("magic"), magic).unwrap();
        let (db, steps) = work::steps(|| MimeDatabase::from_dirs_with([dir], &Limits::unlimited()));
        assert!(db.diagnostics().is_empty(), "{:?}", db.diagnostics());
        let [rule] = &db.magic[..] else {
            panic!("{} rules", db.magic.len())
        };
        (rule.matchlets.clone(), steps)
    }

    #[test]
    fn nested_magic_is_loaded_in_linear_time() {
        let root = tempfile::tempdir().unwrap();
        // Twice the analysed cost. A line is a matchlet with 16 bytes of
        // value and mask: 4 steps, plus 4 per byte (copied and hashed
        // when merged, copied to host order), 68 in all. A byte of the
        // folder's path: 12 (a path made and stamped for each file).
        // About 200 more.
        let bound = |lines, path| 2 * (200 + 12 * path + 68 * lines);
        work::assert_linear(
            300,
            |n| {
                let magic = magic_of(0..n);
                let dir = root.path().join(format!("deep{n}/mime"));
                let (matchlets, steps) = magic_loaded(&dir, &magic);
                let mut depth = 0;
                let mut level = &matchlets;
                while let [only] = &level[..] {
                    depth += 1;
                    level = &only.children;
                }
                assert_eq!((depth, level.len()), (n, 0));
                (steps, dir.as_os_str().len())
            },
            bound,
        );
        work::assert_linear(
            1000,
            |n| {
                let magic = magic_of(std::iter::once(0).chain((0..n).map(|_| 1)));
                let dir = root.path().join(format!("wide{n}/mime"));
                let (matchlets, steps) = magic_loaded(&dir, &magic);
                let [only] = &matchlets[..] else {
                    panic!("{} matchlets", matchlets.len())
                };
                assert_eq!(only.children.len(), n);
                (steps, dir.as_os_str().len())
            },
            // A line at depth 0 and `n` under it.
            |n, path| bound(n + 1, path),
        );
    }

    /// `n` spellings of one type: `text/x-` and fourteen letters whose
    /// cases spell the number's bits (distinct up to 2^14).
    fn spellings(n: usize) -> Vec<String> {
        (0..n)
            .map(|at| {
                let subtype: String = "abcdefghijklmn"
                    .chars()
                    .enumerate()
                    .map(|(bit, c)| {
                        if at >> bit & 1 == 1 {
                            c.to_ascii_uppercase()
                        } else {
                            c
                        }
                    })
                    .collect();
                format!("text/x-{subtype}")
            })
            .collect()
    }

    #[test]
    fn a_type_with_many_registered_spellings_is_looked_up_in_linear_time() {
        let root = tempfile::tempdir().unwrap();
        // Registered by a `types` file (none has a file, so nothing is
        // kept and each lookup does it all again), or by file names.
        for listed in [false, true] {
            work::assert_linear(
                1000,
                |n| {
                    let dir = root.path().join(format!("{listed}-{n}"));
                    fs::create_dir_all(dir.join("text")).unwrap();
                    let spellings = spellings(n);
                    if listed {
                        for spelling in &spellings {
                            fs::write(dir.join(format!("{spelling}.xml")), "").unwrap();
                        }
                    } else {
                        let types = lines(n, |at| spellings[at].clone());
                        fs::write(dir.join("types"), types).unwrap();
                    }
                    let db = MimeDatabase::from_dirs([&dir]);
                    let mime = m(&spellings[0]);
                    let (info, steps) = work::steps(|| db.type_info(&mime));
                    assert_eq!(info.found(), listed);
                    // Each spelling's path.
                    let bytes = spellings
                        .iter()
                        .map(|spelling| dir.as_os_str().len() + spelling.len() + 5)
                        .sum();
                    (steps, bytes)
                },
                // Twice the analysed cost: per spelling a few steps, and
                // per byte of its path about five (the spelling, a third
                // of it, compared, hashed and copied; the path copied by
                // two joins and by its stamp); listed, five more (each
                // name read, checked and copied, then sorted with about
                // ten comparisons of a spelling each).
                |items, bytes| 2 * (10 * items + if listed { 10 } else { 5 } * bytes),
            );
        }
    }

    #[test]
    fn a_lookup_lists_only_its_own_media_folders() {
        use crate::Locales;
        use crate::xml::LISTING_FAULT;
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path().to_owned();
        for folder in ["text", "Text", "video"] {
            fs::create_dir_all(top.join(folder)).unwrap();
        }
        fs::write(
            top.join("Text/x-A.xml"),
            r#"<mime-type xmlns="http://www.freedesktop.org/standards/shared-mime-info"><comment>A</comment></mime-type>"#,
        )
        .unwrap();
        let db = MimeDatabase::from_dirs([&top]);
        let a = m("text/x-a");
        // `video/` cannot be listed, and is not listed for `text/x-a`; its
        // media folders are, in any case.
        LISTING_FAULT.set(Some((
            top.join("video"),
            None,
            io::ErrorKind::PermissionDenied,
        )));
        let info = db.type_info(&a);
        assert!(info.diagnostics().is_empty(), "{:?}", info.diagnostics());
        assert_eq!(
            db.description(&a, &Locales::untranslated()).as_deref(),
            Some("A")
        );
        assert!(Arc::ptr_eq(&info, &db.type_info(&a)), "kept");
        assert!(LISTING_FAULT.with_borrow(Option::is_some), "video/ listed");
        LISTING_FAULT.set(None);
    }

    #[test]
    fn spellings_found_by_listing_are_taken_in_byte_order() {
        use crate::Locales;
        let body = |comment: &str| {
            format!(
                r#"<mime-type xmlns="http://www.freedesktop.org/standards/shared-mime-info"><comment>{comment}</comment></mime-type>"#
            )
        };
        // Every case of `x-bar`, written in each order, so that the order
        // a listing gives cannot pick the first in byte order by chance.
        let mut names: Vec<String> = (0..8)
            .map(|at: usize| {
                "bar"
                    .chars()
                    .enumerate()
                    .map(|(bit, c)| {
                        if at >> bit & 1 == 1 {
                            c
                        } else {
                            c.to_ascii_uppercase()
                        }
                    })
                    .collect()
            })
            .collect();
        let un = Locales::untranslated();
        for reverse in [false, true] {
            if reverse {
                names.reverse();
            }
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir_all(dir.path().join("text")).unwrap();
            for name in &names {
                fs::write(dir.path().join(format!("text/x-{name}.xml")), body(name)).unwrap();
            }
            let db = MimeDatabase::from_dirs([dir.path()]);
            assert_eq!(
                db.description(&m("text/x-bar"), &un).as_deref(),
                Some("BAR"),
                "{names:?}"
            );
            // A `types` file's own order stays.
            fs::write(dir.path().join("types"), "text/x-Bar\ntext/x-BAR\n").unwrap();
            let db = MimeDatabase::from_dirs([dir.path()]);
            assert_eq!(
                db.description(&m("text/x-BAR"), &un).as_deref(),
                Some("Bar")
            );
        }
    }

    #[test]
    fn a_listing_is_taken_in_byte_order_and_a_types_file_in_its_own() {
        // Not in byte order, with a repeat and another type.
        let types: Vec<MimeType> = [
            "text/x-bar",
            "text/x-Bar",
            "text/plain",
            "text/x-BAR",
            "text/x-bar",
            "text/x-bAr",
        ]
        .into_iter()
        .map(m)
        .collect();
        let found = |listed| -> Vec<String> {
            spellings_in(&types, &m("text/x-BAR"), listed)
                .iter()
                .map(ToString::to_string)
                .collect()
        };
        assert_eq!(
            found(true),
            ["text/x-BAR", "text/x-Bar", "text/x-bAr", "text/x-bar"]
        );
        assert_eq!(
            found(false),
            ["text/x-bar", "text/x-Bar", "text/x-BAR", "text/x-bAr"]
        );
    }

    #[test]
    fn refresh_notices_a_spelling_added_to_a_listed_folder() {
        use crate::Locales;
        let body = |comment: &str| {
            format!(
                r#"<mime-type xmlns="http://www.freedesktop.org/standards/shared-mime-info"><comment>{comment}</comment></mime-type>"#
            )
        };
        let un = Locales::untranslated();
        let dir = tempfile::tempdir().unwrap();
        let text = dir.path().join("text");
        fs::create_dir_all(&text).unwrap();
        fs::write(text.join("x-a.xml"), body("a")).unwrap();
        let db = MimeDatabase::from_dirs([dir.path()]);
        let a = m("text/x-a");
        assert_eq!(db.description(&a, &un).as_deref(), Some("a"));
        assert!(db.refresh().is_none(), "nothing changed");
        // First in byte order, so a fresh database takes it.
        fs::write(text.join("x-A.xml"), body("A")).unwrap();
        // Past a coarse clock's tick, as the listing changed in any case.
        fs::File::open(&text)
            .unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(10))
            .unwrap();
        let fresh = MimeDatabase::from_dirs([dir.path()]);
        assert_eq!(fresh.description(&a, &un).as_deref(), Some("A"));
        let db = db.refresh().expect("the added spelling was noticed");
        assert_eq!(db.description(&a, &un), fresh.description(&a, &un));
    }
}
