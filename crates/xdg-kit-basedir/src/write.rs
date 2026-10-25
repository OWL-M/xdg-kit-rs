//! Atomic file replacement with explicit policy.
//!
//! The mechanics are the usual safe sequence — a temporary file in the
//! target's folder, written, synced, renamed over the target, the folder
//! synced — with every policy a caller must choose made explicit in
//! [`WriteOptions`]: creation permissions, whether an existing target's
//! bits survive, and what a symlink target means. [`write_regular_file`]
//! does the whole sequence; [`PreparedWrite`] splits it so a caller can
//! validate the destination between preparing and committing.
//!
//! What this does not do: only permission bits carry over — the old
//! file's ownership, ACLs and extended attributes do not survive
//! replacement. Atomic replacement is not conflict detection; a caller
//! that must not overwrite concurrent changes checks the destination
//! itself (see [`PreparedWrite`]) and still accepts the remaining race.
//! Parent folders are the caller's job, since each caller has its own
//! mode policy for them.

use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::FileStamp;

/// The permissions of a newly created target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateMode {
    /// The mode as `open(2)` applies it: filtered by the process umask.
    Masked(u32),
    /// Exactly these bits, set after writing, umask-independent.
    Exact(u32),
}

/// What a target that is a symbolic link means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymlinkPolicy {
    /// Write through the link: the file it names is replaced, the link
    /// stays. Chains are followed to the system's usual limit.
    FollowTarget,
    /// Replace the link itself with a regular file.
    ReplaceLink,
    /// Refuse with [`WriteFileError::SymlinkRefused`].
    Refuse,
}

/// The policy of one atomic replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteOptions {
    /// Permissions for a newly created target.
    pub create: CreateMode,
    /// Keep an existing target's permission bits; otherwise `create`
    /// decides even when replacing.
    pub preserve_mode: bool,
    /// What to do when the target is a symbolic link.
    pub symlinks: SymlinkPolicy,
}

/// Whether a completed replacement is known to survive a crash.
///
/// Both variants mean the new file is in place. [`Durability::NotSynced`]
/// says only the folder sync failed afterwards, with the error: the
/// replacement happened and must not be treated as not done — redoing it
/// would overwrite whatever happened since.
#[derive(Debug)]
#[must_use = "NotSynced means the file was replaced but may not survive a crash"]
pub enum Durability {
    /// The replacement and the folder entry are on disk.
    Synced,
    /// The file was replaced, but syncing the folder failed: the new
    /// entry may not survive a crash.
    NotSynced(io::Error),
}

/// Why a write did not complete.
#[derive(Debug)]
#[non_exhaustive]
pub enum WriteFileError {
    /// Failure before anything was replaced: the target is untouched and
    /// the temporary file is gone.
    Before(io::Error),
    /// [`PreparedWrite::commit_unless_exists`] found the target created
    /// in the meantime; nothing was replaced.
    Exists,
    /// The target is a symbolic link (to this path) and the policy is
    /// [`SymlinkPolicy::Refuse`]; nothing was written.
    SymlinkRefused(PathBuf),
}

impl std::fmt::Display for WriteFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Before(err) => write!(f, "cannot write the file: {err}"),
            Self::Exists => write!(f, "the file was created by someone else meanwhile"),
            Self::SymlinkRefused(path) => {
                write!(f, "{} is a symbolic link", path.display())
            }
        }
    }
}

impl std::error::Error for WriteFileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Before(err) => Some(err),
            Self::Exists | Self::SymlinkRefused(_) => None,
        }
    }
}

/// A completed replacement: the new file is in place.
#[derive(Debug)]
#[must_use = "NotSynced durability means the file may not survive a crash"]
pub struct Committed {
    /// Whether the replacement survives a crash.
    pub durability: Durability,
    /// The committed file's identity: the written file's stamp taken
    /// through its still-open descriptor after the rename, so a later
    /// replacement by another writer is never mistaken for it. `None`
    /// when that `stat` failed; a caller keeping it as a baseline should
    /// then treat the baseline as unknown, which errs toward reporting a
    /// conflict later, never toward missing one.
    pub stamp: Option<FileStamp>,
}

