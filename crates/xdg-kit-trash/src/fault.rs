//! Failures injected at named stages, in this crate's own tests only.
//! Outside tests every check passes and compiles to nothing.

use std::io;

/// A step that can be made to fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    /// Creating the trash info file.
    CreateInfo,
    /// Writing it.
    WriteInfo,
    /// Syncing it.
    SyncInfo,
    /// Syncing `info` after creating it.
    SyncInfoDir,
    /// Moving the payload into `files` (an I/O error).
    Move,
    /// Just before the move, the source is removed (by "another
    /// process").
    SourceVanishes,
    /// Moving the payload into `files` (another mount).
    MoveCrossDevice,
    /// The native non-replacing rename, refused as unsupported (`EINVAL`).
    NativeRefused,
    /// The link fallback's `linkat`, refused (`EPERM`).
    LinkRefused,
    /// Between the link and removing the source, the source name is given
    /// to another file (by "another process").
    ReplaceSource,
    /// Right after the link, its name is given to another file.
    ReplaceLinked,
    /// Right after the link, its name is given to another hard link of
    /// what the source (a symbolic link) points to.
    ReplaceLinkedWithTarget,
    /// Right after the link (or before a placeholder is checked again),
    /// the destination's directory loses search permission: what was made
    /// there cannot be inspected.
    RevokeDestinationSearch,
    /// Syncing `files` after the link, before the source name goes.
    SyncLink,
    /// Removing the source name after linking it.
    UnlinkSource,
    /// Before removing a link again, its name is given to another file.
    ReplaceDuringRollback,
    /// Removing a link again.
    Rollback,
    /// Between the link and removing the source, the source's directory
    /// loses search permission: the source can be neither removed nor
    /// inspected.
    RevokeSourceSearch,
    /// Before the rename over a placeholder, a file is put into it.
    FillPlaceholder,
    /// Before the rename over a placeholder, it is replaced by another
    /// empty directory.
    ReplacePlaceholder,
    /// Removing a placeholder again.
    RemovePlaceholder,
    /// The rename over a placeholder fails (`EIO`).
    PlaceholderRename,
    /// Right before the move, another process takes the reserved name in
    /// `files` with a regular file.
    TakeNameFile,
    /// … with a directory.
    TakeNameDir,
    /// … with a dangling symbolic link.
    TakeNameLink,
    /// Removing a reservation after a failure.
    RemoveReservation,
    /// Syncing `files` and the source's directory after the move.
    SyncAfterMove,
    /// Moving a payload out of `files` when restoring.
    Restore,
    /// Identifying a directory just made for a restore.
    IdentifyCreated,
    /// Opening a directory just made for a restore.
    OpenCreated,
    /// Removing the trash info file after a restore or an erase.
    RemoveInfo,
    /// Removing one item of a payload when erasing.
    EraseItem,
    /// Replacing `directorysizes`.
    WriteCache,
    /// Looking whether an erased payload is gone.
    Confirm,
    /// Right after the home trash's symbolic link is read, it is pointed
    /// at the [`swap_to`] directory instead (by "another process").
    RetargetHomeLink,
    /// Between looking at a directory on the way to the link's target and
    /// opening it, the directory is moved aside and a symbolic link to the
    /// [`swap_to`] directory put in its place.
    SwapOnTheWay,
    /// Right after the link's target is opened and checked, the link is
    /// pointed at the [`swap_to`] directory and the target renamed to its
    /// name with `.moved` appended.
    MoveHomeTarget,
    /// Opening `$XDG_DATA_HOME` to look at the home trash's link fails
    /// (`EACCES`), as where only a read-only open is available and the
    /// directory may be searched but not read.
    OpenDataHome,
}

#[cfg(test)]
thread_local! {
    /// The stages to fail, each with how many times to pass it first.
    #[allow(
        clippy::missing_const_for_thread_local,
        reason = "both initializers are const already; the lint still fires for Android targets"
    )]
    static FAIL: std::cell::RefCell<Vec<(Stage, u32)>> = const { std::cell::RefCell::new(Vec::new()) };
    /// When set, the home trash is taken as on another mount than every
    /// source, and this as the sources' top directory.
    #[allow(
        clippy::missing_const_for_thread_local,
        reason = "both initializers are const already; the lint still fires for Android targets"
    )]
    static ELSEWHERE: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
    /// Where the stages that swap a link point it.
    #[allow(
        clippy::missing_const_for_thread_local,
        reason = "both initializers are const already; the lint still fires for Android targets"
    )]
    static SWAP_TO: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Sets where [`Stage::RetargetHomeLink`], [`Stage::SwapOnTheWay`] and
/// [`Stage::MoveHomeTarget`] point a link.
#[cfg(test)]
pub(crate) fn swap_to(path: &std::path::Path) {
    SWAP_TO.with(|swap| *swap.borrow_mut() = Some(path.to_path_buf()));
}

/// Where a swapped link points, if set.
#[cfg(test)]
pub(crate) fn swapped_target() -> Option<std::path::PathBuf> {
    SWAP_TO.with(|swap| swap.borrow().clone())
}

/// Never.
#[cfg(not(test))]
#[inline]
pub(crate) fn swapped_target() -> Option<std::path::PathBuf> {
    None
}

/// Fails `stage` once on this thread, after passing it `skip` times.
#[cfg(test)]
pub(crate) fn fail(stage: Stage, skip: u32) {
    FAIL.with(|fail| fail.borrow_mut().push((stage, skip)));
}

/// Stops failing, and stops pretending.
#[cfg(test)]
pub(crate) fn clear() {
    FAIL.with(|fail| fail.borrow_mut().clear());
    ELSEWHERE.with(|elsewhere| *elsewhere.borrow_mut() = None);
    SWAP_TO.with(|swap| *swap.borrow_mut() = None);
}

/// Pretends on this thread that the home trash is on another mount than
/// every source, and that sources' mount is at `topdir`: per-volume
/// selection without mounting anything.
#[cfg(test)]
pub(crate) fn pretend_volume(topdir: &std::path::Path) {
    ELSEWHERE.with(|elsewhere| *elsewhere.borrow_mut() = Some(topdir.to_path_buf()));
}

/// The pretended top directory, if any.
#[cfg(test)]
pub(crate) fn pretended_volume() -> Option<std::path::PathBuf> {
    ELSEWHERE.with(|elsewhere| elsewhere.borrow().clone())
}

/// Never.
#[cfg(not(test))]
#[inline]
pub(crate) fn pretended_volume() -> Option<std::path::PathBuf> {
    None
}

/// Whether `stage` fails now.
#[cfg(test)]
pub(crate) fn hits(stage: Stage) -> bool {
    FAIL.with(|fail| {
        let mut fail = fail.borrow_mut();
        let Some(at) = fail.iter().position(|(failing, _)| *failing == stage) else {
            return false;
        };
        if fail[at].1 == 0 {
            fail.remove(at);
            true
        } else {
            fail[at].1 -= 1;
            false
        }
    })
}

/// Never.
#[cfg(not(test))]
#[inline]
pub(crate) fn hits(_stage: Stage) -> bool {
    false
}

/// An error when `stage` fails now.
#[inline]
pub(crate) fn check(stage: Stage) -> io::Result<()> {
    if hits(stage) {
        Err(io::Error::other(format!("injected failure at {stage:?}")))
    } else {
        Ok(())
    }
}
