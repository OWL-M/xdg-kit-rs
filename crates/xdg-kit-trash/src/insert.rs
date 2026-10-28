//! Trashing: choosing a trash directory, reserving a name with the trash
//! info file, and moving the payload in without replacing anything.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use xdg_kit_basedir::Durability;

use crate::context::Context;
use crate::dir::{self, OpenError, Trash, TrashDir, TrashKind};
use crate::fault::{self, Stage};
use crate::info;
use crate::list::info_name;
use crate::sys::{self, FileKind, Ident, Place, RenameError, Sameness};
use crate::time::DeletionDate;

/// A completed trashing: the payload is in `files`, its trash info file in
/// `info`.
#[derive(Debug)]
#[must_use = "NotSynced durability means the trashing may not survive a crash"]
#[non_exhaustive]
pub struct Trashed {
    /// The trash directory it went to.
    pub trash: TrashDir,
    /// Its storage name there.
    pub name: OsString,
    /// The original location recorded: the canonical path of the parent
    /// directory, with the entry's own name.
    pub original: PathBuf,
    /// Whether the trashing is known to survive a crash: the trash info
    /// file and `info` were synced before the move, then `files` and the
    /// source's directory after it ([`Durability::NotSynced`] when one of
    /// those, or the creation of a missing trash directory, failed to
    /// sync). The move happened either way: never redo it.
    pub durability: Durability,
    /// The trash directories tried before, and why each was not used.
    pub fallbacks: Vec<Attempt>,
    /// The file moved is not the one inspected before: another process
    /// replaced the source name in between. It was trashed under the
    /// source's name all the same, as a move by name does.
    pub replaced_meanwhile: bool,
}

/// A trash directory tried and not used. The payload was not moved there.
#[derive(Debug)]
#[non_exhaustive]
pub struct Attempt {
    /// The trash directory; `None` for per-volume trash directories whose
    /// location could not be determined ([`AttemptFailure::TopdirUnknown`]).
    pub trash: Option<TrashDir>,
    /// Why it was not used.
    pub failure: AttemptFailure,
    /// A trash info file reserved there and then left behind, because
    /// removing it failed: it names no payload.
    pub leftover: Option<Leftover>,
}

/// A trash info file that could not be removed after a failure.
#[derive(Debug)]
#[non_exhaustive]
pub struct Leftover {
    /// Its path.
    pub path: PathBuf,
    /// Why it stayed.
    pub error: io::Error,
}

/// Why a trash directory was not used.
#[derive(Debug)]
#[non_exhaustive]
pub enum AttemptFailure {
    /// It is on another mount than the file: a rename cannot reach it.
    OtherMount,
    /// The policy does not use per-volume trash directories for trashing.
    Disabled,
    /// It could not be opened or created, or failed a check.
    Open(OpenError),
    /// The file is not below its base directory (an explicit per-volume
    /// trash whose top directory does not hold the file).
    OutsideBase,
    /// No storage name was free within
    /// [`Limits::max_name_attempts`](crate::Limits::max_name_attempts), or
    /// names cannot be long enough to hold one.
    NoName,
    /// The name limit of `files` or `info` is not reported and
    /// [`Limits::name_max_fallback`](crate::Limits::name_max_fallback) is
    /// `None`.
    NameLimitUnknown,
    /// Creating, writing or syncing the trash info file failed.
    Metadata(io::Error),
    /// The rename said the trash is on another mount.
    CrossDevice,
    /// The rename failed otherwise.
    Move(io::Error),
    /// The top directory of the file's mount could not be determined: an
    /// ancestor could not be looked at. No per-volume trash was tried
    /// rather than one at a guessed place.
    TopdirUnknown {
        /// The ancestor.
        ancestor: PathBuf,
        /// Why it could not be looked at.
        error: io::Error,
    },
    /// The source disappeared before it could be moved.
    SourceGone,
    /// The file system refused a non-replacing rename
    /// ([`TrashError::Unsupported`] says more).
    Unsupported,
}

