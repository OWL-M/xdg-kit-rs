//! Trash directories: where they are, and opening one with its checks.
//!
//! Opening walks component by component from the base directory (the home
//! trash's `$XDG_DATA_HOME`, a volume's top directory), each step relative
//! to the directory opened before it and refusing a symbolic link. Path
//! lookup follows symbolic links only up to the base directory, which the
//! caller names. Once open, a trash is used through its directory handles:
//! renaming or replacing its path afterwards does not redirect operations.
//!
//! One exception, which [`Policy::follow_home_link`](crate::Policy::follow_home_link)
//! allows: `$XDG_DATA_HOME/Trash` itself may be a symbolic link, to a
//! directory of any name. A per-volume trash is reached through directories
//! other users may control, where a link is an attack; the home trash's own
//! name lies in the user's data directory, where a link is how a home trash
//! is kept elsewhere. The link is read once and its target followed one
//! component at a time from open directories, each looked at before it is
//! opened and opened without following a link, so that a link swapped in on
//! the way is refused rather than followed; links met on the way are read
//! the same way. The directory reached is then held open and checked as
//! the home trash's own directory always is. What the link says after that
//! no longer matters to that open trash.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

use crate::context::Context;
use crate::exclude::{Excluded, Exclusions};
use crate::fault::{self, Stage};
use crate::info::AbsolutePaths;
use crate::limits::Limits;
use crate::sys::{self, FileKind, Ident, Meta};
use crate::transfer::Fallbacks;

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

/// The home trash's own name in `$XDG_DATA_HOME`.
const HOME_NAME: &str = "Trash";

/// How many symbolic links following the home trash's may read, its own
/// included: as many as Linux follows in one lookup.
const MAX_LINKS: u32 = 40;

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
            TrashKind::Home => vec![HOME_NAME.into()],
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
    /// `info` directories. Nothing is created. A home trash reached through
    /// a symbolic link ([`Policy::follow_home_link`](crate::Policy::follow_home_link))
    /// is opened where the link leads now; the [`Trash`] stays on that
    /// directory ([`Trash::link_target`]).
    ///
    /// # Errors
    ///
    /// [`OpenErrorKind::Absent`] when the trash directory, its `files`
    /// or its `info` does not exist; [`OpenErrorKind::Unsafe`] when a
    /// check fails; [`OpenErrorKind::BrokenLink`] when the home trash's
    /// link leads to no directory; [`OpenErrorKind::Io`] for anything
    /// else.
    pub fn open(&self, dir: &TrashDir) -> Result<Trash, OpenError> {
        open(self, dir, false).map(|(trash, _)| trash)
    }

    /// Opens every trash directory that exists among the home trash and
    /// the per-volume ones of `topdirs` (both layouts of each), for
    /// listing. Trash directories that do not exist are left out; ones
    /// that cannot be opened, fail their checks, or lack `files` or `info`
    /// are reported, not passed over — a home trash link that leads
    /// nowhere among them ([`OpenErrorKind::BrokenLink`]).
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
    /// The home trash is a symbolic link
    /// ([`Policy::follow_home_link`](crate::Policy::follow_home_link)) that
    /// leads to no directory: it dangles, it leads to another kind of
    /// file, or links lead to links too often (a loop). Nothing is created
    /// through it.
    BrokenLink {
        /// Where following it stopped.
        stopped: PathBuf,
        /// Why: nothing there (`NotFound`), not a directory
        /// (`NotADirectory`), or too many links (`ELOOP`).
        error: io::Error,
    },
    /// Anything else.
    Io(io::Error),
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = self.path.display();
        match &self.kind {
            OpenErrorKind::Absent => write!(f, "{path} does not exist"),
            OpenErrorKind::Unsafe(check) => write!(f, "{path} is not used: {check}"),
            OpenErrorKind::BrokenLink { stopped, error } => write!(
                f,
                "{path} is a symbolic link that leads to no directory: at {}: {error}",
                stopped.display()
            ),
            OpenErrorKind::Io(err) => write!(f, "{path}: {err}"),
        }
    }
}

impl std::error::Error for OpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            OpenErrorKind::Io(err) | OpenErrorKind::BrokenLink { error: err, .. } => Some(err),
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
    pub(crate) followed: Option<PathBuf>,
    pub(crate) uid: u32,
    pub(crate) absolute_paths: Option<AbsolutePaths>,
    pub(crate) fallbacks: Fallbacks,
    pub(crate) limits: Limits,
}

