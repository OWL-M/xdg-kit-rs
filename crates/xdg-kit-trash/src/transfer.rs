//! Moving a payload to a name that must not exist yet — into `files` when
//! trashing, to a destination when restoring — without ever replacing what
//! is there.
//!
//! The native non-replacing rename comes first. Only when it is refused as
//! unsupported (the system lacks it, or the file system refuses it) do the
//! fallbacks [`Fallbacks`] allows apply; any other failure (a taken name,
//! another mount, permissions, an I/O error) is reported as it is, never
//! turned into a fallback attempt.

use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::os::fd::{AsFd, BorrowedFd};

use rustix::fs::AtFlags;
use rustix::io::Errno;

use crate::fault::{self, Stage};
use crate::sys::{self, FileKind, Ident, RenameError};

/// What may stand in for the native non-replacing rename where it is
/// refused as unsupported.
///
/// - **Link** (non-directories): a hard link is made at the destination
///   name — which fails if the name exists, so nothing is replaced — the
///   destination's directory is synced, and the source name is then
///   removed if it still holds the file inspected. Not atomic: for a
///   moment, and after a crash in between, the file has both names;
///   nothing is lost. A symbolic link is linked as itself (and checked to
///   be so).
/// - **Placeholder** (directories): an empty directory is made at the
///   destination name (exclusive by nature), checked to be still the one
///   made and empty, and the source directory is renamed over it. A rename
///   onto a directory replaces it only when it is empty, so no content can
///   be lost; but between the check and the rename another process with
///   write permission on the destination directory can swap in an empty
///   directory of its own, and that one is then replaced (the check is by
///   identity, which also misses a replacement that reuses the freed
///   inode number). In `files` only
///   the user may write (it is checked); a restore destination can be any
///   directory, writable by others. Hence opt-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Fallbacks {
    /// The native rename only; where it is refused, [`Unsupported`](crate::Unsupported).
    Native,
    /// The native rename, else the link fallback for non-directories.
    /// Directories stay unsupported.
    #[default]
    Link,
    /// The native rename, else the link fallback for non-directories and
    /// the placeholder fallback for directories, accepting the latter's
    /// weaker concurrency model (see the type's docs).
    LinkAndPlaceholder,
}

/// How a payload was moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MoveMethod {
    /// The native non-replacing rename: atomic.
    Rename,
    /// A hard link at the destination, then the source name removed: not
    /// atomic.
    Link,
    /// A rename over a placeholder directory made for it: the rename is
    /// atomic, under the placeholder's concurrency model.
    Placeholder,
}

impl MoveMethod {
    /// Whether the payload changed names in one step (a crash leaves it at
    /// one of them).
    #[must_use]
    pub fn is_atomic(self) -> bool {
        !matches!(self, Self::Link)
    }
}

impl fmt::Display for MoveMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Rename => "a non-replacing rename",
            Self::Link => "a hard link, then removing the source",
            Self::Placeholder => "a rename over a placeholder directory",
        })
    }
}

/// What a partial move left at the two names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PartialState {
    /// Verified: both names hold the payload, one file. Removing either
    /// name loses nothing.
    Duplicated,
    /// Not verified: a name holds a file that is not known to be the
    /// payload, or could not be inspected. Nothing was removed; inspect
    /// both names before removing anything.
    Unverified,
}

/// Why a transfer did not complete.
#[derive(Debug)]
pub(crate) struct TransferError {
    pub(crate) kind: TransferErrorKind,
    /// A placeholder directory this transfer made at the destination name
    /// and could not remove again (here is why): it is still there.
    pub(crate) left: Option<io::Error>,
}

#[derive(Debug)]
pub(crate) enum TransferErrorKind {
    /// The destination name is taken: nothing moved, nothing replaced.
    Exists,
    /// Source and destination are on different mounts: nothing moved.
    CrossDevice,
    /// No way to move without replacing here: nothing moved. The cause is
    /// the refusal of the last way tried.
    Unsupported(Option<io::Error>),
    /// Anything else: nothing moved.
    Other(io::Error),
    /// The link was made, and the move could neither be completed (`error`)
    /// nor undone (`rollback`). `state` says what the two names hold.
    Partial {
        error: io::Error,
        rollback: io::Error,
        state: PartialState,
        /// The destination name is confirmed not to hold the payload
        /// (nothing, or another file): metadata reserved for it describes
        /// nothing there.
        destination_foreign: bool,
    },
}

impl TransferError {
    fn new(kind: TransferErrorKind) -> Self {
        Self { kind, left: None }
    }
}

