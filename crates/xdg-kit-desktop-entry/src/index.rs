//! The installed desktop entries by desktop file ID (Desktop Entry spec
//! "Desktop File ID").

use std::borrow::Borrow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use xdg_kit_basedir::__hashed::{Map, Set};
use xdg_kit_basedir::{BaseDirs, FileStamp};

use crate::error::{quoted, shown_path};
use crate::key_file::work;
use crate::{DesktopEntry, Desktops, Diagnostic, EntryType, Limits};

/// An installed desktop entry (an application, a link or a folder) with
/// its desktop file ID.
#[derive(Debug, Clone)]
pub struct AppEntry {
    id: String,
    path: PathBuf,
    entry: Arc<DesktopEntry>,
}

impl AppEntry {
    /// The desktop file ID, such as `org.example.FooViewer.desktop`.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The file it was read from.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The entry.
    #[must_use]
    pub fn entry(&self) -> &DesktopEntry {
        &self.entry
    }

    /// Whether `OnlyShowIn`/`NotShowIn` let the entry be shown on
    /// `desktops` (see [`DesktopEntry::shown_in`]).
    #[must_use]
    pub fn shown_in(&self, desktops: &Desktops) -> bool {
        self.entry.shown_in(desktops)
    }

    /// `NoDisplay`: installed, but not shown in menus.
    #[must_use]
    pub fn no_display(&self) -> bool {
        self.entry.no_display()
    }

    /// Whether `TryExec` indicates that the application is installed.
    /// Returns true if the key is missing or names an executable file.
    /// Absolute paths are checked directly. All other paths are looked up
    /// in the caller's `PATH`-style `search_path`, as the spec requires.
    /// An empty component of `search_path` means the current folder.
    #[must_use]
    pub fn try_exec(&self, search_path: &OsStr) -> bool {
        let Some(program) = self.entry.try_exec() else {
            return true;
        };
        let executable = |path: &Path| {
            fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
                && rustix::fs::access(path, rustix::fs::Access::EXEC_OK).is_ok()
        };
        if Path::new(&program).is_absolute() {
            return executable(Path::new(&program));
        }
        search_dirs(search_path).any(|dir| executable(&dir.join(&program)))
    }
}

/// The folders of a `PATH`-like list. An empty one is the current folder,
/// as for `execvp`, which a launch uses.
fn search_dirs(search_path: &OsStr) -> impl Iterator<Item = &Path> {
    search_path
        .as_bytes()
        .split(|&byte| byte == b':')
        .map(|dir| {
            if dir.is_empty() {
                Path::new(".")
            } else {
                Path::new(OsStr::from_bytes(dir))
            }
        })
}

/// One file found under an `applications` folder.
#[derive(Debug, Clone)]
struct Found {
    root: usize,
    id: String,
    path: PathBuf,
    stamp: FileStamp,
    entry: Arc<DesktopEntry>,
}

/// A desktop file ID in a sorted map or set, ordered as its text. In
/// counted tests each comparison counts the bytes it looks at: those the
/// two IDs share at the start, and one more. Outside counting this costs
/// nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SortedId(String);

impl Ord for SortedId {
    fn cmp(&self, other: &Self) -> Ordering {
        compared(self.0.as_bytes(), other.0.as_bytes())
    }
}

/// `a` against `b` in byte order. Counted tests count the bytes looked at:
/// those the two share at the start, and one more. Outside counting this
/// costs nothing.
fn compared(a: &[u8], b: &[u8]) -> Ordering {
    work::count_with(|| 1 + a.iter().zip(b).take_while(|(x, y)| x == y).count());
    a.cmp(b)
}

impl PartialOrd for SortedId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// For [`AppIndex::dir_holds`], which no counted path takes: a lookup by
/// `str` compares as `str` does, uncounted.
impl Borrow<str> for SortedId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

/// One root's walk ([`AppIndex::walk_root`]).
struct Walk {
    /// The (device, inode) of each folder on the current path, including the
    /// root. A link back to any of them is a loop.
    ancestors: HashSet<(u64, u64)>,
    /// The `.desktop` files found, as (path relative to the root, path,
    /// the file's device and inode).
    files: Vec<(PathBuf, PathBuf, (u64, u64))>,
    /// The entries listed so far, at most [`Limits::max_listed`].
    listed: usize,
    /// Whether the walk stopped at [`Limits::max_listed`].
    capped: bool,
}

/// A folder on the walk's stack ([`AppIndex::walk_root`]).
struct Level {
    /// Its (device, inode), kept in [`Walk::ancestors`] while it is on the
    /// stack.
    identity: (u64, u64),
    /// Its path's place in [`AppIndex::folders`] (not copied again).
    folder: usize,
    /// The remaining entries in name order: each name and whether the listing
    /// identified it as a symbolic link. Keeping names instead of `DirEntry`
    /// values lets the listing's folder handle close.
    entries: std::vec::IntoIter<(OsString, bool)>,
    /// Its path relative to the root.
    relative: PathBuf,
    /// How deep it is below the root.
    depth: usize,
}

/// The target information that determines whether the walk follows a
/// link: its type, device and inode, or the absence of a target. Unlike a
/// [`FileStamp`], this stays unchanged when the target is edited in place.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LinkStamp(Option<(fs::FileType, u64, u64)>);

impl LinkStamp {
    fn of(path: &Path) -> Self {
        Self(
            fs::metadata(path)
                .ok()
                .map(|metadata| (metadata.file_type(), metadata.dev(), metadata.ino())),
        )
    }
}

/// The installed desktop entries (applications, links and folders), indexed by
/// desktop file ID from the files under the data directories' `applications`
/// folders. The first file for an ID takes precedence. Unreadable files,
/// unsupported encodings, and missing or unknown `Type` values are ignored,
/// allowing a later file to supply the same ID.
///
/// Each path to a file gives it an ID under the spec's rules. A folder reached
/// under two names through a link gives its files IDs under both names, with
/// diagnostics for each path. If paths produce the same ID and lead to the same
/// file (device and inode), the file is listed once without a clash diagnostic.
///
/// A `.desktop` file or a folder whose name is not UTF-8 is left out, with a
/// diagnostic: a desktop file ID is a `String`, and read lossily the name
/// could take the ID of another. `GLib` and `update-desktop-database` keep
/// the name's bytes in the ID.
///
/// Links back to any ancestor folder are not followed. `GLib` and
/// `update-desktop-database` follow these loops until the path is too long: one
/// link produces dozens of IDs, and work grows exponentially with the number of
/// such links.
///
/// Each root's walk stops with a diagnostic past [`Limits::max_listed`]
/// entries listed, counted along every path. This bounds the
/// work caused by links. A folder is still read whole, so that the names
/// the cap keeps are its smallest, the same every time, but only the names
/// the cap still allows are held, so memory does not grow with its width.
/// A folder that cannot be listed in full also gets a diagnostic. This is
/// an immutable snapshot; [`AppIndex::refresh`] returns a new one when files
/// change.
#[derive(Debug)]
pub struct AppIndex {
    /// The `applications` folders, most important first, with stamps.
    roots: Vec<(PathBuf, FileStamp)>,
    /// Every folder scanned, with its stamp, so added files are noticed.
    folders: Vec<(PathBuf, FileStamp)>,
    found: Vec<Found>,
    /// Files skipped as unreadable, in an unsupported encoding or of an
    /// unknown or no type, with their stamps, so a fix to one is noticed.
    ignored: Vec<(PathBuf, FileStamp)>,
    /// Links the walk skipped and additional paths for an ID, together with
    /// their targets, so retargeting is detected.
    links: Vec<(PathBuf, LinkStamp)>,
    /// The winning file of each ID, as an index into `found`; `None` when
    /// it says `Hidden=true` (the entry is deleted). Sorted by ID, for
    /// [`AppIndex::iter`].
    by_id: BTreeMap<SortedId, Option<usize>>,
    /// The same as `by_id`, for looking an ID up: hashing costs the ID's
    /// length, where the sorted map compares it with several IDs, each
    /// compared up to where they differ.
    lookup: Map<work::Text, Option<usize>>,
    /// The IDs each root holds (by index into `roots`), shadowed ones
    /// included; files that could not be read or are ignored for their
    /// encoding or type are not counted, shadowed or not.
    held: Vec<BTreeSet<SortedId>>,
    /// Files a more important folder shadows, read and stamped so that
    /// `held` stays right and a change to one is noticed.
    shadowed: Vec<(PathBuf, FileStamp, Arc<DesktopEntry>)>,
    diagnostics: Vec<Diagnostic>,
    /// The limits it was read within.
    limits: Limits,
}

impl AppIndex {
    /// The desktop entries under `$XDG_DATA_HOME/applications` and each
    /// `$XDG_DATA_DIRS/applications`, in that order of importance.
    #[must_use]
    pub fn load(base_dirs: &BaseDirs) -> Self {
        Self::load_with(base_dirs, &Limits::default())
    }

    /// Loads entries as [`AppIndex::load`] does, using `limits`.
    /// [`AppIndex::refresh`] uses the same limits.
    #[must_use]
    pub fn load_with(base_dirs: &BaseDirs, limits: &Limits) -> Self {
        let dirs = std::iter::once(base_dirs.data_home())
            .chain(base_dirs.data_dirs().iter().map(PathBuf::as_path))
            .map(|dir| dir.join("applications"));
        Self::from_dirs_with(dirs, limits)
    }

    /// Loads desktop entries from exactly these `applications` folders, in
    /// priority order, highest first. Converts relative paths to absolute paths now, without
    /// following links, so later changes to the working directory do not
    /// change their location. Keeps the supplied path if conversion fails
    /// because it is empty or the current directory is gone.
    #[must_use]
    pub fn from_dirs(dirs: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
        Self::from_dirs_with(dirs, &Limits::default())
    }

    /// Reads folders as [`AppIndex::from_dirs`] does, using `limits`.
    /// [`AppIndex::refresh`] uses the same limits.
    #[must_use]
    pub fn from_dirs_with(
        dirs: impl IntoIterator<Item = impl Into<PathBuf>>,
        limits: &Limits,
    ) -> Self {
        let mut roots: Vec<(PathBuf, FileStamp)> = Vec::new();
        let mut seen = Set::<work::PathKey>::new();
        for dir in dirs {
            // Absolute now, so that a later change of folder does not move
            // it; as given when it cannot be.
            let dir = dir.into();
            let dir = std::path::absolute(&dir).unwrap_or(dir);
            // The path made absolute, then the repeat check: a step; the set
            // counts each byte hashed, compared on a match and copied if new.
            work::count(1 + dir.as_os_str().len());
            if seen.insert(&dir) {
                let stamp = FileStamp::of(&dir);
                roots.push((dir, stamp));
            }
        }
        Self::scan(roots, &HashMap::new(), *limits)
    }

