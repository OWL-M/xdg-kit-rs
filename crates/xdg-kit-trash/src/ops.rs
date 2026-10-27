//! Restoring, erasing and discarding listed entries.
//!
//! # What "unchanged" means here
//!
//! Before acting, each operation compares the entry with the trash as it
//! is now ([`Changed`]). These are the guarantees, and their limits:
//!
//! - **Identity.** The payload must have the device and inode it had when
//!   listed, and be the same kind of file; so must the trash info file.
//!   Equal identities show the same file, not the same contents.
//! - **Metadata content.** The trash info file's bytes are read again and
//!   must equal those listed: a rewrite in place is caught, a rewrite
//!   restored to the same bytes is not (nor would it matter).
//! - **Directory handles.** Every name is resolved in the trash's open
//!   `files` and `info` directories: renaming or replacing the trash's
//!   path, or any ancestor, does not redirect an operation. Erasing a
//!   directory removes inside the directory opened (checked against the
//!   listed identity), not whatever its name leads to later.
//! - **Residual races.** The check and the action are separate system
//!   calls. A process that does not cooperate can swap a name in `files`
//!   or `info` between them, and the action then applies to what it put
//!   there. A non-replacing rename protects the destination of a move from
//!   being replaced; it does not tie the source to the checked file.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Component, Path, PathBuf};

use rustix::fs::AtFlags;
use xdg_kit_basedir::Durability;

use crate::dir::Trash;
use crate::fault::{self, Stage};
use crate::insert::Unsupported;
use crate::list::{Entry, Metadata, Origin, Payload, UnknownOrigin, info_name};
use crate::sys::{self, FileKind, Ident, Place, RenameError, Sameness};

/// What no longer matches the listed entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Changed {
    /// The entry was listed from another trash (or this one was replaced
    /// since).
    Trash,
    /// The payload is gone, or another file has its name.
    Payload,
    /// The trash info file appeared, disappeared, was replaced or was
    /// rewritten.
    Metadata,
    /// The payload, or the trash info file, cannot be inspected now (its
    /// directory cannot be searched, say): that it is unchanged — or
    /// absent — cannot be confirmed, so nothing is done.
    Unverifiable,
}

impl fmt::Display for Changed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Trash => "the entry is from another trash",
            Self::Payload => "the payload changed since it was listed",
            Self::Metadata => "the trash info file changed since it was listed",
            Self::Unverifiable => "the entry cannot be inspected to confirm it is unchanged",
        })
    }
}

/// What happened to the trash info file after the payload left.
#[derive(Debug)]
#[non_exhaustive]
pub enum MetadataRemoval {
    /// It was removed.
    Removed,
    /// There was none.
    Absent,
    /// It is still there: removing it failed, or another file has its
    /// name now. It names no payload any more.
    Kept(io::Error),
}

impl Trash {
    /// Compares `entry` with the trash now; see the module docs.
    fn verify(&self, entry: &Entry) -> Result<(), Changed> {
        if entry.trash.ident != self.id || entry.trash.dir != self.dir {
            return Err(Changed::Trash);
        }
        let now = self.read_entry(&entry.name);
        // An inspection that failed confirms nothing: not presence, not
        // absence, not identity.
        if matches!(now.payload, Payload::Unknown(_))
            || (matches!(now.metadata, Metadata::Unreadable(_)) && now.seen.metadata.is_none())
        {
            return Err(Changed::Unverifiable);
        }
        let kind = |payload: &Payload| match payload {
            Payload::Present(kind) => Some(*kind),
            Payload::Missing | Payload::Unknown(_) => None,
        };
        if now.seen.payload != entry.seen.payload || kind(&now.payload) != kind(&entry.payload) {
            return Err(Changed::Payload);
        }
        if now.seen.metadata != entry.seen.metadata || now.seen.bytes != entry.seen.bytes {
            return Err(Changed::Metadata);
        }
        Ok(())
    }

    /// Removes the trash info file of `entry` if it is still the one
    /// listed.
    fn remove_metadata(&self, entry: &Entry) -> MetadataRemoval {
        let Some(ident) = entry.seen.metadata else {
            return MetadataRemoval::Absent;
        };
        let removed = fault::check(Stage::RemoveInfo)
            .and_then(|()| sys::remove_if_same(self.info(), &info_name(&entry.name), ident, false));
        match removed {
            Ok(true) => MetadataRemoval::Removed,
            Ok(false) => MetadataRemoval::Absent,
            Err(err) => MetadataRemoval::Kept(err),
        }
    }
}