impl Trash {
    /// Where it is.
    #[must_use]
    pub fn dir(&self) -> &TrashDir {
        &self.dir
    }

    /// Where the home trash's symbolic link led when this was opened
    /// ([`Policy::follow_home_link`](crate::Policy::follow_home_link)): the
    /// resolved path of the directory opened. `None` when it was not reached
    /// through a link. Observed then, not kept up: this trash stays on the
    /// directory opened even if it is renamed or the link retargeted, and
    /// the path may by now name something else.
    #[must_use]
    pub fn link_target(&self) -> Option<&Path> {
        self.followed.as_deref()
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

/// What [`walk`] does about missing directories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Open only: a missing directory is [`OpenErrorKind::Absent`].
    Open,
    /// Create what is missing (what may be created).
    Create,
    /// Create nothing: stop at the first missing directory that could be
    /// created, and say which; check that an existing trash's `files` and
    /// `info` may be written to.
    Probe,
}

/// What [`walk`] found.
pub(crate) enum Walked {
    /// The trash directory, open; and the first error syncing a directory
    /// a new one was made in (it exists, but may not survive a crash).
    Opened(Trash, Option<io::Error>),
    /// ([`Mode::Probe`] only.) This directory, and all below it, would
    /// have to be made, which the checks so far allow.
    WouldCreate(PathBuf),
}

/// How [`walk_with`] reaches the home trash's own directory. Ignored for
/// the other kinds.
pub(crate) enum HomeRoot {
    /// Look at `$XDG_DATA_HOME/Trash` now, and follow a link there as the
    /// policy allows — in [`Mode::Open`] only, which applies no exclusions.
    /// Creating and probing walks are given what was found before
    /// ([`follow_home`]), so that a link is followed once per operation
    /// and with its exclusions; given `Look`, they refuse a link.
    Look,
    /// Looked at already: no link to follow. A link there now is refused,
    /// as without the policy.
    NoLink,
    /// Followed already: this directory, open and checked.
    Followed(Followed),
    /// Followed already, for `dir`, and it leads to no usable directory.
    Broken {
        /// The home trash followed.
        dir: TrashDir,
        /// Why it cannot be used.
        error: OpenError,
        /// The directory it did lead to, when that failed its checks: still
        /// a trash directory, with what it holds, though it takes nothing
        /// new.
        reached: Option<PathBuf>,
    },
    /// Followed already, and it leads into an excluded place.
    Excluded {
        /// The exclusion met.
        excluded: Excluded,
        /// The directory it did lead to, when only its `files` or `info`
        /// is excluded: reaching it touches neither.
        reached: Option<PathBuf>,
    },
}

impl HomeRoot {
    /// Where the link led, if it led to a directory, usable or not.
    pub(crate) fn target(&self) -> Option<&Path> {
        match self {
            Self::Followed(followed) => Some(&followed.path),
            Self::Broken { reached, .. } | Self::Excluded { reached, .. } => reached.as_deref(),
            Self::Look | Self::NoLink => None,
        }
    }