/// Moves `from` in `from_dir`, a file of `kind` (as inspected, a symbolic
/// link not followed), to `to` in `to_dir`, which must not exist; never
/// replaces anything at `to` but, under [`Fallbacks::LinkAndPlaceholder`],
/// the placeholder made there.
pub(crate) fn transfer(
    from_dir: BorrowedFd<'_>,
    from: &OsStr,
    to_dir: BorrowedFd<'_>,
    to: &OsStr,
    kind: FileKind,
    payload: Ident,
    fallbacks: Fallbacks,
) -> Result<MoveMethod, TransferError> {
    let native = if fault::hits(Stage::NativeRefused) {
        Err(RenameError::Unsupported(Some(Errno::INVAL.into())))
    } else {
        sys::rename_no_replace(from_dir, from, to_dir, to)
    };
    let cause = match native {
        Ok(()) => return Ok(MoveMethod::Rename),
        Err(RenameError::Exists) => return Err(TransferError::new(TransferErrorKind::Exists)),
        Err(RenameError::CrossDevice) => {
            return Err(TransferError::new(TransferErrorKind::CrossDevice));
        }
        Err(RenameError::Other(err)) => {
            return Err(TransferError::new(TransferErrorKind::Other(err)));
        }
        Err(RenameError::Unsupported(cause)) => cause,
    };
    match (kind, fallbacks) {
        (_, Fallbacks::Native) | (FileKind::Directory, Fallbacks::Link) => {
            Err(TransferError::new(TransferErrorKind::Unsupported(cause)))
        }
        (FileKind::Directory, Fallbacks::LinkAndPlaceholder) => {
            over_placeholder(from_dir, from, to_dir, to)
        }
        (_, Fallbacks::Link | Fallbacks::LinkAndPlaceholder) => {
            link_then_unlink(from_dir, from, to_dir, to, kind, payload)
        }
    }
}

/// The link fallback. `payload` is the source's identity as inspected:
/// the only established identity of what is being moved. A name holding
/// anything else is not this operation's to remove.
fn link_then_unlink(
    from_dir: BorrowedFd<'_>,
    from: &OsStr,
    to_dir: BorrowedFd<'_>,
    to: &OsStr,
    kind: FileKind,
    payload: Ident,
) -> Result<MoveMethod, TransferError> {
    let fail = |kind| Err(TransferError::new(kind));
    // No `AT_SYMLINK_FOLLOW`: a symbolic link is linked as itself.
    let linked = if fault::hits(Stage::LinkRefused) {
        Err(Errno::PERM)
    } else {
        rustix::fs::linkat(from_dir, from, to_dir, to, AtFlags::empty())
    };
    match linked {
        Ok(()) => {}
        Err(Errno::EXIST) => return fail(TransferErrorKind::Exists),
        Err(Errno::XDEV) => return fail(TransferErrorKind::CrossDevice),
        // No hard links on this file system, or not for this file
        // (`protected_hardlinks`, too many links): no way left.
        #[allow(
            unreachable_patterns,
            reason = "EOPNOTSUPP equals ENOTSUP on some systems"
        )]
        Err(
            err @ (Errno::PERM | Errno::NOTSUP | Errno::OPNOTSUPP | Errno::NOSYS | Errno::MLINK),
        ) => {
            return fail(TransferErrorKind::Unsupported(Some(err.into())));
        }
        Err(err) => return fail(TransferErrorKind::Other(err.into())),
    }
    if fault::hits(Stage::ReplaceLinked) {
        let _ = rustix::fs::unlinkat(to_dir, to, AtFlags::empty());
        let _ = sys::create_exclusive(to_dir, to, 0o600);
    }
    if fault::hits(Stage::ReplaceLinkedWithTarget) {
        let _ = rustix::fs::unlinkat(to_dir, to, AtFlags::empty());
        let _ = rustix::fs::linkat(from_dir, from, to_dir, to, AtFlags::SYMLINK_FOLLOW);
    }
    if fault::hits(Stage::RevokeDestinationSearch) {
        let _ = rustix::fs::fchmod(to_dir, sys::mode_bits(0o600));
    }
    // The new name must hold the payload itself, of the kind inspected.
    let names = Names {
        from_dir,
        from,
        to_dir,
        to,
        payload,
    };
    match sys::stat_at(to_dir, to) {
        Ok(meta) if meta.ident == payload && meta.kind == kind => {}
        Ok(meta) if kind == FileKind::Symlink && meta.kind != FileKind::Symlink => {
            return names.followed();
        }
        // Something else has the name: the source was replaced before the
        // link, or the link was replaced after it. Neither is ours to
        // remove.
        Ok(_) => {
            return fail(names.partial(
                io::Error::other("the name linked to holds a file other than the one inspected"),
                io::Error::other(
                    "nothing was removed: what the name holds is not known to be ours",
                ),
            ));
        }
        Err(error) => {
            return fail(
                names.partial(io::Error::other("the link made cannot be inspected"), error),
            );
        }
    }
    // The link reaches the disk before the source name may go: otherwise a
    // crash could keep the removal and lose the link.
    if let Err(error) = fault::check(Stage::SyncLink).and_then(|()| sys::sync(to_dir)) {
        return names.undo(error);
    }
    if fault::hits(Stage::RevokeSourceSearch) {
        let _ = rustix::fs::fchmod(from_dir, sys::mode_bits(0o600));
    }
    if fault::hits(Stage::ReplaceSource) {
        let _ = rustix::fs::unlinkat(from_dir, from, AtFlags::empty());
        let _ = sys::create_exclusive(from_dir, from, 0o600);
    }
    // The source name goes only if it is still the payload: removing it by
    // name, a file put there meanwhile would go instead.
    let removed = fault::check(Stage::UnlinkSource)
        .and_then(|()| sys::remove_if_same(from_dir, from, payload, false));
    match removed {
        // Gone already, or given to another file meanwhile: the link is the
        // payload's only name now, which is a completed move.
        Ok(_) => Ok(MoveMethod::Link),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(MoveMethod::Link),
        Err(error) => names.undo(error),
    }
}

