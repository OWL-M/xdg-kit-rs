//! Loading: finding the menu file, reading it and everything it merges
//! (spec "Merging", first step), legacy hierarchies, and the files the
//! resolution asks for — every one stamped, missing ones included, so a
//! refresh notices any of them appearing, changing or going.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use xdg_kit_basedir::FileStamp;
use xdg_kit_desktop_entry::{AppIndex, DesktopEntry, EntryType};

use crate::merge::Defaults;
use crate::model::{self, HARD_DEPTH, Item, MenuNode, MergeTarget, Rule};
use crate::resolve::{DirPool, PoolEntry, Provider, Slot};
use crate::{Diagnostic, MenuContext, MenuError, work};

/// Everything a load read, for the tree and its refresh.
pub(crate) struct Loader<'a> {
    pub context: &'a MenuContext,
    /// The name default merge directories are named after (`applications`
    /// for `…applications.menu`).
    pub merge_base: String,
    pub diagnostics: Vec<Diagnostic>,
    pub stamps: Vec<(PathBuf, FileStamp)>,
    pub stamped: HashSet<PathBuf>,
    pub files: Vec<PathBuf>,
    pub indices: Vec<AppIndex>,
    /// Each entry with a `TryExec` that was checked, and the verdict: the
    /// program may appear, go or change its mode later.
    pub try_execs: Vec<(Arc<DesktopEntry>, bool)>,
    pub degraded: bool,
    elements_left: usize,
    files_loaded: usize,
    menus: usize,
    /// The (device, inode) of each file being merged, outermost first: a
    /// merge of one of them is a cycle. A file merged again elsewhere is
    /// not.
    expanding: Vec<(u64, u64)>,
    app_dirs: HashMap<PathBuf, DirPool>,
    legacy: HashMap<(PathBuf, String), DirPool>,
    directories: HashMap<PathBuf, Option<Arc<DesktopEntry>>>,
}

impl<'a> Loader<'a> {
    pub(crate) fn new(context: &'a MenuContext, merge_base: String) -> Self {
        Self {
            context,
            merge_base,
            diagnostics: Vec::new(),
            stamps: Vec::new(),
            stamped: HashSet::new(),
            files: Vec::new(),
            indices: Vec::new(),
            try_execs: Vec::new(),
            degraded: false,
            elements_left: context.limits.max_elements,
            files_loaded: 0,
            menus: 0,
            expanding: Vec::new(),
            app_dirs: HashMap::new(),
            legacy: HashMap::new(),
            directories: HashMap::new(),
        }
    }

    /// What `<Move>`s may still build, past what loading built.
    pub(crate) fn move_limits(&self) -> crate::merge::MoveLimits {
        crate::merge::MoveLimits {
            max_depth: self.max_depth(),
            menus_left: self.context.limits.max_menus.saturating_sub(self.menus),
            steps_left: self.context.limits.max_matches,
        }
    }

    fn max_depth(&self) -> usize {
        self.context.limits.max_depth.min(HARD_DEPTH)
    }

    fn note(&mut self, path: Option<&Path>, message: impl Into<String>) {
        self.diagnostics
            .push(Diagnostic::new(path.map(Path::to_owned), None, message));
    }

    /// Stamps `path` once, whether or not it exists.
    pub(crate) fn stamp(&mut self, path: &Path) {
        work::count(1 + path.as_os_str().len());
        if self.stamped.insert(path.to_owned()) {
            self.stamps.push((path.to_owned(), FileStamp::of(path)));
        }
    }

    /// The configuration directories, most important first.
    fn config_dirs(&self) -> Vec<PathBuf> {
        let base = &self.context.base_dirs;
        std::iter::once(base.config_home().to_owned())
            .chain(base.config_dirs().iter().cloned())
            .collect()
    }

    pub(crate) fn defaults(&self) -> Defaults {
        let base = &self.context.base_dirs;
        let data: Vec<PathBuf> = std::iter::once(base.data_home().to_owned())
            .chain(base.data_dirs().iter().cloned())
            .collect();
        Defaults {
            app_dirs: data.iter().map(|dir| dir.join("applications")).collect(),
            directory_dirs: data
                .iter()
                .map(|dir| dir.join("desktop-directories"))
                .collect(),
        }
    }

