//! Trash directories: where they are, and opening one with its checks.
//!
//! Opening walks component by component from the base directory (the home
//! trash's `$XDG_DATA_HOME`, a volume's top directory), each step relative
//! to the directory opened before it and refusing a symbolic link. Path
//! lookup follows symbolic links only up to the base directory, which the
//! caller names. Once open, a trash is used through its directory handles:
//! renaming or replacing its path afterwards does not redirect operations.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

use crate::context::Context;
use crate::info::AbsolutePaths;
use crate::limits::Limits;
use crate::sys::{self, FileKind, Ident, Meta};

/// The three kinds of trash directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrashKind {
    /// `$XDG_DATA_HOME/Trash`.
    Home,
    /// `$topdir/.Trash/$uid`, in a shared directory an administrator made.
    Shared,
    /// `$topdir/.Trash-$uid`.
    Private,
}

/// Where a trash directory is: its kind, and the directory its relative
/// `Path` values are taken from — `$XDG_DATA_HOME` for the home trash, the
/// volume's top directory for the others.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TrashDir {
    kind: TrashKind,
    base: PathBuf,
    uid: u32,
}

impl TrashDir {
    pub(crate) fn new(kind: TrashKind, base: PathBuf, uid: u32) -> Self {
        Self { kind, base, uid }
    }

    /// Its kind.
    #[must_use]
    pub fn kind(&self) -> TrashKind {
        self.kind
    }

    /// The directory relative `Path` values are taken from:
    /// `$XDG_DATA_HOME`, or the volume's top directory. (For the shared
    /// layout that is the top directory, not `$topdir/.Trash`: relative
    /// paths name files on the volume.)
    #[must_use]
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// The volume's top directory; `None` for the home trash.
    #[must_use]
    pub fn topdir(&self) -> Option<&Path> {
        (self.kind != TrashKind::Home).then_some(&*self.base)
    }

    /// The user whose trash this is.
    #[must_use]
    pub fn uid(&self) -> u32 {
        self.uid
    }

    /// The trash directory's path (`$trash`).
    #[must_use]
    pub fn path(&self) -> PathBuf {
        let mut path = self.base.clone();
        for component in self.components() {
            path.push(component);
        }
        path
    }

    /// The components below the base, in order.
    fn components(&self) -> Vec<OsString> {
        match self.kind {
            TrashKind::Home => vec!["Trash".into()],
            TrashKind::Shared => vec![".Trash".into(), self.uid.to_string().into()],
            TrashKind::Private => vec![format!(".Trash-{}", self.uid).into()],
        }
    }
}

impl Context {
    /// The home trash, `$XDG_DATA_HOME/Trash`.
    #[must_use]
    pub fn home_trash(&self) -> TrashDir {
        TrashDir::new(TrashKind::Home, self.data_home.clone(), self.uid)
    }

    /// The shared-layout trash of a volume, `$topdir/.Trash/$uid`. A
    /// relative `topdir` is taken from the working directory.
    #[must_use]
    pub fn shared_trash(&self, topdir: impl AsRef<Path>) -> TrashDir {
        TrashDir::new(TrashKind::Shared, self.anchor(topdir.as_ref()), self.uid)
    }

    /// The private-layout trash of a volume, `$topdir/.Trash-$uid`. A
    /// relative `topdir` is taken from the working directory.
    #[must_use]
    pub fn private_trash(&self, topdir: impl AsRef<Path>) -> TrashDir {
        TrashDir::new(TrashKind::Private, self.anchor(topdir.as_ref()), self.uid)
    }

    /// Opens an existing trash directory, checking it and its `files` and
    /// `info` directories. Nothing is created.
    ///
    /// # Errors
    ///
    /// [`OpenErrorKind::Absent`] when the trash directory, its `files`
    /// or its `info` does not exist; [`OpenErrorKind::Unsafe`] when a
    /// check fails; [`OpenErrorKind::Io`] for anything else.
    pub fn open(&self, dir: &TrashDir) -> Result<Trash, OpenError> {
        open(self, dir, false).map(|(trash, _)| trash)
    }

    /// Opens every trash directory that exists among the home trash and
    /// the per-volume ones of `topdirs` (both layouts of each), for
    /// listing. Trash directories that do not exist are left out; ones
    /// that cannot be opened, fail their checks, or lack `files` or `info`
    /// are reported, not passed over.
    pub fn discover<P: AsRef<Path>>(&self, topdirs: impl IntoIterator<Item = P>) -> Discovery {
        let mut discovery = Discovery {
            trashes: Vec::new(),
            problems: Vec::new(),
        };
        let mut dirs = vec![self.home_trash()];
        for topdir in topdirs {
            dirs.push(self.shared_trash(topdir.as_ref()));
            dirs.push(self.private_trash(topdir.as_ref()));
        }
        for dir in dirs {
            match self.open(&dir) {
                Ok(trash) => discovery.trashes.push(trash),
                // No trash directory there. One that exists without its
                // `files` or `info` is a problem, not an empty trash.
                Err(err)
                    if matches!(err.kind, OpenErrorKind::Absent)
                        && (err.path == dir.path() || !err.path.starts_with(dir.path())) => {}
                Err(err) => discovery.problems.push(err),
            }
        }
        discovery
    }
}