/// The two names of a link fallback, and the payload's identity.
struct Names<'a> {
    from_dir: BorrowedFd<'a>,
    from: &'a OsStr,
    to_dir: BorrowedFd<'a>,
    to: &'a OsStr,
    payload: Ident,
}

/// What a name holds, as far as can be told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Holding {
    /// The payload.
    Payload,
    /// Confirmed not: nothing, or another file.
    Other,
    /// It could not be inspected: unknown either way.
    Unknown,
}

impl Names<'_> {
    /// What `name` in `dir` holds now. An inspection that fails confirms
    /// nothing: it is never taken for absence.
    fn holding(&self, dir: BorrowedFd<'_>, name: &OsStr) -> Holding {
        match sys::lookup_at(dir, name) {
            Ok(Some(meta)) if meta.ident == self.payload => Holding::Payload,
            Ok(_) => Holding::Other,
            Err(_) => Holding::Unknown,
        }
    }

    /// Undoes the link after `error`: removes the new name only while the
    /// source is confirmed to hold the payload still (the link is then a
    /// second name, and removing it loses nothing). Should the source be
    /// confirmed not to hold it, and the link to hold it, the link is its
    /// only name, and it stays: that is a completed move. Anything unknown
    /// removes nothing.
    fn undo(&self, error: io::Error) -> Result<MoveMethod, TransferError> {
        if fault::hits(Stage::ReplaceDuringRollback) {
            let _ = rustix::fs::unlinkat(self.to_dir, self.to, AtFlags::empty());
            let _ = sys::create_exclusive(self.to_dir, self.to, 0o600);
        }
        match self.holding(self.from_dir, self.from) {
            Holding::Payload => {}
            Holding::Other if self.holding(self.to_dir, self.to) == Holding::Payload => {
                return Ok(MoveMethod::Link);
            }
            Holding::Other => {
                return Err(TransferError::new(self.partial(
                    error,
                    io::Error::other("the source no longer holds the payload: nothing was removed"),
                )));
            }
            Holding::Unknown => {
                return Err(TransferError::new(self.partial(
                    error,
                    io::Error::other("the source cannot be inspected: nothing was removed"),
                )));
            }
        }
        // Between this look and the removal, another process can still
        // remove the source name: the removal by name cannot be tied to it.
        let removed = fault::check(Stage::Rollback)
            .and_then(|()| sys::remove_if_same(self.to_dir, self.to, self.payload, false));
        match removed {
            Ok(true) => Err(TransferError::new(TransferErrorKind::Other(error))),
            Ok(false) => Err(TransferError::new(self.partial(
                error,
                io::Error::new(io::ErrorKind::NotFound, "the link made disappeared"),
            ))),
            Err(rollback) => Err(TransferError::new(self.partial(error, rollback))),
        }
    }

    /// After linking a symbolic link, the new name was found to hold no
    /// link: `linkat` followed it, or the name was replaced meanwhile.
    /// Which cannot be told, and what the name holds is not known to be
    /// this operation's: it stays.
    fn followed(&self) -> Result<MoveMethod, TransferError> {
        Err(TransferError::new(self.partial(
            io::Error::other(
                "the name linked to holds no symbolic link: linkat may have followed it, \
                 which leaves the link fallback unusable here",
            ),
            io::Error::other("nothing was removed: what the name holds is not known to be ours"),
        )))
    }

    /// A partial outcome, with what the two names hold now: a verified
    /// duplicate only when both are confirmed to hold the payload.
    fn partial(&self, error: io::Error, rollback: io::Error) -> TransferErrorKind {
        let from = self.holding(self.from_dir, self.from);
        let to = self.holding(self.to_dir, self.to);
        TransferErrorKind::Partial {
            error,
            rollback,
            state: if from == Holding::Payload && to == Holding::Payload {
                PartialState::Duplicated
            } else {
                PartialState::Unverified
            },
            destination_foreign: to == Holding::Other,
        }
    }
}