/// How to restore.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreOptions {
    /// Where to put it instead of its origin (absolute). Required when the
    /// origin is unknown.
    pub destination: Option<PathBuf>,
    /// Create missing directories above the destination (mode 0777 less
    /// the umask). Off: a missing directory is
    /// [`RestoreFailure::MissingParent`].
    pub create_parents: bool,
}

/// A completed restore: the payload is at its destination.
#[derive(Debug)]
#[must_use = "NotSynced durability means the restore may not survive a crash"]
#[non_exhaustive]
pub struct Restored {
    /// Where it is now.
    pub destination: PathBuf,
    /// The directories created above it, outermost first.
    pub created: Vec<PathBuf>,
    /// What became of the trash info file.
    pub metadata: MetadataRemoval,
    /// Whether the move is known to survive a crash (`files`, the
    /// destination's directory and those created were synced).
    pub durability: Durability,
}

/// Why a restore did not happen: the payload is still in the trash, with
/// its trash info file.
#[derive(Debug)]
#[non_exhaustive]
pub struct RestoreError {
    /// What went wrong.
    pub failure: RestoreFailure,
    /// Directories created for the destination that could not be removed
    /// again (they are empty, or were filled by someone else meanwhile).
    pub left_behind: Vec<PathBuf>,
}

/// What went wrong restoring.
#[derive(Debug)]
#[non_exhaustive]
pub enum RestoreFailure {
    /// No non-replacing rename here.
    Unsupported(Unsupported),
    /// The entry no longer matches the trash.
    Changed(Changed),
    /// The entry had no payload when listed, or it could not be inspected.
    PayloadMissing,
    /// The origin is unknown and no destination was given.
    OriginUnknown(UnknownOrigin),
    /// The destination is not absolute or does not end in a name.
    InvalidDestination(PathBuf),
    /// Something has the destination's name: nothing was replaced.
    Conflict(PathBuf),
    /// A directory above the destination does not exist (its volume may
    /// be unmounted), and creating it was not asked for.
    MissingParent(PathBuf),
    /// A directory above the destination could not be opened or created.
    Parent {
        /// The directory.
        path: PathBuf,
        /// What failed.
        error: io::Error,
    },
    /// The destination is on another mount than the trash: a rename cannot
    /// reach it, and nothing is copied.
    CrossDevice(PathBuf),
    /// The move failed otherwise.
    Move(io::Error),
}

impl fmt::Display for RestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.failure {
            RestoreFailure::Unsupported(unsupported) => unsupported.fmt(f),
            RestoreFailure::Changed(changed) => changed.fmt(f),
            RestoreFailure::PayloadMissing => f.write_str("the entry has no payload"),
            RestoreFailure::OriginUnknown(why) => write!(f, "the origin is unknown: {why}"),
            RestoreFailure::InvalidDestination(path) => {
                write!(f, "{} is no absolute path ending in a name", path.display())
            }
            RestoreFailure::Conflict(path) => write!(f, "{} exists", path.display()),
            RestoreFailure::MissingParent(path) => write!(f, "{} does not exist", path.display()),
            RestoreFailure::Parent { path, error } => write!(f, "{}: {error}", path.display()),
            RestoreFailure::CrossDevice(path) => {
                write!(f, "{} is on another mount than the trash", path.display())
            }
            RestoreFailure::Move(err) => write!(f, "cannot move the payload: {err}"),
        }
    }
}

impl std::error::Error for RestoreError {}

/// A directory created for a destination: where, and which.
struct Created {
    parent: OwnedFd,
    name: OsString,
    /// `None` when it could not be identified after it was made: it is
    /// then never removed, only reported.
    ident: Option<Ident>,
    path: PathBuf,
}

