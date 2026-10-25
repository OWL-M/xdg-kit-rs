//! Loading: resolving a theme's inheritance chain across the icon roots
//! and building the immutable candidate index lookups read.

use std::path::{Path, PathBuf};

use xdg_kit_basedir::__hashed::{Map, Set, Str};
use xdg_kit_basedir::{BaseDirs, FileStamp};

use crate::theme::{self, ThemeDir, ThemeFile};
use crate::work::{self, Counted};
use crate::{Diagnostic, Limits};

/// The icon search roots, in precedence order: the legacy `$HOME/.icons`,
/// then `data_home()/icons`, then each data directory's.
pub(crate) fn icon_roots(base_dirs: &BaseDirs) -> Vec<PathBuf> {
    let mut roots = vec![base_dirs.home().join(".icons")];
    roots.push(base_dirs.data_home().join("icons"));
    roots.extend(base_dirs.data_dirs().iter().map(|dir| dir.join("icons")));
    roots
}

/// The extra directory the unthemed fallback searches after the roots.
const PIXMAPS: &str = "/usr/share/pixmaps";

/// A loading policy knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadOptions {
    /// Use validated `icon-theme.cache` files to enumerate directories;
    /// `false` scans every directory, for callers that distrust caches
    /// outright.
    pub caches: bool,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self { caches: true }
    }
}

/// A candidate's available formats, as a small mask.
pub(crate) const PNG: u8 = 1;
pub(crate) const SVG: u8 = 2;
pub(crate) const XPM: u8 = 4;

/// One place an icon name exists: a theme subdirectory in one root, with
/// the formats found there. Ordered by (subdirectory, root) — the
/// precedence inside both selection passes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Candidate {
    /// Index into the theme's declared directories.
    pub dir: u32,
    /// Index into the search roots.
    pub root: u32,
    pub formats: u8,
}

/// One theme of the loaded chain, with its name index.
#[derive(Debug)]
pub(crate) struct LoadedTheme {
    pub id: String,
    pub dirs: Vec<ThemeDir>,
    /// The parsed definition, kept for the listing fields.
    pub definition: ThemeFile,
    /// Icon name → candidates, ordered by (dir, root).
    pub names: Map<Str<Counted>, Vec<Candidate>>,
    /// The longest indexed icon name: a generated fallback alternative
    /// longer than this cannot match, and is skipped without hashing.
    pub longest_name: usize,
}

/// An unthemed fallback file: which fallback directory, which formats.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Unthemed {
    pub dir: u32,
    pub formats: u8,
}

/// The loaded snapshot: a theme chain's index, the unthemed fallback,
/// and everything `refresh` compares.
#[derive(Debug)]
pub struct IconThemes {
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) chain: Vec<LoadedTheme>,
    /// The unthemed fallback directories (the roots themselves, then
    /// pixmaps) and their files.
    pub(crate) fallback_dirs: Vec<PathBuf>,
    pub(crate) fallback: Map<Str<Counted>, Vec<Unthemed>>,
    /// The longest name in `fallback`, bounding generated alternatives.
    pub(crate) fallback_longest: usize,
    pub(crate) stamps: Vec<(PathBuf, FileStamp)>,
    /// Directories whose stamp moved must be scanned on the next load,
    /// never served from a cache that still passes its own check.
    pub(crate) distrust: Vec<PathBuf>,
    /// Cache files a rebuild refused although they passed their own
    /// freshness check, with the stamp each had when refused. Carried
    /// into every later snapshot, so a refresh triggered by an unrelated
    /// change cannot re-accept a cache already known not to cover what
    /// the scan found; a record is dropped once the cache file itself
    /// changes, which is the only event that can make it current again.
    pub(crate) distrusted: Vec<(PathBuf, FileStamp)>,
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) theme_id: String,
    pub(crate) limits: Limits,
    pub(crate) options: LoadOptions,
    pub(crate) icons_indexed: usize,
}

