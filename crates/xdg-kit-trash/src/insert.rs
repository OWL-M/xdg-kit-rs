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
use crate::dir::{self, HomeRoot, OpenError, Trash, TrashDir, TrashKind};
use crate::exclude::Excluded;
use crate::fault::{self, Stage};
use crate::info;
use crate::list::info_name;
use crate::sys::{self, FileKind, Ident, Place, Sameness};
use crate::time::DeletionDate;
use crate::transfer::{MoveMethod, PartialState, TransferError, TransferErrorKind, transfer};

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
    /// How the payload was moved: atomically by the native rename, or by a
    /// fallback the policy allows.
    pub method: MoveMethod,
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
    /// What this attempt made there and could not remove again: a trash
    /// info file reserved (it names no payload), or a placeholder
    /// directory (empty, unless someone filled it meanwhile).
    pub leftovers: Vec<Leftover>,
}

/// Something an attempt made and could not remove after a failure.
#[derive(Debug)]
#[non_exhaustive]
pub struct Leftover {
    /// What it is.
    pub kind: LeftoverKind,
    /// Its path.
    pub path: PathBuf,
    /// Why it stayed.
    pub error: io::Error,
}

/// What a [`Leftover`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LeftoverKind {
    /// A trash info file in `info`, without a payload.
    Metadata,
    /// A placeholder directory in `files`, without a trash info file.
    Placeholder,
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
    /// The trash directory is in an excluded place
    /// ([`Policy::exclusions`](crate::Policy::exclusions)): not probed,
    /// not created.
    Excluded(Excluded),
    /// No way to move it without replacing: the native rename was refused
    /// and the policy's fallbacks do not apply or were refused too
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
            Self::Unsupported => f.write_str("the file cannot be moved here without replacing"),
            Self::TopdirUnknown { ancestor, error } => {
                write!(
                    f,
                    "cannot tell the mount of {}: {error}",
                    ancestor.display()
                )
            }
            Self::SourceGone => f.write_str("the file disappeared"),
            Self::Excluded(excluded) => excluded.fmt(f),
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
    /// It is the symbolic link at `$XDG_DATA_HOME/Trash` the home trash is
    /// reached through ([`Policy::follow_home_link`](crate::Policy::follow_home_link)):
    /// moving it would cut the way to what the home trash holds.
    HomeTrashLink,
}

impl fmt::Display for SourceProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoName => "it does not end in a name",
            Self::HoldsTrash => "it is or holds a trash directory",
            Self::InTrash => "it is inside a trash directory",
            Self::HomeTrashLink => "it is the symbolic link the home trash is reached through",
        })
    }
}

/// An operation this system or file system cannot do safely: the native
/// non-replacing rename was refused, and the policy's fallbacks
/// ([`Fallbacks`](crate::Fallbacks)) do not apply to the file or were refused too.
#[derive(Debug)]
#[non_exhaustive]
pub struct Unsupported {
    /// What was refused.
    pub operation: &'static str,
    /// The last refusal: the fallback's, when one was tried, else the
    /// native rename's; `None` when the system has no non-replacing rename
    /// at all and no fallback applied.
    pub cause: Option<io::Error>,
}

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.cause {
            None => write!(
                f,
                "{} needs a move that never replaces its target, which is not available here",
                self.operation
            ),
            Some(err) => write!(
                f,
                "{} needs a move that never replaces its target, which was refused: {err}",
                self.operation
            ),
        }
    }
}