impl Trash {
    /// Moves `entry`'s payload back to its origin (or
    /// [`RestoreOptions::destination`]), never replacing what is there,
    /// then removes its trash info file. Directories created for the
    /// destination are removed again if the move fails, when still the
    /// ones created and empty.
    ///
    /// # Errors
    ///
    /// See [`RestoreFailure`]; the payload stays in the trash.
    pub fn restore(
        &self,
        entry: &Entry,
        options: &RestoreOptions,
    ) -> Result<Restored, RestoreError> {
        let mut created = Vec::new();
        let result = self.restore_with(entry, options, &mut created);
        match result {
            Ok(mut restored) => {
                restored.created = created.into_iter().map(|dir| dir.path).collect();
                Ok(restored)
            }
            Err(failure) => {
                let mut left_behind = Vec::new();
                for dir in created.into_iter().rev() {
                    let removed = dir.ident.is_some_and(|ident| {
                        sys::remove_if_same(dir.parent.as_fd(), &dir.name, ident, true).is_ok()
                    });
                    if !removed {
                        left_behind.push(dir.path);
                    }
                }
                left_behind.reverse();
                Err(RestoreError {
                    failure,
                    left_behind,
                })
            }
        }
    }

    fn restore_with(
        &self,
        entry: &Entry,
        options: &RestoreOptions,
        created: &mut Vec<Created>,
    ) -> Result<Restored, RestoreFailure> {
        if !sys::NO_REPLACE_RENAME {
            return Err(RestoreFailure::Unsupported(Unsupported {
                operation: "restoring",
                cause: None,
            }));
        }
        if !matches!(entry.payload, Payload::Present(_)) {
            return Err(RestoreFailure::PayloadMissing);
        }
        let destination = match (&options.destination, &entry.origin) {
            (Some(path), _) | (None, Origin::Known(path)) => path.clone(),
            (None, Origin::Unknown(why)) => return Err(RestoreFailure::OriginUnknown(why.clone())),
        };
        let invalid = || RestoreFailure::InvalidDestination(destination.clone());
        if !destination.is_absolute()
            || !matches!(
                destination.components().next_back(),
                Some(Component::Normal(_))
            )
        {
            return Err(invalid());
        }
        let (Some(parent), Some(name)) = (destination.parent(), destination.file_name()) else {
            return Err(invalid());
        };
        let parent_fd = open_parent(parent, options.create_parents, created)?;
        self.verify(entry).map_err(RestoreFailure::Changed)?;
        let moved = if fault::hits(Stage::Restore) {
            Err(RenameError::Other(io::Error::other(
                "injected failure at Restore",
            )))
        } else {
            sys::rename_no_replace(self.files(), &entry.name, parent_fd.as_fd(), name)
        };
        match moved {
            Ok(()) => {}
            Err(RenameError::Exists) => return Err(RestoreFailure::Conflict(destination)),
            Err(RenameError::CrossDevice) => return Err(RestoreFailure::CrossDevice(destination)),
            Err(RenameError::Unsupported(cause)) => {
                return Err(RestoreFailure::Unsupported(Unsupported {
                    operation: "restoring",
                    cause,
                }));
            }
            Err(RenameError::Other(err)) => {
                return Err(
                    if matches!(sys::lookup_at(self.files(), &entry.name), Ok(None)) {
                        RestoreFailure::Changed(Changed::Payload)
                    } else {
                        RestoreFailure::Move(err)
                    },
                );
            }
        }
        let mut durability = Durability::Synced;
        let mut dirs: Vec<BorrowedFd<'_>> = vec![self.files(), parent_fd.as_fd()];
        dirs.extend(created.iter().map(|dir| dir.parent.as_fd()));
        for dir in dirs {
            if let Err(err) = sys::sync(dir)
                && matches!(durability, Durability::Synced)
            {
                durability = Durability::NotSynced(err);
            }
        }
        Ok(Restored {
            destination,
            created: Vec::new(),
            metadata: self.remove_metadata(entry),
            durability,
        })
    }
}