    /// The first `menus/<name>` of the configuration directories.
    pub(crate) fn find(&mut self, name: &str) -> Result<PathBuf, MenuError> {
        let mut searched = Vec::new();
        for dir in self.config_dirs() {
            let path = dir.join("menus").join(name);
            self.stamp(&path);
            if fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
                return Ok(path);
            }
            searched.push(path);
        }
        Err(MenuError::NotFound { searched })
    }

    /// Reads the root menu file, merges everything it merges.
    pub(crate) fn load_root(&mut self, path: &Path) -> Result<MenuNode, MenuError> {
        self.stamp(path);
        let bytes = xdg_kit_basedir::read_regular_file_with(path, self.context.limits.max_file_len)
            .map_err(|err| match err.kind() {
                io::ErrorKind::FileTooLarge => too_large(path),
                _ => MenuError::Unreadable(path.to_owned(), err),
            })?;
        let mut node = self.parse(path, &bytes).map_err(|(line, message, limit)| {
            if limit {
                MenuError::Limit {
                    message: format!(
                        "{}: {message}",
                        crate::error::shown(&path.to_string_lossy())
                    ),
                }
            } else {
                MenuError::Malformed {
                    path: path.to_owned(),
                    line,
                    message,
                }
            }
        })?;
        if node.name.is_none() {
            return Err(MenuError::Malformed {
                path: path.to_owned(),
                line: None,
                message: "the root <Menu> has no usable <Name>".to_owned(),
            });
        }
        self.files.push(path.to_owned());
        let identity = identity(path);
        self.expanding.extend(identity);
        self.files_loaded = 1;
        self.expand(&mut node, 0, 0)?;
        self.expanding.clear();
        Ok(node)
    }

    /// The `<Menu>` of a file's bytes, or why not.
    fn parse(
        &mut self,
        path: &Path,
        bytes: &[u8],
    ) -> Result<MenuNode, (Option<usize>, String, bool)> {
        let element = crate::xml::parse(bytes, self.max_depth(), &mut self.elements_left)
            .map_err(|failure| (failure.line, failure.message, failure.limit))?;
        let base = path.parent().unwrap_or(Path::new("/")).to_owned();
        let mut reader = model::Reader {
            file: path,
            base: &base,
            diagnostics: &mut self.diagnostics,
        };
        model::menu(&element, &mut reader).ok_or_else(|| {
            (
                Some(element.line),
                "the root element is not <Menu>".to_owned(),
                false,
            )
        })
    }

    /// Replaces the merge items of `node` and of the menus below it by what
    /// they merge. `depth` counts merges, `menu_depth` menus.
    fn expand(
        &mut self,
        node: &mut MenuNode,
        depth: usize,
        menu_depth: usize,
    ) -> Result<(), MenuError> {
        drop_duplicate_merges(node);
        let items = std::mem::take(&mut node.items);
        for item in items {
            work::count(1);
            match item {
                Item::Menu(mut child) => {
                    self.menus += 1;
                    if self.menus > self.context.limits.max_menus {
                        return Err(MenuError::Limit {
                            message: format!("more than {} menus", self.context.limits.max_menus),
                        });
                    }
                    if menu_depth + 1 > self.max_depth() {
                        return Err(MenuError::Limit {
                            message: format!("menus nested more than {} deep", self.max_depth()),
                        });
                    }
                    self.expand(&mut child, depth, menu_depth + 1)?;
                    node.items.push(Item::Menu(child));
                }
                Item::MergeFile(MergeTarget::Path(path)) => {
                    let merged = self.merge_file(&path, depth, menu_depth)?;
                    Self::splice(node, merged);
                }
                Item::MergeFile(MergeTarget::Parent { containing }) => {
                    if let Some(path) = self.parent_file(&containing) {
                        let merged = self.merge_file(&path, depth, menu_depth)?;
                        Self::splice(node, merged);
                    }
                }
                Item::MergeDir(dir) => {
                    let merged = self.merge_dir(&dir, depth, menu_depth)?;
                    Self::splice(node, merged);
                }
                Item::DefaultMergeDirs => {
                    // The more important directories are merged later, so
                    // that their items take precedence.
                    let name = format!("{}-merged", self.merge_base);
                    for dir in self.config_dirs().into_iter().rev() {
                        let merged =
                            self.merge_dir(&dir.join("menus").join(&name), depth, menu_depth)?;
                        Self::splice(node, merged);
                    }
                }
                Item::LegacyDir { path, prefix } => {
                    let items = self.legacy_dir(&path, &prefix, menu_depth)?;
                    node.items.extend(items);
                }
                Item::KdeLegacyDirs => {
                    let dirs = self.context.kde_legacy_dirs.clone();
                    if dirs.is_empty() {
                        self.note(
                            None,
                            "<KDELegacyDirs> needs `kde-config --path apps`, which loading never runs; \
                             no directories were given (MenuContext::kde_legacy_dirs), so none are loaded",
                        );
                    }
                    for dir in dirs.iter().rev() {
                        let items = self.legacy_dir(dir, "kde-", menu_depth)?;
                        node.items.extend(items);
                    }
                }
                other => node.items.push(other),
            }
        }
        Ok(())
    }

    /// Splices a merged file's items into `node`, recording the file.
    fn splice(node: &mut MenuNode, merged: Option<(PathBuf, Vec<Item>)>) {
        if let Some((path, items)) = merged {
            if !node.sources.contains(&path) {
                node.sources.push(path);
            }
            node.items.extend(items);
        }
    }

    /// The items of the file at `path`, merged: `None` when there is no
    /// such file, it is a cycle, or it cannot be used (a diagnostic, and
    /// the tree is marked degraded).
    fn merge_file(
        &mut self,
        path: &Path,
        depth: usize,
        menu_depth: usize,
    ) -> Result<Option<(PathBuf, Vec<Item>)>, MenuError> {
        self.stamp(path);
        let Some(id) = identity(path) else {
            return Ok(None);
        };
        if self.expanding.contains(&id) {
            self.note(
                Some(path),
                "merges a file that is already being merged (a cycle); skipped",
            );
            return Ok(None);
        }
        let limits = &self.context.limits;
        if depth + 1 > limits.max_merge_depth.min(HARD_DEPTH) {
            return Err(MenuError::Limit {
                message: format!(
                    "merges nested more than {} deep",
                    limits.max_merge_depth.min(HARD_DEPTH)
                ),
            });
        }
        self.files_loaded += 1;
        if self.files_loaded > limits.max_files {
            return Err(MenuError::Limit {
                message: format!("more than {} menu files", limits.max_files),
            });
        }
        let bytes = match xdg_kit_basedir::read_regular_file_with(path, limits.max_file_len) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::FileTooLarge => return Err(too_large(path)),
            Err(err) => {
                self.degraded = true;
                self.note(Some(path), format!("cannot be read: {err}; not merged"));
                return Ok(None);
            }
        };
        let node = match self.parse(path, &bytes) {
            Ok(node) => node,
            Err((_, message, true)) => {
                return Err(MenuError::Limit {
                    message: format!(
                        "{}: {message}",
                        crate::error::shown(&path.to_string_lossy())
                    ),
                });
            }
            Err((line, message, false)) => {
                self.degraded = true;
                self.diagnostics.push(Diagnostic::new(
                    Some(path.to_owned()),
                    line,
                    format!("{message}; not merged"),
                ));
                return Ok(None);
            }
        };
        self.files.push(path.to_owned());
        // The merged root's <Name> is ignored; its items join the menu.
        let mut wrapper = MenuNode {
            name: None,
            items: node.items,
            sources: Vec::new(),
        };
        self.expanding.push(id);
        let expanded = self.expand(&mut wrapper, depth + 1, menu_depth);
        self.expanding.pop();
        expanded?;
        Ok(Some((path.to_owned(), wrapper.items)))
    }

    /// The `.menu` files of `dir`, merged in byte order of their names (the
    /// spec leaves the order open).
    fn merge_dir(
        &mut self,
        dir: &Path,
        depth: usize,
        menu_depth: usize,
    ) -> Result<Option<(PathBuf, Vec<Item>)>, MenuError> {
        self.stamp(dir);
        let names = self.list(dir)?;
        let mut items = Vec::new();
        let mut any = false;
        for name in names {
            if !name.as_bytes().ends_with(b".menu") {
                continue;
            }
            let path = dir.join(&name);
            if let Some((path, merged)) = self.merge_file(&path, depth, menu_depth)? {
                any = true;
                if !self.files.contains(&path) {
                    self.files.push(path);
                }
                items.extend(merged);
            }
        }
        Ok(any.then(|| (dir.to_owned(), items)))
    }

    /// The names in `dir`, sorted; none when it is missing.
    fn list(&mut self, dir: &Path) -> Result<Vec<OsString>, MenuError> {
        let listed = match fs::read_dir(dir) {
            Ok(listed) => listed,
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(Vec::new());
            }
            Err(err) => {
                self.degraded = true;
                self.note(Some(dir), format!("cannot be listed: {err}"));
                return Ok(Vec::new());
            }
        };
        let mut names = Vec::new();
        for entry in listed {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    self.degraded = true;
                    self.note(Some(dir), format!("cannot be listed in full: {err}"));
                    break;
                }
            };
            if names.len() >= self.context.limits.max_listed {
                return Err(MenuError::Limit {
                    message: format!(
                        "{} has more than {} entries",
                        crate::error::shown(&dir.to_string_lossy()),
                        self.context.limits.max_listed
                    ),
                });
            }
            let name = entry.file_name();
            work::count(1 + name.len());
            names.push(name);
        }
        names.sort_unstable();
        Ok(names)
    }

    /// The file `<MergeFile type="parent">` in `containing` merges: the same
    /// relative path in the configuration directories after the one
    /// `containing` is in.
    fn parent_file(&mut self, containing: &Path) -> Option<PathBuf> {
        let dirs = self.config_dirs();
        let Some((at, relative)) = dirs.iter().enumerate().find_map(|(at, dir)| {
            containing
                .strip_prefix(dir)
                .ok()
                .map(|relative| (at, relative.to_owned()))
        }) else {
            self.note(
                Some(containing),
                "<MergeFile type=\"parent\"> in a file outside the configuration directories; ignored",
            );
            return None;
        };
        for dir in &dirs[at + 1..] {
            let candidate = dir.join(&relative);
            self.stamp(&candidate);
            if fs::metadata(&candidate).is_ok() {
                return Some(candidate);
            }
        }
        None
    }

    /// A legacy hierarchy as menu items: the directory's applications as a
    /// legacy application directory and its `.directory` files'
    /// directory, an `<Include>` of each application without `Categories`
    /// directly in the directory, and a menu for each folder below.
    fn legacy_dir(
        &mut self,
        root: &Path,
        prefix: &str,
        menu_depth: usize,
    ) -> Result<Vec<Item>, MenuError> {
        self.stamp(root);
        if !fs::metadata(root).is_ok_and(|metadata| metadata.is_dir()) {
            return Ok(Vec::new());
        }
        let mut pool: Vec<(Arc<str>, Slot)> = Vec::new();
        let mut seen: HashSet<Arc<str>> = HashSet::new();
        let mut ancestors = HashSet::new();
        let node = self.legacy_folder(
            root,
            root,
            &[],
            prefix,
            menu_depth,
            &mut pool,
            &mut seen,
            &mut ancestors,
        )?;
        pool.sort_by(|(a, _), (b, _)| a.cmp(b));
        self.legacy
            .insert((root.to_owned(), prefix.to_owned()), Arc::new(pool));
        let mut items = vec![
            Item::LegacyAppDir {
                path: root.to_owned(),
                prefix: prefix.to_owned(),
            },
            Item::DirectoryDir(root.to_owned()),
        ];
        items.extend(node.items);
        Ok(items)
    }

    #[allow(clippy::too_many_arguments)]
    fn legacy_folder(
        &mut self,
        root: &Path,
        dir: &Path,
        relative: &[String],
        prefix: &str,
        menu_depth: usize,
        pool: &mut Vec<(Arc<str>, Slot)>,
        seen: &mut HashSet<Arc<str>>,
        ancestors: &mut HashSet<(u64, u64)>,
    ) -> Result<MenuNode, MenuError> {
        self.stamp(dir);
        let mut node = MenuNode {
            name: relative.last().cloned(),
            items: Vec::new(),
            sources: Vec::new(),
        };
        let Some(id) = identity(dir) else {
            return Ok(node);
        };
        if !ancestors.insert(id) {
            self.note(Some(dir), "a link back to a folder above it; not followed");
            return Ok(node);
        }
        let mut includes = Vec::new();
        for name in self.list(dir)? {
            let path = dir.join(&name);
            let Ok(metadata) = fs::metadata(&path) else {
                continue;
            };
            let Some(text) = name.to_str() else {
                self.note(Some(&path), "a name that is not UTF-8; skipped");
                continue;
            };
            if metadata.is_dir() {
                if menu_depth + relative.len() + 1 > self.max_depth() {
                    return Err(MenuError::Limit {
                        message: format!(
                            "a legacy hierarchy nested more than {} deep",
                            self.max_depth()
                        ),
                    });
                }
                self.menus += 1;
                if self.menus > self.context.limits.max_menus {
                    return Err(MenuError::Limit {
                        message: format!("more than {} menus", self.context.limits.max_menus),
                    });
                }
                let mut below = relative.to_vec();
                below.push(text.to_owned());
                let child = self.legacy_folder(
                    root, &path, &below, prefix, menu_depth, pool, seen, ancestors,
                )?;
                node.items.push(Item::Menu(Box::new(child)));
            } else if text == ".directory" {
                let mut parts = relative.to_vec();
                parts.push(".directory".to_owned());
                node.items.push(Item::Directory(parts.join("/")));
            } else if text.ends_with(".desktop") && metadata.is_file() {
                self.legacy_entry(root, &path, text, prefix, pool, seen, &mut includes)?;
            }
        }
        ancestors.remove(&id);
        if !includes.is_empty() {
            node.items.insert(0, Item::Include(includes));
        }
        Ok(node)
    }

    #[allow(clippy::too_many_arguments)]
    fn legacy_entry(
        &mut self,
        root: &Path,
        path: &Path,
        name: &str,
        prefix: &str,
        pool: &mut Vec<(Arc<str>, Slot)>,
        seen: &mut HashSet<Arc<str>>,
        includes: &mut Vec<Rule>,
    ) -> Result<(), MenuError> {
        self.stamp(path);
        let id: Arc<str> = Arc::from(format!("{prefix}{name}"));
        if seen.contains(&id) {
            self.note(
                Some(path),
                format!("a second legacy entry with the ID {id}; the first is kept"),
            );
            return Ok(());
        }
        let read = match xdg_kit_basedir::read_stamped(path, self.context.limits.max_file_len) {
            Ok(Some(read)) => read,
            Ok(None) => return Ok(()),
            Err(err) if err.kind() == io::ErrorKind::FileTooLarge => return Err(too_large(path)),
            Err(err) => {
                self.note(Some(path), format!("cannot be read: {err}; skipped"));
                return Ok(());
            }
        };
        work::count(read.bytes.len());
        let entry = Arc::new(DesktopEntry::parse_at(&read.bytes, path));
        seen.insert(Arc::clone(&id));
        if entry.hidden() {
            pool.push((id, Slot::Hidden));
            return Ok(());
        }
        if entry.entry_type() != Some(EntryType::Application) || !self.available(&entry) {
            return Ok(());
        }
        let has_categories = entry.string("Categories").is_some();
        let mut categories = entry.categories();
        categories.push("Legacy".to_owned());
        if !has_categories {
            includes.push(Rule::Filename(id.to_string()));
        }
        pool.push((
            Arc::clone(&id),
            Slot::Entry(PoolEntry {
                id,
                entry,
                categories: categories.into(),
                app_dir: Arc::from(root),
                legacy: true,
            }),
        ));
        Ok(())
    }

    /// Whether `entry` passes the `TryExec` check, when one is asked for;
    /// a verdict that depends on a program is recorded for refreshes.
    fn available(&mut self, entry: &Arc<DesktopEntry>) -> bool {
        let Some(search_path) = &self.context.try_exec else {
            return true;
        };
        let available = entry.try_exec_available(search_path);
        if entry.try_exec().is_some() {
            self.try_execs.push((Arc::clone(entry), available));
        }
        available
    }
}