impl IconThemes {
    /// Loads `theme` and its inheritance chain (closed with `hicolor`),
    /// plus the unthemed fallback files, with the default limits and
    /// options.
    #[must_use]
    pub fn load(base_dirs: &BaseDirs, theme: &str) -> Self {
        Self::load_with(base_dirs, theme, &Limits::default(), LoadOptions::default())
    }

    /// [`IconThemes::load`] within `limits` and under `options`.
    #[must_use]
    pub fn load_with(
        base_dirs: &BaseDirs,
        theme: &str,
        limits: &Limits,
        options: LoadOptions,
    ) -> Self {
        Self::from_roots(
            icon_roots(base_dirs),
            theme,
            limits.clone(),
            options,
            Vec::new(),
            Vec::new(),
        )
    }

    pub(crate) fn from_roots(
        roots: Vec<PathBuf>,
        theme: &str,
        limits: Limits,
        options: LoadOptions,
        distrust: Vec<PathBuf>,
        distrusted: Vec<(PathBuf, FileStamp)>,
    ) -> Self {
        let mut this = Self {
            roots,
            chain: Vec::new(),
            fallback_dirs: Vec::new(),
            fallback: Map::new(),
            fallback_longest: 0,
            stamps: Vec::new(),
            distrust,
            distrusted,
            diagnostics: Vec::new(),
            theme_id: theme.to_owned(),
            limits,
            options,
            icons_indexed: 0,
        };
        this.load_chain(theme);
        this.load_fallback();
        // The change list was for this load; the refused-cache records
        // outlive it, each until its cache file changes.
        this.distrust = Vec::new();
        this
    }

    /// Problems found while loading: skipped files, unusable caches,
    /// absent themes.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// The loaded chain's theme ids, in lookup order.
    pub fn chain(&self) -> impl Iterator<Item = &str> {
        self.chain.iter().map(|theme| theme.id.as_str())
    }

    /// The loaded theme's picker fields, localized for `locales` —
    /// `None` when the requested theme was not installed (the chain then
    /// holds only what could be loaded).
    #[must_use]
    pub fn listing(&self, locales: &xdg_kit_locale::Locales) -> Option<crate::ThemeListing> {
        let first = self
            .chain
            .first()
            .filter(|theme| theme.id == self.theme_id)?;
        Some(crate::theme::listing(&first.id, &first.definition, locales))
    }

    /// A fresh load, when anything this snapshot saw — directories and
    /// subdirectories, `index.theme` files and the would-be locations of
    /// higher-priority ones, caches, absent inherited themes — changed
    /// since; `None` while everything still looks the same. The rebuild
    /// scans every path whose stamp moved rather than trusting a cache
    /// that still passes its own check: noticing a change and rebuilding
    /// the same stale inventory would make this a no-op lie.
    ///
    /// The spec's implementation note expects applications to recheck
    /// theme directories at most every five seconds; this crate never
    /// does that behind the caller's back — calling this at the
    /// application's own cadence (its cheap path is exactly these stamp
    /// checks) is the documented way to meet the note.
    #[must_use]
    pub fn refresh(&self) -> Option<Self> {
        let distrust: Vec<PathBuf> = self
            .stamps
            .iter()
            .inspect(|_| work::count(1))
            .filter(|(path, stamp)| FileStamp::of(path) != *stamp)
            .map(|(path, _)| path.clone())
            .collect();
        if distrust.is_empty() {
            return None;
        }
        Some(Self::from_roots(
            self.roots.clone(),
            &self.theme_id.clone(),
            self.limits.clone(),
            self.options,
            distrust,
            self.distrusted.clone(),
        ))
    }