/// Opens `parent`, creating it and its missing ancestors when `create`;
/// each one created is recorded in `created`, outermost first.
fn open_parent(
    parent: &Path,
    create: bool,
    created: &mut Vec<Created>,
) -> Result<OwnedFd, RestoreFailure> {
    let failed = |path: &Path, error: io::Error| RestoreFailure::Parent {
        path: path.to_path_buf(),
        error,
    };
    match sys::open_dir(parent) {
        Ok(fd) => return Ok(fd),
        Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(failed(parent, err)),
        Err(_) if !create => return Err(RestoreFailure::MissingParent(parent.to_path_buf())),
        Err(_) => {}
    }
    // The nearest ancestor that exists, then each missing level below it,
    // made and opened relative to the one above.
    let mut missing = Vec::new();
    let mut at = parent;
    let mut fd = loop {
        let Some(name) = at.file_name() else {
            return Err(RestoreFailure::MissingParent(parent.to_path_buf()));
        };
        missing.push(name.to_os_string());
        at = at.parent().unwrap_or(Path::new("/"));
        match sys::open_dir(at) {
            Ok(fd) => break fd,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(failed(at, err)),
        }
    };
    let mut path = at.to_path_buf();
    for name in missing.into_iter().rev() {
        path.push(&name);
        let made = match sys::mkdir_at(fd.as_fd(), &name, 0o777) {
            Ok(()) => true,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => false,
            Err(err) => return Err(failed(&path, err)),
        };
        // Recorded at once, identified by `stat` (which needs no access to
        // the new directory itself), before anything else can fail.
        let parent = if made {
            let ident = fault::check(Stage::IdentifyCreated)
                .and_then(|()| sys::stat_at(fd.as_fd(), &name))
                .ok()
                .filter(|meta| meta.kind == FileKind::Directory)
                .map(|meta| meta.ident);
            created.push(Created {
                parent: fd,
                name: name.clone(),
                ident,
                path: path.clone(),
            });
            &created.last().expect("just recorded").parent
        } else {
            &fd
        };
        let child = fault::check(Stage::OpenCreated)
            .and_then(|()| sys::open_dir_at(parent.as_fd(), &name))
            .map_err(|err| failed(&path, err))?;
        if made {
            let opened = sys::fstat(&child).map_err(|err| failed(&path, err))?.ident;
            if created.last().and_then(|dir| dir.ident) != Some(opened) {
                return Err(failed(
                    &path,
                    io::Error::other("the directory made was replaced before it was opened"),
                ));
            }
        }
        fd = child;
    }
    Ok(fd)
}

/// What to do about permissions that keep a trashed directory's contents
/// from being removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Permissions {
    /// Change nothing: what cannot be removed stays, reported.
    #[default]
    Leave,
    /// Give the owner read, write and search permission on each directory
    /// of the payload the user owns, before removing its contents — only
    /// through the directory's own open handle, never by name, so a
    /// directory that cannot be opened at all is not changed. Changed
    /// permissions stay changed if the erase stops early.
    GrantOwner,
}

/// How to erase.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EraseOptions {
    /// What to do about permissions.
    pub permissions: Permissions,
}

/// A completed erase: the payload is gone.
#[derive(Debug)]
#[non_exhaustive]
pub struct Erased {
    /// Files and directories removed.
    pub removed: u64,
    /// Ones that disappeared before they could be removed.
    pub vanished: u64,
    /// Directories whose permissions were changed.
    pub permissions_changed: u64,
    /// What became of the trash info file.
    pub metadata: MetadataRemoval,
}

/// Why an erase did not finish. The trash info file is kept: the payload
/// is not known to be gone.
#[derive(Debug)]
#[non_exhaustive]
pub struct EraseError {
    /// Why it stopped.
    pub failure: EraseFailure,
    /// Files and directories removed before it stopped.
    pub removed: u64,
    /// Ones that disappeared before they could be removed.
    pub vanished: u64,
    /// Directories whose permissions were changed (and stay changed).
    pub permissions_changed: u64,
    /// What could not be removed, and why.
    pub problems: Vec<EraseProblem>,
}

/// Why an erase stopped.
#[derive(Debug)]
#[non_exhaustive]
pub enum EraseFailure {
    /// The entry no longer matches the trash; nothing was removed.
    Changed(Changed),
    /// The entry had no payload when listed, or it could not be
    /// inspected: see [`Trash::discard_metadata`] for the former.
    PayloadMissing,
    /// `cancel` said to stop.
    Cancelled,
    /// Parts of the payload could not be removed ([`EraseError::problems`]).
    Incomplete,
    /// Whether the payload is gone could not be told.
    Unconfirmed(io::Error),
}

/// A part of a payload that was not removed.
#[derive(Debug)]
#[non_exhaustive]
pub struct EraseProblem {
    /// Its path inside the payload (empty for the payload itself).
    pub path: PathBuf,
    /// Why.
    pub kind: EraseProblemKind,
}