impl fmt::Display for AttemptFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OtherMount => f.write_str("it is on another mount"),
            Self::Disabled => f.write_str("per-volume trashing is disabled"),
            Self::Open(err) => err.fmt(f),
            Self::OutsideBase => f.write_str("the file is not below its top directory"),
            Self::NoName => f.write_str("no storage name is free"),
            Self::NameLimitUnknown => f.write_str("the file name limit is unknown"),
            Self::Metadata(err) => write!(f, "cannot write the trash info file: {err}"),
            Self::CrossDevice => f.write_str("the rename crosses mounts"),
            Self::Move(err) => write!(f, "cannot move the file: {err}"),
            Self::Unsupported => f.write_str("the file system cannot rename without replacing"),
            Self::TopdirUnknown { ancestor, error } => {
                write!(
                    f,
                    "cannot tell the mount of {}: {error}",
                    ancestor.display()
                )
            }
            Self::SourceGone => f.write_str("the file disappeared"),
        }
    }
}

/// Why a path cannot be trashed at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SourceProblem {
    /// It ends in no name: `/`, `.` or `..`.
    NoName,
    /// It is a trash directory, or holds one.
    HoldsTrash,
    /// It is inside a trash directory.
    InTrash,
}

impl fmt::Display for SourceProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoName => "it does not end in a name",
            Self::HoldsTrash => "it is or holds a trash directory",
            Self::InTrash => "it is inside a trash directory",
        })
    }
}

/// An operation this system or file system cannot do safely.
#[derive(Debug)]
#[non_exhaustive]
pub struct Unsupported {
    /// What was refused.
    pub operation: &'static str,
    /// The file system's refusal; `None` when the system has no
    /// non-replacing rename at all.
    pub cause: Option<io::Error>,
}

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.cause {
            None => write!(
                f,
                "{} needs a rename that never replaces its target, which this system lacks",
                self.operation
            ),
            Some(err) => write!(
                f,
                "{} needs a rename that never replaces its target, which the file system refused: {err}",
                self.operation
            ),
        }
    }
}

/// Why a file was not trashed. In every case it was not moved and is
/// where it was.
#[derive(Debug)]
#[non_exhaustive]
pub enum TrashError {
    /// No non-replacing rename here; see [`Unsupported`].
    Unsupported {
        /// What is missing.
        unsupported: Unsupported,
        /// The trash directories tried before.
        attempts: Vec<Attempt>,
    },
    /// The path cannot be trashed.
    InvalidSource {
        /// The path, as given.
        path: PathBuf,
        /// Why.
        problem: SourceProblem,
    },
    /// Nothing has that name (found before anything was tried, or gone
    /// while trashing).
    SourceMissing {
        /// The path.
        path: PathBuf,
        /// The trash directories tried before it was found gone (the last
        /// with any trash info file left behind).
        attempts: Vec<Attempt>,
    },
    /// The file or its directory could not be inspected.
    Source {
        /// The path.
        path: PathBuf,
        /// What failed.
        error: io::Error,
    },
    /// The clock is outside the years 0–9999, which `DeletionDate` holds.
    Clock,
    /// No trash directory could take it. Nothing was erased: the spec's
    /// "refuse to trash" — tell the user.
    NoTrash {
        /// The path.
        path: PathBuf,
        /// Every trash directory tried or passed over, and why.
        attempts: Vec<Attempt>,
    },
}