/// Writes `bytes` to `path` atomically under `options`: the whole
/// prepare-and-commit sequence of [`PreparedWrite`] in one call, for
/// callers without a conflict protocol of their own.
///
/// # Errors
///
/// [`WriteFileError::Before`] when nothing was replaced (the target is
/// untouched), or [`WriteFileError::SymlinkRefused`] under that policy.
/// A completed replacement whose folder sync failed is not an error but
/// [`Durability::NotSynced`].
pub fn write_regular_file(
    path: &Path,
    bytes: &[u8],
    options: &WriteOptions,
) -> Result<Committed, WriteFileError> {
    PreparedWrite::new(path, bytes, options)?.commit()
}

/// A replacement that is fully prepared but not yet committed: the
/// temporary file is written, its permissions set, its contents synced.
/// All that remains is the rename, so a caller can validate the
/// destination immediately before [`PreparedWrite::commit`], keeping its
/// own work out of the race window. Dropping it uncommitted removes the
/// temporary file.
#[derive(Debug)]
pub struct PreparedWrite {
    /// The temporary file, open, while not committed.
    file: Option<fs::File>,
    temp: PathBuf,
    target: PathBuf,
    dir: PathBuf,
}

impl PreparedWrite {
    /// Prepares writing `bytes` to `path` under `options`.
    ///
    /// A relative `path` is anchored to the working directory *now*: the
    /// commit replaces the file this call selected, not whatever the
    /// path would name after a working-directory change in between. The
    /// anchor is the absolute pathname, not a directory handle, so a
    /// rename of an ancestor directory between preparing and committing
    /// still redirects the commit — a caller that must survive that holds
    /// the ancestors still itself.
    ///
    /// # Errors
    ///
    /// [`WriteFileError::Before`] for any failure (the target is
    /// untouched and no temporary file remains), or
    /// [`WriteFileError::SymlinkRefused`] under that policy.
    pub fn new(path: &Path, bytes: &[u8], options: &WriteOptions) -> Result<Self, WriteFileError> {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        // Anchored before anything is created, so the temporary file and
        // the rename can never disagree about which directory they are in.
        let path = std::path::absolute(path).map_err(WriteFileError::Before)?;
        let path = path.as_path();
        let target = match options.symlinks {
            SymlinkPolicy::FollowTarget => follow(path).map_err(WriteFileError::Before)?,
            SymlinkPolicy::ReplaceLink => path.to_owned(),
            SymlinkPolicy::Refuse => match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(WriteFileError::SymlinkRefused(path.to_owned()));
                }
                _ => path.to_owned(),
            },
        };
        // The mode the finished file must have, when one must be set
        // after writing; `Masked` leaves the kernel's umask filtering as
        // the final word.
        let created = || match options.create {
            CreateMode::Exact(mode) => Some(mode),
            CreateMode::Masked(_) => None,
        };
        let exact = match fs::symlink_metadata(&target) {
            Ok(metadata) if options.preserve_mode && !metadata.file_type().is_symlink() => {
                Some(metadata.permissions().mode() & 0o7777)
            }
            Ok(_) => created(),
            Err(err) if err.kind() == io::ErrorKind::NotFound => created(),
            // Whether the target exists cannot be told, so neither can
            // the mode the policy asks for.
            Err(err) => return Err(WriteFileError::Before(err)),
        };
        let open_mode = exact.unwrap_or(match options.create {
            CreateMode::Masked(mode) | CreateMode::Exact(mode) => mode,
        });
        let name = target
            .file_name()
            .ok_or_else(|| {
                WriteFileError::Before(io::Error::new(io::ErrorKind::InvalidInput, "no file name"))
            })?
            .to_string_lossy()
            .into_owned();
        // Short enough that the temporary name stays within NAME_MAX.
        let mut end = name.len().min(64);
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        let stem = &name[..end];
        let dir = target
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_owned();
        let (temp, mut file) = loop {
            let count = COUNTER.fetch_add(1, Ordering::Relaxed);
            let temp = dir.join(format!(".{stem}.{}.{count}.tmp", std::process::id()));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(open_mode)
                .open(&temp)
            {
                Ok(file) => break (temp, file),
                // A name left over from a crashed writer is skipped.
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(WriteFileError::Before(err)),
            }
        };
        let prepared = (|| {
            file.write_all(bytes)?;
            if let Some(mode) = exact {
                // The umask may have narrowed the mode at creation.
                file.set_permissions(fs::Permissions::from_mode(mode))?;
            }
            file.sync_all()
        })();
        if let Err(err) = prepared {
            let _ = fs::remove_file(&temp);
            return Err(WriteFileError::Before(err));
        }
        Ok(Self {
            file: Some(file),
            temp,
            target,
            dir,
        })
    }

    /// Replaces the target.
    ///
    /// # Errors
    ///
    /// [`WriteFileError::Before`] when the rename failed: nothing was
    /// replaced and the temporary file is gone. A failure of the folder
    /// sync afterwards is [`Durability::NotSynced`], not an error — the
    /// replacement happened.
    pub fn commit(self) -> Result<Committed, WriteFileError> {
        self.rename(|temp, target| fs::rename(temp, target), false)
    }

    /// Replaces the target only if nothing exists there: the atomic
    /// installation for a file believed absent, so a competing creator is
    /// never silently overwritten.
    ///
    /// # Errors
    ///
    /// As [`PreparedWrite::commit`], plus [`WriteFileError::Exists`] when
    /// the target came to exist in the meantime.
    pub fn commit_unless_exists(self) -> Result<Committed, WriteFileError> {
        self.rename(rename_noreplace, true)
    }

    fn rename(
        mut self,
        rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
        exists_is_race: bool,
    ) -> Result<Committed, WriteFileError> {
        match rename(&self.temp, &self.target) {
            Ok(()) => {}
            Err(err) => {
                let _ = fs::remove_file(&self.temp);
                // `Drop` has nothing left to remove.
                self.temp.clear();
                return Err(
                    if exists_is_race && err.kind() == io::ErrorKind::AlreadyExists {
                        WriteFileError::Exists
                    } else {
                        WriteFileError::Before(err)
                    },
                );
            }
        }
        self.temp.clear();
        // The descriptor stayed open through the rename, so this is the
        // committed file's identity, with the status change the rename
        // itself made — never a later replacement's.
        let stamp = self
            .file
            .take()
            .and_then(|file| FileStamp::of_file(&file).ok());
        // The rename itself is made durable by syncing the folder.
        let durability = match fs::File::open(&self.dir).and_then(|dir| dir.sync_all()) {
            Ok(()) => Durability::Synced,
            Err(err) => Durability::NotSynced(err),
        };
        Ok(Committed { durability, stamp })
    }
}