    /// The exclusion following met, if any.
    pub(crate) fn excluded(&self) -> Option<&Excluded> {
        match self {
            Self::Excluded { excluded, .. } => Some(excluded),
            _ => None,
        }
    }
}

/// The home trash's own directory, reached through the symbolic link at
/// `$XDG_DATA_HOME/Trash`: open, checked, and where the link led.
#[derive(Debug)]
pub(crate) struct Followed {
    /// The home trash it is the directory of.
    pub(crate) dir: TrashDir,
    pub(crate) fd: OwnedFd,
    /// The resolved path of the directory, as followed.
    pub(crate) path: PathBuf,
}

/// Why following the home trash's link stopped.
enum FollowError {
    /// It cannot be used: see [`OpenError`].
    Open(OpenError),
    /// It leads to the directory `reached`, which cannot be used: it fails
    /// its checks, or cannot be opened for reading or inspected. It is a
    /// trash directory all the same, with what it holds.
    Refused { error: OpenError, reached: PathBuf },
    /// It would visit an excluded place, which was not touched; `reached`
    /// when that is only the `files` or `info` of the directory reached.
    Excluded {
        excluded: Box<Excluded>,
        reached: Option<PathBuf>,
    },
}

/// Looks once at the home trash `dir` (`$XDG_DATA_HOME/Trash`, this
/// context's or another's), and when it is a symbolic link the policy
/// follows, follows it, checking every place that would visit against
/// `exclusions` before touching it: for trashing and planning, which then
/// use what is found here for the mount comparison, the checks and the
/// move alike. [`HomeRoot::NoLink`] when there is no link to follow (or no
/// `$XDG_DATA_HOME`, which the walk then creates or reports). Failing to
/// look is not taken for "no link": that is [`HomeRoot::Broken`].
pub(crate) fn follow_home(context: &Context, dir: &TrashDir, exclusions: &Exclusions) -> HomeRoot {
    if !context.policy.follow_home_link || dir.kind != TrashKind::Home {
        return HomeRoot::NoLink;
    }
    let broken = |error, reached| HomeRoot::Broken {
        dir: dir.clone(),
        error,
        reached,
    };
    match look_home(dir, Some(exclusions)) {
        None => HomeRoot::NoLink,
        Some(Ok(followed)) => HomeRoot::Followed(followed),
        // Not followed to a directory: where it leads may still be found
        // by path, for recognising what that holds.
        Some(Err(FollowError::Open(error))) => broken(error, locate(dir, exclusions)),
        Some(Err(FollowError::Refused { error, reached })) => broken(error, Some(reached)),
        Some(Err(FollowError::Excluded { excluded, reached })) => HomeRoot::Excluded {
            excluded: *excluded,
            reached,
        },
    }
}

/// Opens the home trash `dir`'s `$XDG_DATA_HOME` for lookups (search
/// permission suffices where the system allows) and follows `Trash` there
/// if it is a symbolic link. `None` when it is not one, or when there is no
/// `$XDG_DATA_HOME`; failing to open or look is an error, not `None`.
fn look_home(
    dir: &TrashDir,
    exclusions: Option<&Exclusions>,
) -> Option<Result<Followed, FollowError>> {
    let opened = if fault::hits(Stage::OpenDataHome) {
        Err(rustix::io::Errno::ACCESS.into())
    } else {
        sys::open_search(dir.base())
    };
    let base = match opened {
        Ok(base) => base,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
        Err(err) => return Some(Err(io_failure(dir.base(), err))),
    };
    match sys::lookup_at(base.as_fd(), OsStr::new(HOME_NAME)) {
        Ok(Some(meta)) if meta.kind == FileKind::Symlink => {}
        Ok(_) => return None,
        Err(err) => return Some(Err(io_failure(&dir.path(), err))),
    }
    Some(follow_at(dir, base.as_fd(), exclusions))
}

/// Where the home trash `dir`'s link leads, found by path alone: for
/// recognising what that directory holds as trashed where following it
/// from open directories failed (on systems without `O_PATH`, an
/// `$XDG_DATA_HOME` that can be searched but not read), never for using
/// it. Looking at a name and reading a link need search permission only.
/// Each step is checked against `exclusions` before it is taken, as in
/// following; an excluded `files` or `info` does not matter here, as
/// neither is entered. `None` when it leads to no directory, or cannot be
/// told.
pub(crate) fn locate(dir: &TrashDir, exclusions: &Exclusions) -> Option<PathBuf> {
    let following = Following {
        link: dir.path(),
        exclusions: Some(exclusions),
        deferred: std::cell::RefCell::new(None),
    };
    let link = dir.path();
    if sys::lookup_path_nofollow(&link).ok()??.kind != FileKind::Symlink {
        return None;
    }
    let target = sys::read_link(&link).ok()?;
    if target.is_empty() {
        return None;
    }
    let mut path = if Path::new(&target).is_absolute() {
        following.check_trash(Path::new(&target)).ok()?;
        PathBuf::from("/")
    } else {
        following.check_trash(&dir.base().join(&target)).ok()?;
        let real = sys::canonicalize(dir.base()).ok()?;
        following.check_trash(&real.join(&target)).ok()?;
        real
    };
    let mut pending = steps(&target);
    let mut links = 1;
    while let Some(name) = pending.pop_front() {
        // The path so far has no link in it: `..` is its parent.
        if name == ".." {
            path.pop();
            continue;
        }
        let at = path.join(&name);
        match sys::lookup_path_nofollow(&at).ok()??.kind {
            FileKind::Symlink => {
                links += 1;
                if links > MAX_LINKS {
                    return None;
                }
                let next = sys::read_link(&at).ok()?;
                let mut ahead = path.join(&next);
                ahead.extend(&pending);
                following.check_trash(&ahead).ok()?;
                if Path::new(&next).is_absolute() {
                    path = PathBuf::from("/");
                }
                for step in steps(&next).into_iter().rev() {
                    pending.push_front(step);
                }
            }
            FileKind::Directory => path = at,
            _ => return None,
        }
    }
    following.check(&path).ok()?;
    Some(path)
}

/// Follows the symbolic link `Trash` in `base` (the home trash `dir`'s
/// `$XDG_DATA_HOME`, open) to a directory, and checks that directory as
/// the home trash's own: its user's, and not writable by group or others.
///
/// The link is read once. Its target is then followed one component at a
/// time from open directories: each name looked at without following it,
/// a directory opened without following a link (so that a link swapped in
/// after the look is refused, not followed) and for lookups only (search
/// permission suffices, as for a path lookup, where the system allows), a
/// link read and its target followed the same way, `..` taken from the
/// directory open. With `exclusions`, each link's target is checked, with
/// what remains to follow, before any of it is looked at. A relative
/// target starts at `$XDG_DATA_HOME`'s resolved path, which must still name
/// `base`. The directory reached is opened again for reading.
fn follow_at(
    dir: &TrashDir,
    base: BorrowedFd<'_>,
    exclusions: Option<&Exclusions>,
) -> Result<Followed, FollowError> {
    let following = Following {
        link: dir.path(),
        exclusions,
        deferred: std::cell::RefCell::new(None),
    };
    let target = sys::read_link_at(base, OsStr::new(HOME_NAME))
        .map_err(|err| io_failure(&following.link, err))?;
    if fault::hits(Stage::RetargetHomeLink) {
        swap_link(base, OsStr::new(HOME_NAME));
    }
    // Some systems store an empty link; lookup takes it for nothing, not
    // for the directory holding it.
    if target.is_empty() {
        return Err(following.broken(
            &following.link,
            io::Error::new(io::ErrorKind::NotFound, "the link is empty"),
        ));
    }
    let (mut fd, mut path) = following.start(dir, base, &target)?;
    let mut pending = steps(&target);
    let mut links = 1;
    while let Some(name) = pending.pop_front() {
        if name == ".." {
            fd = sys::open_search_at(fd.as_fd(), &name).map_err(|err| io_failure(&path, err))?;
            path.pop();
            continue;
        }
        let at = path.join(&name);
        let meta = match sys::lookup_at(fd.as_fd(), &name) {
            Ok(Some(meta)) => meta,
            Ok(None) => {
                return Err(following.broken(
                    &at,
                    io::Error::new(io::ErrorKind::NotFound, "nothing has this name"),
                ));
            }
            Err(err) => return Err(io_failure(&at, err)),
        };
        match meta.kind {
            FileKind::Symlink => {
                links += 1;
                if links > MAX_LINKS {
                    return Err(following.broken(&at, rustix::io::Errno::LOOP.into()));
                }
                let next =
                    sys::read_link_at(fd.as_fd(), &name).map_err(|err| io_failure(&at, err))?;
                let mut ahead = path.join(&next);
                ahead.extend(&pending);
                following.check_trash(&ahead)?;
                if Path::new(&next).is_absolute() {
                    (fd, path) = open_root()?;
                }
                for step in steps(&next).into_iter().rev() {
                    pending.push_front(step);
                }
            }
            FileKind::Directory => {
                if fault::hits(Stage::SwapOnTheWay) {
                    let mut aside = name.clone();
                    aside.push(".orig");
                    let _ = rustix::fs::renameat(fd.as_fd(), &name, fd.as_fd(), &aside);
                    swap_link(fd.as_fd(), &name);
                }
                fd = match sys::open_search_at(fd.as_fd(), &name) {
                    Ok(fd) => fd,
                    // The last name: the directory reached, though it cannot
                    // be opened (read-only opens need read permission).
                    Err(err) if pending.is_empty() => {
                        return Err(FollowError::Refused {
                            error: OpenError {
                                path: at.clone(),
                                kind: OpenErrorKind::Io(err),
                            },
                            reached: at,
                        });
                    }
                    Err(err) => return Err(io_failure(&at, err)),
                };
                path = at;
            }
            kind => {
                return Err(following.broken(
                    &at,
                    io::Error::new(
                        io::ErrorKind::NotADirectory,
                        format!("this is {kind:?}, not a directory"),
                    ),
                ));
            }
        }
    }
    following.finish(dir, base, fd.as_fd(), path)
}

/// The root directory, open: where an absolute target starts.
fn open_root() -> Result<(OwnedFd, PathBuf), FollowError> {
    let root = Path::new("/");
    let fd = sys::open_search_at(rustix::fs::CWD, root.as_os_str())
        .map_err(|err| io_failure(root, err))?;
    Ok((fd, root.to_path_buf()))
}

/// An I/O error at `path` while following the home trash's link.
fn io_failure(path: &Path, err: io::Error) -> FollowError {
    FollowError::Open(OpenError {
        path: path.to_path_buf(),
        kind: OpenErrorKind::Io(err),
    })
}

/// Following the home trash's link: where it is, for errors, and the
/// exclusions that apply, if any.
struct Following<'a> {
    link: PathBuf,
    exclusions: Option<&'a Exclusions>,
    /// The first exclusion met on a `files` or `info`: it bars using the
    /// trash, not reaching its directory, so following goes on to learn
    /// where that is.
    deferred: std::cell::RefCell<Option<Excluded>>,
}