    /// Resolves the inheritance chain and indexes each theme.
    /// `max_chain` bounds the distinct ids *examined* — failed and
    /// missing parents consume it too, since each costs filesystem
    /// probes and stamps — and the mandatory hicolor fallback has a
    /// reserved allowance on top, so a tight limit cannot cut the spec's
    /// final fallback.
    fn load_chain(&mut self, theme: &str) {
        // A stack, parents pushed in reverse: depth-first without the
        // quadratic shifting of a front-popped queue.
        let mut stack = vec![theme.to_owned()];
        let mut seen = Set::<Str<Counted>>::new();
        while let Some(id) = stack.pop() {
            // The id hashed and, when new, copied (counted by the set):
            // the cycle and diamond guard, and the examined budget.
            work::count(1);
            if !seen.insert(&id) {
                continue;
            }
            if seen.len() > self.limits.max_chain {
                self.diagnostics.push(Diagnostic::new(
                    None,
                    format!(
                        "the inheritance chain is cut at {} themes",
                        self.limits.max_chain
                    ),
                ));
                break;
            }
            if !theme::single_component(&id) {
                self.diagnostics.push(Diagnostic::new(
                    None,
                    format!("\"{}\" is not a theme name", crate::error::shown(&id)),
                ));
                continue;
            }
            // hicolor is the spec's *final* fallback wherever an
            // Inherits list names it — except when it is the requested
            // theme itself.
            if id == "hicolor" && theme != "hicolor" {
                continue;
            }
            match self.define(&id) {
                Some(file) => {
                    for parent in file.inherits.iter().rev() {
                        work::count(1 + parent.len());
                        stack.push(parent.clone());
                    }
                    self.index_theme(&id, file);
                }
                None => {
                    self.diagnostics.push(Diagnostic::new(
                        None,
                        format!("the theme \"{id}\" is not installed"),
                    ));
                }
            }
        }
        if theme != "hicolor" {
            if let Some(file) = self.define("hicolor") {
                self.index_theme("hicolor", file);
            } else {
                // Stamped by `define`, so its appearance is noticed.
                self.diagnostics.push(Diagnostic::new(
                    None,
                    "the hicolor fallback theme is not installed".to_owned(),
                ));
            }
        }
    }

    /// The theme's definition: the first root with a readable, parseable
    /// `index.theme` supplies it whole; roots are never merged. Every
    /// root's would-be location is stamped, so a higher-priority file
    /// appearing later is noticed.
    fn define(&mut self, id: &str) -> Option<ThemeFile> {
        let mut defined = None;
        for root in &self.roots.clone() {
            // The top-level theme directory is the spec's notification
            // point: installers touch it to signal a change.
            let top = root.join(id);
            self.stamps.push((top.clone(), FileStamp::of(&top)));
            let path = theme::index_path(root, id);
            self.stamps.push((path.clone(), FileStamp::of(&path)));
            if defined.is_some() {
                continue;
            }
            let bytes =
                match xdg_kit_basedir::read_regular_file_with(&path, self.limits.max_file_len) {
                    Ok(bytes) => bytes,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(err) => {
                        self.diagnostics.push(Diagnostic::new(
                            Some(path),
                            format!("cannot be read: {err}"),
                        ));
                        continue;
                    }
                };
            defined = theme::parse(&bytes, &path, &mut self.diagnostics);
        }
        defined
    }

    /// Indexes one theme: every root's copy of every declared directory,
    /// each root through its validated cache where one serves, scanned
    /// otherwise.
    fn index_theme(&mut self, id: &str, file: ThemeFile) {
        let caches: Vec<Option<crate::cache::ThemeCache>> = self
            .roots
            .clone()
            .iter()
            .map(|root| self.theme_cache(&root.join(id), &file.dirs))
            .collect();
        let mut names = Map::<Str<Counted>, Vec<Candidate>>::new();
        let mut longest = 0usize;
        let dirs = file.dirs.clone();
        for (dir_at, dir) in dirs.iter().enumerate() {
            for (root_at, root) in self.roots.clone().iter().enumerate() {
                let path = root.join(id).join(&dir.path);
                self.stamps.push((path.clone(), FileStamp::of(&path)));
                let candidate = Candidate {
                    dir: u32::try_from(dir_at).unwrap_or(u32::MAX),
                    root: u32::try_from(root_at).unwrap_or(u32::MAX),
                    formats: 0,
                };
                match &caches[root_at] {
                    Some(cache) => {
                        for (name, formats) in
                            cache.by_dir.get(&dir.path).map_or(&[][..], Vec::as_slice)
                        {
                            work::count(1);
                            let granted = self.charge(*formats);
                            if granted != 0 {
                                push_candidate(&mut names, name, candidate, granted, &mut longest);
                            }
                            if granted != *formats {
                                self.truncated(&path);
                                break;
                            }
                        }
                    }
                    None => self.enumerate(&path, candidate, &mut names, &mut longest),
                }
            }
        }
        self.chain.push(LoadedTheme {
            id: id.to_owned(),
            dirs,
            definition: file,
            names,
            longest_name: longest,
        });
    }