impl Drop for PreparedWrite {
    fn drop(&mut self) {
        if !self.temp.as_os_str().is_empty() {
            let _ = fs::remove_file(&self.temp);
        }
    }
}

/// Renames `temp` to `target` only if nothing exists there:
/// `renameat2(2)` with `RENAME_NOREPLACE` (`RENAME_EXCL` on Apple
/// systems), where rustix has it.
#[cfg(any(
    target_vendor = "apple",
    target_os = "linux",
    target_os = "android",
    target_os = "redox"
))]
fn rename_noreplace(temp: &Path, target: &Path) -> io::Result<()> {
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        temp,
        rustix::fs::CWD,
        target,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(Into::into)
}

/// Renames `temp` to `target` only if nothing exists there, on systems
/// without a no-replace rename: a hard link never replaces (`EEXIST` when
/// the target exists), and the temporary name is dropped afterwards. If
/// that last unlink fails, the installed file simply keeps a second name
/// until the next write's cleanup; the installation itself succeeded.
#[cfg(not(any(
    target_vendor = "apple",
    target_os = "linux",
    target_os = "android",
    target_os = "redox"
)))]
fn rename_noreplace(temp: &Path, target: &Path) -> io::Result<()> {
    fs::hard_link(temp, target)?;
    let _ = fs::remove_file(temp);
    Ok(())
}

/// Most links [`SymlinkPolicy::FollowTarget`] follows when resolving the
/// path to write: the system's `MAXSYMLINKS`, which the `libc` crate
/// defines for FreeBSD (32) and AIX (20).
#[cfg(any(target_os = "freebsd", target_os = "aix"))]
const MAX_LINKS: usize = libc::MAXSYMLINKS.unsigned_abs() as usize;

