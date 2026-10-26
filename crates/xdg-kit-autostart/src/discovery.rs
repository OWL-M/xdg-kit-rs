//! Finding the autostart entries: the directories in precedence order,
//! one effective file per name, and everything shadowed behind it.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use xdg_kit_basedir::{BaseDirs, FileStamp};
use xdg_kit_desktop_entry::DesktopEntry;

use crate::{Diagnostic, Limits, work};

/// Which autostart directory a file is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Origin {
    /// The user's directory, `$XDG_CONFIG_HOME/autostart`: the most
    /// important, and the only one overrides are written to.
    User,
    /// A system directory, `$XDG_CONFIG_DIRS/autostart`, by its position
    /// in [`AutostartEntries::system_dirs`] (0 is the most important).
    System(usize),
}

impl Origin {
    /// The position in precedence order: 0 for the user's directory.
    fn rank(self) -> usize {
        match self {
            Self::User => 0,
            Self::System(at) => at + 1,
        }
    }

    fn of_rank(rank: usize) -> Self {
        match rank {
            0 => Self::User,
            at => Self::System(at - 1),
        }
    }
}

/// Why a file that claims its name could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadProblem {
    kind: io::ErrorKind,
    message: String,
}

impl ReadProblem {
    fn new(err: &io::Error) -> Self {
        Self {
            kind: err.kind(),
            message: err.to_string(),
        }
    }

    /// The kind of the error.
    #[must_use]
    pub fn kind(&self) -> io::ErrorKind {
        self.kind
    }

    /// The error's message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// One file of an autostart directory that claims its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    pub(crate) path: PathBuf,
    pub(crate) origin: Origin,
    pub(crate) content: Content,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Content {
    /// Read whole; the entry's key file gives back exactly these bytes
    /// while unedited, and the stamp is the very file they came from.
    Read {
        entry: Box<DesktopEntry>,
        stamp: FileStamp,
    },
    Unreadable(ReadProblem),
}

impl SourceFile {
    /// Where the file is.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Which directory it is in.
    #[must_use]
    pub fn origin(&self) -> Origin {
        self.origin
    }

    /// The parsed entry, when the file could be read. Problems in its
    /// text are the key file's diagnostics.
    #[must_use]
    pub fn entry(&self) -> Option<&DesktopEntry> {
        match &self.content {
            Content::Read { entry, .. } => Some(entry),
            Content::Unreadable(_) => None,
        }
    }

    /// Why the file could not be read, when it could not.
    #[must_use]
    pub fn problem(&self) -> Option<&ReadProblem> {
        match &self.content {
            Content::Read { .. } => None,
            Content::Unreadable(problem) => Some(problem),
        }
    }

    pub(crate) fn stamp(&self) -> Option<&FileStamp> {
        match &self.content {
            Content::Read { stamp, .. } => Some(stamp),
            Content::Unreadable(_) => None,
        }
    }
}

/// One autostart name (a file name such as `foo.desktop`) and every file
/// that has it, most important first. Only the first is ever used: the
/// others are shadowed whatever they say — the spec's rule, which is
/// also why a `Hidden=true` there hides every less important copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Autostart {
    pub(crate) name: OsString,
    pub(crate) sources: Vec<SourceFile>,
    /// A more important directory that could not be listed in full: it
    /// may hold a file of this name that would shadow the one found.
    pub(crate) uncertain: Option<PathBuf>,
}

impl Autostart {
    /// The file name, such as `foo.desktop`.
    #[must_use]
    pub fn name(&self) -> &OsStr {
        &self.name
    }

    /// The effective file: the one in the most important directory.
    #[must_use]
    pub fn source(&self) -> &SourceFile {
        &self.sources[0]
    }

    /// The files of the same name in less important directories, in
    /// precedence order: never used, kept for inspection (what a reset of
    /// a user override would bring back, say).
    #[must_use]
    pub fn shadowed(&self) -> &[SourceFile] {
        &self.sources[1..]
    }

    /// The effective file's entry, when it could be read.
    #[must_use]
    pub fn entry(&self) -> Option<&DesktopEntry> {
        self.source().entry()
    }

    /// Whether the effective file is the user's own, overriding a system
    /// file of the same name.
    #[must_use]
    pub fn is_user_override(&self) -> bool {
        self.source().origin == Origin::User && self.sources.len() > 1
    }