    /// One root's cache for this theme, when the policy allows it, it is
    /// sound and fresh, and nothing it covers is distrusted (a directory
    /// whose stamp moved must be scanned, never served from a cache that
    /// still passes its own check). A refusal of an unchanged cache is
    /// recorded on the snapshot and honored by every later rebuild, so a
    /// refresh triggered by an unrelated change cannot quietly bring a
    /// known-stale cache back; the record dies when the cache file itself
    /// changes.
    fn theme_cache(
        &mut self,
        theme_dir: &Path,
        declared: &[ThemeDir],
    ) -> Option<crate::cache::ThemeCache> {
        let cache_path = theme_dir.join("icon-theme.cache");
        let stamp = FileStamp::of(&cache_path);
        self.stamps.push((cache_path.clone(), stamp.clone()));
        if !self.options.caches {
            return None;
        }
        let mut covered_changed = false;
        let mut cache_changed = false;
        for moved in &self.distrust {
            work::count(1);
            covered_changed |= moved.starts_with(theme_dir);
            cache_changed |= *moved == cache_path;
        }
        if let Some(at) = self.distrusted.iter().position(|(path, _)| {
            work::count(1);
            *path == cache_path
        }) {
            if self.distrusted[at].1 == stamp {
                // The cache file is exactly the one already refused.
                return None;
            }
            // The cache file changed: the old refusal no longer applies.
            self.distrusted.swap_remove(at);
        }
        if covered_changed {
            if !cache_changed {
                // The cache passes its own check yet something it covers
                // moved — remember the refusal; a changed cache file is
                // instead given one scanned load and judged fresh next
                // time on its own check.
                self.distrusted.push((cache_path.clone(), stamp));
            }
            return None;
        }
        match crate::cache::read_theme_cache(theme_dir, declared, &self.limits) {
            Ok(cache) => cache,
            Err(unusable) => {
                self.diagnostics.push(Diagnostic::new(
                    Some(cache_path),
                    format!("the icon cache is not used: {unusable}"),
                ));
                None
            }
        }
    }

