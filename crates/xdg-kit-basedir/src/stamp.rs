//! File metadata for detecting later changes.

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::SystemTime;

/// A file's modification time, status change time, size, device and inode,
/// as reported by `stat` (following links). Comparing stamps detects changes
/// to the file. All `stat` errors produce the same stamp, including a missing
/// path or a parent folder that cannot be searched. A file appearing or
/// disappearing therefore changes its stamp too.
///
/// The status change time also detects changes that leave the contents
/// alone, such as `chmod` or `chown`, which can change whether a file is
/// readable, or another file renamed over this one. Equal stamps mean the
/// file looks unchanged. On a filesystem with coarse timestamps, two writes
/// within one tick that keep the same size cannot be told apart.
///
/// ```
/// use xdg_kit_basedir::FileStamp;
///
/// let folder = tempfile::tempdir()?;
/// let file = folder.path().join("settings.list");
/// let before = FileStamp::of(&file);
/// std::fs::write(&file, "x")?;
/// assert_ne!(FileStamp::of(&file), before, "it appeared");
/// assert_eq!(FileStamp::of(&file), FileStamp::of(&file));
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStamp(Option<Parts>);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Parts {
    modified: Option<SystemTime>,
    /// Seconds and nanoseconds.
    changed: (i64, i64),
    len: u64,
    dev: u64,
    ino: u64,
}

impl FileStamp {
    /// The stamp of `path` now.
    #[must_use]
    pub fn of(path: impl AsRef<Path>) -> Self {
        Self(fs::metadata(path).ok().map(|metadata| Parts::of(&metadata)))
    }

    /// The stamp of an opened file. Unlike [`FileStamp::of`], this reports
    /// a `stat` failure instead of folding it into absence, and the stamp
    /// belongs to the file that was opened — not to whatever its path
    /// names later. Use it to build a baseline for detecting changes to
    /// what was actually read.
    ///
    /// # Errors
    ///
    /// Any error of reading the open file's metadata.
    pub fn of_file(file: &fs::File) -> io::Result<Self> {
        Ok(Self(Some(Parts::of(&file.metadata()?))))
    }
}

impl Parts {
    fn of(metadata: &fs::Metadata) -> Self {
        Self {
            modified: metadata.modified().ok(),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
            len: metadata.len(),
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn changes_are_seen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        let absent = FileStamp::of(&path);
        assert_eq!(absent, FileStamp::of(&path));
        fs::write(&path, "a").unwrap();
        let written = FileStamp::of(&path);
        assert_ne!(written, absent, "it appeared");
        assert_eq!(written, FileStamp::of(&path), "untouched");
        fs::write(&path, "ab").unwrap();
        let grown = FileStamp::of(&path);
        assert_ne!(grown, written, "the contents changed");
        // Only the status change time tells this one.
        change_mode_until_ctime_moves(&path);
        let mode = FileStamp::of(&path);
        assert_ne!(mode, grown, "the mode changed");
        let other = dir.path().join("g");
        fs::write(&other, "ab").unwrap();
        fs::rename(&other, &path).unwrap();
        assert_ne!(FileStamp::of(&path), mode, "replaced by another file");
        fs::remove_file(&path).unwrap();
        assert_eq!(FileStamp::of(&path), absent, "gone");
    }

    #[test]
    fn a_files_stamp_matches_its_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        fs::write(&path, "a").unwrap();
        let file = fs::File::open(&path).unwrap();
        let of_file = FileStamp::of_file(&file).unwrap();
        assert_eq!(of_file, FileStamp::of(&path));
        // The stamp follows the opened file when the path moves on (the
        // replaced file's own stamp moves too: losing its name is a
        // status change).
        let other = dir.path().join("g");
        fs::write(&other, "bb").unwrap();
        fs::rename(&other, &path).unwrap();
        assert_ne!(FileStamp::of(&path), FileStamp::of_file(&file).unwrap());
        assert_ne!(FileStamp::of(&path), of_file);
    }

    /// Flips a permission bit of `path` until `stat` shows a new status
    /// change time. On a filesystem with coarse timestamps, this takes
    /// until the next tick. Gives up after about two seconds.
    fn change_mode_until_ctime_moves(path: &Path) {
        let before = fs::metadata(path).unwrap();
        let ctime = (before.ctime(), before.ctime_nsec());
        let mut mode = before.permissions().mode();
        for _ in 0..200 {
            mode ^= 0o004;
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
            let now = fs::metadata(path).unwrap();
            if (now.ctime(), now.ctime_nsec()) != ctime {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the status change time of {} did not move", path.display());
    }
}