impl Provider for Loader<'_> {
    fn app_dir(&mut self, path: &Path) -> Result<DirPool, MenuError> {
        if let Some(pool) = self.app_dirs.get(path) {
            return Ok(Arc::clone(pool));
        }
        self.stamp(path);
        let index = AppIndex::from_dirs_with([path], &self.context.limits.for_entries());
        for diagnostic in index.diagnostics() {
            self.diagnostics.push(Diagnostic::new(
                diagnostic.path.clone(),
                diagnostic.line,
                diagnostic.message.clone(),
            ));
        }
        if !index.is_complete() {
            return Err(MenuError::Limit {
                message: format!(
                    "the desktop entries under {} could not all be read (see the diagnostics); a menu \
                     without them could show what they would hide",
                    crate::error::shown(&path.to_string_lossy())
                ),
            });
        }
        let app_dir: Arc<Path> = Arc::from(path);
        let mut pool: Vec<(Arc<str>, Slot)> = Vec::new();
        for id in index.dir_ids(0) {
            work::count(1 + id.len());
            let id_arc: Arc<str> = Arc::from(id);
            match index.get(id) {
                None => pool.push((id_arc, Slot::Hidden)),
                Some(app) => {
                    if app.entry().entry_type() != Some(EntryType::Application) {
                        continue;
                    }
                    let entry = Arc::new(app.entry().clone());
                    if !self.available(&entry) {
                        // Ignored as if absent: a less important copy of the
                        // ID may take its place.
                        continue;
                    }
                    pool.push((
                        Arc::clone(&id_arc),
                        Slot::Entry(PoolEntry {
                            id: id_arc,
                            categories: entry.categories().into(),
                            entry,
                            app_dir: Arc::clone(&app_dir),
                            legacy: false,
                        }),
                    ));
                }
            }
        }
        pool.sort_by(|(a, _), (b, _)| a.cmp(b));
        let pool = Arc::new(pool);
        self.app_dirs.insert(path.to_owned(), Arc::clone(&pool));
        self.indices.push(index);
        Ok(pool)
    }

    fn legacy_dir(&mut self, path: &Path, prefix: &str) -> Result<DirPool, MenuError> {
        Ok(self
            .legacy
            .get(&(path.to_owned(), prefix.to_owned()))
            .cloned()
            .unwrap_or_default())
    }

    fn directory(
        &mut self,
        dir: &Path,
        relative: &str,
    ) -> Result<Option<Arc<DesktopEntry>>, MenuError> {
        let path = dir.join(relative);
        if let Some(found) = self.directories.get(&path) {
            return Ok(found.clone());
        }
        self.stamp(&path);
        let found = match xdg_kit_basedir::read_stamped(&path, self.context.limits.max_file_len) {
            Ok(Some(read)) => {
                work::count(read.bytes.len());
                let entry = DesktopEntry::parse_at(&read.bytes, &path);
                if entry.hidden() {
                    None
                } else if entry
                    .entry_type()
                    .is_some_and(|kind| kind != EntryType::Directory)
                {
                    self.note(
                        Some(&path),
                        "a .directory file whose Type is not Directory; ignored",
                    );
                    None
                } else {
                    Some(Arc::new(entry))
                }
            }
            Ok(None) => None,
            Err(err) if err.kind() == io::ErrorKind::FileTooLarge => return Err(too_large(&path)),
            Err(err) => {
                if err.kind() != io::ErrorKind::NotFound {
                    self.note(Some(&path), format!("cannot be read: {err}; ignored"));
                }
                None
            }
        };
        self.directories.insert(path, found.clone());
        Ok(found)
    }
}