    /// Adds `dir`'s icon files to `names` as copies of `candidate` with
    /// the formats found, by scanning.
    fn enumerate(
        &mut self,
        dir: &Path,
        candidate: Candidate,
        names: &mut Map<Str<Counted>, Vec<Candidate>>,
        longest: &mut usize,
    ) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            work::count(1 + name.len());
            let Some((stem, format)) = split_icon_name(&name) else {
                continue;
            };
            if !regular_file(&entry) {
                continue;
            }
            if self.icons_indexed >= self.limits.max_icons {
                self.truncated(dir);
                return;
            }
            self.icons_indexed += 1;
            push_candidate(names, stem, candidate, format, longest);
        }
    }

    /// The budget cut a directory short: say so, once per place.
    fn truncated(&mut self, dir: &Path) {
        self.diagnostics.push(Diagnostic::new(
            Some(dir.to_owned()),
            format!(
                "more than {} icons; the rest are skipped",
                self.limits.max_icons
            ),
        ));
    }

    /// Charges `formats` against the icon budget one format at a time —
    /// the same unit the scanning paths count, a file per format — and
    /// grants what still fit.
    fn charge(&mut self, formats: u8) -> u8 {
        let mut granted = 0;
        for bit in [PNG, SVG, XPM] {
            if formats & bit == 0 {
                continue;
            }
            if self.icons_indexed >= self.limits.max_icons {
                break;
            }
            self.icons_indexed += 1;
            granted |= bit;
        }
        granted
    }

    /// The unthemed fallback: files directly in the roots, then pixmaps.
    fn load_fallback(&mut self) {
        self.fallback_dirs = self
            .roots
            .clone()
            .into_iter()
            .chain([PathBuf::from(PIXMAPS)])
            .collect();
        for (at, dir) in self.fallback_dirs.clone().iter().enumerate() {
            self.stamps.push((dir.clone(), FileStamp::of(dir)));
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let name = entry.file_name();
                work::count(1 + name.len());
                let Some((stem, format)) = split_icon_name(&name) else {
                    continue;
                };
                if !regular_file(&entry) {
                    continue;
                }
                if self.icons_indexed >= self.limits.max_icons {
                    self.truncated(dir);
                    return;
                }
                self.icons_indexed += 1;
                self.fallback_longest = self.fallback_longest.max(stem.len());
                let lookup = self.fallback.lookup(stem);
                if let Some(found) = self.fallback.get_mut_at(&lookup) {
                    // Fallback directories are visited in order too.
                    match found.last_mut() {
                        Some(had) if had.dir == u32::try_from(at).unwrap_or(u32::MAX) => {
                            had.formats |= format;
                        }
                        _ => found.push(Unthemed {
                            dir: u32::try_from(at).unwrap_or(u32::MAX),
                            formats: format,
                        }),
                    }
                } else {
                    self.fallback.insert_at(
                        &lookup,
                        vec![Unthemed {
                            dir: u32::try_from(at).unwrap_or(u32::MAX),
                            formats: format,
                        }],
                    );
                }
            }
        }
    }
}

/// Adds one found file to the name index, merging formats into an
/// existing candidate for the same place.
fn push_candidate(
    names: &mut Map<Str<Counted>, Vec<Candidate>>,
    stem: &str,
    candidate: Candidate,
    format: u8,
    longest: &mut usize,
) {
    *longest = (*longest).max(stem.len());
    // The name hashed and, when new, copied (counted by the map).
    work::count(1);
    let lookup = names.lookup(stem);
    if let Some(found) = names.get_mut_at(&lookup) {
        // Candidates arrive ordered by (dir, root) — directories are
        // outer, roots inner during the build — so a repeat of the same
        // place can only be the last entry: one comparison, however many
        // directories carry the name.
        match found.last_mut() {
            Some(had) if had.dir == candidate.dir && had.root == candidate.root => {
                had.formats |= format;
            }
            _ => found.push(Candidate {
                formats: format,
                ..candidate
            }),
        }
    } else {
        names.insert_at(
            &lookup,
            vec![Candidate {
                formats: format,
                ..candidate
            }],
        );
    }
}

/// Whether a directory entry is a regular file, a symlink resolving to
/// one included: a directory named `x.png`, a dangling link or a FIFO is
/// no icon, and returning one could mask a usable candidate elsewhere or
/// block the consumer's read.
fn regular_file(entry: &std::fs::DirEntry) -> bool {
    match entry.file_type() {
        Ok(kind) if kind.is_file() => true,
        Ok(kind) if kind.is_symlink() => {
            std::fs::metadata(entry.path()).is_ok_and(|found| found.is_file())
        }
        _ => false,
    }
}

/// Splits `file.png`-style names into the icon name and its format bit.
fn split_icon_name(name: &std::ffi::OsStr) -> Option<(&str, u8)> {
    let name = name.to_str()?;
    let (stem, extension) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    let format = match extension {
        "png" => PNG,
        "svg" => SVG,
        "xpm" => XPM,
        _ => return None,
    };
    Some((stem, format))
}