/// The trash directories found by [`Context::discover`].
#[derive(Debug)]
#[non_exhaustive]
pub struct Discovery {
    /// The ones opened, in order: the home trash, then for each top
    /// directory its shared and its private trash.
    pub trashes: Vec<Trash>,
    /// The ones that exist, or may, but cannot be used: a listing without
    /// them is incomplete.
    pub problems: Vec<OpenError>,
}

impl Discovery {
    /// Whether every trash directory that exists was opened.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.problems.is_empty()
    }
}

/// A check a trash directory failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CheckFailure {
    /// It is a symbolic link.
    Symlink,
    /// It is not a directory.
    NotDirectory(FileKind),
    /// `$topdir/.Trash` lacks the sticky bit (these are its mode bits).
    NotSticky(u32),
    /// It is owned by another user.
    Owner {
        /// The user it must belong to.
        expected: u32,
        /// The user it belongs to.
        found: u32,
    },
    /// Its group or others may write to it (these are its mode bits).
    Writable(u32),
}

impl fmt::Display for CheckFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Symlink => f.write_str("it is a symbolic link"),
            Self::NotDirectory(kind) => write!(f, "it is not a directory but {kind:?}"),
            Self::NotSticky(mode) => write!(f, "it lacks the sticky bit (mode {mode:04o})"),
            Self::Owner { expected, found } => {
                write!(f, "it belongs to user {found}, not {expected}")
            }
            Self::Writable(mode) => {
                write!(f, "its group or others may write to it (mode {mode:04o})")
            }
        }
    }
}

/// Why a trash directory could not be opened.
#[derive(Debug)]
pub struct OpenError {
    /// The directory concerned: the trash, one of its ancestors up to the
    /// base, or its `files` or `info`.
    pub path: PathBuf,
    /// What went wrong.
    pub kind: OpenErrorKind,
}

/// What went wrong opening a trash directory.
#[derive(Debug)]
#[non_exhaustive]
pub enum OpenErrorKind {
    /// It does not exist (and was not to be created).
    Absent,
    /// It failed a check: it must not be used.
    Unsafe(CheckFailure),
    /// Anything else.
    Io(io::Error),
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = self.path.display();
        match &self.kind {
            OpenErrorKind::Absent => write!(f, "{path} does not exist"),
            OpenErrorKind::Unsafe(check) => write!(f, "{path} is not used: {check}"),
            OpenErrorKind::Io(err) => write!(f, "{path}: {err}"),
        }
    }
}

impl std::error::Error for OpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            OpenErrorKind::Io(err) => Some(err),
            OpenErrorKind::Absent | OpenErrorKind::Unsafe(_) => None,
        }
    }
}

/// An open trash directory, with its `files` and `info` directories.
#[derive(Debug)]
pub struct Trash {
    pub(crate) dir: TrashDir,
    pub(crate) root: OwnedFd,
    pub(crate) files: OwnedFd,
    pub(crate) info: OwnedFd,
    pub(crate) id: Ident,
    pub(crate) uid: u32,
    pub(crate) absolute_paths: Option<AbsolutePaths>,
    pub(crate) limits: Limits,
}

impl Trash {
    /// Where it is.
    #[must_use]
    pub fn dir(&self) -> &TrashDir {
        &self.dir
    }

    pub(crate) fn files(&self) -> BorrowedFd<'_> {
        self.files.as_fd()
    }

    pub(crate) fn info(&self) -> BorrowedFd<'_> {
        self.info.as_fd()
    }
}

/// What a step of opening checks.
#[derive(Debug, Clone, Copy)]
enum Check {
    /// `$topdir/.Trash`: a real directory with the sticky bit, unless the
    /// policy exempts the top directory.
    Sticky { unchecked: bool },
    /// The user's own directory: theirs, and not writable by others.
    Owned,
}