/// Why a part of a payload was not removed.
#[derive(Debug)]
#[non_exhaustive]
pub enum EraseProblemKind {
    /// It is a directory on another mount (or the root of one): erasing
    /// never crosses mounts.
    MountBoundary,
    /// It is deeper than [`Limits::max_depth`](crate::Limits::max_depth).
    TooDeep,
    /// [`Limits::max_tree_entries`](crate::Limits::max_tree_entries)
    /// entries were visited.
    TooMany,
    /// A system call failed.
    Io(io::Error),
}

impl fmt::Display for EraseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.failure {
            EraseFailure::Changed(changed) => changed.fmt(f),
            EraseFailure::PayloadMissing => f.write_str("the entry has no payload"),
            EraseFailure::Cancelled => write!(f, "cancelled after removing {} items", self.removed),
            EraseFailure::Incomplete => {
                write!(
                    f,
                    "{} parts of the payload could not be removed",
                    self.problems.len()
                )
            }
            EraseFailure::Unconfirmed(err) => {
                write!(f, "cannot tell whether the payload is gone: {err}")
            }
        }
    }
}

impl std::error::Error for EraseError {}

/// An open directory being emptied.
struct Frame {
    fd: OwnedFd,
    dir: rustix::fs::Dir,
    name: OsString,
    path: PathBuf,
}

/// The progress of one erase.
struct Eraser<'a> {
    trash: &'a Trash,
    options: &'a EraseOptions,
    cancel: &'a dyn Fn() -> bool,
    root: Place,
    visited: u64,
    removed: u64,
    vanished: u64,
    permissions_changed: u64,
    problems: Vec<EraseProblem>,
}

impl Trash {
    /// Removes `entry`'s payload for good, then its trash info file — only
    /// once the payload is confirmed gone. A directory is emptied depth
    /// first through directory handles, never following a symbolic link
    /// and never descending into another mount; `cancel` is asked before
    /// each item.
    ///
    /// # Errors
    ///
    /// See [`EraseFailure`]. What was removed stays removed; the trash
    /// info file stays while any of the payload does.
    pub fn erase(
        &self,
        entry: &Entry,
        options: &EraseOptions,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Erased, EraseError> {
        let mut eraser = Eraser {
            trash: self,
            options,
            cancel,
            root: Place {
                dev: 0,
                mount: None,
            },
            visited: 0,
            removed: 0,
            vanished: 0,
            permissions_changed: 0,
            problems: Vec::new(),
        };
        let failure = match eraser.run(entry) {
            Ok(()) => match fault::check(Stage::Confirm)
                .and_then(|()| sys::lookup_at(self.files(), &entry.name))
            {
                Ok(None) => {
                    return Ok(Erased {
                        removed: eraser.removed,
                        vanished: eraser.vanished,
                        permissions_changed: eraser.permissions_changed,
                        metadata: self.remove_metadata(entry),
                    });
                }
                Ok(Some(_)) => EraseFailure::Incomplete,
                Err(err) => EraseFailure::Unconfirmed(err),
            },
            Err(failure) => failure,
        };
        Err(EraseError {
            failure,
            removed: eraser.removed,
            vanished: eraser.vanished,
            permissions_changed: eraser.permissions_changed,
            problems: eraser.problems,
        })
    }
}

impl Eraser<'_> {
    fn problem(&mut self, path: &Path, kind: EraseProblemKind) {
        self.problems.push(EraseProblem {
            path: path.to_path_buf(),
            kind,
        });
    }