    /// The limits it was read within.
    #[must_use]
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    fn scan(
        roots: Vec<(PathBuf, FileStamp)>,
        cache: &HashMap<PathBuf, (FileStamp, Arc<DesktopEntry>)>,
        limits: Limits,
    ) -> Self {
        let mut index = Self {
            roots: Vec::new(),
            folders: Vec::new(),
            found: Vec::new(),
            ignored: Vec::new(),
            links: Vec::new(),
            by_id: BTreeMap::new(),
            lookup: Map::new(),
            held: vec![BTreeSet::new(); roots.len()],
            shadowed: Vec::new(),
            diagnostics: Vec::new(),
            limits,
        };
        // A root reached again under another path is scanned once; it holds
        // what it held the first time.
        let mut roots_seen: HashMap<(u64, u64), usize> = HashMap::new();
        for (root_index, (root, _)) in roots.iter().enumerate() {
            if let Ok(metadata) = fs::metadata(root) {
                match roots_seen.entry((metadata.dev(), metadata.ino())) {
                    std::collections::hash_map::Entry::Occupied(first) => {
                        let held = index.held[*first.get()].clone();
                        // A step per ID copied, and one per byte.
                        work::count_with(|| held.iter().map(|id| 1 + id.0.len()).sum());
                        index.held[root_index] = held;
                        continue;
                    }
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        slot.insert(root_index);
                    }
                }
            }
            let mut files = index.walk_root(root);
            // Byte order of the relative paths, so `foo-bar.desktop` comes
            // before `foo/bar.desktop`.
            files.sort_by(|a, b| compared(a.0.as_os_str().as_bytes(), b.0.as_os_str().as_bytes()));
            let mut ids_here = Map::<work::Text, (PathBuf, (u64, u64))>::new();
            for (relative, path, file) in files {
                let Some(id) = index.id_of(&relative, &path) else {
                    continue;
                };
                // A step; the map counts each byte hashed, and compared on a
                // match.
                work::count(1);
                if let Some((first, first_file)) = ids_here.get(&id) {
                    // Skipped, but whether it is the first path's file
                    // changes when a link on the way is pointed elsewhere,
                    // so `refresh` notices. A step, and the path copied.
                    work::count(1 + path.as_os_str().len());
                    index.links.push((path.clone(), LinkStamp::of(&path)));
                    // The same file under another path (a link): kept once.
                    if *first_file == file {
                        continue;
                    }
                    // The first path checked as UTF-8 and escaped (2 per
                    // byte); the ID's first 32 characters, bounded.
                    work::count(2 * first.as_os_str().len());
                    index.report(Diagnostic::new(
                        Some(path.clone()),
                        None,
                        format!(
                            "desktop file ID {} is also {}; the spec leaves the choice open, the first is kept",
                            quoted(&id),
                            shown_path(first)
                        ),
                    ));
                    continue;
                }
                // A step, and the path copied; the map counts each byte of
                // the ID hashed again and copied.
                work::count(1 + path.as_os_str().len());
                ids_here.insert(&id, (path.clone(), file));
                // A more important folder has this ID: the file is read only
                // to know whether the folder holds it. A step; the map
                // counts each byte hashed, and compared on a match.
                work::count(1);
                let shadowed = index.lookup.contains_key(&id);
                let stamp = FileStamp::of(&path);
                // A step, and the path hashed and compared (counted even when
                // the cache is empty).
                work::count(1 + 2 * path.as_os_str().len());
                let entry = match cache.get(&path) {
                    Some((cached, entry)) if *cached == stamp => Arc::clone(entry),
                    _ => match DesktopEntry::read_with_limits(&path, &limits) {
                        Ok(entry) => Arc::new(entry),
                        Err(err) => {
                            index.report(Diagnostic::new(
                                Some(path.clone()),
                                None,
                                format!("cannot be read: {err}"),
                            ));
                            index.ignored.push((path, stamp));
                            continue;
                        }
                    },
                };
                if !shadowed {
                    for diagnostic in entry.key_file().diagnostics() {
                        index.report(diagnostic.clone());
                    }
                }
                if let Some(reason) = ignored_because(&entry) {
                    // The reason written.
                    work::count(reason.len());
                    index.report(Diagnostic::new(
                        Some(path.clone()),
                        None,
                        format!("{reason}; the file is ignored"),
                    ));
                    index.ignored.push((path, stamp));
                    continue;
                }
                if shadowed {
                    index.held[root_index].insert(SortedId(id));
                    index.shadowed.push((path, stamp, entry));
                    continue;
                }
                index.keep(root_index, id, path, stamp, entry);
            }
        }
        index.roots = roots;
        index
    }

    /// Reports the folder at `path`, whose name is not UTF-8, as left out.
    /// A link to it is stamped, as links the walk does not follow are, so
    /// that `refresh` notices when it leads elsewhere.
    fn left_out(&mut self, path: PathBuf, symlink: bool) {
        if symlink {
            // A step, and the path copied.
            work::count(1 + path.as_os_str().len());
            self.links.push((path.clone(), LinkStamp::of(&path)));
        }
        self.report(Diagnostic::new(Some(path), None, NOT_UTF8_FOLDER));
    }

    /// The desktop file ID of the file at `relative` under its root
    /// ([`desktop_file_id`]); `None`, with a diagnostic for `path`, when a
    /// name is not UTF-8.
    fn id_of(&mut self, relative: &Path, path: &Path) -> Option<String> {
        let id = desktop_file_id(relative);
        if id.is_none() {
            self.report(Diagnostic::new(Some(path.to_owned()), None, NOT_UTF8_FILE));
        }
        id
    }

    /// Adds `diagnostic`, counting it as made: a step, its path copied and
    /// its message written.
    fn report(&mut self, diagnostic: Diagnostic) {
        work::count_with(|| {
            1 + diagnostic
                .path
                .as_ref()
                .map_or(0, |path| path.as_os_str().len())
                + diagnostic.message.len()
        });
        self.diagnostics.push(diagnostic);
    }

    /// Keeps `entry`, read from `path` in the root `root`, as the winning
    /// file of `id`.
    fn keep(
        &mut self,
        root: usize,
        id: String,
        path: PathBuf,
        stamp: FileStamp,
        entry: Arc<DesktopEntry>,
    ) {
        let at = (!entry.hidden()).then_some(self.found.len());
        // The ID copied for the entry and for the folder's set.
        work::count(2 * id.len());
        self.found.push(Found {
            root,
            id: id.clone(),
            path,
            stamp,
            entry,
        });
        self.held[root].insert(SortedId(id.clone()));
        // The ID put in the lookup map too: a step; the map counts each
        // byte hashed, compared on a match and copied if new.
        work::count(1);
        self.lookup.insert(&id, at);
        self.by_id.insert(SortedId(id), at);
    }

    /// Collects `.desktop` files under `root` as (relative path, full path,
    /// device and inode). Each path gets its own ID, including both paths
    /// through a folder alias. Links back to ancestor folders are skipped. Past
    /// [`Limits::max_listed`] entries, the walk stops with a diagnostic.
    ///
    /// Uses an explicit stack so unlimited folder nesting cannot overflow the
    /// thread stack. Visits folders depth first in name order, handling each
    /// folder's entries before moving to the next.
    fn walk_root(&mut self, root: &Path) -> Vec<(PathBuf, PathBuf, (u64, u64))> {
        let mut walk = Walk {
            ancestors: HashSet::new(),
            files: Vec::new(),
            listed: 0,
            capped: false,
        };
        let mut stack = Vec::new();
        self.enter(root, PathBuf::new(), 0, &mut walk, &mut stack);
        while let Some(level) = stack.last_mut() {
            let Some((name, symlink)) = level.entries.next() else {
                if let Some(level) = stack.pop() {
                    walk.ancestors.remove(&level.identity);
                }
                continue;
            };
            let path = self.folders[level.folder].0.join(&name);
            let relative = level.relative.join(&name);
            let depth = level.depth;
            // Both paths made: one per byte of each.
            work::count(path.as_os_str().len() + relative.as_os_str().len());
            match fs::metadata(&path) {
                Ok(metadata) if metadata.is_dir() => {
                    if walk.capped {
                        // Past the cap no folder is entered or reported, but
                        // a link to one is stamped, so `refresh` notices it
                        // being retargeted. A step, and the path copied.
                        if symlink {
                            work::count(1 + path.as_os_str().len());
                            let stamp = LinkStamp::of(&path);
                            self.links.push((path, stamp));
                        }
                    } else if utf8(&name) {
                        self.enter(&path, relative, depth + 1, &mut walk, &mut stack);
                    } else {
                        self.left_out(path, symlink);
                    }
                }
                Ok(metadata) if metadata.is_file() && name.as_bytes().ends_with(b".desktop") => {
                    // A link left out ([`AppIndex::id_of`]) is stamped
                    // here, where the listing says it is a link.
                    if symlink && !utf8(&name) {
                        // A step, and the path copied.
                        work::count(1 + path.as_os_str().len());
                        let stamp = LinkStamp::of(&path);
                        self.links.push((path.clone(), stamp));
                    }
                    walk.files
                        .push((relative, path, (metadata.dev(), metadata.ino())));
                }
                // A link to nothing (yet), or to something that is neither a
                // folder nor a `.desktop` file: what it leads to changes when
                // the target appears or a link on the way is pointed
                // elsewhere, so `refresh` notices.
                _ if symlink => {
                    let stamp = LinkStamp::of(&path);
                    self.links.push((path, stamp));
                }
                _ => {}
            }
        }
        if walk.capped {
            let max = self.limits.max_listed.unwrap_or(usize::MAX);
            self.report(Diagnostic::new(
                Some(root.to_owned()),
                None,
                format!("more than {max} entries to list; not scanned further"),
            ));
        }
        walk.files
    }

    /// Enters `dir` at `relative` under the root and `depth` levels below it.
    /// Stamps the folder and pushes its entries onto `stack` in name order.
    /// Skips non-folders and loops back to ancestors (stamped as links). Stops
    /// entering folders once the walk reaches its cap.
    fn enter(
        &mut self,
        dir: &Path,
        relative: PathBuf,
        depth: usize,
        walk: &mut Walk,
        stack: &mut Vec<Level>,
    ) {
        if walk.capped {
            return;
        }
        let Ok(metadata) = fs::metadata(dir) else {
            return;
        };
        if !metadata.is_dir() {
            return;
        }
        let identity = (metadata.dev(), metadata.ino());
        if !walk.ancestors.insert(identity) {
            // A link back to a folder on the path (a loop) is not walked,
            // but what it leads to changes when a link on the way is pointed
            // elsewhere, so `refresh` notices. A step, and the path copied.
            work::count(1 + dir.as_os_str().len());
            self.links.push((dir.to_owned(), LinkStamp::of(dir)));
            return;
        }
        // A step, and the path copied.
        work::count(1 + dir.as_os_str().len());
        let folder = self.folders.len();
        self.folders.push((dir.to_owned(), FileStamp::of(dir)));
        if self.limits.max_depth.is_some_and(|max| depth > max) {
            self.report(Diagnostic::new(
                Some(dir.to_owned()),
                None,
                "folders nested too deep; not scanned further",
            ));
            walk.ancestors.remove(&identity);
            return;
        }
        // In name order, so the walk, and where a cap stops it, are the same
        // every time: of all the names, the smallest still allowed.
        let left = self.limits.max_listed.map(|max| max - walk.listed);
        let (entries, read, failed) = listing(dir, left);
        if let Some(err) = failed {
            self.report(Diagnostic::new(
                Some(dir.to_owned()),
                None,
                format!("cannot be listed: {err}; the entries not read are left out"),
            ));
        }
        // Past the cap the walk stops: what was found stays.
        if left.is_some_and(|left| read > left) {
            walk.capped = true;
        }
        walk.listed += entries.len();
        // Each entry kept, with whether the listing gives it as a link; the
        // entries are dropped, and the listing's handle with them. A step
        // per entry kept.
        let entries: Vec<_> = entries
            .into_iter()
            .map(|(name, entry)| {
                work::count(1);
                let symlink = entry.file_type().is_ok_and(|kind| kind.is_symlink());
                (name, symlink)
            })
            .collect();
        stack.push(Level {
            identity,
            folder,
            entries: entries.into_iter(),
            relative,
            depth,
        });
    }

    /// Returns a new snapshot if any source has changed, reusing entries whose
    /// files are unchanged. Returns `None` if nothing changed.
    ///
    /// Checks the [`FileStamp`] of each `applications` folder, sub-folder,
    /// entry and skipped file. Checks sub-folders through every path walked,
    /// following links, so retargeting a link along a path is detected.
    ///
    /// Links the walk did not follow are compared by target identity: type,
    /// device and inode, or absence of a target. These include loops back to
    /// ancestor folders, links to ignored or missing targets, links left out
    /// because their name is not UTF-8, links to folders met after the walk
    /// reaches its cap (whatever their name), and paths skipped because
    /// they duplicate an ID. Retargeting a link or replacing its target atomically (a new
    /// inode) is detected; editing the target in place is not. The new
    /// snapshot uses the same [`AppIndex::limits`].
    #[must_use]
    pub fn refresh(&self) -> Option<Self> {
        // A step per path checked, up to the first that changed; the stat
        // is file I/O.
        let changed = self
            .roots
            .iter()
            .chain(&self.folders)
            .chain(&self.ignored)
            .inspect(|_| work::count(1))
            .any(|(path, stamp)| FileStamp::of(path) != *stamp)
            || self
                .links
                .iter()
                .inspect(|_| work::count(1))
                .any(|(path, stamp)| LinkStamp::of(path) != *stamp)
            || self
                .found
                .iter()
                .inspect(|_| work::count(1))
                .any(|found| FileStamp::of(&found.path) != found.stamp)
            || self
                .shadowed
                .iter()
                .inspect(|_| work::count(1))
                .any(|(path, stamp, _)| FileStamp::of(path) != *stamp);
        if !changed {
            return None;
        }
        let cache = self.cache();
        let roots = self
            .roots
            .iter()
            .map(|(path, _)| {
                // A step, and the path copied.
                work::count(1 + path.as_os_str().len());
                (path.clone(), FileStamp::of(path))
            })
            .collect();
        Some(Self::scan(roots, &cache, self.limits))
    }

    /// The entries read, by path with their stamps, for a rebuild to reuse.
    fn cache(&self) -> HashMap<PathBuf, (FileStamp, Arc<DesktopEntry>)> {
        self.found
            .iter()
            .map(|found| (&found.path, &found.stamp, &found.entry))
            .chain(
                self.shadowed
                    .iter()
                    .map(|(path, stamp, entry)| (path, stamp, entry)),
            )
            .map(|(path, stamp, entry)| {
                // A step, and the path copied, hashed and compared (the
                // stamp is of fixed size, the entry an `Arc`).
                work::count(1 + 3 * path.as_os_str().len());
                (path.clone(), (stamp.clone(), Arc::clone(entry)))
            })
            .collect()
    }

    /// The desktop entry with desktop file ID `id`, unless its winning file
    /// says `Hidden=true`. It may be a link or a folder as well as an
    /// application (see [`DesktopEntry::entry_type`]).
    #[must_use]
    pub fn get(&self, id: &str) -> Option<AppEntry> {
        let at = (*self.find(id)?)?;
        Some(self.app(at))
    }

    /// The winning file of `id`, as [`AppIndex::by_id`] has it, looked up
    /// in [`AppIndex::lookup`].
    fn find(&self, id: &str) -> Option<&Option<usize>> {
        // A step; the map counts each byte hashed, and compared on a match.
        work::count(1);
        self.lookup.get(id)
    }

    fn app(&self, at: usize) -> AppEntry {
        let found = &self.found[at];
        // A step, and one per byte of the ID and path copied (the entry is
        // an `Arc`).
        work::count(1 + found.id.len() + found.path.as_os_str().len());
        AppEntry {
            id: found.id.clone(),
            path: found.path.clone(),
            entry: Arc::clone(&found.entry),
        }
    }

    /// Every desktop entry (applications, links and folders), by desktop
    /// file ID, without the hidden ones.
    pub fn iter(&self) -> impl Iterator<Item = AppEntry> + '_ {
        self.by_id
            .values()
            // A step per ID, hidden ones too.
            .inspect(|_| work::count(1))
            .filter_map(|at| at.map(|at| self.app(at)))
    }

    /// The `applications` folders, most important first, made absolute when
    /// given.
    #[must_use]
    pub fn dirs(&self) -> Vec<&Path> {
        // A step per folder listed.
        work::count(self.roots.len());
        self.roots.iter().map(|(path, _)| path.as_path()).collect()
    }

    /// Diagnostics for unreadable files, ignored encodings or types
    /// (including shadowed files), names that are not UTF-8, ID clashes
    /// between different files, and problems inside the winning files. Also
    /// reports incomplete or failed folder listings, folders skipped for
    /// exceeding [`Limits::max_depth`], and walks stopped at
    /// [`Limits::max_listed`] entries. A file reached through several paths
    /// has its problems reported under each.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Whether the folder `dir` (an index into [`AppIndex::dirs`]) holds a
    /// desktop file with ID `id`, even one a more important folder shadows
    /// or one that says `Hidden=true`. Files that could not be read, or that
    /// are ignored for their encoding or for an unknown or missing `Type`,
    /// do not count. The MIME Applications spec's rules work folder by
    /// folder and need this.
    #[must_use]
    pub fn dir_holds(&self, dir: usize, id: &str) -> bool {
        self.held.get(dir).is_some_and(|ids| ids.contains(id))
    }

    /// The desktop file IDs the folder `dir` holds, as for
    /// [`AppIndex::dir_holds`], in order.
    pub fn dir_ids(&self, dir: usize) -> impl Iterator<Item = &str> {
        self.held
            .get(dir)
            .into_iter()
            .flatten()
            .map(|id| id.0.as_str())
    }

    /// The folder (index into [`AppIndex::dirs`]) the desktop entry `id`
    /// came from.
    #[must_use]
    pub fn dir_of(&self, id: &str) -> Option<usize> {
        let at = (*self.find(id)?)?;
        Some(self.found[at].root)
    }
}

/// Why the index ignores `entry`, if it does. The spec: an implementation
/// that does not support an encoding should ignore the file, and
/// implementations should ignore desktop entries with an unknown type
/// (`Type` is required, so one without it is ignored too). `Hidden=true`
/// deletes the ID whatever the type; minimal deletion files often have
/// none.
fn ignored_because(entry: &DesktopEntry) -> Option<String> {
    if !entry.supports_encoding() {
        let encoding = entry.string("Encoding").unwrap_or_default();
        return Some(format!("Encoding={} is not supported", quoted(&encoding)));
    }
    if entry.hidden() {
        return None;
    }
    match entry.entry_type() {
        None => Some("no Type".to_owned()),
        Some(EntryType::Other(other)) => Some(format!(
            "Type={} is not a type the spec lists",
            quoted(&other)
        )),
        Some(_) => None,
    }
}