impl Following<'_> {
    fn broken(&self, stopped: &Path, error: io::Error) -> FollowError {
        FollowError::Open(OpenError {
            path: self.link.clone(),
            kind: OpenErrorKind::BrokenLink {
                stopped: stopped.to_path_buf(),
                error,
            },
        })
    }

    fn check(&self, path: &Path) -> Result<(), FollowError> {
        match self.exclusions {
            Some(exclusions) => exclusions
                .check(path)
                .map_err(|excluded| FollowError::Excluded {
                    excluded: Box::new(excluded),
                    reached: None,
                }),
            None => Ok(()),
        }
    }

    /// Checks `path`, a spelling of where the trash directory may be, as
    /// trashing into it would reach it: the directory, then `files` and
    /// `info` in it. Each spelling is checked so, as an exclusion may name
    /// any of them, and resolving the path further discards it. An
    /// excluded `files` or `info` is kept for later ([`Following::deferred`]):
    /// following never enters them.
    fn check_trash(&self, path: &Path) -> Result<(), FollowError> {
        self.check(path)?;
        let Some(exclusions) = self.exclusions else {
            return Ok(());
        };
        for child in ["files", "info"] {
            if self.deferred.borrow().is_some() {
                break;
            }
            if let Err(excluded) = exclusions.check(&path.join(child)) {
                self.deferred.replace(Some(excluded));
            }
        }
        Ok(())
    }

    /// Where following `target`, the link's own, starts — once the whole
    /// target is checked: the root, or for a relative one `base` by
    /// `$XDG_DATA_HOME`'s resolved path, so that `..` is taken as lookup
    /// takes it. That path must still name `base`. A relative target is
    /// checked first from `$XDG_DATA_HOME` as configured, before resolving
    /// it, then from the resolved path: either spelling may be the one an
    /// exclusion names.
    fn start(
        &self,
        dir: &TrashDir,
        base: BorrowedFd<'_>,
        target: &OsStr,
    ) -> Result<(OwnedFd, PathBuf), FollowError> {
        if Path::new(target).is_absolute() {
            self.check_trash(Path::new(target))?;
            return open_root();
        }
        let data_home = dir.base();
        self.check_trash(&data_home.join(target))?;
        let real = sys::canonicalize(data_home).map_err(|err| io_failure(data_home, err))?;
        let named = sys::lookup_path(&real).ok().flatten();
        let opened = sys::fstat(base).map_err(|err| io_failure(data_home, err))?;
        if named.is_none_or(|named| named.ident != opened.ident) {
            return Err(io_failure(
                data_home,
                io::Error::other("it was replaced while the home trash's link was followed"),
            ));
        }
        self.check_trash(&real.join(target))?;
        let fd = base
            .try_clone_to_owned()
            .map_err(|err| io_failure(data_home, err))?;
        Ok((fd, real))
    }

    /// The directory reached, `fd` at `path`: checked as the trash
    /// directory itself is (opened for reading and inspected, then `files`
    /// and `info` in it), and as the home trash's own directory.
    fn finish(
        &self,
        dir: &TrashDir,
        base: BorrowedFd<'_>,
        fd: BorrowedFd<'_>,
        path: PathBuf,
    ) -> Result<Followed, FollowError> {
        self.check_trash(&path)?;
        if let Some(excluded) = self.deferred.take() {
            return Err(FollowError::Excluded {
                excluded: Box::new(excluded),
                reached: Some(path),
            });
        }
        // From here on the directory is known: whatever fails, its place is
        // kept, so that what it holds is still recognised as trashed.
        let refused = |at: &Path, kind| FollowError::Refused {
            error: OpenError {
                path: at.to_path_buf(),
                kind,
            },
            reached: path.clone(),
        };
        let fd = sys::reopen_dir(fd).map_err(|err| refused(&path, OpenErrorKind::Io(err)))?;
        let meta = sys::fstat(&fd).map_err(|err| refused(&path, OpenErrorKind::Io(err)))?;
        if let Some(failure) = checked(&meta, Check::Owned, dir.uid) {
            return Err(refused(&self.link, OpenErrorKind::Unsafe(failure)));
        }
        if fault::hits(Stage::MoveHomeTarget) {
            swap_link(base, OsStr::new(HOME_NAME));
            let mut moved = path.clone().into_os_string();
            moved.push(".moved");
            let _ = std::fs::rename(&path, moved);
        }
        Ok(Followed {
            dir: dir.clone(),
            fd,
            path,
        })
    }
}