/// The error for a file over the size limit: never read as absent, since
/// what it says (`Hidden`, `NoDisplay`, merged rules) may be what hides
/// something.
fn too_large(path: &Path) -> MenuError {
    MenuError::Limit {
        message: format!(
            "{} is larger than the size limit",
            crate::error::shown(&path.to_string_lossy())
        ),
    }
}

/// Of duplicate merge items (the same file or directory, or the same
/// default), only the last stays.
fn drop_duplicate_merges(node: &mut MenuNode) {
    #[derive(PartialEq, Eq, Hash)]
    enum Key {
        File(PathBuf),
        Parent,
        Dir(PathBuf),
        Defaults,
        Legacy(PathBuf, String),
        KdeLegacy,
    }
    let key = |item: &Item| match item {
        Item::MergeFile(MergeTarget::Path(path)) => Some(Key::File(path.clone())),
        Item::MergeFile(MergeTarget::Parent { .. }) => Some(Key::Parent),
        Item::MergeDir(path) => Some(Key::Dir(path.clone())),
        Item::DefaultMergeDirs => Some(Key::Defaults),
        Item::LegacyDir { path, prefix } => Some(Key::Legacy(path.clone(), prefix.clone())),
        Item::KdeLegacyDirs => Some(Key::KdeLegacy),
        _ => None,
    };
    let mut last = HashMap::new();
    for (at, item) in node.items.iter().enumerate() {
        if let Some(key) = key(item) {
            last.insert(key, at);
        }
    }
    if last.is_empty() {
        return;
    }
    let items = std::mem::take(&mut node.items);
    node.items = items
        .into_iter()
        .enumerate()
        .filter(|(at, item)| key(item).is_none_or(|key| last.get(&key) == Some(at)))
        .map(|(_, item)| item)
        .collect();
}

/// A file's (device, inode), links followed; `None` when it is missing.
fn identity(path: &Path) -> Option<(u64, u64)> {
    fs::metadata(path)
        .ok()
        .map(|metadata| (metadata.dev(), metadata.ino()))
}