    /// The most important system file of this name, if any: the default
    /// a user override hides, or the effective file without one.
    #[must_use]
    pub fn system_default(&self) -> Option<&SourceFile> {
        self.sources
            .iter()
            .find(|source| source.origin != Origin::User)
    }
}

/// The autostart entries of a session: an immutable snapshot of every
/// autostart directory, one [`Autostart`] per file name.
///
/// A directory entry claims its name when it is a regular file, a
/// symbolic link to one included, whether or not it can then be read: an
/// unreadable, oversized or undecodable file still shadows every less
/// important copy and is itself never started
/// ([`Ineligible::Unreadable`](crate::Ineligible::Unreadable)), so a
/// user's broken override can never let the system entry it was meant to
/// hide start instead. A name whose entry cannot even be looked at (a
/// `stat` failing other than for a missing target) claims it too, for the
/// same reason. Directories, dangling links and other non-files named
/// `*.desktop` claim nothing. Names starting with `.` are hidden files
/// and not looked at; only names ending in `.desktop` are.
///
/// A directory that cannot be listed in full (a listing error other than
/// its absence, or more names than [`Limits::max_listed`]) contributes no
/// entries, and every entry of a less important directory is reported
/// uncertain ([`Ineligible::OverridesUnknown`](crate::Ineligible::OverridesUnknown)):
/// the unlisted names might have hidden them.
#[derive(Debug, Clone)]
pub struct AutostartEntries {
    user_dir: PathBuf,
    system_dirs: Vec<PathBuf>,
    /// Sorted by name.
    entries: Vec<Autostart>,
    /// User directory names that are not files (a dangling link, a
    /// directory), sorted: an override cannot be installed over them
    /// without replacing them.
    obstructed: Vec<OsString>,
    /// The most important directory that could not be listed in full.
    incomplete: Option<(usize, PathBuf)>,
    diagnostics: Vec<Diagnostic>,
    stamps: Vec<(PathBuf, FileStamp)>,
    limits: Limits,
}

impl AutostartEntries {
    /// Reads every `*.desktop` file in `config_home()/autostart` and each
    /// `config_dirs()` one, with the default [`Limits`].
    #[must_use]
    pub fn load(base_dirs: &BaseDirs) -> Self {
        Self::load_with(base_dirs, &Limits::default())
    }

    /// [`AutostartEntries::load`] within `limits`.
    #[must_use]
    pub fn load_with(base_dirs: &BaseDirs, limits: &Limits) -> Self {
        // Base directories are absolute: nothing to anchor.
        Self::anchored(
            base_dirs.config_home().join("autostart"),
            base_dirs
                .config_dirs()
                .iter()
                .map(|dir| dir.join("autostart"))
                .collect(),
            limits,
        )
    }

