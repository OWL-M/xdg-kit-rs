//! The installed desktop entries by desktop file ID (Desktop Entry spec
//! "Desktop File ID").

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use xdg_kit_basedir::{BaseDirs, FileStamp};

use crate::error::{quoted, shown_path};
use crate::{DesktopEntry, Desktops, Diagnostic, EntryType};

/// Deepest folder nesting followed below an `applications` folder.
const MAX_DEPTH: usize = 32;

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

/// The installed desktop entries (applications, links and folders),
/// indexed by desktop file ID from the files under the data directories'
/// `applications` folders. The first file for an ID takes precedence.
/// Unreadable files, unsupported encodings, and missing or unknown `Type`
/// values are ignored, allowing a later file to supply the same ID.
/// This is an immutable snapshot; [`AppIndex::refresh`] returns a new one
/// when files change.
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
    /// Links the walk met but did not follow, with what they lead to, so
    /// that pointing one elsewhere is noticed.
    links: Vec<(PathBuf, LinkStamp)>,
    /// The winning file of each ID, as an index into `found`; `None` when
    /// it says `Hidden=true` (the entry is deleted).
    by_id: BTreeMap<String, Option<usize>>,
    /// The IDs each root holds (by index into `roots`), shadowed ones
    /// included; files that could not be read or are ignored for their
    /// encoding or type are not counted, shadowed or not.
    held: Vec<BTreeSet<String>>,
    /// Files a more important folder shadows, read and stamped so that
    /// `held` stays right and a change to one is noticed.
    shadowed: Vec<(PathBuf, FileStamp, Arc<DesktopEntry>)>,
    diagnostics: Vec<Diagnostic>,
}

impl AppIndex {
    /// The desktop entries under `$XDG_DATA_HOME/applications` and each
    /// `$XDG_DATA_DIRS/applications`, in that order of importance.
    #[must_use]
    pub fn load(base_dirs: &BaseDirs) -> Self {
        let dirs = std::iter::once(base_dirs.data_home())
            .chain(base_dirs.data_dirs().iter().map(PathBuf::as_path))
            .map(|dir| dir.join("applications"));
        Self::from_dirs(dirs)
    }

    /// The desktop entries under exactly these `applications` folders, most
    /// important first.
    #[must_use]
    pub fn from_dirs(dirs: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
        let mut roots: Vec<(PathBuf, FileStamp)> = Vec::new();
        for dir in dirs {
            let dir = dir.into();
            if !roots.iter().any(|(known, _)| *known == dir) {
                let stamp = FileStamp::of(&dir);
                roots.push((dir, stamp));
            }
        }
        Self::scan(roots, &HashMap::new())
    }