/// The names a link's target has to be followed through: `..` kept, `.`
/// and the root left out (an absolute target starts at the root).
fn steps(target: &OsStr) -> VecDeque<OsString> {
    Path::new(target)
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(name) => Some(name.to_os_string()),
            std::path::Component::ParentDir => Some("..".into()),
            _ => None,
        })
        .collect()
}

/// What tests have "another process" do: point the link `name` in `dir`
/// at the [`fault::swap_to`] directory. Nothing outside tests.
fn swap_link(dir: BorrowedFd<'_>, name: &OsStr) {
    if let Some(to) = fault::swapped_target() {
        let _ = rustix::fs::unlinkat(dir, name, rustix::fs::AtFlags::empty());
        let _ = rustix::fs::symlinkat(&to, dir, name);
    }
}

/// Opens (and when `create`, creates) the trash directory `dir`. Also
/// returns the first error syncing a directory a new one was made in: the
/// directory exists, but may not survive a crash.
pub(crate) fn open(
    context: &Context,
    dir: &TrashDir,
    create: bool,
) -> Result<(Trash, Option<io::Error>), OpenError> {
    open_with(context, dir, create, HomeRoot::Look)
}

/// [`open`], reaching the home trash's own directory as `home` says.
pub(crate) fn open_with(
    context: &Context,
    dir: &TrashDir,
    create: bool,
    home: HomeRoot,
) -> Result<(Trash, Option<io::Error>), OpenError> {
    let mode = if create { Mode::Create } else { Mode::Open };
    match walk_with(context, dir, mode, home)? {
        Walked::Opened(trash, unsynced) => Ok((trash, unsynced)),
        Walked::WouldCreate(path) => unreachable!("only a probe stops at {}", path.display()),
    }
}