    /// Reads the autostart directories given: `user_dir`, the most
    /// important and the one overrides are written to, then
    /// `system_dirs` in precedence order. A system directory repeating an
    /// earlier one (the same path once absolute) is read once, at its
    /// first place. No environment variable is consulted.
    ///
    /// Relative paths are anchored to the working directory *now*, so the
    /// snapshot, its refreshes and every override made from it keep
    /// addressing the directories this call read, whatever the working
    /// directory is later.
    ///
    /// # Errors
    ///
    /// When a path cannot be made absolute: it is empty, or the working
    /// directory is gone.
    pub fn from_dirs(
        user_dir: impl AsRef<Path>,
        system_dirs: impl IntoIterator<Item = impl AsRef<Path>>,
        limits: &Limits,
    ) -> io::Result<Self> {
        let user_dir = std::path::absolute(user_dir)?;
        let system_dirs = system_dirs
            .into_iter()
            .map(std::path::absolute)
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self::anchored(user_dir, system_dirs, limits))
    }

    /// Discovery over absolute directories.
    fn anchored(user_dir: PathBuf, system_dirs: Vec<PathBuf>, limits: &Limits) -> Self {
        let mut kept: Vec<PathBuf> = Vec::new();
        for dir in system_dirs {
            // A few directories at most: the repeat check stays small.
            work::count(1 + dir.as_os_str().len());
            if dir != user_dir && !kept.contains(&dir) {
                kept.push(dir);
            }
        }
        let mut this = Self {
            user_dir,
            system_dirs: kept,
            entries: Vec::new(),
            obstructed: Vec::new(),
            incomplete: None,
            diagnostics: Vec::new(),
            stamps: Vec::new(),
            limits: limits.clone(),
        };
        this.discover();
        this
    }

    /// The user's autostart directory: where overrides go.
    #[must_use]
    pub fn user_dir(&self) -> &Path {
        &self.user_dir
    }

    /// The system autostart directories, most important first.
    #[must_use]
    pub fn system_dirs(&self) -> &[PathBuf] {
        &self.system_dirs
    }

    /// The directory of `origin`.
    #[must_use]
    pub fn dir_of(&self, origin: Origin) -> Option<&Path> {
        match origin {
            Origin::User => Some(&self.user_dir),
            Origin::System(at) => self.system_dirs.get(at).map(PathBuf::as_path),
        }
    }

    /// Every name, in byte order of the names: the eligible and the
    /// ineligible alike, so disabled entries stay inspectable.
    pub fn iter(&self) -> impl Iterator<Item = &Autostart> {
        self.entries.iter()
    }

    /// The entry of `name` (a file name such as `foo.desktop`).
    #[must_use]
    pub fn get(&self, name: impl AsRef<OsStr>) -> Option<&Autostart> {
        let name = name.as_ref();
        work::count(1 + name.len());
        self.entries
            .binary_search_by(|entry| entry.name.as_os_str().cmp(name))
            .ok()
            .map(|at| &self.entries[at])
    }

    /// Problems found while discovering: directories that could not be
    /// listed in full, files that could not be read, the budget cut.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// The limits this snapshot was read within.
    #[must_use]
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// A fresh snapshot, when any directory or file this one looked at
    /// changed since; `None` while everything still looks the same (one
    /// `stat` per directory and candidate). The snapshot itself never
    /// changes — after writing an override, refresh to see it.
    #[must_use]
    pub fn refresh(&self) -> Option<Self> {
        let unchanged = self.stamps.iter().all(|(path, stamp)| {
            work::count(1);
            FileStamp::of(path) == *stamp
        });
        if unchanged {
            return None;
        }
        Some(Self::anchored(
            self.user_dir.clone(),
            self.system_dirs.clone(),
            &self.limits,
        ))
    }

    pub(crate) fn is_obstructed(&self, name: &OsStr) -> bool {
        work::count(1 + name.len());
        self.obstructed
            .binary_search_by(|had| had.as_os_str().cmp(name))
            .is_ok()
    }

    /// Whether the user's directory was listed in full, so a name missing
    /// from it is really absent.
    pub(crate) fn user_dir_listed(&self) -> bool {
        !matches!(self.incomplete, Some((0, _)))
    }

    fn discover(&mut self) {
        let dirs: Vec<PathBuf> = std::iter::once(self.user_dir.clone())
            .chain(self.system_dirs.iter().cloned())
            .collect();
        let mut candidates: Vec<(usize, OsString)> = Vec::new();
        for (rank, dir) in dirs.iter().enumerate() {
            self.stamps.push((dir.clone(), FileStamp::of(dir)));
            // Past the budget already, a less important directory could
            // only add names the cut below drops: it is not listed, so the
            // names collected stay within the budget and one directory's
            // listing bound.
            if candidates.len() > self.limits.max_entries {
                continue;
            }
            match self.list(dir) {
                Ok(names) => candidates.extend(names.into_iter().map(|name| (rank, name))),
                Err(message) => {
                    self.diagnostics
                        .push(Diagnostic::new(Some(dir.clone()), message));
                    if self.incomplete.is_none() {
                        self.incomplete = Some((rank, dir.clone()));
                    }
                }
            }
        }
        // Precedence order, then byte order of the names: what the budget
        // keeps is deterministic, and cutting it can only drop entries,
        // never let one through that a dropped file would have shadowed.
        candidates.sort_unstable_by(|(a_rank, a), (b_rank, b)| {
            work::count(1 + a.len().min(b.len()));
            a_rank.cmp(b_rank).then_with(|| a.cmp(b))
        });
        if candidates.len() > self.limits.max_entries {
            let (rank, _) = &candidates[self.limits.max_entries];
            self.diagnostics.push(Diagnostic::new(
                Some(dirs[*rank].clone()),
                format!(
                    "more than {} autostart files; the rest are skipped",
                    self.limits.max_entries
                ),
            ));
            candidates.truncate(self.limits.max_entries);
        }
        let mut claims: Vec<(OsString, SourceFile)> = Vec::new();
        for (rank, name) in candidates {
            let path = dirs[rank].join(&name);
            work::count(1 + path.as_os_str().len());
            self.stamps.push((path.clone(), FileStamp::of(&path)));
            match self.claim(&path, Origin::of_rank(rank)) {
                Some(source) => claims.push((name, source)),
                None if rank == 0 => self.obstructed.push(name),
                None => {}
            }
        }
        // Grouped by name, each name's files in precedence order: the
        // sort is stable, and the claims arrived in precedence order.
        claims.sort_by(|(a, _), (b, _)| {
            work::count(1 + a.len().min(b.len()));
            a.cmp(b)
        });
        let incomplete = self.incomplete.clone();
        for (name, source) in claims {
            work::count(1 + name.len());
            match self.entries.last_mut() {
                Some(last) if last.name == name => last.sources.push(source),
                _ => {
                    let uncertain = incomplete
                        .as_ref()
                        .filter(|(rank, _)| source.origin.rank() > *rank)
                        .map(|(_, dir)| dir.clone());
                    self.entries.push(Autostart {
                        name,
                        sources: vec![source],
                        uncertain,
                    });
                }
            }
        }
    }

    /// The `*.desktop` names of `dir`, all of them or an error saying why
    /// not. An absent directory (or a path that is not one) has none.
    fn list(&self, dir: &Path) -> Result<Vec<OsString>, String> {
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
                return Err(format!(
                    "cannot be listed: {err}; less important autostart entries are uncertain"
                ));
            }
        };
        let mut names = Vec::new();
        // Every entry visited counts, `.desktop` or not: the bound is on
        // the listing's work, not only on what it keeps.
        let mut visited = 0usize;
        for entry in listed {
            let entry = entry.map_err(|err| {
                format!(
                    "cannot be listed in full: {err}; less important autostart entries are uncertain"
                )
            })?;
            visited += 1;
            if visited > self.limits.max_listed {
                return Err(format!(
                    "more than {} directory entries; the directory is skipped and less \
                     important autostart entries are uncertain",
                    self.limits.max_listed
                ));
            }
            let name = entry.file_name();
            work::count(1 + name.len());
            if is_candidate(&name) {
                names.push(name);
            }
        }
        Ok(names)
    }

    /// The file at `path` as a claim of its name, or `None` when it
    /// claims nothing (not a file, or gone).
    fn claim(&mut self, path: &Path, origin: Origin) -> Option<SourceFile> {
        let unreadable = |this: &mut Self, err: &io::Error| {
            this.diagnostics.push(Diagnostic::new(
                Some(path.to_owned()),
                format!("cannot be read: {err}; it still hides less important copies"),
            ));
            Some(SourceFile {
                path: path.to_owned(),
                origin,
                content: Content::Unreadable(ReadProblem::new(err)),
            })
        };
        match fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => return None,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
            Err(err) => return unreadable(self, &err),
        }
        match xdg_kit_basedir::read_stamped(path, self.limits.max_file_len) {
            Ok(Some(read)) => {
                work::count(read.bytes.len());
                // Parsed as the file it was read from, so launching has its
                // path (`%k`, a D-Bus activation's bus name), with no second
                // read that could pair other bytes with this baseline.
                let entry = Box::new(DesktopEntry::parse_at(&read.bytes, path));
                Some(SourceFile {
                    path: path.to_owned(),
                    origin,
                    content: Content::Read {
                        entry,
                        stamp: read.stamp,
                    },
                })
            }
            // Gone between the listing and the read: it claims nothing.
            Ok(None) => None,
            Err(err) => unreadable(self, &err),
        }
    }
}

/// Whether a directory entry's name is an autostart file name: ending in
/// `.desktop` after a non-empty stem, and not hidden (a leading `.`).
pub(crate) fn is_candidate(name: &OsStr) -> bool {
    let bytes = name.as_bytes();
    bytes.len() > ".desktop".len() && bytes.ends_with(b".desktop") && !bytes.starts_with(b".")
}