/// Lists the entries of `dir`, keeping the `most` smallest names (all when
/// `None`) in byte order with their entries. Also returns how many entries
/// were read, and any error that cut the listing short. If opening or
/// reading the folder is interrupted by a signal, discards the partial
/// listing and restarts, retrying as often as needed. `NotFound` when
/// opening the folder returns no entries and no error. Other errors end the
/// listing and keep what was read so far.
fn listing(
    dir: &Path,
    most: Option<usize>,
) -> (Vec<(OsString, fs::DirEntry)>, usize, Option<io::Error>) {
    'again: loop {
        let entries = match read_dir(dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            // Gone since it was looked at: as if it were never there.
            Err(err) if err.kind() == io::ErrorKind::NotFound => return (Vec::new(), 0, None),
            Err(err) => return (Vec::new(), 0, Some(err)),
        };
        let mut kept = Smallest::new(most);
        let mut read = 0;
        for entry in entries {
            #[cfg(test)]
            let entry = tests::listing_fault(dir, Some(read)).map_or(entry, Err);
            match entry {
                Ok(entry) => {
                    read += 1;
                    kept.add(entry);
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {
                    // A step per entry read and dropped.
                    work::count(read);
                    continue 'again;
                }
                Err(err) => return (kept.sorted(), read, Some(err)),
            }
        }
        return (kept.sorted(), read, None);
    }
}

/// A listed name with its entry, ordered by the name's bytes; counted
/// tests count each comparison as [`compared`] does.
struct Listed(OsString, fs::DirEntry);

impl PartialEq for Listed {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Listed {}

impl Ord for Listed {
    fn cmp(&self, other: &Self) -> Ordering {
        compared(self.0.as_bytes(), other.0.as_bytes())
    }
}

impl PartialOrd for Listed {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The smallest names of a listing, at most `most`, so that a wide folder
/// takes memory for the names the walk may still list, not for all.
/// Until `most` are held they are kept in read order, and sorted at the
/// end as a whole listing would be; from then on they are a max-heap
/// (built once, about 2 comparisons per name), and each later name is
/// compared with the largest and replaces it if smaller (about 2 log2
/// `most` comparisons). So `n` names cost O(n log `most`) comparisons.
struct Smallest {
    /// The most names held.
    most: usize,
    /// The names held while fewer than `most`.
    filling: Vec<Listed>,
    /// The names held once `most` were reached.
    full: BinaryHeap<Listed>,
}

impl Smallest {
    fn new(most: Option<usize>) -> Self {
        Self {
            most: most.unwrap_or(usize::MAX),
            filling: Vec::new(),
            full: BinaryHeap::new(),
        }
    }

    /// Adds `entry`, unless `most` smaller names are held. Its name is
    /// copied (a step, and one per byte) whether it is kept or not.
    fn add(&mut self, entry: fs::DirEntry) {
        let name = entry.file_name();
        work::count(1 + name.len());
        let listed = Listed(name, entry);
        if self.full.is_empty() {
            if self.filling.len() < self.most {
                self.filling.push(listed);
                #[cfg(test)]
                tests::note_held(self.filling.len());
                return;
            }
            if self.most == 0 {
                return;
            }
            self.full = BinaryHeap::from(std::mem::take(&mut self.filling));
        }
        if let Some(mut largest) = self.full.peek_mut()
            && listed < *largest
        {
            *largest = listed;
        }
    }

    /// The names held, in byte order, with their entries.
    fn sorted(self) -> Vec<(OsString, fs::DirEntry)> {
        let mut held = if self.full.is_empty() {
            self.filling
        } else {
            self.full.into_vec()
        };
        held.sort();
        held.into_iter()
            .map(|Listed(name, entry)| (name, entry))
            .collect()
    }
}

/// `fs::read_dir`; in tests, failing as `LISTING_FAULT` asks.
fn read_dir(dir: &Path) -> io::Result<fs::ReadDir> {
    #[cfg(test)]
    if let Some(err) = tests::listing_fault(dir, None) {
        return Err(err);
    }
    #[cfg(all(test, target_os = "linux"))]
    tests::note_open_folders();
    fs::read_dir(dir)
}

/// The desktop file ID of a file at `relative` under `applications`: its
/// components joined by `-`. `None` when a component is not UTF-8: a
/// `String` ID cannot hold it, and read lossily it could take the ID of
/// another name (`GLib` and `update-desktop-database` keep the bytes).
fn desktop_file_id(relative: &Path) -> Option<String> {
    let id = relative
        .iter()
        .map(OsStr::to_str)
        .collect::<Option<Vec<_>>>()
        .map(|parts| parts.join("-"));
    // A step; each byte of the path split into parts and checked as UTF-8
    // (2), and each byte of the ID written.
    work::count(1 + 2 * relative.as_os_str().len() + id.as_ref().map_or(0, String::len));
    id
}

/// Whether the name `name` is UTF-8, as a part of a desktop file ID must be
/// ([`desktop_file_id`]). Each byte is checked.
fn utf8(name: &OsStr) -> bool {
    work::count(name.len());
    name.to_str().is_some()
}

/// The diagnostic of a `.desktop` file whose name is not UTF-8.
const NOT_UTF8_FILE: &str = "name not in UTF-8, so it has no desktop file ID; the file is ignored";

/// The diagnostic of a folder whose name is not UTF-8.
const NOT_UTF8_FOLDER: &str =
    "name not in UTF-8, so its files have no desktop file ID; not scanned";

#[cfg(test)]
#[allow(
    clippy::missing_const_for_thread_local,
    reason = "its thread locals are const; clippy flags them on targets without native thread locals (Android, OpenBSD, illumos)"
)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;

    thread_local! {
        /// The folder whose listing fails on this thread: when opened
        /// (`None`) or once that many entries were read, with that error
        /// (a raw OS error). Forgotten once given if it is `EINTR`.
        pub(super) static LISTING_FAULT: RefCell<Option<(PathBuf, Option<usize>, i32)>> =
            const { RefCell::new(None) };
        /// The listings interrupted on this thread.
        static INTERRUPTED: Cell<usize> = const { Cell::new(0) };
        /// The most entries one listing held at once on this thread.
        static MOST_HELD: Cell<usize> = const { Cell::new(0) };
    }

    /// Notes that a listing holds `held` entries ([`MOST_HELD`]).
    pub(super) fn note_held(held: usize) {
        MOST_HELD.with(|most| most.set(most.get().max(held)));
    }

    #[cfg(target_os = "linux")]
    thread_local! {
        /// While set: a folder (canonical, as `/proc/self/fd` gives paths),
        /// and the most handles this process held open on it and under it
        /// when a folder listing began on this thread.
        static OPEN_FOLDERS: RefCell<Option<(PathBuf, usize)>> = const { RefCell::new(None) };
    }