/// Walks to the trash directory `dir` and its `files` and `info`, component
/// by component with every check, doing about missing ones what `mode`
/// says, and reaching the home trash's own directory as `home` says.
/// Opening, creating and probing are the one walk, so that a probe answers
/// by the same checks a trashing meets.
pub(crate) fn walk_with(
    context: &Context,
    dir: &TrashDir,
    mode: Mode,
    home: HomeRoot,
) -> Result<Walked, OpenError> {
    let mut unsynced = None;
    let io_error = |path: &Path, err: io::Error| OpenError {
        path: path.to_path_buf(),
        kind: if err.kind() == io::ErrorKind::NotFound {
            OpenErrorKind::Absent
        } else {
            OpenErrorKind::Io(err)
        },
    };
    let (fd, followed) = match home {
        // Only for the home trash it was found for: another is looked at
        // as given.
        HomeRoot::Followed(found) if found.dir == *dir => (found.fd, Some(found.path)),
        HomeRoot::Broken { dir: of, error, .. } if of == *dir => return Err(error),
        home => {
            let look = matches!(home, HomeRoot::Look);
            match reach(context, dir, mode, look, &mut unsynced)? {
                Ok(reached) => reached,
                Err(missing) => return Ok(Walked::WouldCreate(missing)),
            }
        }
    };
    let path = dir.path();
    let id = sys::fstat(&fd).map_err(|err| io_error(&path, err))?.ident;
    let mut inner = Vec::new();
    for name in ["files", "info"] {
        let inner_path = path.join(name);
        let Some(opened) = step(
            fd.as_fd(),
            OsStr::new(name),
            &inner_path,
            Check::Owned,
            mode,
            dir.uid,
            &mut unsynced,
        )?
        else {
            return Ok(Walked::WouldCreate(inner_path));
        };
        // A probe of a trash that exists: trashing writes into both.
        if mode == Mode::Probe {
            sys::may_create_in(opened.as_fd()).map_err(|err| io_error(&inner_path, err))?;
        }
        inner.push(opened);
    }
    let (Some(info), Some(files)) = (inner.pop(), inner.pop()) else {
        unreachable!("both were opened");
    };
    let absolute_paths = (dir.kind != TrashKind::Home).then_some(context.policy.absolute_paths);
    Ok(Walked::Opened(
        Trash {
            dir: dir.clone(),
            root: fd,
            files,
            info,
            id,
            followed,
            uid: dir.uid,
            absolute_paths,
            fallbacks: context.policy.fallbacks,
            limits: context.limits.clone(),
        },
        unsynced,
    ))
}