/// Most links [`SymlinkPolicy::FollowTarget`] follows when resolving the
/// path to write: the Linux kernel's `MAXSYMLINKS` (`fs/namei.c`), which
/// the `libc` crate does not define for Linux or Android.
#[cfg(any(target_os = "linux", target_os = "android"))]
const MAX_LINKS: usize = 40;

/// Most links [`SymlinkPolicy::FollowTarget`] follows when resolving the
/// path to write (NetBSD, OpenBSD, `DragonFly`, illumos, Solaris, Apple
/// systems, …): 40, as on Linux, a fallback not verified on those systems
/// that may differ from their own limit.
#[cfg(not(any(
    target_os = "freebsd",
    target_os = "aix",
    target_os = "linux",
    target_os = "android"
)))]
const MAX_LINKS: usize = 40;

/// The file `path` names once its trailing symbolic links are followed.
fn follow(path: &Path) -> io::Result<PathBuf> {
    let mut current = path.to_owned();
    let mut followed = 0usize;
    loop {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                if followed == MAX_LINKS {
                    return Err(rustix::io::Errno::LOOP.into());
                }
                followed += 1;
                let link = fs::read_link(&current)?;
                current = match current.parent() {
                    Some(dir) if link.is_relative() => dir.join(link),
                    _ => link,
                };
            }
            _ => return Ok(current),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    fn no_temp_left(dir: &Path) {
        let left: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().ends_with(".tmp"))
            .collect();
        assert_eq!(left, Vec::<std::ffi::OsString>::new());
    }

    fn committed(result: Result<Committed, WriteFileError>) {
        assert!(matches!(result.unwrap().durability, Durability::Synced));
    }

    const FOLLOW: WriteOptions = WriteOptions {
        create: CreateMode::Exact(0o644),
        preserve_mode: true,
        symlinks: SymlinkPolicy::FollowTarget,
    };

    #[test]
    fn modes_follow_the_policy_against_and_with_the_umask() {
        // The umask is process-global, so every assertion that depends on
        // it lives in this one test.
        let dir = tempfile::tempdir().unwrap();
        let before = rustix::process::umask(rustix::fs::Mode::from_bits_truncate(0o077));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let exact = dir.path().join("exact");
            committed(write_regular_file(
                &exact,
                b"x",
                &WriteOptions {
                    create: CreateMode::Exact(0o644),
                    preserve_mode: false,
                    symlinks: SymlinkPolicy::Refuse,
                },
            ));
            assert_eq!(mode_of(&exact), 0o644, "exact defeats the umask");
            let masked = dir.path().join("masked");
            committed(write_regular_file(
                &masked,
                b"x",
                &WriteOptions {
                    create: CreateMode::Masked(0o666),
                    preserve_mode: false,
                    symlinks: SymlinkPolicy::Refuse,
                },
            ));
            assert_eq!(mode_of(&masked), 0o600, "masked honors the umask");
        }));
        rustix::process::umask(before);
        outcome.unwrap();
        no_temp_left(dir.path());
    }

    #[test]
    fn an_existing_targets_bits_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        committed(write_regular_file(&path, b"new", &FOLLOW));
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(mode_of(&path), 0o640);
    }

    #[test]
    fn follow_target_writes_through_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real");
        let link = dir.path().join("link");
        fs::write(&target, "old").unwrap();
        symlink("real", &link).unwrap();
        committed(write_regular_file(&link, b"new", &FOLLOW));
        assert!(fs::symlink_metadata(&link).unwrap().is_symlink(), "stays");
        assert_eq!(fs::read(&target).unwrap(), b"new");
    }

    #[test]
    fn replace_link_replaces_the_link_itself() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real");
        let link = dir.path().join("link");
        fs::write(&target, "old").unwrap();
        symlink("real", &link).unwrap();
        committed(write_regular_file(
            &link,
            b"new",
            &WriteOptions {
                create: CreateMode::Masked(0o666),
                preserve_mode: false,
                symlinks: SymlinkPolicy::ReplaceLink,
            },
        ));
        assert!(fs::symlink_metadata(&link).unwrap().is_file(), "replaced");
        assert_eq!(fs::read(&target).unwrap(), b"old", "untouched");
    }

    #[test]
    fn refuse_rejects_a_link_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real");
        let link = dir.path().join("link");
        fs::write(&target, "old").unwrap();
        symlink("real", &link).unwrap();
        let refused = write_regular_file(
            &link,
            b"new",
            &WriteOptions {
                create: CreateMode::Exact(0o644),
                preserve_mode: true,
                symlinks: SymlinkPolicy::Refuse,
            },
        );
        assert!(matches!(refused, Err(WriteFileError::SymlinkRefused(_))));
        assert_eq!(fs::read(&target).unwrap(), b"old");
        no_temp_left(dir.path());
    }

    #[test]
    fn commit_unless_exists_loses_a_race_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        let prepared = PreparedWrite::new(&path, b"mine", &FOLLOW).unwrap();
        fs::write(&path, "theirs").unwrap();
        assert!(matches!(
            prepared.commit_unless_exists(),
            Err(WriteFileError::Exists)
        ));
        assert_eq!(fs::read(&path).unwrap(), b"theirs");
        no_temp_left(dir.path());
    }

    #[test]
    fn dropping_uncommitted_cleans_the_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let prepared = PreparedWrite::new(&dir.path().join("f"), b"x", &FOLLOW).unwrap();
        drop(prepared);
        no_temp_left(dir.path());
        assert!(!dir.path().join("f").exists());
    }

    // Only where the limit is known: on other systems `MAX_LINKS` is an
    // unverified fallback.
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "aix"
    ))]
    /// Makes `links` symbolic links in `dir`, each to the next, the last
    /// to `real`, which holds `old`. Returns the first link.
    fn link_chain(dir: &Path, links: usize) -> PathBuf {
        fs::write(dir.join("real"), "old").unwrap();
        let mut next = PathBuf::from("real");
        for at in (0..links).rev() {
            let link = PathBuf::from(format!("l{at}"));
            symlink(&next, dir.join(&link)).unwrap();
            next = link;
        }
        dir.join(next)
    }

    // Only where the limit is known: on other systems `MAX_LINKS` is an
    // unverified fallback.
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "aix"
    ))]
    #[test]
    fn writing_follows_as_many_links_as_reading() {
        // The folder without links on the way, so that only the chain counts
        // toward the system's limit when reading.
        let folder = |dir: &tempfile::TempDir| dir.path().canonicalize().unwrap();
        for links in [MAX_LINKS - 1, MAX_LINKS] {
            let dir = tempfile::tempdir().unwrap();
            let first = link_chain(&folder(&dir), links);
            fs::read_to_string(&first).unwrap();
            committed(write_regular_file(&first, b"new", &FOLLOW));
            assert!(fs::symlink_metadata(&first).unwrap().is_symlink());
            assert_eq!(
                fs::read_to_string(dir.path().join("real")).unwrap(),
                "new",
                "{links} links"
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let first = link_chain(&folder(&dir), MAX_LINKS + 1);
        let read = fs::read_to_string(&first).unwrap_err();
        let Err(WriteFileError::Before(write)) = write_regular_file(&first, b"new", &FOLLOW) else {
            panic!("one more link must fail as reading does");
        };
        assert_eq!(
            read.raw_os_error(),
            Some(rustix::io::Errno::LOOP.raw_os_error())
        );
        assert_eq!(
            (write.kind(), write.raw_os_error()),
            (read.kind(), read.raw_os_error())
        );
        assert_eq!(fs::read_to_string(dir.path().join("real")).unwrap(), "old");
    }

    #[test]
    fn the_committed_stamp_is_the_committed_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        fs::write(&path, "old").unwrap();
        let committed = PreparedWrite::new(&path, b"new", &FOLLOW)
            .unwrap()
            .commit()
            .unwrap();
        assert!(matches!(committed.durability, Durability::Synced));
        assert_eq!(FileStamp::of(&path), committed.stamp.unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"new");
        no_temp_left(dir.path());
    }
}