    fn scan(
        roots: Vec<(PathBuf, FileStamp)>,
        cache: &HashMap<PathBuf, (FileStamp, Arc<DesktopEntry>)>,
    ) -> Self {
        let mut index = Self {
            roots: Vec::new(),
            folders: Vec::new(),
            found: Vec::new(),
            ignored: Vec::new(),
            links: Vec::new(),
            by_id: BTreeMap::new(),
            held: vec![BTreeSet::new(); roots.len()],
            shadowed: Vec::new(),
            diagnostics: Vec::new(),
        };
        // A root reached again under another path is scanned once; it holds
        // what it held the first time.
        let mut roots_seen: HashMap<(u64, u64), usize> = HashMap::new();
        for (root_index, (root, _)) in roots.iter().enumerate() {
            if let Ok(metadata) = fs::metadata(root) {
                match roots_seen.entry((metadata.dev(), metadata.ino())) {
                    std::collections::hash_map::Entry::Occupied(first) => {
                        let held = index.held[*first.get()].clone();
                        index.held[root_index] = held;
                        continue;
                    }
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        slot.insert(root_index);
                    }
                }
            }
            // Within a root, a folder reached again (a link) is scanned once.
            let mut visited: HashSet<(u64, u64)> = HashSet::new();
            let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
            index.walk(root, Path::new(""), 0, &mut visited, &mut files);
            // Byte order of the relative paths, so `foo-bar.desktop` comes
            // before `foo/bar.desktop`.
            files.sort_by(|a, b| a.0.as_os_str().as_bytes().cmp(b.0.as_os_str().as_bytes()));
            let mut ids_here: HashMap<String, PathBuf> = HashMap::new();
            for (relative, path) in files {
                let id = desktop_file_id(&relative);
                if let Some(first) = ids_here.get(&id) {
                    index.diagnostics.push(Diagnostic::new(
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
                ids_here.insert(id.clone(), path.clone());
                // A more important folder has this ID: the file is read only
                // to know whether the folder holds it.
                let shadowed = index.by_id.contains_key(&id);
                let stamp = FileStamp::of(&path);
                let entry = match cache.get(&path) {
                    Some((cached, entry)) if *cached == stamp => Arc::clone(entry),
                    _ => match DesktopEntry::read(&path) {
                        Ok(entry) => Arc::new(entry),
                        Err(err) => {
                            index.diagnostics.push(Diagnostic::new(
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
                    index
                        .diagnostics
                        .extend(entry.key_file().diagnostics().iter().cloned());
                }
                if let Some(reason) = ignored_because(&entry) {
                    index.diagnostics.push(Diagnostic::new(
                        Some(path.clone()),
                        None,
                        format!("{reason}; the file is ignored"),
                    ));
                    index.ignored.push((path, stamp));
                    continue;
                }
                if shadowed {
                    index.held[root_index].insert(id);
                    index.shadowed.push((path, stamp, entry));
                    continue;
                }
                let hidden = entry.hidden();
                index.found.push(Found {
                    root: root_index,
                    id: id.clone(),
                    path,
                    stamp,
                    entry,
                });
                let at = index.found.len() - 1;
                index.held[root_index].insert(id.clone());
                index.by_id.insert(id, (!hidden).then_some(at));
            }
        }
        index.roots = roots;
        index
    }

    /// Collects the `.desktop` files under `dir` as (path relative to the
    /// root, full path), and stamps every folder and every link it does
    /// not follow.
    fn walk(
        &mut self,
        dir: &Path,
        relative: &Path,
        depth: usize,
        visited: &mut HashSet<(u64, u64)>,
        files: &mut Vec<(PathBuf, PathBuf)>,
    ) {
        let Ok(metadata) = fs::metadata(dir) else {
            return;
        };
        if !metadata.is_dir() {
            return;
        }
        if !visited.insert((metadata.dev(), metadata.ino())) {
            // A folder reached again (a link) is not walked, but what it
            // leads to changes when a link on the way is pointed elsewhere,
            // so `refresh` notices.
            self.links.push((dir.to_owned(), LinkStamp::of(dir)));
            return;
        }
        self.folders.push((dir.to_owned(), FileStamp::of(dir)));
        if depth > MAX_DEPTH {
            self.diagnostics.push(Diagnostic::new(
                Some(dir.to_owned()),
                None,
                "folders nested too deep; not scanned further",
            ));
            return;
        }
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        // In name order, so a folder reached under two names gets the same
        // one every time.
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let name = entry.file_name();
            let path = entry.path();
            let relative = relative.join(&name);
            match fs::metadata(&path) {
                Ok(metadata) if metadata.is_dir() => {
                    self.walk(&path, &relative, depth + 1, visited, files);
                }
                Ok(metadata) if metadata.is_file() && name.as_bytes().ends_with(b".desktop") => {
                    files.push((relative, path));
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

    /// Returns a new snapshot if any source has changed, reusing entries
    /// whose files are unchanged. Returns `None` if nothing changed.
    ///
    /// Changes to `applications` folders, sub-folders, entries and skipped
    /// files are detected by their [`FileStamp`]. Links the walk encountered
    /// but did not follow are compared by target identity: its type, device
    /// and inode, or the absence of a target. These include aliases of folders
    /// already walked, links to ignored targets and links that lead nowhere.
    /// Retargeting such a link or replacing its target atomically (a new inode)
    /// is detected; editing its target in place is not.
    #[must_use]
    pub fn refresh(&self) -> Option<Self> {
        let changed = self
            .roots
            .iter()
            .chain(&self.folders)
            .chain(&self.ignored)
            .any(|(path, stamp)| FileStamp::of(path) != *stamp)
            || self
                .links
                .iter()
                .any(|(path, stamp)| LinkStamp::of(path) != *stamp)
            || self
                .found
                .iter()
                .any(|found| FileStamp::of(&found.path) != found.stamp)
            || self
                .shadowed
                .iter()
                .any(|(path, stamp, _)| FileStamp::of(path) != *stamp);
        if !changed {
            return None;
        }
        let cache = self
            .found
            .iter()
            .map(|found| (&found.path, &found.stamp, &found.entry))
            .chain(
                self.shadowed
                    .iter()
                    .map(|(path, stamp, entry)| (path, stamp, entry)),
            )
            .map(|(path, stamp, entry)| (path.clone(), (stamp.clone(), Arc::clone(entry))))
            .collect();
        let roots = self
            .roots
            .iter()
            .map(|(path, _)| (path.clone(), FileStamp::of(path)))
            .collect();
        Some(Self::scan(roots, &cache))
    }

    /// The desktop entry with desktop file ID `id`, unless its winning file
    /// says `Hidden=true`. It may be a link or a folder as well as an
    /// application (see [`DesktopEntry::entry_type`]).
    #[must_use]
    pub fn get(&self, id: &str) -> Option<AppEntry> {
        let at = (*self.by_id.get(id)?)?;
        Some(self.app(at))
    }

    fn app(&self, at: usize) -> AppEntry {
        let found = &self.found[at];
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
            .filter_map(|at| at.map(|at| self.app(at)))
    }

    /// The `applications` folders, most important first.
    #[must_use]
    pub fn dirs(&self) -> Vec<&Path> {
        self.roots.iter().map(|(path, _)| path.as_path()).collect()
    }

    /// Files that could not be read or are ignored for their encoding or
    /// type (shadowed ones too), ID clashes, and problems inside the
    /// winning files.
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
        self.held.get(dir).into_iter().flatten().map(String::as_str)
    }

    /// The folder (index into [`AppIndex::dirs`]) the desktop entry `id`
    /// came from.
    #[must_use]
    pub fn dir_of(&self, id: &str) -> Option<usize> {
        let at = (*self.by_id.get(id)?)?;
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

/// The desktop file ID of a file at `relative` under `applications`: its
/// components joined by `-`.
fn desktop_file_id(relative: &Path) -> String {
    relative
        .iter()
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn a_folder_link_skipped_as_an_alias_is_watched() {
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
        assert_eq!(ids(&index), ["a-one.desktop"], "b is the same folder");
        assert!(index.refresh().is_none());
        // Only a link outside the watched folders changes.
        retarget(&mid, &d2);
        let index = index.refresh().expect("b now leads to another folder");
        assert_same(&index, &AppIndex::from_dirs([apps.path()]));
        assert_eq!(ids(&index), ["a-one.desktop", "b-two.desktop"]);
        assert!(index.refresh().is_none());
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