/// A trashing that went part of the way ([`TrashError::Partial`]).
#[derive(Debug)]
#[non_exhaustive]
pub struct PartialTrash {
    /// The path, as given.
    pub path: PathBuf,
    /// The trash directory linked into.
    pub trash: TrashDir,
    /// The storage name linked to there.
    pub name: OsString,
    /// What the two names hold: [`PartialState::Duplicated`] (verified:
    /// one file under both names, the trash entry complete; removing either
    /// name loses nothing) or [`PartialState::Unverified`] (a name holds a
    /// file not known to be the payload, or could not be inspected; nothing
    /// was removed; inspect both names before removing anything).
    pub state: PartialState,
    /// Whether the trash info file reserved for `name` is still there. It
    /// is removed only when `name` is confirmed not to hold the payload;
    /// a name that could not be inspected keeps it, as it may hold it.
    pub metadata_kept: bool,
    /// Why the move did not complete.
    pub error: io::Error,
    /// Why it was not undone.
    pub rollback: io::Error,
    /// What could not be removed: the trash info file reserved, when it was
    /// to go and removing it failed.
    pub leftovers: Vec<Leftover>,
    /// The trash directories tried before.
    pub attempts: Vec<Attempt>,
}

/// Why a file was not trashed. In every case but [`TrashError::Partial`]
/// it was not moved and is where it was.
#[derive(Debug)]
#[non_exhaustive]
pub enum TrashError {
    /// The path is in an excluded place
    /// ([`Policy::exclusions`](crate::Policy::exclusions)), found from the
    /// path alone: nothing was touched.
    Excluded(Excluded),
    /// The link fallback made a link in the trash, then could neither
    /// complete the move nor undo it; no other trash was tried. See
    /// [`PartialTrash`], and its [`PartialTrash::state`] before removing
    /// anything.
    Partial(Box<PartialTrash>),
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
            Self::Excluded(excluded) => excluded.fmt(f),
            Self::Partial(partial) => {
                let (path, trash, name) = (
                    partial.path.display(),
                    partial.trash.path(),
                    partial.name.display(),
                );
                let (error, rollback) = (&partial.error, &partial.rollback);
                match partial.state {
                    PartialState::Duplicated => write!(
                        f,
                        "{path} is both in {} as {name} and still where it was (verified): the \
                         move could not complete ({error}) nor be undone ({rollback})",
                        trash.display()
                    ),
                    _ => write!(
                        f,
                        "trashing {path} into {} as {name} could not complete ({error}) nor be \
                         undone ({rollback}), and what the two names hold is not verified: \
                         nothing was removed",
                        trash.display()
                    ),
                }
            }
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
    /// How the home trash, if a candidate, is reached: its link followed
    /// once for the whole trashing.
    pub(crate) home: HomeRoot,
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
        self.insert(
            &source,
            selection.candidates,
            selection.skipped,
            selection.home,
        )
    }

    /// The trash directories [`Context::trash`] tries for `source`, in
    /// order, and the ones it passes over, with why; refuses a source that
    /// is or holds a trash directory, or is inside one. Shared by
    /// [`Context::trash`] and [`Context::plan`], so that the query answers
    /// by the very selection trashing makes. Looks only: creates nothing.
    pub(crate) fn select(&self, source: &Source) -> Result<Selection, TrashError> {
        let home = self.home_trash();
        // Excluded where the link leads, it is still known where that is,
        // for the guard below.
        let (home_root, home_excluded) = match self.home_root() {
            Ok(root) => {
                let excluded = root.excluded().cloned();
                (root, excluded)
            }
            Err(excluded) => (HomeRoot::NoLink, Some(excluded)),
        };
        let parent_place =
            Place::of(source.parent_fd.as_fd()).map_err(|error| TrashError::Source {
                path: source.given.clone(),
                error,
            })?;
        let pretended = fault::pretended_volume();
        // Where the home trash's mount cannot be told, it is tried: the
        // rename decides. An excluded home trash is not looked at. One
        // reached through a link is on its target's mount, as opened.
        let sameness = match (&pretended, &home_root) {
            (Some(_), _) => Sameness::Different,
            _ if home_excluded.is_some() => Sameness::SameDevice,
            (None, HomeRoot::Followed(followed)) => match Place::of(followed.fd.as_fd()) {
                Ok(home) => parent_place.same(home),
                Err(_) => Sameness::SameDevice,
            },
            // A link that leads nowhere is placed where the link is: files
            // there are refused with it, as with any home trash that fails,
            // rather than given a per-volume trash beside it.
            (None, HomeRoot::Broken { .. }) => match nearest_place(home.base()) {
                Ok(home) => parent_place.same(home),
                Err(_) => Sameness::SameDevice,
            },
            (None, _) => match nearest_place(&home.path()) {
                Ok(home) => parent_place.same(home),
                Err(_) => Sameness::SameDevice,
            },
        };
        let topdir = match pretended {
            Some(topdir) => Ok(topdir),
            None => topdir(&source.parent, parent_place),
        };
        let linked: Vec<&Path> = home_root.target().into_iter().collect();
        self.check_not_trash(source, &[], topdir.as_deref().ok(), &linked)?;
        let mut candidates = Vec::new();
        let mut skipped = Vec::new();
        if let Some(excluded) = home_excluded {
            skipped.push(self::skipped(
                Some(home),
                AttemptFailure::Excluded(excluded),
            ));
        } else if sameness == Sameness::Different {
            skipped.push(self::skipped(Some(home), AttemptFailure::OtherMount));
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
                        let mut volume = vec![shared];
                        if self.policy.private_volume_trash {
                            volume.push(private);
                        }
                        for trash in volume {
                            match self.excluded_trash(&trash) {
                                Ok(()) => candidates.push(trash),
                                Err(excluded) => skipped.push(self::skipped(
                                    Some(trash),
                                    AttemptFailure::Excluded(excluded),
                                )),
                            }
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
            home: home_root,
        })
    }

    /// The home trash's link followed, with the exclusions, unless the
    /// home trash is excluded itself (then nothing is looked at).
    fn home_root(&self) -> Result<HomeRoot, Excluded> {
        self.followed(&self.home_trash())
    }

    /// The home trash `trash`'s link followed, with the exclusions, unless
    /// the trash directory is excluded itself (then nothing is looked at).
    /// An exclusion met where the link leads is in the [`HomeRoot`]; so is
    /// one of the trash's own `files` or `info`, which following never
    /// enters: the trash is not used, but where its link leads still holds
    /// what it holds.
    fn followed(&self, trash: &TrashDir) -> Result<HomeRoot, Excluded> {
        let path = trash.path();
        self.policy.exclusions.check(&path)?;
        let child = ["files", "info"]
            .into_iter()
            .find_map(|child| self.policy.exclusions.check(&path.join(child)).err());
        let root = dir::follow_home(self, trash, &self.policy.exclusions);
        let Some(excluded) = child else {
            return Ok(root);
        };
        match root.target() {
            Some(reached) => Ok(HomeRoot::Excluded {
                excluded,
                reached: Some(reached.to_path_buf()),
            }),
            None => Err(excluded),
        }
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
        let excluded = |excluded| TrashError::NoTrash {
            path: source.given.clone(),
            attempts: vec![skipped(
                Some(trash.clone()),
                AttemptFailure::Excluded(excluded),
            )],
        };
        self.excluded_trash(trash).map_err(excluded)?;
        // This context's home trash is followed whatever the trash chosen,
        // for the guard below; an excluded one is not looked at, which
        // matters only when it is the one chosen. A home trash chosen that
        // is another's (another data home or user) is followed as its own.
        let own = trash == &self.home_trash();
        let home = match self.home_root() {
            Ok(home) => home,
            Err(err) if own => return Err(excluded(err)),
            Err(_) => HomeRoot::NoLink,
        };
        let chosen = if trash.kind() == TrashKind::Home && !own {
            self.followed(trash).map_err(excluded)?
        } else {
            HomeRoot::NoLink
        };
        let target_excluded = if own {
            home.excluded()
        } else {
            chosen.excluded()
        };
        if let Some(err) = target_excluded {
            return Err(excluded(err.clone()));
        }
        let candidates = vec![trash.clone()];
        let place = Place::of(source.parent_fd.as_fd()).map_err(|error| TrashError::Source {
            path: source.given.clone(),
            error,
        })?;
        let topdir = topdir(&source.parent, place).ok();
        let linked: Vec<&Path> = [home.target(), chosen.target()]
            .into_iter()
            .flatten()
            .collect();
        self.check_not_trash(&source, &candidates, topdir.as_deref(), &linked)?;
        self.insert(
            &source,
            candidates,
            Vec::new(),
            if own { home } else { chosen },
        )
    }

    /// Whether trashing into `trash` would reach an excluded place: the
    /// directories it opens or creates are the trash directory, its `files`
    /// and its `info`, each with its ancestors. The trash directory is
    /// checked as such: it is opened and inspected, not only passed through
    /// (a mount rule's exemption for the way to a nested mount does not
    /// apply to it).
    fn excluded_trash(&self, trash: &TrashDir) -> Result<(), Excluded> {
        let path = trash.path();
        self.policy.exclusions.check(&path)?;
        for child in ["files", "info"] {
            self.policy.exclusions.check(&path.join(child))?;
        }
        Ok(())
    }

    pub(crate) fn inspect(&self, given: &Path) -> Result<Source, TrashError> {
        let invalid = |problem| TrashError::InvalidSource {
            path: given.to_path_buf(),
            problem,
        };
        if given.as_os_str().is_empty() {
            return Err(invalid(SourceProblem::NoName));
        }
        // From the path alone, before anything touches the file system.
        let path = self.anchor(given);
        self.policy
            .exclusions
            .check(&path)
            .map_err(TrashError::Excluded)?;
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
        let parent =
            sys::canonicalize(Path::new(OsStr::from_bytes(parent))).map_err(source_error)?;
        // Again where it really is: an alias (a symbolic link on the way)
        // can lead into an excluded place. Resolving it got there already;
        // nothing more does.
        self.policy
            .exclusions
            .check(&parent.join(OsStr::from_bytes(name)))
            .map_err(TrashError::Excluded)?;
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
    /// trashed like any. A home trash reached through a link is also
    /// matched at `linked`, where such links led — usable or not, as a
    /// trash that fails its checks still holds what it holds; and the link
    /// itself is refused while the policy follows it.
    fn check_not_trash(
        &self,
        source: &Source,
        candidates: &[TrashDir],
        topdir: Option<&Path>,
        linked: &[&Path],
    ) -> Result<(), TrashError> {
        let path = source.path();
        let refuse = |problem| {
            Err(TrashError::InvalidSource {
                path: source.given.clone(),
                problem,
            })
        };
        for target in linked {
            if path.starts_with(target) && path != *target {
                return refuse(SourceProblem::InTrash);
            }
            if target.starts_with(&path) && source.kind == FileKind::Directory {
                return refuse(SourceProblem::HoldsTrash);
            }
        }
        let mut trashes: Vec<TrashDir> = candidates.to_vec();
        trashes.push(self.home_trash());
        if let Some(topdir) = topdir {
            trashes.push(self.shared_trash(topdir));
            trashes.push(self.private_trash(topdir));
        }
        for trash in trashes {
            // Its path as written, and — unless its base is excluded, which
            // forbids looking at it — as resolved: the source's path is
            // resolved, so a trash under a symbolic link is matched by the
            // latter. Resolving looks at the base only, never below it. The
            // comparison itself is lexical either way.
            let written = trash.path();
            let mut places = vec![written.clone()];
            if self.policy.exclusions.check(trash.base()).is_ok()
                && let Ok(canonical) = sys::canonicalize(trash.base())
            {
                places.push(canonical.join(written.strip_prefix(trash.base()).unwrap_or(&written)));
            }
            if trash.kind() == TrashKind::Home {
                // The link is reached by name like any file: matched as
                // written and as resolved up to it, as the source is.
                if self.policy.follow_home_link
                    && source.kind == FileKind::Symlink
                    && places.contains(&path)
                {
                    return refuse(SourceProblem::HomeTrashLink);
                }
            }
            for trash_path in places {
                if path.starts_with(&trash_path) && path != trash_path {
                    return refuse(SourceProblem::InTrash);
                }
                // Only a directory can hold one: a file or a link with a
                // trash directory's name holds nothing (and a link is
                // trashed itself, not its target).
                if trash_path.starts_with(&path) && source.kind == FileKind::Directory {
                    return refuse(SourceProblem::HoldsTrash);
                }
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
            let is_dir = || {
                sys::lookup_path_nofollow(&prefix)
                    .is_ok_and(|meta| meta.is_some_and(|meta| meta.kind == FileKind::Directory))
            };
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
        mut home: HomeRoot,
    ) -> Result<Trashed, TrashError> {
        let original = source.path();
        let date = DeletionDate::at(self.clock.now(), &self.zone).ok_or(TrashError::Clock)?;
        for candidate in candidates {
            let root = if candidate.kind() == TrashKind::Home {
                std::mem::replace(&mut home, HomeRoot::NoLink)
            } else {
                HomeRoot::NoLink
            };
            let (trash, unsynced) = match dir::open_with(self, &candidate, true, root) {
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
                Ok((name, durability, method)) => {
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
                        method,
                        durability,
                        fallbacks: attempts,
                        replaced_meanwhile,
                    });
                }
                Err(MoveFailure::Attempt(failure, leftovers)) => attempts.push(Attempt {
                    trash: Some(candidate),
                    failure,
                    leftovers,
                }),
                Err(MoveFailure::Partial {
                    name,
                    error,
                    rollback,
                    state,
                    metadata_kept,
                    leftovers,
                }) => {
                    return Err(TrashError::Partial(Box::new(PartialTrash {
                        path: source.given.clone(),
                        trash: candidate,
                        name,
                        state,
                        error,
                        rollback,
                        metadata_kept,
                        leftovers,
                        attempts,
                    })));
                }
                Err(MoveFailure::Unsupported(cause, leftovers)) => {
                    attempts.push(Attempt {
                        trash: Some(candidate),
                        failure: AttemptFailure::Unsupported,
                        leftovers,
                    });
                    return Err(TrashError::Unsupported {
                        unsupported: Unsupported {
                            operation: "trashing",
                            cause,
                        },
                        attempts,
                    });
                }
                Err(MoveFailure::SourceMissing(leftovers)) => {
                    attempts.push(Attempt {
                        trash: Some(candidate),
                        failure: AttemptFailure::SourceGone,
                        leftovers,
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
    #[allow(
        clippy::result_large_err,
        reason = "internal: one per attempt, handled at once"
    )]
    fn move_in(
        &self,
        trash: &Trash,
        source: &Source,
        bytes: &[u8],
    ) -> Result<(OsString, Durability, MoveMethod), MoveFailure> {
        let mut names = Names::new(trash, &source.name, &self.limits)?;
        loop {
            let reservation = names.reserve(trash, bytes)?;
            injected_before_move(trash, source, &reservation.name);
            let moved = if fault::hits(Stage::Move) {
                Err(TransferError {
                    kind: TransferErrorKind::Other(io::Error::other("injected failure at Move")),
                    left: None,
                })
            } else if fault::hits(Stage::MoveCrossDevice) {
                Err(TransferError {
                    kind: TransferErrorKind::CrossDevice,
                    left: None,
                })
            } else {
                transfer(
                    source.parent_fd.as_fd(),
                    &source.name,
                    trash.files(),
                    &reservation.name,
                    source.kind,
                    source.ident,
                    self.policy.fallbacks,
                )
            };
            let error = match moved {
                Ok(method) => {
                    return Ok((reservation.name, after_move(trash, source), method));
                }
                Err(error) => error,
            };
            let mut leftovers: Vec<Leftover> = error
                .left
                .map(|error| Leftover {
                    kind: LeftoverKind::Placeholder,
                    path: trash.dir().path().join("files").join(&reservation.name),
                    error,
                })
                .into_iter()
                .collect();
            let failure = match error.kind {
                // Taken in `files` (an orphaned payload, another trashing, a
                // placeholder filled meanwhile): release the name, take the
                // next — unless something of this attempt is left behind.
                TransferErrorKind::Exists if leftovers.is_empty() => {
                    if let Some(leftover) = reservation.release(trash) {
                        return Err(MoveFailure::Attempt(AttemptFailure::NoName, vec![leftover]));
                    }
                    continue;
                }
                // The placeholder made was filled meanwhile, and stays.
                TransferErrorKind::Exists => AttemptFailure::Move(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "the placeholder directory made was filled meanwhile",
                )),
                TransferErrorKind::CrossDevice => AttemptFailure::CrossDevice,
                TransferErrorKind::Unsupported(cause) => {
                    leftovers.extend(reservation.release(trash));
                    return Err(MoveFailure::Unsupported(cause, leftovers));
                }
                // Stop, whatever is where. The trash info file stays unless
                // the name it describes is confirmed not to hold the payload:
                // a name that could not be inspected may well hold it.
                TransferErrorKind::Partial {
                    error,
                    rollback,
                    state,
                    destination_foreign,
                } => {
                    let _ = after_move(trash, source);
                    let name = reservation.name.clone();
                    let mut metadata_kept = true;
                    if destination_foreign {
                        match reservation.release(trash) {
                            None => metadata_kept = false,
                            Some(leftover) => leftovers.push(leftover),
                        }
                    }
                    return Err(MoveFailure::Partial {
                        name,
                        error,
                        rollback,
                        state,
                        metadata_kept,
                        leftovers,
                    });
                }
                TransferErrorKind::Other(err) => {
                    if matches!(
                        sys::lookup_at(source.parent_fd.as_fd(), &source.name),
                        Ok(None)
                    ) {
                        leftovers.extend(reservation.release(trash));
                        return Err(MoveFailure::SourceMissing(leftovers));
                    }
                    AttemptFailure::Move(err)
                }
            };
            leftovers.extend(reservation.release(trash));
            return Err(MoveFailure::Attempt(failure, leftovers));
        }
    }
}

/// What tests have "another process" do right before the move: remove the
/// source, or take the reserved name in `files`. Nothing outside tests.
fn injected_before_move(trash: &Trash, source: &Source, name: &OsStr) {
    if fault::hits(Stage::SourceVanishes) {
        let _ = rustix::fs::unlinkat(
            source.parent_fd.as_fd(),
            &source.name,
            rustix::fs::AtFlags::empty(),
        );
    }
    if fault::hits(Stage::TakeNameFile) {
        let _ = sys::create_exclusive(trash.files(), name, 0o600);
    } else if fault::hits(Stage::TakeNameDir) {
        let _ = sys::mkdir_at(trash.files(), name, 0o700);
    } else if fault::hits(Stage::TakeNameLink) {
        let _ = rustix::fs::symlinkat("/nonexistent", trash.files(), name);
    }
}

/// Syncs `files` and the source's directory after a move: whether it is
/// known to survive a crash.
fn after_move(trash: &Trash, source: &Source) -> Durability {
    let mut durability = Durability::Synced;
    for dir in [trash.files(), source.parent_fd.as_fd()] {
        if let Err(err) = fault::check(Stage::SyncAfterMove).and_then(|()| sys::sync(dir))
            && matches!(durability, Durability::Synced)
        {
            durability = Durability::NotSynced(err);
        }
    }
    durability
}

enum MoveFailure {
    Attempt(AttemptFailure, Vec<Leftover>),
    Unsupported(Option<io::Error>, Vec<Leftover>),
    SourceMissing(Vec<Leftover>),
    Partial {
        name: OsString,
        error: io::Error,
        rollback: io::Error,
        state: PartialState,
        metadata_kept: bool,
        leftovers: Vec<Leftover>,
    },
}

impl From<AttemptFailure> for MoveFailure {
    fn from(failure: AttemptFailure) -> Self {
        Self::Attempt(failure, Vec::new())
    }
}

fn skipped(trash: Option<TrashDir>, failure: AttemptFailure) -> Attempt {
    Attempt {
        trash,
        failure,
        leftovers: Vec::new(),
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
    let base = sys::canonicalize(trash.dir().base()).ok();
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
            kind: LeftoverKind::Metadata,
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
    #[allow(
        clippy::result_large_err,
        reason = "internal: one per attempt, handled at once"
    )]
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
                        leftover.into_iter().collect(),
                    ))
                }
            };
        }
    }
}