    fn run(&mut self, entry: &Entry) -> Result<(), EraseFailure> {
        let trash = self.trash;
        let kind = match entry.payload {
            Payload::Present(kind) => kind,
            Payload::Missing | Payload::Unknown(_) => return Err(EraseFailure::PayloadMissing),
        };
        trash.verify(entry).map_err(EraseFailure::Changed)?;
        if (self.cancel)() {
            return Err(EraseFailure::Cancelled);
        }
        if kind != FileKind::Directory {
            self.unlink(trash.files(), &entry.name, Path::new(""), false);
            return Ok(());
        }
        let root = self.open_root(entry)?;
        let mut stack = Vec::new();
        self.push(&mut stack, root, entry.name.clone(), PathBuf::new());
        while let Some(last) = stack.len().checked_sub(1) {
            if (self.cancel)() {
                return Err(EraseFailure::Cancelled);
            }
            let item = match stack[last].dir.read() {
                None => {
                    let frame = stack.pop().expect("a frame");
                    let parent = stack.last().map_or(trash.files(), |above| above.fd.as_fd());
                    drop(frame.dir);
                    drop(frame.fd);
                    self.unlink(parent, &frame.name, &frame.path, true);
                    continue;
                }
                Some(Err(err)) => {
                    // Stop reading it: it stays, reported.
                    let frame = stack.pop().expect("a frame");
                    self.problem(&frame.path, EraseProblemKind::Io(err.into()));
                    continue;
                }
                Some(Ok(item)) => item,
            };
            let name = OsStr::from_bytes(item.file_name().to_bytes()).to_os_string();
            if name == "." || name == ".." {
                continue;
            }
            self.visited += 1;
            let path = stack[last].path.join(&name);
            if self.visited > trash.limits.max_tree_entries {
                self.problem(&path, EraseProblemKind::TooMany);
                return Err(EraseFailure::Incomplete);
            }
            let fd = stack[last].fd.as_fd();
            let is_dir = match sys::entry_type(&item) {
                rustix::fs::FileType::Directory => true,
                rustix::fs::FileType::Unknown => match sys::stat_at(fd, &name) {
                    Ok(meta) => meta.kind == FileKind::Directory,
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {
                        self.vanished += 1;
                        continue;
                    }
                    Err(err) => {
                        self.problem(&path, EraseProblemKind::Io(err));
                        continue;
                    }
                },
                _ => false,
            };
            if !is_dir {
                self.unlink(fd, &name, &path, false);
                continue;
            }
            if stack.len() >= trash.limits.max_depth {
                self.problem(&path, EraseProblemKind::TooDeep);
                continue;
            }
            let child = match sys::open_dir_at(fd, &name) {
                Ok(child) => child,
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    self.vanished += 1;
                    continue;
                }
                Err(err) => {
                    // Replaced by a non-directory since: removed as one.
                    if matches!(sys::stat_at(fd, &name), Ok(meta) if meta.kind != FileKind::Directory)
                    {
                        self.unlink(fd, &name, &path, false);
                    } else {
                        self.problem(&path, EraseProblemKind::Io(err));
                    }
                    continue;
                }
            };
            self.push(&mut stack, child, name, path);
        }
        Ok(())
    }

    /// Opens the payload directory of `entry`, checking it is the one
    /// listed, and notes the place of `files`, which no part of the payload
    /// may leave.
    fn open_root(&mut self, entry: &Entry) -> Result<OwnedFd, EraseFailure> {
        let root = match sys::open_dir_at(self.trash.files(), &entry.name) {
            Ok(fd) => fd,
            Err(err) => {
                self.problem(Path::new(""), EraseProblemKind::Io(err));
                return Err(EraseFailure::Incomplete);
            }
        };
        // The directory opened must be the one checked.
        match sys::fstat(&root) {
            Ok(meta) if Some(meta.ident) == entry.seen.payload => {}
            Ok(_) => return Err(EraseFailure::Changed(Changed::Payload)),
            Err(err) => {
                self.problem(Path::new(""), EraseProblemKind::Io(err));
                return Err(EraseFailure::Incomplete);
            }
        }
        // The boundary is `files` itself: a payload that is a mount point
        // (another file system mounted on `files/<name>`) is not entered.
        self.root = Place::of(self.trash.files()).map_err(|err| {
            self.problem(Path::new(""), EraseProblemKind::Io(err));
            EraseFailure::Incomplete
        })?;
        Ok(root)
    }

    /// Starts emptying the open directory `fd`, unless it is on another
    /// mount; gives its owner access first when the policy says so.
    fn push(&mut self, stack: &mut Vec<Frame>, fd: OwnedFd, name: OsString, path: PathBuf) {
        let meta = match sys::fstat(&fd) {
            Ok(meta) => meta,
            Err(err) => {
                self.problem(&path, EraseProblemKind::Io(err));
                return;
            }
        };
        match Place::of(fd.as_fd()) {
            Ok(place) if place.same(self.root) != Sameness::Different => {}
            Ok(_) => {
                self.problem(&path, EraseProblemKind::MountBoundary);
                return;
            }
            Err(err) => {
                self.problem(&path, EraseProblemKind::Io(err));
                return;
            }
        }
        if self.options.permissions == Permissions::GrantOwner
            && meta.uid == self.trash.uid
            && meta.mode & 0o700 != 0o700
        {
            match rustix::fs::fchmod(&fd, sys::mode_bits(meta.mode | 0o700)) {
                Ok(()) => self.permissions_changed += 1,
                Err(err) => self.problem(&path, EraseProblemKind::Io(err.into())),
            }
        }
        let dir = match rustix::fs::Dir::read_from(&fd) {
            Ok(dir) => dir,
            Err(err) => {
                self.problem(&path, EraseProblemKind::Io(err.into()));
                return;
            }
        };
        stack.push(Frame {
            fd,
            dir,
            name,
            path,
        });
    }

    /// Removes `name` from `dir`; a name already gone counts as vanished.
    fn unlink(&mut self, dir: BorrowedFd<'_>, name: &OsStr, path: &Path, is_dir: bool) {
        let flags = if is_dir {
            AtFlags::REMOVEDIR
        } else {
            AtFlags::empty()
        };
        let removed = fault::check(Stage::EraseItem)
            .and_then(|()| rustix::fs::unlinkat(dir, name, flags).map_err(io::Error::from));
        match removed {
            Ok(()) => self.removed += 1,
            Err(err) if err.kind() == io::ErrorKind::NotFound => self.vanished += 1,
            // A directory left non-empty by problems inside it: those were
            // reported already.
            Err(err)
                if is_dir
                    && err.kind() == io::ErrorKind::DirectoryNotEmpty
                    && self
                        .problems
                        .iter()
                        .any(|problem| problem.path.starts_with(path)) => {}
            Err(err) => self.problem(path, EraseProblemKind::Io(err)),
        }
    }
}