impl fmt::Display for TrashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported { unsupported, .. } => unsupported.fmt(f),
            Self::InvalidSource { path, problem } => {
                write!(f, "cannot trash {}: {problem}", path.display())
            }
            Self::SourceMissing { path, .. } => write!(f, "{} does not exist", path.display()),
            Self::Clock => f.write_str("the clock is outside the years 0-9999"),
            Self::Source { path, error } => write!(f, "cannot inspect {}: {error}", path.display()),
            Self::NoTrash { path, attempts } => {
                write!(f, "no trash directory takes {}", path.display())?;
                for attempt in attempts {
                    match &attempt.trash {
                        Some(trash) => {
                            write!(f, "; {}: {}", trash.path().display(), attempt.failure)?;
                        }
                        None => write!(f, "; per-volume trash: {}", attempt.failure)?,
                    }
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for TrashError {}

/// The trash directories a trashing tries, in order, and the ones it
/// passes over.
pub(crate) struct Selection {
    pub(crate) candidates: Vec<TrashDir>,
    pub(crate) skipped: Vec<Attempt>,
}

/// The file to trash, inspected.
pub(crate) struct Source {
    /// As given, for errors.
    given: PathBuf,
    /// Its directory, canonical, and open.
    parent: PathBuf,
    parent_fd: OwnedFd,
    /// Its own name, not resolved.
    name: OsString,
    /// Its identity when inspected.
    ident: Ident,
    /// Its kind when inspected (a symbolic link is not followed).
    kind: FileKind,
}

impl Source {
    pub(crate) fn path(&self) -> PathBuf {
        self.parent.join(&self.name)
    }
}

impl Context {
    /// Moves `path` into a trash directory: the home trash when the file is
    /// on its mount, otherwise `$topdir/.Trash/$uid` on the file's own
    /// mount, else `$topdir/.Trash-$uid` (as the policy allows). A trash
    /// directory or its `files` and `info` that are missing are created.
    ///
    /// `path` itself is trashed — a symbolic link as a link — after
    /// following symbolic links in its directory; a relative path is taken
    /// from the context's working directory, and trailing slashes are
    /// ignored. The file is never copied and never erased: if no trash
    /// directory on its mount works, it stays where it is.
    ///
    /// # Errors
    ///
    /// See [`TrashError`]: in every case the file was not moved.
    pub fn trash(&self, path: impl AsRef<Path>) -> Result<Trashed, TrashError> {
        let source = self.inspect(path.as_ref())?;
        let selection = self.select(&source)?;
        self.insert(&source, selection.candidates, selection.skipped)
    }

    /// The trash directories [`Context::trash`] tries for `source`, in
    /// order, and the ones it passes over, with why; refuses a source that
    /// is or holds a trash directory, or is inside one. Shared by
    /// [`Context::trash`] and [`Context::plan`], so that the query answers
    /// by the very selection trashing makes. Looks only: creates nothing.
    pub(crate) fn select(&self, source: &Source) -> Result<Selection, TrashError> {
        let home = self.home_trash();
        let parent_place =
            Place::of(source.parent_fd.as_fd()).map_err(|error| TrashError::Source {
                path: source.given.clone(),
                error,
            })?;
        let pretended = fault::pretended_volume();
        // Where the home trash's mount cannot be told, it is tried: the
        // rename decides.
        let sameness = match (&pretended, nearest_place(&home.path())) {
            (Some(_), _) => Sameness::Different,
            (None, Ok(home)) => parent_place.same(home),
            (None, Err(_)) => Sameness::SameDevice,
        };
        let topdir = match pretended {
            Some(topdir) => Ok(topdir),
            None => topdir(&source.parent, parent_place),
        };
        self.check_not_trash(source, &[], topdir.as_deref().ok())?;
        let mut candidates = Vec::new();
        let mut skipped = Vec::new();
        if sameness == Sameness::Different {
            skipped.push(self::skipped(
                Some(home.clone()),
                AttemptFailure::OtherMount,
            ));
        } else {
            candidates.push(home);
        }
        if sameness != Sameness::Same {
            match topdir {
                Err((ancestor, error)) => {
                    skipped.push(self::skipped(
                        None,
                        AttemptFailure::TopdirUnknown { ancestor, error },
                    ));
                }
                Ok(topdir) => {
                    let shared = self.shared_trash(&topdir);
                    let private = self.private_trash(&topdir);
                    if self.policy.volume_trash {
                        candidates.push(shared);
                        if self.policy.private_volume_trash {
                            candidates.push(private);
                        }
                    } else {
                        skipped.push(self::skipped(Some(shared), AttemptFailure::Disabled));
                        skipped.push(self::skipped(Some(private), AttemptFailure::Disabled));
                    }
                }
            }
        }
        Ok(Selection {
            candidates,
            skipped,
        })
    }

    /// Moves `path` into the trash directory `trash`, and only there,
    /// creating it (or its `files` and `info`) when missing: for callers
    /// that choose the trash directory themselves. A per-volume trash
    /// takes only files below its top directory.
    ///
    /// # Errors
    ///
    /// As [`Context::trash`]; [`TrashError::NoTrash`] holds the one
    /// attempt.
    pub fn trash_into(
        &self,
        trash: &TrashDir,
        path: impl AsRef<Path>,
    ) -> Result<Trashed, TrashError> {
        let source = self.inspect(path.as_ref())?;
        let candidates = vec![trash.clone()];
        let place = Place::of(source.parent_fd.as_fd()).map_err(|error| TrashError::Source {
            path: source.given.clone(),
            error,
        })?;
        let topdir = topdir(&source.parent, place).ok();
        self.check_not_trash(&source, &candidates, topdir.as_deref())?;
        self.insert(&source, candidates, Vec::new())
    }

    pub(crate) fn inspect(&self, given: &Path) -> Result<Source, TrashError> {
        let invalid = |problem| TrashError::InvalidSource {
            path: given.to_path_buf(),
            problem,
        };
        if !sys::NO_REPLACE_RENAME {
            return Err(TrashError::Unsupported {
                unsupported: Unsupported {
                    operation: "trashing",
                    cause: None,
                },
                attempts: Vec::new(),
            });
        }
        if given.as_os_str().is_empty() {
            return Err(invalid(SourceProblem::NoName));
        }
        let path = self.anchor(given);
        let mut bytes = path.as_os_str().as_bytes();
        while bytes.len() > 1 && bytes.ends_with(b"/") {
            bytes = &bytes[..bytes.len() - 1];
        }
        let split = bytes
            .iter()
            .rposition(|&byte| byte == b'/')
            .map_or(0, |at| at + 1);
        let (parent, name) = bytes.split_at(split);
        if matches!(name, b"" | b"." | b"..") {
            return Err(invalid(SourceProblem::NoName));
        }
        let source_error = |error: io::Error| {
            if error.kind() == io::ErrorKind::NotFound {
                TrashError::SourceMissing {
                    path: given.to_path_buf(),
                    attempts: Vec::new(),
                }
            } else {
                TrashError::Source {
                    path: given.to_path_buf(),
                    error,
                }
            }
        };
        let parent = std::fs::canonicalize(OsStr::from_bytes(parent)).map_err(source_error)?;
        let parent_fd = sys::open_dir(&parent).map_err(source_error)?;
        let name = OsStr::from_bytes(name).to_os_string();
        let meta = sys::stat_at(parent_fd.as_fd(), &name).map_err(source_error)?;
        Ok(Source {
            given: given.to_path_buf(),
            parent,
            parent_fd,
            name,
            ident: meta.ident,
            kind: meta.kind,
        })
    }

    /// Refuses a source that is or holds a trash directory, or is inside
    /// one — trashing the trash — whichever trash would take it: the home
    /// trash, both layouts of the source's own top directory (when known),
    /// the explicit `candidates`, and any per-volume trash root on the way
    /// (a directory `.Trash` or `.Trash-<digits>` right below a mount's top
    /// directory), another user's included. The names alone mean nothing:
    /// a file, a link, or a directory elsewhere with such a name is
    /// trashed like any.
    fn check_not_trash(
        &self,
        source: &Source,
        candidates: &[TrashDir],
        topdir: Option<&Path>,
    ) -> Result<(), TrashError> {
        let path = source.path();
        let refuse = |problem| {
            Err(TrashError::InvalidSource {
                path: source.given.clone(),
                problem,
            })
        };
        let mut trashes: Vec<TrashDir> = candidates.to_vec();
        trashes.push(self.home_trash());
        if let Some(topdir) = topdir {
            trashes.push(self.shared_trash(topdir));
            trashes.push(self.private_trash(topdir));
        }
        for trash in trashes {
            let mut trash_path = trash.path();
            if let Ok(canonical) = std::fs::canonicalize(trash.base()) {
                trash_path =
                    canonical.join(trash_path.strip_prefix(trash.base()).unwrap_or(&trash_path));
            }
            if path.starts_with(&trash_path) && path != trash_path {
                return refuse(SourceProblem::InTrash);
            }
            // Only a directory can hold one: a file or a link with a trash
            // directory's name holds nothing (and a link is trashed itself,
            // not its target).
            if trash_path.starts_with(&path) && source.kind == FileKind::Directory {
                return refuse(SourceProblem::HoldsTrash);
            }
        }
        // Other trash roots on the way: a directory (not a link) named
        // `.Trash` or `.Trash-<digits>` right below a mount's top
        // directory, another user's included.
        let mut prefix = PathBuf::new();
        let last = path.components().count();
        for (index, component) in path.components().enumerate() {
            prefix.push(component);
            let std::path::Component::Normal(name) = component else {
                continue;
            };
            let name = name.as_bytes();
            let trash_like = name == b".Trash"
                || name
                    .strip_prefix(b".Trash-")
                    .is_some_and(|uid| !uid.is_empty() && uid.iter().all(u8::is_ascii_digit));
            let is_dir = || std::fs::symlink_metadata(&prefix).is_ok_and(|meta| meta.is_dir());
            let at_top = || {
                prefix
                    .parent()
                    .is_some_and(|parent| is_mount_top(parent, topdir))
            };
            if trash_like && is_dir() && at_top() {
                return refuse(if index + 1 == last {
                    SourceProblem::HoldsTrash
                } else {
                    SourceProblem::InTrash
                });
            }
        }
        Ok(())
    }

    fn insert(
        &self,
        source: &Source,
        candidates: Vec<TrashDir>,
        mut attempts: Vec<Attempt>,
    ) -> Result<Trashed, TrashError> {
        let original = source.path();
        let date = DeletionDate::at(self.clock.now(), &self.zone).ok_or(TrashError::Clock)?;
        for candidate in candidates {
            let (trash, unsynced) = match dir::open(self, &candidate, true) {
                Ok(opened) => opened,
                Err(err) => {
                    attempts.push(skipped(Some(candidate), AttemptFailure::Open(err)));
                    continue;
                }
            };
            let Some(recorded) = recorded_path(&trash, &original) else {
                attempts.push(skipped(Some(candidate), AttemptFailure::OutsideBase));
                continue;
            };
            let bytes = info::format(recorded.as_os_str().as_bytes(), date);
            match self.move_in(&trash, source, &bytes) {
                Ok((name, durability)) => {
                    let durability = match (durability, unsynced) {
                        (Durability::Synced, Some(err)) => Durability::NotSynced(err),
                        (durability, _) => durability,
                    };
                    let replaced_meanwhile = sys::stat_at(trash.files(), &name)
                        .map_or(true, |meta| meta.ident != source.ident);
                    return Ok(Trashed {
                        trash: candidate,
                        name,
                        original,
                        durability,
                        fallbacks: attempts,
                        replaced_meanwhile,
                    });
                }
                Err(MoveFailure::Attempt(failure, leftover)) => attempts.push(Attempt {
                    trash: Some(candidate),
                    failure,
                    leftover,
                }),
                Err(MoveFailure::Unsupported(cause, leftover)) => {
                    attempts.push(Attempt {
                        trash: Some(candidate),
                        failure: AttemptFailure::Unsupported,
                        leftover,
                    });
                    return Err(TrashError::Unsupported {
                        unsupported: Unsupported {
                            operation: "trashing",
                            cause,
                        },
                        attempts,
                    });
                }
                Err(MoveFailure::SourceMissing(leftover)) => {
                    attempts.push(Attempt {
                        trash: Some(candidate),
                        failure: AttemptFailure::SourceGone,
                        leftover,
                    });
                    return Err(TrashError::SourceMissing {
                        path: source.given.clone(),
                        attempts,
                    });
                }
            }
        }
        Err(TrashError::NoTrash {
            path: source.given.clone(),
            attempts,
        })
    }

    /// Reserves a name in `trash` with the trash info file `bytes` and
    /// moves the source in under it.
    fn move_in(
        &self,
        trash: &Trash,
        source: &Source,
        bytes: &[u8],
    ) -> Result<(OsString, Durability), MoveFailure> {
        let mut names = Names::new(trash, &source.name, &self.limits)?;
        loop {
            let reservation = names.reserve(trash, bytes)?;
            if fault::hits(Stage::SourceVanishes) {
                let _ = rustix::fs::unlinkat(
                    source.parent_fd.as_fd(),
                    &source.name,
                    rustix::fs::AtFlags::empty(),
                );
            }
            let moved = if fault::hits(Stage::Move) {
                Err(RenameError::Other(io::Error::other(
                    "injected failure at Move",
                )))
            } else if fault::hits(Stage::MoveCrossDevice) {
                Err(RenameError::CrossDevice)
            } else {
                sys::rename_no_replace(
                    source.parent_fd.as_fd(),
                    &source.name,
                    trash.files(),
                    &reservation.name,
                )
            };
            let failure = match moved {
                Ok(()) => {
                    let mut durability = Durability::Synced;
                    for dir in [trash.files(), source.parent_fd.as_fd()] {
                        if let Err(err) =
                            fault::check(Stage::SyncAfterMove).and_then(|()| sys::sync(dir))
                            && matches!(durability, Durability::Synced)
                        {
                            durability = Durability::NotSynced(err);
                        }
                    }
                    return Ok((reservation.name, durability));
                }
                // Taken in `files` (an orphaned payload, or another
                // trashing): release the name, take the next.
                Err(RenameError::Exists) => {
                    if let Some(leftover) = reservation.release(trash) {
                        return Err(MoveFailure::Attempt(AttemptFailure::NoName, Some(leftover)));
                    }
                    continue;
                }
                Err(RenameError::CrossDevice) => AttemptFailure::CrossDevice,
                Err(RenameError::Unsupported(cause)) => {
                    let leftover = reservation.release(trash);
                    return Err(MoveFailure::Unsupported(cause, leftover));
                }
                Err(RenameError::Other(err)) => {
                    if matches!(
                        sys::lookup_at(source.parent_fd.as_fd(), &source.name),
                        Ok(None)
                    ) {
                        return Err(MoveFailure::SourceMissing(reservation.release(trash)));
                    }
                    AttemptFailure::Move(err)
                }
            };
            let leftover = reservation.release(trash);
            return Err(MoveFailure::Attempt(failure, leftover));
        }
    }
}

enum MoveFailure {
    Attempt(AttemptFailure, Option<Leftover>),
    Unsupported(Option<io::Error>, Option<Leftover>),
    SourceMissing(Option<Leftover>),
}

impl From<AttemptFailure> for MoveFailure {
    fn from(failure: AttemptFailure) -> Self {
        Self::Attempt(failure, None)
    }
}

fn skipped(trash: Option<TrashDir>, failure: AttemptFailure) -> Attempt {
    Attempt {
        trash,
        failure,
        leftover: None,
    }
}

/// Whether `dir` (canonical) is the top directory of a mount: the source's
/// own top directory, or on another place than its parent. When that
/// cannot be told it counts as one, so that a trash is never trashed for
/// want of a look.
fn is_mount_top(dir: &Path, topdir: Option<&Path>) -> bool {
    if topdir == Some(dir) {
        return true;
    }
    let Some(up) = dir.parent() else {
        return true;
    };
    match (Place::of_path(dir), Place::of_path(up)) {
        (Ok(here), Ok(above)) => here.same(above) == Sameness::Different,
        _ => true,
    }
}

/// The place of the nearest existing directory at or above `path`.
fn nearest_place(path: &Path) -> io::Result<Place> {
    for dir in path.ancestors() {
        match Place::of_path(dir) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            found => return found,
        }
    }
    Err(io::ErrorKind::NotFound.into())
}

/// The top directory of the mount `dir` (canonical) is on: its highest
/// ancestor in the same place, looked at with `stat` (search permission
/// suffices). Without mount IDs that is the highest on the same device,
/// which a bind mount can make too high; the rename then fails and says
/// so. An ancestor that cannot be looked at makes it unknown: that
/// ancestor, and why.
pub(crate) fn topdir(dir: &Path, place: Place) -> Result<PathBuf, (PathBuf, io::Error)> {
    let mut top = dir;
    for ancestor in dir.ancestors().skip(1) {
        match Place::of_path(ancestor) {
            Ok(other) if other.same(place) != Sameness::Different => top = ancestor,
            Ok(_) => break,
            Err(error) => return Err((ancestor.to_path_buf(), error)),
        }
    }
    Ok(top.to_path_buf())
}

/// The `Path` to record for `original` in `trash`: relative to the base
/// when below it, else absolute (the home trash only).
pub(crate) fn recorded_path(trash: &Trash, original: &Path) -> Option<PathBuf> {
    let base = std::fs::canonicalize(trash.dir().base()).ok();
    let relative = base
        .as_deref()
        .and_then(|base| original.strip_prefix(base).ok())
        .filter(|relative| !relative.as_os_str().is_empty());
    match (relative, trash.dir().kind()) {
        (Some(relative), _) => Some(relative.to_path_buf()),
        (None, TrashKind::Home) => Some(original.to_path_buf()),
        (None, TrashKind::Shared | TrashKind::Private) => None,
    }
}

/// A trash info file created for a name, before the payload moves in.
struct Reservation {
    name: OsString,
    ident: Ident,
}

impl Reservation {
    /// Removes the trash info file, if it is still the one created. Returns
    /// what is left behind when that fails.
    fn release(self, trash: &Trash) -> Option<Leftover> {
        let info = info_name(&self.name);
        let removed = fault::check(Stage::RemoveReservation)
            .and_then(|()| sys::remove_if_same(trash.info(), &info, self.ident, false));
        removed.err().map(|error| Leftover {
            path: trash.dir().path().join("info").join(&info),
            error,
        })
    }
}

/// Storage names to try: the source's name cut to fit, then with `.2`,
/// `.3`… appended.
struct Names {
    base: Vec<u8>,
    max_len: usize,
    next: u32,
    left: u32,
}

/// `.trashinfo`'s length.
const SUFFIX_LEN: usize = info::SUFFIX.len();

impl Names {
    fn new(trash: &Trash, name: &OsStr, limits: &crate::Limits) -> Result<Self, AttemptFailure> {
        let limit = |fd: BorrowedFd<'_>| sys::name_max(fd).or(limits.name_max_fallback);
        let (Some(files), Some(info)) = (limit(trash.files()), limit(trash.info())) else {
            return Err(AttemptFailure::NameLimitUnknown);
        };
        let max_len = info
            .checked_sub(SUFFIX_LEN)
            .ok_or(AttemptFailure::NoName)?
            .min(files);
        Ok(Self {
            base: name.as_bytes().to_vec(),
            max_len,
            next: 1,
            left: limits.max_name_attempts,
        })
    }

    /// The next name to try, cut to `max_len`.
    fn candidate(&self) -> Option<OsString> {
        let suffix = if self.next == 1 {
            String::new()
        } else {
            format!(".{}", self.next)
        };
        let room = self.max_len.checked_sub(suffix.len())?;
        let mut cut = self.base.len().min(room);
        // Not inside a UTF-8 character, when the name is UTF-8.
        if let Ok(text) = std::str::from_utf8(&self.base) {
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
        }
        let mut name = self.base[..cut].to_vec();
        name.extend_from_slice(suffix.as_bytes());
        Some(OsStr::from_bytes(&name).to_os_string())
    }

    /// Creates the trash info file for the next free name, holding
    /// `bytes`, synced with `info`.
    fn reserve(&mut self, trash: &Trash, bytes: &[u8]) -> Result<Reservation, MoveFailure> {
        loop {
            if self.left == 0 {
                return Err(AttemptFailure::NoName.into());
            }
            self.left -= 1;
            let Some(name) = self.candidate() else {
                return Err(AttemptFailure::NoName.into());
            };
            self.next += 1;
            if name.is_empty() || name == "." || name == ".." {
                continue;
            }
            // Taken in `files`: no need to reserve it (the move would fail).
            if matches!(sys::lookup_at(trash.files(), &name), Ok(Some(_))) {
                continue;
            }
            let info = info_name(&name);
            let created = fault::check(Stage::CreateInfo)
                .and_then(|()| sys::create_exclusive(trash.info(), &info, 0o600));
            let fd = match created {
                Ok(fd) => fd,
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                // Shorter than reported: shrink and try this number again.
                Err(err)
                    if err.raw_os_error()
                        == Some(rustix::io::Errno::NAMETOOLONG.raw_os_error()) =>
                {
                    self.next -= 1;
                    self.max_len = self.max_len.saturating_sub(self.max_len.div_ceil(4).max(1));
                    continue;
                }
                Err(err) => return Err(AttemptFailure::Metadata(err).into()),
            };
            let reservation = match sys::fstat(&fd) {
                Ok(meta) => Reservation {
                    name,
                    ident: meta.ident,
                },
                Err(err) => {
                    // Unidentified: removed by name, the only way left.
                    let _ = rustix::fs::unlinkat(trash.info(), &info, rustix::fs::AtFlags::empty());
                    return Err(AttemptFailure::Metadata(err).into());
                }
            };
            let written = fault::check(Stage::WriteInfo)
                .and_then(|()| sys::write_all(&fd, bytes))
                .and_then(|()| fault::check(Stage::SyncInfo))
                .and_then(|()| sys::sync(&fd))
                .and_then(|()| fault::check(Stage::SyncInfoDir))
                .and_then(|()| sys::sync(trash.info()));
            drop(fd);
            return match written {
                Ok(()) => Ok(reservation),
                Err(err) => {
                    let leftover = reservation.release(trash);
                    Err(MoveFailure::Attempt(
                        AttemptFailure::Metadata(err),
                        leftover,
                    ))
                }
            };
        }
    }
}