/// Opens the base directory (creating or probing a missing
/// `$XDG_DATA_HOME` as `mode` says) and goes down to the trash directory:
/// following the home trash's link when `look` and the policy allow it in
/// [`Mode::Open`] (looked at before `$XDG_DATA_HOME` is opened for
/// reading, which following does not need), else component by component. The directory, and where
/// a link led; or (probing) the first directory that would have to be
/// made.
fn reach(
    context: &Context,
    dir: &TrashDir,
    mode: Mode,
    look: bool,
    unsynced: &mut Option<io::Error>,
) -> Result<Result<(OwnedFd, Option<PathBuf>), PathBuf>, OpenError> {
    let base = dir.base();
    let io_error = |path: &Path, err: io::Error| OpenError {
        path: path.to_path_buf(),
        kind: if err.kind() == io::ErrorKind::NotFound {
            OpenErrorKind::Absent
        } else {
            OpenErrorKind::Io(err)
        },
    };
    if look && mode == Mode::Open && dir.kind == TrashKind::Home && context.policy.follow_home_link
    {
        match look_home(dir, None) {
            None => {}
            Some(Ok(found)) => return Ok(Ok((found.fd, Some(found.path)))),
            Some(Err(FollowError::Open(err) | FollowError::Refused { error: err, .. })) => {
                return Err(err);
            }
            Some(Err(FollowError::Excluded { .. })) => unreachable!("no exclusions were given"),
        }
    }
    let base_fd = match sys::open_dir(base) {
        Err(err)
            if err.kind() == io::ErrorKind::NotFound
                && mode == Mode::Probe
                && dir.kind == TrashKind::Home =>
        {
            return creatable(base).map(Err);
        }
        Err(err)
            if err.kind() == io::ErrorKind::NotFound
                && mode == Mode::Create
                && dir.kind == TrashKind::Home =>
        {
            match sys::create_dir_all(base) {
                Ok(xdg_kit_basedir::Durability::Synced) => {}
                Ok(xdg_kit_basedir::Durability::NotSynced(err)) => {
                    *unsynced = unsynced.take().or(Some(err));
                }
                Err(err) => return Err(io_error(base, err)),
            }
            sys::open_dir(base).map_err(|err| io_error(base, err))?
        }
        other => other.map_err(|err| io_error(base, err))?,
    };
    Ok(walk_components(context, dir, mode, base_fd, unsynced)?.map(|fd| (fd, None)))
}