    /// Notes the handles open on [`OPEN_FOLDERS`]' folder, if one is set.
    #[cfg(target_os = "linux")]
    pub(super) fn note_open_folders() {
        OPEN_FOLDERS.with_borrow_mut(|watched| {
            let Some((root, most)) = watched else {
                return;
            };
            let open = fs::read_dir("/proc/self/fd")
                .unwrap()
                .filter_map(|fd| fs::read_link(fd.ok()?.path()).ok())
                .filter(|target| target.starts_with(&*root))
                .count();
            *most = (*most).max(open);
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_deep_walk_holds_no_handle_on_the_folders_on_its_path() {
        // 200 levels, each with a sibling `z` listed after the way down,
        // so that every folder on the path has an entry left.
        let root = tempfile::tempdir().unwrap();
        let mut deepest = root.path().to_owned();
        for _ in 0..200 {
            fs::create_dir(deepest.join("z")).unwrap();
            deepest.push("d");
            fs::create_dir(&deepest).unwrap();
        }
        write(&deepest.join("a.desktop"), &app("a"));
        let limits = Limits {
            max_depth: None,
            ..Limits::default()
        };
        // `/proc/self/fd` gives canonical paths: the temporary folder may be
        // reached through a link.
        let watched = fs::canonicalize(root.path()).unwrap();
        // The count sees a handle held under the folder.
        OPEN_FOLDERS.set(Some((watched.clone(), 0)));
        let held = fs::File::open(root.path().join("d")).unwrap();
        note_open_folders();
        drop(held);
        assert_eq!(OPEN_FOLDERS.take(), Some((watched.clone(), 1)));
        OPEN_FOLDERS.set(Some((watched, 0)));
        let index = AppIndex::from_dirs_with([root.path()], &limits);
        let (_, most) = OPEN_FOLDERS.take().unwrap();
        assert!(index.diagnostics().is_empty(), "{:?}", index.diagnostics());
        assert!(
            index
                .get(&format!("{}a.desktop", "d-".repeat(200)))
                .is_some()
        );
        // Noted as each listing begins: every folder listed before was
        // closed when its listing ended.
        assert_eq!(most, 0, "{most} folders held open");
        let [recursive, own_stack] = both_walks(root.path(), limits);
        assert_eq!(own_stack, recursive);
    }

    /// The error [`LISTING_FAULT`] gives for listing `dir` at `at`, if any.
    /// Panics once listings on this thread were interrupted 100 times, so a
    /// test fails rather than hang.
    pub(super) fn listing_fault(dir: &Path, at: Option<usize>) -> Option<std::io::Error> {
        LISTING_FAULT.with_borrow_mut(|fault| {
            let (path, when, errno) = fault.as_ref()?;
            if path != dir || *when != at {
                return None;
            }
            let errno = *errno;
            if errno == rustix::io::Errno::INTR.raw_os_error() {
                *fault = None;
                let interrupted = INTERRUPTED.with(|count| count.replace(count.get() + 1));
                assert!(interrupted < 100, "interrupted again and again");
            }
            Some(std::io::Error::from_raw_os_error(errno))
        })
    }

    /// A recursive version of the walk, used to compare results with the
    /// version that keeps an explicit stack.
    impl AppIndex {
        /// The `.desktop` files under the root `root`, as (path relative to it,
        /// full path, device and inode). Every path gets its ID, a folder
        /// reached under two names (a link) under each; a link back to a folder
        /// on the path ends there. Past the cap entries the walk stops,
        /// with a diagnostic.
        fn walk_root_recursive(&mut self, root: &Path) -> Vec<(PathBuf, PathBuf, (u64, u64))> {
            let mut walk = Walk {
                ancestors: HashSet::new(),
                files: Vec::new(),
                listed: 0,
                capped: false,
            };
            self.walk_recursive(root, Path::new(""), 0, &mut walk);
            if walk.capped {
                self.report(Diagnostic::new(
                    Some(root.to_owned()),
                    None,
                    format!(
                        "more than {} entries to list; not scanned further",
                        self.limits.max_listed.unwrap_or(usize::MAX)
                    ),
                ));
            }
            walk.files
        }

        /// Collects the `.desktop` files under `dir` as (path relative to the
        /// root, full path, device and inode) in `walk`, and stamps every
        /// folder and every link it does not follow.
        fn walk_recursive(&mut self, dir: &Path, relative: &Path, depth: usize, walk: &mut Walk) {
            if walk.capped {
                return;
            }
            let Ok(metadata) = fs::metadata(dir) else {
                return;
            };
            if !metadata.is_dir() {
                return;
            }
            let identity = (metadata.dev(), metadata.ino());
            if !walk.ancestors.insert(identity) {
                // A link back to a folder on the path (a loop) is not walked,
                // but what it leads to changes when a link on the way is pointed
                // elsewhere, so `refresh` notices. A step, and the path copied.
                work::count(1 + dir.as_os_str().len());
                self.links.push((dir.to_owned(), LinkStamp::of(dir)));
                return;
            }
            self.walk_folder_recursive(dir, relative, depth, walk);
            walk.ancestors.remove(&identity);
        }

        /// `walk_recursive` in the folder `dir`, not on the path before.
        fn walk_folder_recursive(
            &mut self,
            dir: &Path,
            relative: &Path,
            depth: usize,
            walk: &mut Walk,
        ) {
            // A step, and the path copied.
            work::count(1 + dir.as_os_str().len());
            self.folders.push((dir.to_owned(), FileStamp::of(dir)));
            if self.limits.max_depth.is_some_and(|max| depth > max) {
                self.report(Diagnostic::new(
                    Some(dir.to_owned()),
                    None,
                    "folders nested too deep; not scanned further",
                ));
                return;
            }
            // Every name, in name order, cut below at the cap.
            let (mut entries, _, failed) = listing(dir, None);
            if let Some(err) = failed {
                self.report(Diagnostic::new(
                    Some(dir.to_owned()),
                    None,
                    format!("cannot be listed: {err}; the entries not read are left out"),
                ));
            }
            // Past the cap the walk stops: what was found stays.
            if let Some(max) = self.limits.max_listed {
                let left = max - walk.listed;
                if entries.len() > left {
                    entries.truncate(left);
                    walk.capped = true;
                }
            }
            walk.listed += entries.len();
            for (name, entry) in entries {
                let path = entry.path();
                let relative = relative.join(&name);
                // Both paths made: one per byte of each.
                work::count(path.as_os_str().len() + relative.as_os_str().len());
                match fs::metadata(&path) {
                    Ok(metadata) if metadata.is_dir() && walk.capped => {
                        if entry.file_type().is_ok_and(|kind| kind.is_symlink()) {
                            work::count(1 + path.as_os_str().len());
                            let stamp = LinkStamp::of(&path);
                            self.links.push((path, stamp));
                        }
                    }
                    Ok(metadata) if metadata.is_dir() && utf8(&name) => {
                        self.walk_recursive(&path, &relative, depth + 1, walk);
                    }
                    Ok(metadata) if metadata.is_dir() => {
                        let symlink = entry.file_type().is_ok_and(|kind| kind.is_symlink());
                        self.left_out(path, symlink);
                    }
                    Ok(metadata)
                        if metadata.is_file() && name.as_bytes().ends_with(b".desktop") =>
                    {
                        if entry.file_type().is_ok_and(|kind| kind.is_symlink()) && !utf8(&name) {
                            work::count(1 + path.as_os_str().len());
                            let stamp = LinkStamp::of(&path);
                            self.links.push((path.clone(), stamp));
                        }
                        walk.files
                            .push((relative, path, (metadata.dev(), metadata.ino())));
                    }
                    // A link to nothing (yet), or to something that is neither a
                    // folder nor a `.desktop` file: what it leads to changes when
                    // the target appears or a link on the way is pointed
                    // elsewhere, so `refresh` notices.
                    _ if entry.file_type().is_ok_and(|kind| kind.is_symlink()) => {
                        let stamp = LinkStamp::of(&path);
                        self.links.push((path, stamp));
                    }
                    _ => {}
                }
            }
        }
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn app(name: &str) -> String {
        format!("[Desktop Entry]\nType=Application\nName={name}\nExec={name}\n")
    }

    #[test]
    fn ids_come_from_the_path_and_the_first_folder_wins() {
        let high = tempfile::tempdir().unwrap();
        let low = tempfile::tempdir().unwrap();
        write(&high.path().join("org.foo.desktop"), &app("high"));
        write(&low.path().join("org.foo.desktop"), &app("low"));
        write(&low.path().join("kde/konsole.desktop"), &app("konsole"));
        write(&low.path().join("readme.txt"), "not an entry");
        let index = AppIndex::from_dirs([high.path(), low.path()]);
        let ids: Vec<_> = index.iter().map(|app| app.id().to_owned()).collect();
        assert_eq!(ids, ["kde-konsole.desktop", "org.foo.desktop"]);
        let foo = index.get("org.foo.desktop").unwrap();
        assert_eq!(foo.entry().exec().as_deref(), Some("high"));
        assert_eq!(index.dir_of("org.foo.desktop"), Some(0));
        assert_eq!(index.dir_of("kde-konsole.desktop"), Some(1));
    }

    #[test]
    fn many_folders_are_told_apart_in_linear_time() {
        let dir = tempfile::tempdir().unwrap();
        work::assert_linear(
            2_000,
            |n| {
                // `n` folders that do not exist, a long one among them, each
                // given twice.
                let long = dir.path().join("d/".repeat(n)).join("applications");
                let dirs: Vec<PathBuf> = (0..n)
                    .map(|at| dir.path().join(format!("m{at}/applications")))
                    .chain([long])
                    .collect();
                let bytes = 2 * dirs.iter().map(|d| d.as_os_str().len()).sum::<usize>();
                let (index, steps) =
                    work::steps(|| AppIndex::from_dirs(dirs.iter().chain(&dirs).cloned()));
                let roots: Vec<_> = index.roots.iter().map(|(root, _)| root).collect();
                assert_eq!(roots, dirs.iter().collect::<Vec<_>>(), "the first kept");
                (steps, bytes)
            },
            // A step per folder given (twice per item) checked for repeats,
            // and per character of each copied, hashed and compared: 3 per
            // byte. Twice that.
            |items, bytes| 4 * items + 6 * bytes,
        );
    }

    #[test]
    fn entries_given_out_count_their_lookups_and_copies() {
        let dir = tempfile::tempdir().unwrap();
        work::assert_linear(
            200,
            |n| {
                // `n` entries in a folder of their own, one with a long name.
                let root = dir.path().join(n.to_string());
                for at in 0..n {
                    write(
                        &root.join(format!("org.example.App{at}.desktop")),
                        &app("a"),
                    );
                }
                write(
                    &root.join(format!("{}.desktop", "l".repeat(200))),
                    &app("a"),
                );
                let index = AppIndex::from_dirs([&root]);
                let ids: Vec<String> = index.iter().map(|app| app.id().to_owned()).collect();
                assert_eq!(ids.len(), n + 1);
                // The ID and path of each, copied by `iter` and by `get`, and
                // the ID looked up (hashed and compared) by `get` and by
                // `dir_of`.
                let bytes: usize = index
                    .iter()
                    .map(|app| 2 * (app.id().len() + app.path().as_os_str().len()))
                    .chain(ids.iter().map(|id| 4 * id.len()))
                    .sum();
                let ((), steps) = work::steps(|| {
                    assert_eq!(index.dirs(), [root.as_path()]);
                    assert_eq!(index.iter().count(), n + 1);
                    for id in &ids {
                        assert!(index.get(id).is_some());
                        assert_eq!(index.dir_of(id), Some(0));
                    }
                });
                assert!(steps >= bytes, "{steps} steps for {bytes} bytes");
                (steps, bytes)
            },
            // A step per folder listed; per entry `iter` visits, per entry
            // given out by `iter` and by `get`, per lookup by `get` and by
            // `dir_of`, and per byte of an ID and path copied or of an ID
            // hashed and compared (2 each): 5 per entry and 1 per byte
            // counted in `bytes`. Twice that.
            |items, bytes| 2 + 10 * (items + 1) + 2 * bytes,
        );
    }

    #[test]
    fn ids_sharing_a_long_prefix_are_looked_up_in_linear_time() {
        let dir = tempfile::tempdir().unwrap();
        work::assert_linear(
            200,
            |n| {
                // `n` entries whose IDs share a 200-byte prefix, so a lookup
                // that compares IDs in a sorted map walks it at every step.
                let root = dir.path().join(n.to_string());
                let prefix = "l".repeat(200);
                let ids: Vec<String> = (0..n).map(|at| format!("{prefix}{at}.desktop")).collect();
                for id in &ids {
                    write(&root.join(id), &app("a"));
                }
                let index = AppIndex::from_dirs([&root]);
                let bytes: usize = ids.iter().map(String::len).sum();
                let ((), steps) = work::steps(|| {
                    for id in &ids {
                        assert!(index.find(id).is_some());
                    }
                });
                (steps, bytes)
            },
            // A step per lookup, and per byte of its ID hashed and compared
            // (2). Twice that.
            |items, bytes| 2 * (items + 2 * bytes),
        );
    }

    /// The bytes `a` and `b` share at the start.
    fn shared(a: &str, b: &str) -> usize {
        a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count()
    }

    /// The least a sort or sorted structure of `items` compares: each item
    /// after the first once, each comparison the bytes they share at the
    /// start (at least those neighbours in order share) and one more.
    fn least_compared(items: &[String]) -> usize {
        let mut sorted = items.to_vec();
        sorted.sort();
        sorted
            .windows(2)
            .map(|pair| shared(&pair[0], &pair[1]) + 1)
            .min()
            .map_or(0, |each| (sorted.len() - 1) * each)
    }

    /// What indexing `files` (path relative to the root, contents) under
    /// the folder `root` costs, the key file's own work left out (reading
    /// each file `scan` reads, and looking up its encoding, type and
    /// `Hidden`: the first file of each ID in byte order of the relative
    /// paths, as an ID clash is never read), and the bytes of the full
    /// paths. `ids` are the IDs indexed. Checks that at least what every
    /// file costs is counted, whatever becomes of it: its name copied and
    /// its paths made by the walk (a step, and 1 per byte of the name and
    /// of both paths), its ID made (a step, 2 per byte of the relative path
    /// and 1 of the ID) and looked up (a step, 2 per byte), and the least
    /// the sort of the files and the sorted map and set of the IDs compare.
    fn scan_cost(root: &Path, files: &[(String, &str)], ids: &[String]) -> (usize, usize) {
        for (relative, text) in files {
            write(&root.join(relative), text);
        }
        let (index, built) = work::steps(|| AppIndex::from_dirs([root]));
        let mut read: Vec<&str> = files
            .iter()
            .map(|(relative, _)| relative.as_str())
            .collect();
        read.sort_unstable();
        let mut ids_read = HashSet::new();
        read.retain(|relative| ids_read.insert(relative.replace('/', "-")));
        let key_file: usize = read
            .iter()
            .map(|relative| {
                work::steps(|| {
                    let entry = DesktopEntry::read(root.join(relative)).unwrap();
                    if ignored_because(&entry).is_none() {
                        let _ = entry.hidden();
                    }
                })
                .1
            })
            .sum();
        assert_eq!(index.iter().count(), ids.len());
        let steps = built - key_file;
        let path = |relative: &str| root.join(relative).as_os_str().len();
        let each: usize = files
            .iter()
            .map(|(relative, _)| {
                let name = relative.rsplit('/').next().unwrap().len();
                let id = relative.len();
                3 + name + path(relative) + 3 * relative.len() + 3 * id
            })
            .sum();
        let relatives: Vec<String> = files.iter().map(|(relative, _)| relative.clone()).collect();
        let least = each + least_compared(&relatives) + 2 * least_compared(ids);
        assert!(steps >= least, "{steps} steps, at least {least} expected");
        let path_bytes = files.iter().map(|(relative, _)| path(relative)).sum();
        (steps, path_bytes)
    }

    #[test]
    fn ids_sharing_a_long_prefix_are_indexed_in_n_log_n_time() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = "l".repeat(200);
        let folder = "d".repeat(200);
        let junk = "[Desktop Entry]\nName=a\njunk\n";
        let application = app("a");
        // The IDs (or relative paths) share 200 bytes at the start, so each
        // comparison looks at about all of them: sorting is not linear. A
        // B-tree searches each node from its first key (up to 11) and is
        // about log6(n) deep, so an insert compares an ID with about
        // 2 (log2(n) + 1) others (15 at 200, measured); a sort compares
        // each item with about log2(n) + 1 others, and once when the items
        // come in order, `n` being the items sorted. Per byte of the
        // relative paths that is `sorting`: so many sorts or B-trees times
        // log2(n) + 1. The growth per doubling is then about 2.2, under
        // the 2.5 allowed.
        for case in ["long names", "a long folder", "diagnostics"] {
            let dir = dir.path().join(case.replace(' ', "-"));
            let layout = |n: usize| -> (Vec<(String, &str)>, Vec<String>) {
                match case {
                    "long names" => (0..n)
                        .map(|at| {
                            let name = format!("{prefix}{at}.desktop");
                            ((name.clone(), application.as_str()), name)
                        })
                        .unzip(),
                    "a long folder" => (0..n)
                        .map(|at| {
                            (
                                (format!("{folder}/{at}.desktop"), application.as_str()),
                                format!("{folder}-{at}.desktop"),
                            )
                        })
                        .unzip(),
                    // Each ID twice, `a-x` and `a/x`: the first has no type
                    // and a line that is not an entry, the second (after it
                    // in byte order) is an ID clash, never read; three
                    // diagnostics per ID.
                    _ => (
                        (0..n)
                            .flat_map(|at| {
                                [
                                    (format!("{prefix}{at}-x.desktop"), junk),
                                    (format!("{prefix}{at}/x.desktop"), application.as_str()),
                                ]
                            })
                            .collect(),
                        Vec::new(),
                    ),
                }
            };
            let paths = |n: usize| -> (usize, usize) {
                let (files, _) = layout(n);
                let root = dir.join(n.to_string());
                let bytes = files
                    .iter()
                    .map(|(relative, _)| root.join(relative).as_os_str().len())
                    .sum();
                (files.len(), bytes)
            };
            work::assert_linear(
                200,
                |n| {
                    let (files, ids) = layout(n);
                    let (steps, _) = scan_cost(&dir.join(n.to_string()), &files, &ids);
                    let relative_bytes = files.iter().map(|(relative, _)| relative.len()).sum();
                    (steps, relative_bytes)
                },
                |items, relative_bytes| {
                    let (files, path_bytes) = paths(items);
                    let log = |sorted: usize| usize::try_from(sorted.ilog2()).unwrap() + 1;
                    let (linear, sorting) = match case {
                        // Per file a step for each of the walk (2: its name
                        // copied, its entry kept), the ID made, looked up
                        // and put in the folder's map, the shadowing check,
                        // the cache lookup and keeping it (8); per byte of
                        // the relative path 17 (the walk 2, the ID made 3,
                        // looked up 2 twice, put in two maps 3 twice,
                        // copied twice); per byte of the full path 4 (made
                        // by the walk, put in the folder's map, hashed for
                        // the cache); a folder's name checked as UTF-8 once
                        // (1 per byte). Sorted: the IDs in two B-trees (4);
                        // the names sorted by the walk in read order (1),
                        // unless short; the files then in order (about 0).
                        "long names" => (
                            8 * files + 17 * relative_bytes + 4 * path_bytes,
                            5 * log(items),
                        ),
                        "a long folder" => (
                            8 * files + 17 * relative_bytes + 4 * path_bytes + folder.len(),
                            4 * log(items),
                        ),
                        // Per ID, with two files of `a` bytes of relative
                        // path and `p` of full path each: at most 293 steps
                        // (the walk 16: three entries, 2 each, the folder,
                        // and the inner name's 9 bytes; the IDs made and
                        // looked up 4; the first kept, checked for
                        // shadowing and looked up in the cache 3; the key
                        // file's message for its line made, 3 steps and 58
                        // bytes; its reason, `no Type`, written 7; its two
                        // messages 2, and 58 and 28 bytes besides their
                        // paths; the second's path stamped 1; its clash
                        // message 1, and at most 112 bytes besides the
                        // first's path: 77 and the ID cut to 32 characters
                        // and `…`); 21 per byte of `a` (the walk 6: both
                        // names and relative paths, the inner file's, and
                        // the folder's name checked as UTF-8;
                        // the IDs made 6 and looked up 4; the first put in
                        // the folder's map 3 and checked for shadowing 2);
                        // 15 per byte of `p` (the walk 4: both entries, the
                        // folder kept and the inner file; the folder's map
                        // 1; the cache 2; the key file's message for the
                        // first's line made with its path 1; the messages
                        // 4, the first's two and the clash's with both
                        // paths; the second's path stamped 1; the first's
                        // path made readable for it 2). Per file, half that
                        // (at most 146.5 steps, taken as 147; 10.5 per byte
                        // of its relative path, 7.5 of its full path).
                        // Sorted: the walk's 2n names in read order (1) and
                        // the 2n files out of order (1), each compared with
                        // about log2(2n) + 1 others (9.7 measured at 400),
                        // taken as floor(log2(2n)) + 2; no ID is indexed.
                        _ => (
                            147 * files + 21 * relative_bytes / 2 + 15 * path_bytes / 2,
                            2 * (log(files) + 1),
                        ),
                    };
                    // Twice the analysed cost.
                    2 * (linear + sorting * relative_bytes)
                },
            );
        }
    }

    #[test]
    fn many_paths_are_refreshed_in_linear_time() {
        let dir = tempfile::tempdir().unwrap();
        for changed in [false, true] {
            let dir = dir.path().join(if changed { "changed" } else { "same" });
            let root_bytes = |n: usize| dir.join(n.to_string()).as_os_str().len();
            work::assert_linear(
                200,
                |n| {
                    let root = dir.join(n.to_string());
                    let prefix = "l".repeat(200);
                    let mut ids: Vec<String> =
                        (0..n).map(|at| format!("{prefix}{at}.desktop")).collect();
                    for id in &ids {
                        write(&root.join(id), &app("a"));
                    }
                    ids.sort();
                    let index = AppIndex::from_dirs([&root]);
                    // The last file checked, the last in byte order.
                    let last = root.join(ids.last().unwrap());
                    if changed {
                        write(&last, &app("changed"));
                    }
                    let (fresh, steps) = work::steps(|| index.refresh());
                    assert_eq!(fresh.is_some(), changed);
                    // The refresh's own work: when it rebuilds, what the new
                    // scan counts (with the same entries reused, whose
                    // lookups in the key file may cost less the second
                    // time) is left out.
                    let own = if changed {
                        let cache = index.cache();
                        let roots = vec![(root.clone(), FileStamp::of(&root))];
                        steps - work::steps(|| AppIndex::scan(roots, &cache, Limits::default())).1
                    } else {
                        steps
                    };
                    let path_bytes = ids.iter().map(|id| root.join(id).as_os_str().len()).sum();
                    // At least each path checked and, when it rebuilds,
                    // each file's path put in the cache.
                    let at_least = n + 2 + if changed { n + 3 * path_bytes } else { 0 };
                    assert!(own >= at_least, "{own} steps, at least {at_least} expected");
                    (own, path_bytes)
                },
                // The analysed cost: a step per path checked (the root, its
                // folder and each file, up to the last). When it rebuilds,
                // each file's path also put in the cache (a step, and
                // copied, hashed and compared: 3 per byte), and the root
                // copied (a step, and 1 per byte). Twice that.
                |items, path_bytes| {
                    let checked = items + 2;
                    let rebuilt = if changed {
                        items + 3 * path_bytes + 1 + root_bytes(items)
                    } else {
                        0
                    };
                    2 * (checked + rebuilt)
                },
            );
        }
    }

    #[test]
    fn names_and_values_in_diagnostics_are_shown_escaped_and_cut() {
        let dir = tempfile::tempdir().unwrap();
        let name = format!("a\x1b{}", "b".repeat(200));
        write(&dir.path().join(format!("{name}-c.desktop")), &app("x"));
        write(&dir.path().join(format!("{name}/c.desktop")), &app("y"));
        let bad = format!("\x1b\\n{}", "a".repeat(5000));
        write(
            &dir.path().join("type.desktop"),
            &format!("[Desktop Entry]\nType={bad}\nName=x\n"),
        );
        write(
            &dir.path().join("encoding.desktop"),
            &format!("[Desktop Entry]\nType=Application\nName=x\nEncoding={bad}\n"),
        );
        let index = AppIndex::from_dirs([dir.path()]);
        assert_eq!(index.diagnostics().len(), 3, "{:?}", index.diagnostics());
        let (repeated, others) = index
            .diagnostics()
            .split_first()
            .expect("the repeated ID first");
        for found in others {
            crate::error::assert_shown_safely(&found.message);
        }
        // The ID is cut; the path of the file kept is shown whole.
        let first = dir.path().join(format!("{name}-c.desktop"));
        let first = shown_path(&first);
        assert!(!repeated.message.chars().any(char::is_control));
        assert!(
            repeated.message.starts_with(&format!(
                "desktop file ID a\\u{{1b}}{}… is also {first};",
                "b".repeat(30)
            )),
            "{}",
            repeated.message
        );
    }

    #[test]
    fn hidden_deletes_the_id_for_lower_folders_too() {
        let high = tempfile::tempdir().unwrap();
        let low = tempfile::tempdir().unwrap();
        write(
            &high.path().join("org.foo.desktop"),
            "[Desktop Entry]\nType=Application\nName=x\nHidden=true\n",
        );
        write(&low.path().join("org.foo.desktop"), &app("low"));
        let index = AppIndex::from_dirs([high.path(), low.path()]);
        assert!(index.get("org.foo.desktop").is_none());
        assert_eq!(index.iter().count(), 0);
    }

    /// `relative` under `root`, from its bytes.
    fn raw(root: &Path, relative: &[u8]) -> PathBuf {
        root.join(OsStr::from_bytes(relative))
    }

    /// The paths and messages of `index`'s diagnostics, the paths as shown.
    fn shown_diagnostics(index: &AppIndex) -> Vec<String> {
        index
            .diagnostics()
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn a_name_not_in_utf8_does_not_hide_the_name_it_looks_like() {
        let high = tempfile::tempdir().unwrap();
        let low = tempfile::tempdir().unwrap();
        let hidden = "[Desktop Entry]\nType=Application\nName=x\nHidden=true\n";
        // Read lossily, both would be `bad\u{fffd}.desktop`.
        write(&raw(high.path(), b"bad\xff.desktop"), hidden);
        write(&low.path().join("bad\u{fffd}.desktop"), &app("low"));
        // So would a folder `d\xff` and `d\u{fffd}` give `d\u{fffd}-a.desktop`.
        write(&raw(high.path(), b"d\xff/a.desktop"), hidden);
        write(&low.path().join("d\u{fffd}/a.desktop"), &app("low"));
        let index = AppIndex::from_dirs([high.path(), low.path()]);
        let ids: Vec<_> = index.iter().map(|app| app.id().to_owned()).collect();
        assert_eq!(ids, ["bad\u{fffd}.desktop", "d\u{fffd}-a.desktop"]);
        assert_eq!(index.dir_of("bad\u{fffd}.desktop"), Some(1));
        // Folders as the walk meets them, then files.
        let high = shown_path(high.path());
        assert_eq!(
            shown_diagnostics(&index),
            [
                format!(r"{high}/d\xFF: {NOT_UTF8_FOLDER}"),
                format!(r"{high}/bad\xFF.desktop: {NOT_UTF8_FILE}"),
            ]
        );
    }

    #[test]
    fn a_name_not_in_utf8_is_left_out_in_its_own_folder_too() {
        let root = tempfile::tempdir().unwrap();
        // `\x80` comes before the UTF-8 of U+FFFD in byte order: read
        // lossily, the first of each pair would win.
        write(&raw(root.path(), b"bad\x80.desktop"), &app("raw"));
        write(&root.path().join("bad\u{fffd}.desktop"), &app("text"));
        write(&raw(root.path(), b"d\x80/a.desktop"), &app("raw"));
        write(&root.path().join("d\u{fffd}/a.desktop"), &app("text"));
        // Below a folder whose name is not UTF-8, nothing is looked at.
        write(&raw(root.path(), b"e\x80/f/a.desktop"), &app("raw"));
        write(&raw(root.path(), b"e\x80/b\x80.desktop"), &app("raw"));
        let index = AppIndex::from_dirs([root.path()]);
        let found: Vec<_> = index
            .iter()
            .map(|app| (app.id().to_owned(), app.path().to_owned()))
            .collect();
        assert_eq!(
            found,
            ["bad\u{fffd}.desktop", "d\u{fffd}/a.desktop"]
                .map(|relative| (relative.replace('/', "-"), root.path().join(relative)))
        );
        let shown = shown_path(root.path());
        assert_eq!(
            shown_diagnostics(&index),
            [
                format!(r"{shown}/d\x80: {NOT_UTF8_FOLDER}"),
                format!(r"{shown}/e\x80: {NOT_UTF8_FOLDER}"),
                format!(r"{shown}/bad\x80.desktop: {NOT_UTF8_FILE}"),
            ]
        );
        let folders: Vec<_> = index.folders.iter().map(|(path, _)| path.clone()).collect();
        assert_eq!(
            folders,
            [root.path().to_owned(), root.path().join("d\u{fffd}")]
        );
        let [recursive, own_stack] = both_walks(root.path(), Limits::default());
        assert_eq!(own_stack, recursive);
        // Adding or removing such a file or folder is a change to its
        // folder; a change inside a folder left out is none.
        assert!(index.refresh().is_none());
        write(&raw(root.path(), b"e\x80/f/a.desktop"), &app("changed"));
        assert!(index.refresh().is_none());
        fs::remove_file(raw(root.path(), b"bad\x80.desktop")).unwrap();
        let fresh = index.refresh().unwrap();
        assert_eq!(shown_diagnostics(&fresh).len(), 2);
        write(&raw(root.path(), b"g\x80.desktop"), &app("raw"));
        let fresh = fresh.refresh().unwrap();
        assert_eq!(shown_diagnostics(&fresh).len(), 3);
    }

    #[test]
    fn a_link_not_in_utf8_left_out_is_watched() {
        let root = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        fs::create_dir(ext.path().join("dir")).unwrap();
        write(&ext.path().join("plain"), "");
        write(&ext.path().join("e.desktop"), &app("e"));
        // Each through a link outside, so that only the target changes.
        for (link, via, target) in [
            (&b"d\x80"[..], "t", "dir"),
            (b"l\x80.desktop", "u", "e.desktop"),
        ] {
            std::os::unix::fs::symlink(ext.path().join(target), ext.path().join(via)).unwrap();
            std::os::unix::fs::symlink(ext.path().join(via), raw(root.path(), link)).unwrap();
        }
        let index = AppIndex::from_dirs([root.path()]);
        let shown = shown_path(root.path());
        assert_eq!(
            shown_diagnostics(&index),
            [
                format!(r"{shown}/d\x80: {NOT_UTF8_FOLDER}"),
                format!(r"{shown}/l\x80.desktop: {NOT_UTF8_FILE}"),
            ]
        );
        assert!(index.refresh().is_none());
        // A folder no more: the diagnostic goes.
        retarget(&ext.path().join("t"), &ext.path().join("plain"));
        let fresh = index.refresh().expect("retargeted");
        assert_eq!(
            shown_diagnostics(&fresh),
            [format!(r"{shown}/l\x80.desktop: {NOT_UTF8_FILE}")]
        );
        assert!(fresh.refresh().is_none());
        // A file no more.
        retarget(&ext.path().join("u"), &ext.path().join("gone"));
        let fresh = fresh.refresh().expect("retargeted");
        assert_eq!(shown_diagnostics(&fresh), Vec::<String>::new());
    }

    #[test]
    fn past_the_cap_no_folder_is_entered_or_reported_whatever_its_name() {
        let root = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        // `a` reaches the cap; the folder `b\x80`, the link `c\x80` to a
        // folder and the link `d\x80.desktop`, all listed, come after it.
        // The folders are looked at (their type, their name checked as
        // UTF-8) but neither entered nor reported; the link to the folder
        // is stamped, so `refresh` notices it being retargeted, and the
        // `.desktop` file is reported and its link stamped as usual.
        for name in ["x", "y", "z"] {
            write(&root.path().join(format!("a/{name}.desktop")), &app(name));
        }
        write(&raw(root.path(), b"b\x80/a.desktop"), &app("b"));
        fs::create_dir(ext.path().join("dir")).unwrap();
        std::os::unix::fs::symlink(ext.path().join("dir"), raw(root.path(), b"c\x80")).unwrap();
        write(&ext.path().join("d.desktop"), &app("d"));
        let file = raw(root.path(), b"d\x80.desktop");
        std::os::unix::fs::symlink(ext.path().join("d.desktop"), &file).unwrap();
        let limits = Limits {
            max_listed: Some(5),
            ..Limits::default()
        };
        let index = AppIndex::from_dirs_with([root.path()], &limits);
        let shown = shown_path(root.path());
        assert_eq!(
            shown_diagnostics(&index),
            [
                format!("{shown}: more than 5 entries to list; not scanned further"),
                format!("{}: {NOT_UTF8_FILE}", shown_path(&file)),
            ]
        );
        let ids: Vec<_> = index.iter().map(|app| app.id().to_owned()).collect();
        assert_eq!(ids, ["a-x.desktop"]);
        let links: Vec<_> = index.links.iter().map(|(path, _)| path).collect();
        assert_eq!(links, [&raw(root.path(), b"c\x80"), &file]);
        let [recursive, own_stack] = both_walks(root.path(), limits);
        assert_eq!(own_stack, recursive);
    }

    /// `root/a.desktop`, a link through `ext/bridge` to the folder
    /// `ext/folder`, with `ext/valid.desktop` to point the bridge at.
    fn a_desktop_link_to_a_folder(root: &Path, ext: &Path) {
        fs::create_dir(ext.join("folder")).unwrap();
        write(&ext.join("valid.desktop"), &app("valid"));
        std::os::unix::fs::symlink(ext.join("folder"), ext.join("bridge")).unwrap();
        std::os::unix::fs::symlink(ext.join("bridge"), root.join("a.desktop")).unwrap();
    }

    #[test]
    fn a_folder_link_kept_past_the_cap_is_watched() {
        // The root's listing reaches the cap; `a.desktop` is kept but not
        // walked.
        let root = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        a_desktop_link_to_a_folder(root.path(), ext.path());
        write(&root.path().join("b.desktop"), &app("b"));
        let limits = Limits {
            max_listed: Some(1),
            ..Limits::default()
        };
        let index = AppIndex::from_dirs_with([root.path()], &limits);
        assert_eq!(index.iter().count(), 0);
        let [recursive, own_stack] = both_walks(root.path(), limits);
        assert_eq!(own_stack, recursive);
        assert!(index.refresh().is_none());
        retarget(
            &ext.path().join("bridge"),
            &ext.path().join("valid.desktop"),
        );
        let fresh = AppIndex::from_dirs_with([root.path()], &limits);
        assert_eq!(fresh.iter().count(), 1);
        let refreshed = index.refresh().expect("retargeted");
        assert_same(&refreshed, &fresh);
    }

    #[test]
    fn stamping_a_folder_link_past_the_cap_is_counted() {
        // Two roots alike but for `b*`, past the cap: links to a folder
        // (stamped) in one, folders (not entered) in the other. The links
        // cost a step and their path copied each, no more.
        let dir = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        let limits = Limits {
            max_listed: Some(7),
            ..Limits::default()
        };
        let names: Vec<String> = (0..5)
            .map(|at| format!("b{at}{}", "n".repeat(100)))
            .collect();
        let steps = |root: &Path, links: bool| {
            for name in ["x", "y", "z"] {
                write(&root.join(format!("a/{name}.desktop")), &app(name));
            }
            for name in &names {
                if links {
                    std::os::unix::fs::symlink(ext.path(), root.join(name)).unwrap();
                } else {
                    fs::create_dir(root.join(name)).unwrap();
                }
            }
            let (index, steps) = work::steps(|| AppIndex::from_dirs_with([root], &limits));
            assert_eq!(index.links.len(), if links { names.len() } else { 0 });
            steps
        };
        let (linked, plain) = (dir.path().join("l"), dir.path().join("p"));
        let stamped: usize = names
            .iter()
            .map(|name| 1 + linked.join(name).as_os_str().len())
            .sum();
        assert_eq!(steps(&linked, true) - steps(&plain, false), stamped);
    }

    #[test]
    fn a_folder_link_kept_after_a_subfolder_reaches_the_cap_is_watched() {
        // The cap is reached inside `0`; the root's `a.desktop`, listed
        // before, comes after it.
        let root = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        a_desktop_link_to_a_folder(root.path(), ext.path());
        write(&root.path().join("0/x.desktop"), &app("x"));
        write(&root.path().join("0/y.desktop"), &app("y"));
        let limits = Limits {
            max_listed: Some(3),
            ..Limits::default()
        };
        let index = AppIndex::from_dirs_with([root.path()], &limits);
        let ids: Vec<_> = index.iter().map(|app| app.id().to_owned()).collect();
        assert_eq!(ids, ["0-x.desktop"]);
        let [recursive, own_stack] = both_walks(root.path(), limits);
        assert_eq!(own_stack, recursive);
        assert!(index.refresh().is_none());
        retarget(
            &ext.path().join("bridge"),
            &ext.path().join("valid.desktop"),
        );
        let fresh = AppIndex::from_dirs_with([root.path()], &limits);
        assert_eq!(fresh.iter().count(), 2);
        let refreshed = index.refresh().expect("retargeted");
        assert_same(&refreshed, &fresh);
    }

    #[test]
    fn hidden_without_a_type_deletes_the_id_too() {
        let high = tempfile::tempdir().unwrap();
        let low = tempfile::tempdir().unwrap();
        write(
            &high.path().join("org.foo.desktop"),
            "[Desktop Entry]\nHidden=true\n",
        );
        write(&low.path().join("org.foo.desktop"), &app("low"));
        let index = AppIndex::from_dirs([high.path(), low.path()]);
        assert!(index.get("org.foo.desktop").is_none());
        assert!(index.dir_holds(0, "org.foo.desktop"));
        assert!(index.diagnostics().is_empty(), "{:?}", index.diagnostics());
    }

    #[test]
    fn entries_of_an_unknown_or_no_type_are_ignored() {
        let high = tempfile::tempdir().unwrap();
        let low = tempfile::tempdir().unwrap();
        write(
            &high.path().join("org.foo.desktop"),
            "[Desktop Entry]\nType=Service\nName=x\nExec=service\n",
        );
        write(
            &high.path().join("org.bar.desktop"),
            "[Desktop Entry]\nName=x\nExec=untyped\n",
        );
        write(&low.path().join("org.foo.desktop"), &app("low-foo"));
        write(&low.path().join("org.bar.desktop"), &app("low-bar"));
        // Shadowed, so read only to know whether the folder holds them.
        write(&high.path().join("shadow.desktop"), &app("high"));
        write(
            &low.path().join("shadow.desktop"),
            "[Desktop Entry]\nType=Service\nName=x\n",
        );
        write(
            &low.path().join("link.desktop"),
            "[Desktop Entry]\nType=Link\nName=x\nURL=https://x.org/\n",
        );
        write(
            &low.path().join("menu.desktop"),
            "[Desktop Entry]\nType=Directory\nName=x\n",
        );
        let index = AppIndex::from_dirs([high.path(), low.path()]);
        let exec = |id: &str| index.get(id).unwrap().entry().exec();
        assert_eq!(exec("org.foo.desktop").as_deref(), Some("low-foo"));
        assert_eq!(exec("org.bar.desktop").as_deref(), Some("low-bar"));
        assert_eq!(index.dir_of("org.foo.desktop"), Some(1));
        assert_eq!(
            index.dir_ids(0).collect::<Vec<_>>(),
            ["shadow.desktop"],
            "ignored files are not held"
        );
        assert_eq!(
            index.dir_ids(1).collect::<Vec<_>>(),
            [
                "link.desktop",
                "menu.desktop",
                "org.bar.desktop",
                "org.foo.desktop"
            ]
        );
        assert_eq!(
            index
                .iter()
                .map(|app| app.id().to_owned())
                .collect::<Vec<_>>(),
            [
                "link.desktop",
                "menu.desktop",
                "org.bar.desktop",
                "org.foo.desktop",
                "shadow.desktop"
            ]
        );
        let messages: Vec<&str> = index
            .diagnostics()
            .iter()
            .map(|d| d.message.as_str())
            .collect();
        assert_eq!(
            messages,
            [
                "no Type; the file is ignored",
                "Type=Service is not a type the spec lists; the file is ignored",
                "Type=Service is not a type the spec lists; the file is ignored",
            ]
        );
        assert!(index.refresh().is_none());
        write(&high.path().join("org.foo.desktop"), &app("fixed"));
        let index = index.refresh().expect("the ignored file was fixed");
        assert_eq!(
            index
                .get("org.foo.desktop")
                .unwrap()
                .entry()
                .exec()
                .as_deref(),
            Some("fixed")
        );
        write(
            &low.path().join("shadow.desktop"),
            "[Desktop Entry]\nType=Application\nName=x\nExec=x\n",
        );
        let index = index
            .refresh()
            .expect("the ignored shadowed file was fixed");
        assert!(index.dir_holds(1, "shadow.desktop"));
    }

    #[test]
    fn a_dash_and_a_folder_giving_one_id_keep_the_first_in_name_order() {
        let root = tempfile::tempdir().unwrap();
        write(&root.path().join("foo-bar.desktop"), &app("dash"));
        write(&root.path().join("foo/bar.desktop"), &app("folder"));
        let index = AppIndex::from_dirs([root.path()]);
        assert_eq!(
            index
                .get("foo-bar.desktop")
                .unwrap()
                .entry()
                .exec()
                .as_deref(),
            Some("dash")
        );
        assert_eq!(index.diagnostics().len(), 1, "{:?}", index.diagnostics());
    }

    #[test]
    fn one_file_under_two_paths_of_one_id_is_kept_quietly() {
        let root = tempfile::tempdir().unwrap();
        // `x-y/foo.desktop` and `x/y/foo.desktop` are one file: kept once,
        // under the first path in byte order, with no clash reported.
        write(&root.path().join("x/y/foo.desktop"), &app("foo"));
        std::os::unix::fs::symlink("x/y", root.path().join("x-y")).unwrap();
        // So are `s-t.desktop`, a link, and `s/t.desktop`.
        write(&root.path().join("s/t.desktop"), &app("t"));
        std::os::unix::fs::symlink("s/t.desktop", root.path().join("s-t.desktop")).unwrap();
        // Two files of one ID: the clash is still reported.
        write(&root.path().join("p-q.desktop"), &app("dash"));
        write(&root.path().join("p/q.desktop"), &app("folder"));
        let index = AppIndex::from_dirs([root.path()]);
        let (ids, messages) = ids_and_messages(&index);
        assert_eq!(ids, ["p-q.desktop", "s-t.desktop", "x-y-foo.desktop"]);
        assert_eq!(
            index.get("x-y-foo.desktop").unwrap().path(),
            root.path().join("x-y/foo.desktop")
        );
        let clash = root.path().join("p/q.desktop");
        assert_eq!(
            messages,
            [format!(
                "desktop file ID {} is also {}; the spec leaves the choice open, the first is kept",
                quoted("p-q.desktop"),
                shown_path(&root.path().join("p-q.desktop"))
            )]
        );
        assert_eq!(index.diagnostics()[0].path.as_deref(), Some(&*clash));
        assert!(index.refresh().is_none());
    }

    #[test]
    fn a_link_to_a_lower_root_does_not_hide_it() {
        let high = tempfile::tempdir().unwrap();
        let low = tempfile::tempdir().unwrap();
        write(&low.path().join("org.foo.desktop"), &app("foo"));
        std::os::unix::fs::symlink(low.path(), high.path().join("linked")).unwrap();
        let index = AppIndex::from_dirs([high.path(), low.path()]);
        assert!(index.get("org.foo.desktop").is_some());
        assert!(index.get("linked-org.foo.desktop").is_some());
    }

    #[test]
    fn a_permission_fix_is_noticed() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("a.desktop");
        write(&path, &app("a"));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let index = AppIndex::from_dirs([root.path()]);
        if fs::read(&path).is_ok() {
            eprintln!("an unreadable file can be read (root?): not checked");
            return;
        }
        assert!(index.get("a.desktop").is_none());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let index = index.refresh().expect("the chmod was noticed");
        assert!(index.get("a.desktop").is_some());
    }

    #[test]
    fn empty_search_path_folders_are_the_current_folder() {
        let dirs: Vec<&Path> = search_dirs(OsStr::new(":/bin::/usr/bin:")).collect();
        assert_eq!(dirs, [".", "/bin", ".", "/usr/bin", "."].map(Path::new));
    }

    #[test]
    fn each_folder_knows_its_ids_shadowed_ones_too() {
        let high = tempfile::tempdir().unwrap();
        let low = tempfile::tempdir().unwrap();
        write(&high.path().join("a.desktop"), &app("high"));
        write(
            &high.path().join("gone.desktop"),
            &format!("{}Hidden=true\n", app("gone")),
        );
        write(&high.path().join("legacy.desktop"), &app("high"));
        write(&low.path().join("a.desktop"), &app("low"));
        write(&low.path().join("sub/b.desktop"), &app("b"));
        // Ignored for their encoding, shadowed or not: not held.
        let legacy = format!("{}Encoding=Legacy-Mixed\n", app("legacy"));
        write(&low.path().join("legacy.desktop"), &legacy);
        write(&low.path().join("only-legacy.desktop"), &legacy);
        let index = AppIndex::from_dirs([high.path(), low.path()]);
        assert_eq!(
            index.dir_ids(0).collect::<Vec<_>>(),
            ["a.desktop", "gone.desktop", "legacy.desktop"]
        );
        assert_eq!(
            index.dir_ids(1).collect::<Vec<_>>(),
            ["a.desktop", "sub-b.desktop"]
        );
        assert!(index.refresh().is_none());
        write(&low.path().join("legacy.desktop"), &app("fixed"));
        let index = index.refresh().expect("a shadowed file changed");
        assert!(index.dir_holds(1, "legacy.desktop"));
        assert!(index.dir_holds(1, "a.desktop"), "shadowed, still held");
        assert!(!index.dir_holds(1, "gone.desktop"));
        assert!(!index.dir_holds(2, "a.desktop"), "no such folder");
        assert!(index.get("gone.desktop").is_none());
    }

    #[test]
    fn shadowed_copies_are_read_quietly_and_watched() {
        let high = tempfile::tempdir().unwrap();
        let low = tempfile::tempdir().unwrap();
        write(&high.path().join("a.desktop"), &app("high"));
        write(&high.path().join("b.desktop"), &app("high"));
        write(
            &low.path().join("a.desktop"),
            &format!("{}junk line\n", app("low")),
        );
        write(&low.path().join("b.desktop"), &app("low"));
        let index = AppIndex::from_dirs([high.path(), low.path()]);
        assert!(
            index.diagnostics().is_empty(),
            "a shadowed file's own problems stay quiet: {:?}",
            index.diagnostics()
        );
        assert!(index.dir_holds(1, "a.desktop"));
        // An edit in place leaves the folder's time alone; the file's stamp
        // notices it.
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(low.path().join("a.desktop"))
            .unwrap();
        std::io::Write::write_all(&mut file, b"Encoding=Legacy-Mixed\n").unwrap();
        drop(file);
        let refreshed = index.refresh().expect("a shadowed file changed");
        assert!(!refreshed.dir_holds(1, "a.desktop"));
        assert!(refreshed.dir_holds(1, "b.desktop"));
        let reused = |index: &AppIndex| {
            index
                .shadowed
                .iter()
                .find(|(path, _, _)| path.ends_with("b.desktop"))
                .map(|(_, _, entry)| Arc::clone(entry))
                .unwrap()
        };
        assert!(
            Arc::ptr_eq(&reused(&index), &reused(&refreshed)),
            "an unchanged shadowed file is not read again"
        );
    }

    #[test]
    fn a_folder_reached_again_holds_the_same() {
        let real = tempfile::tempdir().unwrap();
        let links = tempfile::tempdir().unwrap();
        write(&real.path().join("a.desktop"), &app("a"));
        let alias = links.path().join("alias");
        std::os::unix::fs::symlink(real.path(), &alias).unwrap();
        let index = AppIndex::from_dirs([real.path(), alias.as_path()]);
        assert_eq!(index.dirs().len(), 2);
        assert!(index.dir_holds(1, "a.desktop"));
        assert_eq!(index.dir_of("a.desktop"), Some(0));
    }

    #[test]
    fn a_link_whose_target_appears_later_is_noticed() {
        let apps = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let target = elsewhere.path().join("later.desktop");
        std::os::unix::fs::symlink(&target, apps.path().join("link.desktop")).unwrap();
        let index = AppIndex::from_dirs([apps.path()]);
        assert!(index.get("link.desktop").is_none());
        assert!(index.refresh().is_none());
        write(&target, &app("later"));
        let index = index.refresh().expect("the target appeared");
        assert!(index.get("link.desktop").is_some());
    }

    /// Points the link `link` at `target` in one step.
    fn retarget(link: &Path, target: &Path) {
        let new = link.with_extension("new");
        std::os::unix::fs::symlink(target, &new).unwrap();
        fs::rename(&new, link).unwrap();
    }

    /// What two indexes hold and say, side by side.
    fn assert_same(index: &AppIndex, fresh: &AppIndex) {
        let contents = |index: &AppIndex| {
            let apps: Vec<_> = index
                .iter()
                .map(|app| {
                    (
                        app.id().to_owned(),
                        app.path().to_owned(),
                        app.entry().exec(),
                    )
                })
                .collect();
            let held: Vec<Vec<&str>> = (0..index.dirs().len())
                .map(|dir| index.dir_ids(dir).collect())
                .collect();
            format!(
                "{apps:?} {held:?} {:?} {:?}",
                index.dirs(),
                index.diagnostics()
            )
        };
        assert_eq!(contents(index), contents(fresh));
    }

    #[test]
    fn a_folder_link_retargeted_outside_is_noticed() {
        let apps = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        let (d1, d2) = (ext.path().join("d1"), ext.path().join("d2"));
        write(&d1.join("one.desktop"), &app("one"));
        write(&d2.join("two.desktop"), &app("two"));
        let mid = ext.path().join("mid");
        std::os::unix::fs::symlink(&d1, apps.path().join("a")).unwrap();
        std::os::unix::fs::symlink(&d1, &mid).unwrap();
        std::os::unix::fs::symlink(&mid, apps.path().join("b")).unwrap();
        let index = AppIndex::from_dirs([apps.path()]);
        let ids = |index: &AppIndex| {
            index
                .iter()
                .map(|app| app.id().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(&index),
            ["a-one.desktop", "b-one.desktop"],
            "the same folder under each name"
        );
        assert!(index.refresh().is_none());
        // Only a link outside the watched folders changes.
        retarget(&mid, &d2);
        let index = index.refresh().expect("b now leads to another folder");
        assert_same(&index, &AppIndex::from_dirs([apps.path()]));
        assert_eq!(ids(&index), ["a-one.desktop", "b-two.desktop"]);
        assert!(index.refresh().is_none());
    }

    /// `root/c-d.desktop`, and `root/c` a link to `ext/c`, whose
    /// `d.desktop` links to `ext/u/d.desktop`, a link to `target`: two
    /// paths of the ID `c-d.desktop`.
    fn one_id_through_links(root: &Path, ext: &Path, target: &Path) -> PathBuf {
        write(&root.join("c-d.desktop"), &app("dash"));
        fs::create_dir_all(ext.join("c")).unwrap();
        fs::create_dir_all(ext.join("u")).unwrap();
        std::os::unix::fs::symlink(ext.join("c"), root.join("c")).unwrap();
        let inner = ext.join("u/d.desktop");
        std::os::unix::fs::symlink(&inner, ext.join("c/d.desktop")).unwrap();
        std::os::unix::fs::symlink(target, &inner).unwrap();
        inner
    }

    #[test]
    fn a_second_path_of_an_id_turning_into_another_file_is_noticed() {
        let root = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        let other = ext.path().join("other.desktop");
        write(&other, &app("other"));
        let inner = one_id_through_links(root.path(), ext.path(), &root.path().join("c-d.desktop"));
        let index = AppIndex::from_dirs([root.path()]);
        assert!(index.diagnostics().is_empty(), "{:?}", index.diagnostics());
        assert!(index.refresh().is_none());
        // Only a link outside the watched folders changes.
        retarget(&inner, &other);
        let index = index.refresh().expect("c/d.desktop is now another file");
        assert_same(&index, &AppIndex::from_dirs([root.path()]));
        assert_eq!(index.diagnostics().len(), 1, "{:?}", index.diagnostics());
        assert!(index.refresh().is_none());
    }

    #[test]
    fn a_second_path_of_an_id_turning_into_the_same_file_is_noticed() {
        let root = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        let other = ext.path().join("other.desktop");
        write(&other, &app("other"));
        let inner = one_id_through_links(root.path(), ext.path(), &other);
        let index = AppIndex::from_dirs([root.path()]);
        assert_eq!(index.diagnostics().len(), 1, "{:?}", index.diagnostics());
        assert!(index.refresh().is_none());
        // Only a link outside the watched folders changes.
        retarget(&inner, &root.path().join("c-d.desktop"));
        let index = index.refresh().expect("c/d.desktop is now the same file");
        assert_same(&index, &AppIndex::from_dirs([root.path()]));
        assert!(index.diagnostics().is_empty(), "{:?}", index.diagnostics());
        assert!(index.refresh().is_none());
    }

    #[test]
    fn a_folder_linked_under_two_names_gives_an_id_under_each() {
        let apps = tempfile::tempdir().unwrap();
        write(&apps.path().join("b/foo.desktop"), &app("foo"));
        std::os::unix::fs::symlink("b", apps.path().join("a")).unwrap();
        let index = AppIndex::from_dirs([apps.path()]);
        let ids: Vec<_> = index.iter().map(|app| app.id().to_owned()).collect();
        assert_eq!(ids, ["a-foo.desktop", "b-foo.desktop"]);
        assert_eq!(
            index.dir_ids(0).collect::<Vec<_>>(),
            ["a-foo.desktop", "b-foo.desktop"]
        );
        assert!(index.diagnostics().is_empty(), "{:?}", index.diagnostics());
        assert!(index.refresh().is_none());
    }

    #[test]
    fn a_link_back_to_a_folder_on_the_path_ends_there_and_is_watched() {
        let apps = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        write(&apps.path().join("b/foo.desktop"), &app("foo"));
        write(&ext.path().join("x.desktop"), &app("x"));
        let up = apps.path().join("b/self");
        std::os::unix::fs::symlink("..", &up).unwrap();
        std::os::unix::fs::symlink(".", apps.path().join("b/here")).unwrap();
        let index = AppIndex::from_dirs([apps.path()]);
        let ids = |index: &AppIndex| {
            index
                .iter()
                .map(|app| app.id().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&index), ["b-foo.desktop"]);
        assert!(index.refresh().is_none());
        retarget(&up, ext.path());
        let index = index.refresh().expect("the link now leads elsewhere");
        assert_same(&index, &AppIndex::from_dirs([apps.path()]));
        assert_eq!(ids(&index), ["b-foo.desktop", "b-self-x.desktop"]);
    }

    #[test]
    fn a_link_to_an_ancestor_further_up_ends_there() {
        let apps = tempfile::tempdir().unwrap();
        write(&apps.path().join("a/b/x.desktop"), &app("x"));
        write(&apps.path().join("a/b/c/d/e/f.desktop"), &app("f"));
        // At depth 5, a link to `a/b`, three levels up.
        let up = apps.path().join("a/b/c/d/e/up3");
        std::os::unix::fs::symlink("../../..", &up).unwrap();
        let index = AppIndex::from_dirs([apps.path()]);
        let (ids, messages) = ids_and_messages(&index);
        assert_eq!(ids, ["a-b-c-d-e-f.desktop", "a-b-x.desktop"]);
        assert!(messages.is_empty(), "{messages:?}");
        assert!(index.refresh().is_none());
    }

    /// Makes `levels` folders `d1`, `d2`, ... in `ext`, each but the last
    /// with two links to the next, and two links to `d1` in `apps`: the
    /// paths to the last folder double at each level.
    fn fan_out(apps: &Path, ext: &Path, levels: usize) {
        for level in 1..=levels {
            let folder = ext.join(format!("d{level}"));
            fs::create_dir_all(&folder).unwrap();
            if level < levels {
                for name in ["l0", "l1"] {
                    let next = ext.join(format!("d{}", level + 1));
                    std::os::unix::fs::symlink(next, folder.join(name)).unwrap();
                }
            }
        }
        for name in ["l0", "l1"] {
            std::os::unix::fs::symlink(ext.join("d1"), apps.join(name)).unwrap();
        }
    }

    #[test]
    fn a_wide_folder_holds_only_the_names_still_allowed() {
        let root = tempfile::tempdir().unwrap();
        write(&root.path().join("a.desktop"), &app("a"));
        // Not in name order, so that the cut is not the order of reading.
        for at in (0..1000).rev() {
            write(&root.path().join(format!("w/{at:03}.desktop")), &app("w"));
        }
        for max in [0, 1, 2, 3, 10, 500, 1001, 1002, 1003] {
            let limits = Limits {
                max_listed: Some(max),
                ..Limits::default()
            };
            MOST_HELD.set(0);
            let index = AppIndex::from_dirs_with([root.path()], &limits);
            // The root's two names, then those of `w` still allowed.
            let most = max.min(2).max(max.saturating_sub(2).min(1000));
            assert_eq!(MOST_HELD.get(), most, "{max}");
            let (ids, messages) = ids_and_messages(&index);
            let mut expected: Vec<_> = (0..max.saturating_sub(2).min(1000))
                .map(|at| format!("w-{at:03}.desktop"))
                .collect();
            if max > 0 {
                expected.insert(0, "a.desktop".to_owned());
            }
            assert_eq!(ids, expected, "{max}");
            let capped = format!("more than {max} entries to list; not scanned further");
            assert_eq!(messages, vec![capped; usize::from(max < 1002)], "{max}");
            let [recursive, own_stack] = both_walks(root.path(), limits);
            assert_eq!(own_stack, recursive, "{max}");
        }
    }

    #[test]
    fn a_wide_folder_past_the_cap_is_listed_in_linear_time() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = "l".repeat(200);
        // Few enough that sorting every name would pass the bound.
        let most = 3;
        let limits = Limits {
            max_listed: Some(most),
            ..Limits::default()
        };
        work::assert_linear(
            500,
            |n| {
                let root = dir.path().join(n.to_string());
                // Not entries: only the names are compared and copied.
                // Made in a fixed shuffled order (Fisher-Yates, xorshift64):
                // tmpfs lists names newest first, and sorting every name
                // takes runs already sorted in linear time.
                let mut order: Vec<usize> = (0..n).collect();
                let mut state = 0x9e37_79b9_7f4a_7c15_u64;
                for at in (1..n).rev() {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    let bound = u64::try_from(at + 1).unwrap();
                    order.swap(at, usize::try_from(state % bound).unwrap());
                }
                for at in order {
                    write(&root.join(format!("{prefix}{at}.txt")), "");
                }
                let (index, steps) = work::steps(|| AppIndex::from_dirs_with([&root], &limits));
                assert_eq!(index.diagnostics().len(), 1);
                let names = (0..n).map(|at| prefix.len() + format!("{at}.txt").len());
                (steps, names.sum())
            },
            |names, name_bytes| {
                let root = dir.path().join(names.to_string()).as_os_str().len();
                let longest = prefix.len() + format!("{names}.txt").len();
                // A comparison stops after the bytes two names share: the
                // prefix, at most the digits of the shorter, and one more.
                let digits = (names - 1).to_string().len();
                let comparison = prefix.len() + digits + 1;
                // Per name: copied (a step, and a byte each). The first 3
                // are held; the 4th makes the heap (2 comparisons). From
                // the 4th on, in any order, each is compared with the
                // largest held and, if smaller, takes its place and the
                // heap of 3 is sifted: at most 3 comparisons. The 3 kept
                // are sorted at the end (at most 3). Per name kept: its
                // paths made (both, the full one the root, a `/` and the
                // name) and a step. The root: its repeat check, kept (1 + 4
                // per byte) and its cap message (its path and 48).
                let compared = (2 + 3 * (names - most) + 3) * comparison;
                let kept = most * (1 + root + 1 + 2 * longest);
                let analysed = names + name_bytes + compared + kept + 3 + 5 * root + 48;
                // Twice the analysed cost.
                2 * analysed
            },
        );
    }

    #[test]
    fn fan_out_through_links_stops_at_the_cap() {
        let apps = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        write(&apps.path().join("a.desktop"), &app("a"));
        // About 2^18 entries along every path, more than the cap.
        fan_out(apps.path(), ext.path(), 17);
        let index = AppIndex::from_dirs([apps.path()]);
        assert!(index.get("a.desktop").is_some(), "what was found is kept");
        let messages: Vec<_> = index
            .diagnostics()
            .iter()
            .map(|d| (d.path.clone(), d.message.clone()))
            .collect();
        assert_eq!(
            messages,
            [(
                Some(apps.path().to_owned()),
                "more than 100000 entries to list; not scanned further".to_owned()
            )]
        );
    }

    /// Returns results from both walks of `root` within `limits`, recursive
    /// and with an explicit stack, for comparison: files found, folders and
    /// links recorded, and diagnostics.
    fn both_walks(root: &Path, limits: Limits) -> [String; 2] {
        [true, false].map(|recursive| one_walk(root, limits, recursive))
    }

    /// The result of one walk of `root`, as [`both_walks`] gives it.
    fn one_walk(root: &Path, limits: Limits, recursive: bool) -> String {
        let mut index = AppIndex::scan(Vec::new(), &HashMap::new(), limits);
        let files = if recursive {
            index.walk_root_recursive(root)
        } else {
            index.walk_root(root)
        };
        format!(
            "{:?}",
            (files, &index.folders, &index.links, &index.diagnostics)
        )
    }

    #[test]
    fn the_walk_gives_what_the_recursive_walk_gave() {
        let apps = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        let root = apps.path();
        let mut folder = root.to_owned();
        for level in 0..6 {
            write(&folder.join(format!("e{level}.desktop")), &app("e"));
            write(&folder.join(format!("f{level}.txt")), "");
            folder.push(format!("n{level}"));
        }
        std::os::unix::fs::symlink(root.join("n0"), root.join("n0/n1/up")).unwrap();
        std::os::unix::fs::symlink(root.join("n0/n1"), root.join("twice")).unwrap();
        std::os::unix::fs::symlink(root.join("e0.desktop"), root.join("n0/same.desktop")).unwrap();
        std::os::unix::fs::symlink(root.join("gone"), root.join("n0/dangling")).unwrap();
        fan_out(root, ext.path(), 6);
        let mut limits = Limits::unlimited();
        let [recursive, own_stack] = both_walks(root, limits);
        assert!(recursive.contains("e5.desktop") && recursive.contains("up"));
        assert_eq!(own_stack, recursive);
        for max_depth in 0..8 {
            limits.max_depth = Some(max_depth);
            let [recursive, own_stack] = both_walks(root, limits);
            assert_eq!(own_stack, recursive, "max_depth {max_depth}");
        }
        limits.max_depth = None;
        for max_listed in (0..200).step_by(7) {
            limits.max_listed = Some(max_listed);
            let [recursive, own_stack] = both_walks(root, limits);
            assert_eq!(own_stack, recursive, "max_listed {max_listed}");
        }
        let [recursive, own_stack] = both_walks(root, Limits::default());
        assert_eq!(own_stack, recursive);
        // A listing cut short.
        LISTING_FAULT.set(Some((root.join("n0"), Some(1), libc_eio())));
        let [recursive, own_stack] = both_walks(root, Limits::default());
        LISTING_FAULT.set(None);
        assert!(recursive.contains("cannot be listed"), "{recursive}");
        assert_eq!(own_stack, recursive);
    }

    /// `EIO`.
    fn libc_eio() -> i32 {
        rustix::io::Errno::IO.raw_os_error()
    }

    #[test]
    fn the_limits_default_to_those_without_with() {
        let limits = Limits::default();
        assert_eq!(
            (limits.max_file_len, limits.max_depth, limits.max_listed),
            (Some(xdg_kit_basedir::MAX_FILE_LEN), Some(32), Some(100_000))
        );
        let unlimited = Limits::unlimited();
        assert_eq!(
            (
                unlimited.max_file_len,
                unlimited.max_depth,
                unlimited.max_listed
            ),
            (None, None, None)
        );
        let root = tempfile::tempdir().unwrap();
        write(&root.path().join("a/b.desktop"), &app("b"));
        let index = AppIndex::from_dirs([root.path()]);
        assert_eq!(index.limits(), &Limits::default());
        assert!(index.get("a-b.desktop").is_some());
    }

    /// `name.desktop` in `depth` nested folders `d` under `root`.
    fn nested(root: &Path, depth: usize, name: &str) -> String {
        let mut path = root.to_owned();
        for _ in 0..depth {
            path.push("d");
        }
        write(&path.join(format!("{name}.desktop")), &app(name));
        format!("{}{name}.desktop", "d-".repeat(depth))
    }

    #[test]
    fn the_depth_walked_can_be_set_or_unlimited() {
        let root = tempfile::tempdir().unwrap();
        let deepest = nested(root.path(), 33, "deepest");
        let fourth = nested(root.path(), 4, "fourth");
        let default = AppIndex::from_dirs([root.path()]);
        assert!(default.get(&fourth).is_some());
        assert!(default.get(&deepest).is_none());
        let too_deep = |index: &AppIndex| {
            index
                .diagnostics()
                .iter()
                .filter(|d| d.message == "folders nested too deep; not scanned further")
                .count()
        };
        assert_eq!(too_deep(&default), 1);
        let mut limits = Limits {
            max_depth: Some(3),
            ..Limits::default()
        };
        let shallow = AppIndex::from_dirs_with([root.path()], &limits);
        assert!(shallow.get(&fourth).is_none());
        assert_eq!(too_deep(&shallow), 1);
        limits.max_depth = None;
        let all = AppIndex::from_dirs_with([root.path()], &limits);
        assert!(all.get(&deepest).is_some() && all.get(&fourth).is_some());
        assert_eq!(too_deep(&all), 0);
    }

    #[test]
    fn the_entries_listed_can_be_set_or_unlimited() {
        let root = tempfile::tempdir().unwrap();
        for name in ["a", "b", "c", "d"] {
            write(&root.path().join(format!("{name}.desktop")), &app(name));
        }
        let mut limits = Limits {
            max_listed: Some(2),
            ..Limits::default()
        };
        let index = AppIndex::from_dirs_with([root.path()], &limits);
        let (ids, messages) = ids_and_messages(&index);
        assert_eq!(ids, ["a.desktop", "b.desktop"]);
        assert_eq!(
            messages,
            ["more than 2 entries to list; not scanned further"]
        );
        // Past the default cap: links fan out to about 2^18 entries.
        let ext = tempfile::tempdir().unwrap();
        fan_out(root.path(), ext.path(), 17);
        limits.max_listed = None;
        let index = AppIndex::from_dirs_with([root.path()], &limits);
        assert_eq!(
            ids_and_messages(&index),
            (
                ["a", "b", "c", "d"]
                    .map(|name| format!("{name}.desktop"))
                    .to_vec(),
                Vec::new()
            )
        );
    }

    #[test]
    fn files_larger_than_the_limit_cannot_be_read() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("a.desktop");
        let text = app("a");
        write(&path, &text);
        let mut limits = Limits {
            max_file_len: Some(text.len() as u64 - 1),
            ..Limits::default()
        };
        let err = DesktopEntry::read_with_limits(&path, &limits).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);
        let err =
            crate::KeyFile::read_with_limits(&path, crate::KeyNames::Any, &limits).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);
        let index = AppIndex::from_dirs_with([root.path()], &limits);
        assert!(index.get("a.desktop").is_none());
        assert!(
            index.diagnostics()[0].message.starts_with("cannot be read"),
            "{:?}",
            index.diagnostics()
        );
        limits.max_file_len = Some(text.len() as u64);
        assert!(DesktopEntry::read_with_limits(&path, &limits).is_ok());
        let unlimited = Limits::unlimited();
        assert!(DesktopEntry::read_with_limits(&path, &unlimited).is_ok());
        assert!(crate::KeyFile::read_with_limits(&path, crate::KeyNames::Any, &unlimited).is_ok());
        assert!(
            AppIndex::from_dirs_with([root.path()], &unlimited)
                .get("a.desktop")
                .is_some()
        );
    }