/// A caller's statement that no trashing into this trash is under way
/// anywhere (no process, this one included, is between creating a trash
/// info file and moving its payload in), so a trash info file without a
/// payload is abandoned. Nothing in the trash can establish this: an
/// unfinished trashing looks the same, its writer only paused.
#[derive(Debug, Clone, Copy)]
pub struct Quiescent(());

impl Quiescent {
    /// Asserts the trash is quiescent (see the type's docs).
    #[must_use]
    pub fn asserted() -> Self {
        Self(())
    }
}

/// Why trash info was not discarded.
#[derive(Debug)]
#[non_exhaustive]
pub enum DiscardError {
    /// The entry has a payload: restore or erase it instead.
    PayloadPresent,
    /// The entry has no trash info file.
    NoMetadata,
    /// The entry no longer matches the trash.
    Changed(Changed),
    /// Removing it failed.
    Io(io::Error),
}

impl fmt::Display for DiscardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PayloadPresent => f.write_str("the entry has a payload"),
            Self::NoMetadata => f.write_str("the entry has no trash info file"),
            Self::Changed(changed) => changed.fmt(f),
            Self::Io(err) => write!(f, "cannot remove the trash info file: {err}"),
        }
    }
}

impl std::error::Error for DiscardError {}

impl Trash {
    /// Removes the trash info file of an entry without a payload: a
    /// destructive maintenance operation. Nothing removes such files on
    /// its own, by age or otherwise, because a trashing in progress (or
    /// paused for any length of time) looks exactly like an abandoned one;
    /// discarding its file would leave its payload, once moved, without
    /// metadata. Call this only when the trash is quiescent, which the
    /// caller states with `quiescent`. The checks made (no payload now,
    /// the file unchanged since listed) do not prove it.
    ///
    /// # Errors
    ///
    /// See [`DiscardError`].
    pub fn discard_metadata(
        &self,
        entry: &Entry,
        quiescent: Quiescent,
    ) -> Result<(), DiscardError> {
        let _ = quiescent;
        if !matches!(entry.payload, Payload::Missing) {
            return Err(DiscardError::PayloadPresent);
        }
        let Some(ident) = entry.seen.metadata else {
            return Err(DiscardError::NoMetadata);
        };
        self.verify(entry).map_err(DiscardError::Changed)?;
        match sys::remove_if_same(self.info(), &info_name(&entry.name), ident, false) {
            Ok(true) => Ok(()),
            Ok(false) => Err(DiscardError::Changed(Changed::Metadata)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                Err(DiscardError::Changed(Changed::Metadata))
            }
            Err(err) => Err(DiscardError::Io(err)),
        }
    }
}