/// The walk from the open base directory down to the trash directory, a
/// step and its checks per component, no link followed: the directory, or
/// (probing) the first that would have to be made.
fn walk_components(
    context: &Context,
    dir: &TrashDir,
    mode: Mode,
    base_fd: OwnedFd,
    unsynced: &mut Option<io::Error>,
) -> Result<Result<OwnedFd, PathBuf>, OpenError> {
    let base = dir.base();
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
            _ => (Check::Owned, true),
        };
        let Some(next) = step(
            fd.as_fd(),
            component,
            &path,
            check,
            mode.when(may_create),
            dir.uid,
            unsynced,
        )?
        else {
            return Ok(Err(path));
        };
        fd = next;
    }
    Ok(Ok(fd))
}

impl Mode {
    /// The mode for a directory that may be created only when `may`: one
    /// that may not is only opened.
    fn when(self, may: bool) -> Self {
        if may { self } else { Mode::Open }
    }
}

/// Opens `name` in `parent` as a directory meeting `check`. When it is
/// missing, [`Mode::Create`] creates it (mode 0700) first, and
/// [`Mode::Probe`] returns `None` if `parent` would let this process create
/// it.
fn step(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    path: &Path,
    check: Check,
    mode: Mode,
    uid: u32,
    unsynced: &mut Option<io::Error>,
) -> Result<Option<OwnedFd>, OpenError> {
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
        Ok(None) if mode == Mode::Probe => {
            return match sys::may_create_in(parent) {
                Ok(()) => Ok(None),
                Err(err) => Err(fail(OpenErrorKind::Io(err))),
            };
        }
        Ok(None) if mode == Mode::Create => match sys::mkdir_at(parent, name, 0o700) {
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
    match checked(&meta, check, uid) {
        Some(failure) => Err(fail(OpenErrorKind::Unsafe(failure))),
        None => Ok(Some(fd)),
    }
}

/// The check an open directory fails, if any.
fn checked(meta: &Meta, check: Check, uid: u32) -> Option<CheckFailure> {
    match check {
        Check::Sticky { unchecked } => {
            (!unchecked && meta.mode & 0o1000 == 0).then_some(CheckFailure::NotSticky(meta.mode))
        }
        Check::Owned if meta.uid != uid => Some(CheckFailure::Owner {
            expected: uid,
            found: meta.uid,
        }),
        Check::Owned => (meta.mode & 0o022 != 0).then_some(CheckFailure::Writable(meta.mode)),
    }
}

/// The first of `path` and its ancestors that is missing below the nearest
/// one that exists, if that one would let this process create in it: what
/// creating `path` would start with. Nothing is made.
fn creatable(path: &Path) -> Result<PathBuf, OpenError> {
    let fail = |path: &Path, kind| OpenError {
        path: path.to_path_buf(),
        kind,
    };
    // Rebuilt from its components: a trailing `/` or `.` would make the
    // look at the final name follow a link, and `ancestors` would then never
    // name the link itself.
    let path: PathBuf = path.components().collect();
    let path = path.as_path();
    let mut first_missing = None;
    for at in path.ancestors() {
        // Looked at without following its final component: a dangling
        // symbolic link is a name that exists, not a missing directory, and
        // creating through it fails.
        let meta = match sys::lookup_path_nofollow(at) {
            Ok(None) => {
                first_missing = Some(at);
                continue;
            }
            Ok(Some(meta)) if meta.kind == FileKind::Symlink => match sys::lookup_path(at) {
                Ok(Some(target)) => target,
                Ok(None) => {
                    return Err(fail(
                        at,
                        OpenErrorKind::Io(io::Error::new(
                            io::ErrorKind::NotADirectory,
                            "a dangling symbolic link",
                        )),
                    ));
                }
                Err(err) => return Err(fail(at, OpenErrorKind::Io(err))),
            },
            Ok(Some(meta)) => meta,
            Err(err) => return Err(fail(at, OpenErrorKind::Io(err))),
        };
        if meta.kind != FileKind::Directory {
            return Err(fail(
                at,
                OpenErrorKind::Unsafe(CheckFailure::NotDirectory(meta.kind)),
            ));
        }
        let Some(first_missing) = first_missing else {
            // Nothing is missing after all: it appeared meanwhile.
            return Ok(path.to_path_buf());
        };
        return sys::may_create_at(at)
            .map(|()| first_missing.to_path_buf())
            .map_err(|err| fail(first_missing, OpenErrorKind::Io(err)));
    }
    Err(fail(path, OpenErrorKind::Absent))
}