    #[test]
    fn refresh_keeps_the_limits() {
        let root = tempfile::tempdir().unwrap();
        let second = nested(root.path(), 2, "second");
        let limits = Limits {
            max_depth: Some(1),
            max_file_len: Some(app("x").len() as u64),
            ..Limits::default()
        };
        let index = AppIndex::from_dirs_with([root.path()], &limits);
        assert!(index.get(&second).is_none());
        write(&root.path().join("long.desktop"), &app("longer"));
        let index = index.refresh().expect("a file was added");
        assert_eq!(index.limits(), &limits);
        assert!(index.get(&second).is_none());
        assert!(index.get("long.desktop").is_none());
        let messages = ids_and_messages(&index).1;
        assert!(
            messages.iter().any(|m| m.starts_with("cannot be read"))
                && messages
                    .iter()
                    .any(|m| m.starts_with("folders nested too deep")),
            "{messages:?}"
        );
    }

    #[test]
    fn folders_nested_as_deep_as_paths_allow_are_walked_on_a_small_stack() {
        let root = tempfile::tempdir().unwrap();
        // The walk works by path: it goes as deep as the system's path
        // length allows (4,096 bytes on Linux).
        let mut deepest = root.path().to_owned();
        let mut depth = 0;
        while deepest.join("d/a.desktop").as_os_str().len() < 4_000 {
            deepest.push("d");
            depth += 1;
        }
        fs::create_dir_all(&deepest).unwrap();
        write(&deepest.join("a.desktop"), &app("a"));
        assert!(depth > 1_900, "{depth}");
        let id = format!("{}a.desktop", "d-".repeat(depth));
        let root = root.path().to_owned();
        std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(move || {
                let limits = Limits {
                    max_depth: None,
                    ..Limits::default()
                };
                let index = AppIndex::from_dirs_with([&root], &limits);
                assert!(index.diagnostics().is_empty(), "{:?}", index.diagnostics());
                assert!(index.get(&id).is_some());
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// The IDs of `index`, and its diagnostics' messages.
    fn ids_and_messages(index: &AppIndex) -> (Vec<String>, Vec<String>) {
        (
            index.iter().map(|app| app.id().to_owned()).collect(),
            index
                .diagnostics()
                .iter()
                .map(|d| d.message.clone())
                .collect(),
        )
    }

    /// A root with `a.desktop`, `b.desktop`, `sub/c.desktop` and
    /// `sub/d.desktop`.
    fn four_entries() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        for name in ["a.desktop", "b.desktop", "sub/c.desktop", "sub/d.desktop"] {
            write(&root.path().join(name), &app("x"));
        }
        root
    }

    /// IDs and diagnostics are as without the interruption. This does not
    /// show that the partial listing is dropped: a name kept twice gives the
    /// same ID and file, listed once without a diagnostic.
    /// `an_interrupted_listing_keeps_none_of_its_names_below_the_cap` does.
    #[test]
    fn an_interrupted_listing_is_listed_again_from_the_start() {
        let root = four_entries();
        let all = ids_and_messages(&AppIndex::from_dirs([root.path()]));
        assert_eq!(all.0.len(), 4);
        let interrupted = rustix::io::Errno::INTR.raw_os_error();
        for folder in [root.path().to_owned(), root.path().join("sub")] {
            for at in [None, Some(1)] {
                LISTING_FAULT.set(Some((folder.clone(), at, interrupted)));
                let index = AppIndex::from_dirs([root.path()]);
                assert!(
                    LISTING_FAULT.with_borrow(Option::is_none),
                    "{folder:?} {at:?}: interrupted"
                );
                assert_eq!(ids_and_messages(&index), all, "{folder:?} {at:?}");
            }
        }
    }

    #[test]
    fn an_interrupted_listing_keeps_none_of_its_names_below_the_cap() {
        let root = four_entries();
        // The root's 3 names and `sub`'s 2 fit; those read before the
        // interruption, kept again, would not.
        let limits = Limits {
            max_listed: Some(5),
            ..Limits::default()
        };
        let whole = one_walk(root.path(), limits, true);
        assert!(!whole.contains("more than"), "{whole}");
        let interrupted = rustix::io::Errno::INTR.raw_os_error();
        for recursive in [true, false] {
            LISTING_FAULT.set(Some((root.path().to_owned(), Some(1), interrupted)));
            let walked = one_walk(root.path(), limits, recursive);
            assert!(LISTING_FAULT.with_borrow(Option::is_none), "{recursive}");
            assert_eq!(walked, whole, "{recursive}");
        }
    }

    #[test]
    fn a_folder_gone_when_opened_is_skipped_quietly() {
        let root = four_entries();
        let gone = rustix::io::Errno::NOENT.raw_os_error();
        for folder in [root.path().to_owned(), root.path().join("sub")] {
            LISTING_FAULT.set(Some((folder.clone(), None, gone)));
            let index = AppIndex::from_dirs([root.path()]);
            LISTING_FAULT.set(None);
            let (ids, messages) = ids_and_messages(&index);
            let left = if folder == root.path() { 0 } else { 2 };
            assert_eq!(ids.len(), left, "{folder:?}: {ids:?}");
            assert_eq!(messages, Vec::<String>::new(), "{folder:?}");
        }
    }

    #[test]
    fn a_listing_error_is_reported() {
        let root = four_entries();
        let failed = rustix::io::Errno::IO.raw_os_error();
        let sub = root.path().join("sub");
        for at in [None, Some(1)] {
            LISTING_FAULT.set(Some((sub.clone(), at, failed)));
            let index = AppIndex::from_dirs([root.path()]);
            LISTING_FAULT.set(None);
            let ids: Vec<_> = index.iter().map(|app| app.id().to_owned()).collect();
            // What was read before the error is kept.
            assert_eq!(ids.len(), 2 + at.unwrap_or(0), "{at:?}: {ids:?}");
            let messages: Vec<_> = index
                .diagnostics()
                .iter()
                .map(|d| (d.path.clone(), d.message.clone()))
                .collect();
            let error = std::io::Error::from_raw_os_error(failed);
            assert_eq!(
                messages,
                [(
                    Some(sub.clone()),
                    format!("cannot be listed: {error}; the entries not read are left out")
                )],
                "{at:?}"
            );
        }
    }

    #[test]
    fn a_link_to_neither_a_folder_nor_an_entry_is_watched() {
        let apps = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        let plain = ext.path().join("plain");
        write(&plain, "not an entry");
        let d2 = ext.path().join("d2");
        write(&d2.join("two.desktop"), &app("two"));
        let mid = ext.path().join("mid");
        std::os::unix::fs::symlink(&plain, &mid).unwrap();
        std::os::unix::fs::symlink(&mid, apps.path().join("c")).unwrap();
        let index = AppIndex::from_dirs([apps.path()]);
        assert_eq!(index.iter().count(), 0);
        assert!(index.refresh().is_none());
        retarget(&mid, &d2);
        let index = index.refresh().expect("c now leads to a folder");
        assert_same(&index, &AppIndex::from_dirs([apps.path()]));
        assert!(index.get("c-two.desktop").is_some());
        assert!(index.refresh().is_none());
    }

    #[test]
    fn links_that_stay_as_they_are_change_nothing() {
        let apps = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        write(&ext.path().join("d1/one.desktop"), &app("one"));
        write(&ext.path().join("plain"), "not an entry");
        let link = |target: &Path, name: &str| {
            std::os::unix::fs::symlink(target, apps.path().join(name)).unwrap();
        };
        link(&ext.path().join("d1"), "a");
        link(&ext.path().join("d1"), "b");
        link(&ext.path().join("plain"), "c");
        link(&ext.path().join("missing"), "dangling.desktop");
        link(&apps.path().join("loop"), "loop");
        link(apps.path(), "root");
        let index = AppIndex::from_dirs([apps.path()]);
        assert_same(&index, &AppIndex::from_dirs([apps.path()]));
        assert!(index.refresh().is_none());
        assert!(index.refresh().is_none(), "asked again");
    }

    #[test]
    fn editing_a_file_a_link_leads_to_changes_nothing() {
        let apps = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        let plain = ext.path().join("notes.txt");
        write(&plain, "one");
        std::os::unix::fs::symlink(&plain, apps.path().join("notes")).unwrap();
        let later = ext.path().join("later.desktop");
        std::os::unix::fs::symlink(&later, apps.path().join("later.desktop")).unwrap();
        let index = AppIndex::from_dirs([apps.path()]);
        // In place: the same file, so the link would be skipped as before.
        write(&plain, "one and two");
        assert!(index.refresh().is_none(), "only the linked file changed");
        write(&later, &app("later"));
        let index = index.refresh().expect("the dangling link now resolves");
        assert_same(&index, &AppIndex::from_dirs([apps.path()]));
        assert!(index.get("later.desktop").is_some());
    }

    #[test]
    fn files_in_an_unsupported_encoding_are_ignored() {
        let high = tempfile::tempdir().unwrap();
        let low = tempfile::tempdir().unwrap();
        write(
            &high.path().join("org.foo.desktop"),
            &format!("{}Encoding=Legacy-Mixed\n", app("legacy")),
        );
        write(&low.path().join("org.foo.desktop"), &app("low"));
        write(
            &low.path().join("utf8.desktop"),
            &format!("{}Encoding=UTF-8\n", app("u")),
        );
        let index = AppIndex::from_dirs([high.path(), low.path()]);
        assert_eq!(
            index
                .get("org.foo.desktop")
                .unwrap()
                .entry()
                .exec()
                .as_deref(),
            Some("low")
        );
        assert!(index.get("utf8.desktop").is_some());
        assert!(
            index
                .diagnostics()
                .iter()
                .any(|d| d.message.contains("Legacy-Mixed"))
        );
        assert!(index.refresh().is_none());
        write(&high.path().join("org.foo.desktop"), &app("fixed"));
        let index = index.refresh().expect("the ignored file was fixed");
        assert_eq!(
            index
                .get("org.foo.desktop")
                .unwrap()
                .entry()
                .exec()
                .as_deref(),
            Some("fixed")
        );
    }

    #[test]
    fn folder_links_that_loop_are_scanned_once() {
        let root = tempfile::tempdir().unwrap();
        write(&root.path().join("sub/a.desktop"), &app("a"));
        std::os::unix::fs::symlink(root.path(), root.path().join("sub/again")).unwrap();
        let index = AppIndex::from_dirs([root.path()]);
        assert_eq!(index.iter().count(), 1);
    }

    #[test]
    fn refresh_notices_new_changed_and_removed_files() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("a.desktop");
        write(&path, &app("one"));
        let index = AppIndex::from_dirs([root.path()]);
        assert!(index.refresh().is_none());
        write(&root.path().join("deep/b.desktop"), &app("b"));
        let index = index.refresh().expect("a new folder and file");
        assert!(index.get("deep-b.desktop").is_some());
        write(&path, &app("twotwo"));
        let index = index.refresh().expect("a changed file");
        assert_eq!(
            index.get("a.desktop").unwrap().entry().exec().as_deref(),
            Some("twotwo")
        );
        fs::remove_file(&path).unwrap();
        let index = index.refresh().expect("a removed file");
        assert!(index.get("a.desktop").is_none());
        assert!(index.refresh().is_none());
    }

    #[test]
    fn try_exec_looks_in_the_given_search_path() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let tool = bin.path().join("present-tool");
        fs::write(&tool, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        let with_try = |id: &str, try_exec: &str| {
            write(
                &root.path().join(id),
                &format!("[Desktop Entry]\nType=Application\nName=x\nExec=x\nTryExec={try_exec}\n"),
            );
        };
        with_try("yes.desktop", "present-tool");
        with_try("no.desktop", "absent-tool");
        with_try(
            "abs.desktop",
            &crate::key_file::escape(tool.to_str().unwrap(), false),
        );
        write(&root.path().join("none.desktop"), &app("none"));
        let index = AppIndex::from_dirs([root.path()]);
        // A `:` in the temporary folder's path would split it in a search
        // path, so the folder is named by its open descriptor instead.
        let bin_fd = fs::File::open(bin.path()).unwrap();
        let search =
            std::ffi::OsString::from(format!("/nonexistent:/proc/self/fd/{}", bin_fd.as_raw_fd()));
        assert!(index.get("yes.desktop").unwrap().try_exec(&search));
        assert!(!index.get("no.desktop").unwrap().try_exec(&search));
        assert!(index.get("abs.desktop").unwrap().try_exec(OsStr::new("")));
        with_try("relative.desktop", "sub/tool");
        fs::create_dir(bin.path().join("sub")).unwrap();
        fs::copy(&tool, bin.path().join("sub/tool")).unwrap();
        let index = AppIndex::from_dirs([root.path()]);
        assert!(index.get("relative.desktop").unwrap().try_exec(&search));
        assert!(index.get("none.desktop").unwrap().try_exec(OsStr::new("")));
    }

    /// Fails at once when `path`, under the temporary folder, holds a `:`,
    /// which would split it in an XDG path list.
    fn assert_listable(path: &Path) {
        assert!(
            !path.as_os_str().as_bytes().contains(&b':'),
            "TMPDIR contains ':', which an XDG path list cannot hold; set TMPDIR to a path without ':'"
        );
    }

    #[test]
    fn an_empty_folder_path_is_kept_as_given() {
        let index = AppIndex::from_dirs([""]);
        assert_eq!(index.dirs(), [Path::new("")]);
        assert_eq!(index.iter().count(), 0);
    }

    #[test]
    fn load_reads_data_home_then_data_dirs() {
        let home = tempfile::tempdir().unwrap();
        let system = tempfile::tempdir().unwrap();
        assert_listable(system.path());
        write(
            &home.path().join("applications/org.foo.desktop"),
            &app("home"),
        );
        write(
            &system.path().join("applications/org.foo.desktop"),
            &app("system"),
        );
        let (home_path, system_path) = (home.path().to_owned(), system.path().to_owned());
        let base = BaseDirs::from_vars(move |name| match name {
            "HOME" => Some("/home/u".into()),
            "XDG_DATA_HOME" => Some(home_path.clone().into_os_string()),
            "XDG_DATA_DIRS" => Some(system_path.clone().into_os_string()),
            _ => None,
        })
        .unwrap();
        let index = AppIndex::load(&base);
        assert_eq!(
            index
                .get("org.foo.desktop")
                .unwrap()
                .entry()
                .exec()
                .as_deref(),
            Some("home")
        );
    }
}
