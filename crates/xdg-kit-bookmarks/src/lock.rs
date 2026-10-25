//! Serializing cooperating writers: an advisory lock around a whole
//! load→edit→write transaction.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// An advisory lock for a bookmark file, held by users of this library
/// (and anything else honoring the convention) around a whole
/// load→edit→write transaction, so their writes never race each other.
///
/// The lock is an `flock` on a stable sidecar — `<file>.lock` next to the
/// bookmark file — never on the bookmark file itself: an `flock` stays
/// with the open file it was taken on, so after an atomic replacement it
/// would no longer protect the object the pathname names, while the
/// sidecar is never replaced. The sidecar is created when missing and
/// deliberately never removed (removing it would race new acquirers).
///
/// Entirely optional, and only cooperating processes benefit: GTK and
/// other writers of `recently-used.xbel` do not take it. The lock
/// releases when the guard drops (or the process ends).
#[derive(Debug)]
pub struct BookmarkLock {
    /// Held for the lock; the descriptor is the lock.
    _sidecar: fs::File,
}

impl BookmarkLock {
    /// Takes the lock for the bookmark file at `path`, waiting for a
    /// holder to release it. The wait is unbounded; use
    /// [`BookmarkLock::try_acquire`] to refuse waiting.
    ///
    /// # Errors
    ///
    /// Any error of creating, opening or locking the sidecar.
    pub fn acquire(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::lock(path.as_ref(), rustix::fs::FlockOperation::LockExclusive)
    }

    /// Takes the lock for the bookmark file at `path`, or refuses at once
    /// with [`io::ErrorKind::WouldBlock`] while someone else holds it.
    ///
    /// # Errors
    ///
    /// `WouldBlock` for a held lock; otherwise as
    /// [`BookmarkLock::acquire`].
    pub fn try_acquire(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::lock(
            path.as_ref(),
            rustix::fs::FlockOperation::NonBlockingLockExclusive,
        )
    }

    fn lock(path: &Path, operation: rustix::fs::FlockOperation) -> io::Result<Self> {
        let sidecar = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(sidecar_of(path))?;
        rustix::fs::flock(&sidecar, operation)?;
        Ok(Self { _sidecar: sidecar })
    }
}

/// The sidecar's path: the file's name with `.lock` appended.
fn sidecar_of(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(".lock");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_excludes_a_second_acquirer_until_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recently-used.xbel");
        let held = BookmarkLock::try_acquire(&path).unwrap();
        let refused = BookmarkLock::try_acquire(&path).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::WouldBlock);
        drop(held);
        let again = BookmarkLock::try_acquire(&path).unwrap();
        drop(again);
        // The sidecar stays: removing it would race new acquirers.
        assert!(dir.path().join("recently-used.xbel.lock").exists());
        // The bookmark file itself was never touched.
        assert!(!path.exists());
    }

    #[test]
    fn acquiring_after_a_release_does_not_wait() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.xbel");
        drop(BookmarkLock::acquire(&path).unwrap());
        drop(BookmarkLock::acquire(&path).unwrap());
    }
}