/// Opens (and when `create`, creates) the trash directory `dir`. Also
/// returns the first error syncing a directory a new one was made in: the
/// directory exists, but may not survive a crash.
pub(crate) fn open(
    context: &Context,
    dir: &TrashDir,
    create: bool,
) -> Result<(Trash, Option<io::Error>), OpenError> {
    let mut unsynced = None;
    let base = dir.base();
    let io_error = |path: &Path, err: io::Error| OpenError {
        path: path.to_path_buf(),
        kind: if err.kind() == io::ErrorKind::NotFound {
            OpenErrorKind::Absent
        } else {
            OpenErrorKind::Io(err)
        },
    };
    let base_fd = match sys::open_dir(base) {
        Err(err)
            if err.kind() == io::ErrorKind::NotFound && create && dir.kind == TrashKind::Home =>
        {
            match xdg_kit_basedir::create_dir_all_synced(base, 0o700) {
                Ok(xdg_kit_basedir::Durability::Synced) => {}
                Ok(xdg_kit_basedir::Durability::NotSynced(err)) => {
                    unsynced = unsynced.or(Some(err));
                }
                Err(err) => return Err(io_error(base, err)),
            }
            sys::open_dir(base).map_err(|err| io_error(base, err))?
        }
        other => other.map_err(|err| io_error(base, err))?,
    };
    let mut path = base.to_path_buf();
    let mut fd = base_fd;
    let unchecked = context
        .policy
        .unchecked_sticky
        .iter()
        .any(|exempt| exempt == base);
    for (index, component) in dir.components().iter().enumerate() {
        path.push(component);
        let (check, may_create) = match (dir.kind, index) {
            // The administrator makes `.Trash`; it is never created here.
            (TrashKind::Shared, 0) => (Check::Sticky { unchecked }, false),
            _ => (Check::Owned, create),
        };
        fd = step(
            fd.as_fd(),
            component,
            &path,
            check,
            may_create,
            dir.uid,
            &mut unsynced,
        )?;
    }
    let id = sys::fstat(&fd).map_err(|err| io_error(&path, err))?.ident;
    let files = step(
        fd.as_fd(),
        OsStr::new("files"),
        &path.join("files"),
        Check::Owned,
        create,
        dir.uid,
        &mut unsynced,
    )?;
    let info = step(
        fd.as_fd(),
        OsStr::new("info"),
        &path.join("info"),
        Check::Owned,
        create,
        dir.uid,
        &mut unsynced,
    )?;
    let absolute_paths = (dir.kind != TrashKind::Home).then_some(context.policy.absolute_paths);
    Ok((
        Trash {
            dir: dir.clone(),
            root: fd,
            files,
            info,
            id,
            uid: dir.uid,
            absolute_paths,
            limits: context.limits.clone(),
        },
        unsynced,
    ))
}

/// Opens `name` in `parent` as a directory meeting `check`, creating it
/// (mode 0700) first when missing and `create`.
fn step(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    path: &Path,
    check: Check,
    create: bool,
    uid: u32,
    unsynced: &mut Option<io::Error>,
) -> Result<OwnedFd, OpenError> {
    let fail = |kind| OpenError {
        path: path.to_path_buf(),
        kind,
    };
    let classify = |meta: Meta| match meta.kind {
        FileKind::Symlink => OpenErrorKind::Unsafe(CheckFailure::Symlink),
        kind => OpenErrorKind::Unsafe(CheckFailure::NotDirectory(kind)),
    };
    match sys::lookup_at(parent, name) {
        Err(err) => return Err(fail(OpenErrorKind::Io(err))),
        Ok(None) if create => match sys::mkdir_at(parent, name, 0o700) {
            Ok(()) => {
                if let Err(err) = sys::sync(parent) {
                    unsynced.get_or_insert(err);
                }
            }
            // Someone else made it meanwhile: checked below like any.
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(fail(OpenErrorKind::Io(err))),
        },
        Ok(None) => return Err(fail(OpenErrorKind::Absent)),
        Ok(Some(meta)) if meta.kind != FileKind::Directory => return Err(fail(classify(meta))),
        Ok(Some(_)) => {}
    }
    let fd = match sys::open_dir_at(parent, name) {
        Ok(fd) => fd,
        // Replaced since the look: say by what, when it can be told.
        Err(err) => {
            return Err(fail(match sys::lookup_at(parent, name) {
                Ok(Some(meta)) if meta.kind != FileKind::Directory => classify(meta),
                Ok(None) => OpenErrorKind::Absent,
                _ => OpenErrorKind::Io(err),
            }));
        }
    };
    let meta = sys::fstat(&fd).map_err(|err| fail(OpenErrorKind::Io(err)))?;
    let failure = match check {
        Check::Sticky { unchecked } => {
            (!unchecked && meta.mode & 0o1000 == 0).then_some(CheckFailure::NotSticky(meta.mode))
        }
        Check::Owned if meta.uid != uid => Some(CheckFailure::Owner {
            expected: uid,
            found: meta.uid,
        }),
        Check::Owned => (meta.mode & 0o022 != 0).then_some(CheckFailure::Writable(meta.mode)),
    };
    match failure {
        Some(failure) => Err(fail(OpenErrorKind::Unsafe(failure))),
        None => Ok(fd),
    }
}