/// The placeholder fallback.
fn over_placeholder(
    from_dir: BorrowedFd<'_>,
    from: &OsStr,
    to_dir: BorrowedFd<'_>,
    to: &OsStr,
) -> Result<MoveMethod, TransferError> {
    match sys::mkdir_at(to_dir, to, 0o700) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            return Err(TransferError::new(TransferErrorKind::Exists));
        }
        Err(err) => return Err(TransferError::new(TransferErrorKind::Other(err))),
    }
    let made = match sys::stat_at(to_dir, to) {
        Ok(meta) if meta.kind == FileKind::Directory => meta.ident,
        // Not identifiable: it cannot be removed safely either.
        Ok(_) | Err(_) => {
            return Err(TransferError {
                kind: TransferErrorKind::Other(io::Error::other(
                    "the placeholder made cannot be identified",
                )),
                left: Some(io::Error::other("the placeholder cannot be identified")),
            });
        }
    };
    let give_up = |kind| {
        let left = remove_placeholder(to_dir, to, made).err();
        Err(TransferError { kind, left })
    };
    if fault::hits(Stage::FillPlaceholder)
        && let Ok(dir) = sys::open_dir_at(to_dir, to)
    {
        let _ = sys::create_exclusive(dir.as_fd(), OsStr::new("intruder"), 0o600);
    }
    if fault::hits(Stage::ReplacePlaceholder) {
        // Made before ours goes, so that it cannot reuse its inode number.
        let _ = sys::mkdir_at(to_dir, OsStr::new(".intruder"), 0o700);
        let _ = rustix::fs::unlinkat(to_dir, to, AtFlags::REMOVEDIR);
        let _ = rustix::fs::renameat(to_dir, ".intruder", to_dir, to);
    }
    // Still the one made, and empty: as far as can be told now. Between
    // this look and the rename another process able to write to `to_dir`
    // can swap in an empty directory of its own, which the rename then
    // replaces; a non-empty one it cannot replace.
    if fault::hits(Stage::RevokeDestinationSearch) {
        let _ = rustix::fs::fchmod(to_dir, sys::mode_bits(0o600));
    }
    match sys::stat_at(to_dir, to) {
        Ok(meta) if meta.ident == made => {}
        // Someone else's now: theirs to keep, nothing of ours to remove.
        Ok(_) => return Err(TransferError::new(TransferErrorKind::Exists)),
        // Not known whose: removed only if still ours and empty, which the
        // removal checks; else reported as left.
        Err(err) => return give_up(TransferErrorKind::Other(err)),
    }
    match is_empty(to_dir, to) {
        Ok(true) => {}
        Ok(false) => return give_up(TransferErrorKind::Exists),
        Err(err) => return give_up(TransferErrorKind::Other(err)),
    }
    let renamed = if fault::hits(Stage::PlaceholderRename) {
        Err(Errno::IO)
    } else {
        rustix::fs::renameat(from_dir, from, to_dir, to)
    };
    match renamed {
        Ok(()) => Ok(MoveMethod::Placeholder),
        // Filled meanwhile: nothing replaced.
        Err(Errno::NOTEMPTY | Errno::EXIST) => give_up(TransferErrorKind::Exists),
        Err(Errno::XDEV) => give_up(TransferErrorKind::CrossDevice),
        Err(err) => give_up(TransferErrorKind::Other(err.into())),
    }
}

/// Removes the placeholder `to`, if it is still the one made and empty.
/// Another file there now is not ours: nothing to remove, no error.
fn remove_placeholder(to_dir: BorrowedFd<'_>, to: &OsStr, made: Ident) -> io::Result<()> {
    fault::check(Stage::RemovePlaceholder)?;
    match sys::remove_if_same(to_dir, to, made, true) {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err),
    }
}

/// Whether the directory `name` in `dir` has no entries.
fn is_empty(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<bool> {
    let fd = sys::open_dir_at(dir, name)?;
    let mut entries = rustix::fs::Dir::read_from(&fd)?;
    while let Some(entry) = entries.read() {
        let name = entry?.file_name().to_bytes().to_vec();
        if name != b"." && name != b".." {
            return Ok(false);
        }
    }
    Ok(true)
}
