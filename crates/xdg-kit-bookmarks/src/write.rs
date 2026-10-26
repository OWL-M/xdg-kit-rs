//! Reading from and writing back to the file: the baseline captured at
//! load, the conflict check that refuses stale writes, and the state the
//! object carries forward after a committed write.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use xdg_kit_basedir::{
    BaseDirs, CreateMode, Durability, FileStamp, PreparedWrite, SymlinkPolicy, WriteFileError,
    WriteOptions,
};

use crate::{BookmarkFile, Limits, WriteError, work};

/// What the destination must still look like for a write to commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Baseline {
    /// Parsed from bytes: no file, no write.
    Detached,
    /// The file was absent at load; a write installs without replacing.
    Absent,
    /// The file's identity when exactly these source bytes were read.
    Stamped(FileStamp),
    /// A write committed, but the committed file's identity could not be
    /// read; the next write reports a conflict rather than guess.
    Unknown,
}

/// The canonical empty document: what an absent file reads as, and the
/// frame new entries are inserted into.
const EMPTY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<xbel version="1.0"
      xmlns:bookmark="http://www.freedesktop.org/standards/desktop-bookmarks"
      xmlns:mime="http://www.freedesktop.org/standards/shared-mime-info">
</xbel>
"#;

/// How recently-used data is replaced: recent history is private, so a
/// fresh file is `0600`; an existing file keeps its bits; a symlink means
/// the target is replaced, the link kept.
const OPTIONS: WriteOptions = WriteOptions {
    create: CreateMode::Exact(0o600),
    preserve_mode: true,
    symlinks: SymlinkPolicy::FollowTarget,
};

impl BookmarkFile {
    /// Reads the user's recently-used list,
    /// `$XDG_DATA_HOME/recently-used.xbel`, with the default [`Limits`]. A
    /// missing file loads as an empty document that the first write
    /// creates.
    ///
    /// # Errors
    ///
    /// Any error of reading an existing file, a file over the size limit
    /// included; a missing file is not an error.
    pub fn load_recently_used(base_dirs: &BaseDirs) -> io::Result<Self> {
        Self::load_recently_used_with(base_dirs, &Limits::default())
    }

    /// [`BookmarkFile::load_recently_used`] within `limits`.
    ///
    /// # Errors
    ///
    /// As [`BookmarkFile::load_recently_used`].
    pub fn load_recently_used_with(base_dirs: &BaseDirs, limits: &Limits) -> io::Result<Self> {
        Self::read_with(base_dirs.data_home().join("recently-used.xbel"), limits)
    }

    /// Reads the XBEL file at `path`, with the default [`Limits`]. A
    /// missing file loads as an empty document that the first write
    /// creates.
    ///
    /// # Errors
    ///
    /// As [`BookmarkFile::load_recently_used`].
    pub fn read(path: impl Into<PathBuf>) -> io::Result<Self> {
        Self::read_with(path, &Limits::default())
    }

    /// [`BookmarkFile::read`] within `limits`.
    ///
    /// A relative `path` is anchored to the working directory *now*:
    /// [`BookmarkFile::refresh`] and the writes keep addressing the file
    /// this call read, not whatever the path would name after a
    /// working-directory change.
    ///
    /// # Errors
    ///
    /// As [`BookmarkFile::load_recently_used`].
    pub fn read_with(path: impl Into<PathBuf>, limits: &Limits) -> io::Result<Self> {
        let path = std::path::absolute(path.into())?;
        let (bytes, baseline) = load(&path, limits)?;
        let mut file = Self::from_bytes_with(&bytes, limits);
        file.path = Some(path);
        file.baseline = baseline;
        Ok(file)
    }

    /// Writes the logical document back, atomically, unless someone else
    /// changed the file since it was loaded. The replacement is fully
    /// prepared and synced first; immediately before committing, the
    /// destination's identity **and bytes** are compared against what was
    /// loaded (equal stamps cannot distinguish same-sized writes within
    /// one timestamp tick, so the bytes are always compared). A file that
    /// was absent at load is installed without replacing, so a competing
    /// creator is a conflict too, never silently overwritten. The check
    /// and the rename are still two steps, not one atomic operation:
    /// another writer can slip between them, or overwrite the result
    /// afterwards — [`BookmarkLock`](crate::BookmarkLock) serializes
    /// writers that cooperate.
    ///
    /// After a committed write — [`Durability::NotSynced`] included, since
    /// the replacement happened — the object moves forward: the output
    /// becomes its source, pending edits are cleared, and the committed
    /// file's identity (taken through the written file's own descriptor,
    /// never the pathname) becomes the next baseline. Missing parent
    /// folders are created with mode `0700`. A fresh file gets mode
    /// `0600`: recent history is private data. Only permission bits
    /// survive replacement — ownership, ACLs and extended attributes of
    /// the old file do not.
    ///
    /// # Errors
    ///
    /// [`WriteError::NotEditable`] or [`WriteError::NoFile`] before
    /// anything is prepared; [`WriteError::Conflict`] when the check
    /// fails (the destination is untouched and the pending edits stay —
    /// reload, reapply, retry); [`WriteError::Io`] for everything the
    /// filesystem refuses before a replacement.
    pub fn write(&mut self) -> Result<Durability, WriteError> {
        self.write_checked(true)
    }

    /// Writes like [`BookmarkFile::write`], skipping the baseline check:
    /// the explicitly named last-writer-wins operation. Whatever the file
    /// holds now is replaced (or it is created). The post-write state
    /// advances the same way.
    ///
    /// # Errors
    ///
    /// As [`BookmarkFile::write`], without the conflict outcome.
    pub fn write_overwriting(&mut self) -> Result<Durability, WriteError> {
        self.write_checked(false)
    }

    fn write_checked(&mut self, check: bool) -> Result<Durability, WriteError> {
        if !self.is_editable() {
            return Err(WriteError::NotEditable);
        }
        let Some(path) = self.path.clone() else {
            return Err(WriteError::NoFile);
        };
        if check && self.baseline == Baseline::Absent && self.edits.is_empty() {
            // Nothing to say: an empty document is what a reader of the
            // absent file already sees.
            return Ok(Durability::Synced);
        }
        let bytes = self.to_bytes();
        // The result must obey the document's own limits — otherwise this
        // very object (or the next load) could not read back what it
        // wrote. Checked before anything touches the disk; the parse is
        // reused as the post-commit state.
        if let Some(max) = self.limits.max_file_len
            && bytes.len() as u64 > max
        {
            return Err(WriteError::OverLimits);
        }
        let written = Self::from_bytes_with(&bytes, &self.limits);
        if !written.is_editable() {
            return Err(WriteError::OverLimits);
        }
        let folders = match path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            Some(parent) => {
                xdg_kit_basedir::create_dir_all_synced(parent, 0o700).map_err(WriteError::Io)?
            }
            None => Durability::Synced,
        };
        let prepared = PreparedWrite::new(&path, &bytes, &OPTIONS).map_err(|err| match err {
            WriteFileError::Before(err) => WriteError::Io(err),
            // Links are followed; no-replace comes later.
            other => WriteError::Io(io::Error::other(other.to_string())),
        })?;
        // Revalidation, after all the write latency and immediately
        // before the rename.
        let install_fresh = match (&self.baseline, check) {
            (Baseline::Detached, _) => return Err(WriteError::NoFile),
            (Baseline::Unknown, true) => return Err(WriteError::Conflict),
            (Baseline::Absent, true) => {
                // Follow links, as loading did: a dangling symlink means
                // the target is still absent, and the install replaces
                // the target, not the link.
                if fs::metadata(&path).is_ok() {
                    return Err(WriteError::Conflict);
                }
                true
            }
            (Baseline::Stamped(stamp), true) => {
                self.revalidate(&path, stamp)?;
                false
            }
            // Overwriting: no check; plain rename both creates and
            // replaces.
            (_, false) => false,
        };
        let committed = if install_fresh {
            prepared.commit_unless_exists()
        } else {
            prepared.commit()
        }
        .map_err(|err| match err {
            WriteFileError::Exists => WriteError::Conflict,
            WriteFileError::Before(err) => WriteError::Io(err),
            other => WriteError::Io(io::Error::other(other.to_string())),
        })?;
        // The write happened (durable or not): the object moves forward
        // onto the state parsed from its own output.
        let mut written = written;
        written.path = Some(path);
        written.baseline = committed.stamp.map_or(Baseline::Unknown, Baseline::Stamped);
        *self = written;
        // A new folder whose entry could not be synced leaves the file
        // reachable only until a crash, however the file itself synced.
        Ok(match folders {
            Durability::NotSynced(err) => Durability::NotSynced(err),
            Durability::Synced => committed.durability,
        })
    }

    /// Whether the destination still holds exactly what was loaded.
    /// Equal stamps cannot tell same-sized writes within one timestamp
    /// tick apart, so the bytes are always compared — reading no more
    /// than what could still match.
    fn revalidate(&self, path: &Path, stamp: &FileStamp) -> Result<(), WriteError> {
        // Gone, no longer a regular file (a FIFO put in the way must
        // neither hang this nor pass it), or grown past the loaded size
        // is changed.
        let read = match xdg_kit_basedir::read_stamped(path, Some(self.source.len() as u64)) {
            Ok(Some(read)) => read,
            Ok(None) => return Err(WriteError::Conflict),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::InvalidInput | io::ErrorKind::FileTooLarge
                ) =>
            {
                return Err(WriteError::Conflict);
            }
            Err(err) => return Err(WriteError::Io(err)),
        };
        work::count(read.bytes.len());
        if read.stamp == *stamp && read.bytes == self.source {
            Ok(())
        } else {
            Err(WriteError::Conflict)
        }
    }

    /// A fresh load of the same file, when it changed since this one —
    /// `None` while the file still looks like this document's baseline (a
    /// cheap `stat`), and for a document that has no file. The snapshot
    /// itself never mutates.
    #[must_use]
    pub fn refresh(&self) -> Option<io::Result<Self>> {
        let path = self.path.as_deref()?;
        let unchanged = match &self.baseline {
            Baseline::Detached => return None,
            Baseline::Unknown => false,
            Baseline::Absent => fs::metadata(path).is_err(),
            Baseline::Stamped(stamp) => FileStamp::of(path) == *stamp,
        };
        if unchanged {
            return None;
        }
        Some(Self::read_with(path, &self.limits))
    }
}

/// Reads `path` whole through [`xdg_kit_basedir::read_stamped`], pairing
/// the bytes with the identity of the very file they came from. An absent
/// file is the canonical empty document.
fn load(path: &Path, limits: &Limits) -> io::Result<(Vec<u8>, Baseline)> {
    match xdg_kit_basedir::read_stamped(path, limits.max_file_len)? {
        None => Ok((EMPTY.as_bytes().to_vec(), Baseline::Absent)),
        Some(read) => {
            work::count(read.bytes.len());
            Ok((read.bytes, Baseline::Stamped(read.stamp)))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    use super::*;
    use crate::Opened;

    fn opened(name: &str) -> Opened {
        Opened {
            name: name.to_owned(),
            exec: format!("'{name} %u'"),
            mime: xdg_kit_mime::MimeType::new("text/plain").unwrap(),
            groups: Vec::new(),
            private: false,
            when: crate::time::parse("2024-01-02T03:04:05Z").unwrap(),
        }
    }

    #[test]
    fn a_missing_file_is_created_and_written_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data").join("recently-used.xbel");
        let mut file = BookmarkFile::read(&path).unwrap();
        assert_eq!(file.bookmarks().count(), 0);
        assert!(file.is_editable());
        file.open("file:///a", &opened("viewer")).unwrap();
        assert!(matches!(file.write().unwrap(), Durability::Synced));
        // Private data in a private folder: 0600 in a fresh 0700 parent.
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        // The object moved forward: edit and write again, no reload.
        assert_eq!(file.bookmarks().count(), 1);
        let mut second = opened("viewer");
        second.when += Duration::from_secs(60);
        file.open("file:///a", &second).unwrap();
        file.open("file:///b", &opened("other")).unwrap();
        assert!(matches!(file.write().unwrap(), Durability::Synced));
        let reread = BookmarkFile::read(&path).unwrap();
        assert_eq!(reread.bookmarks().count(), 2);
        let entry = reread.get("file:///a").unwrap();
        assert_eq!(entry.applications()[0].count(), Some(2));
        assert_eq!(entry.visited(), Some(second.when));
    }

    #[test]
    fn a_concurrent_change_is_a_conflict_and_nothing_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recently-used.xbel");
        fs::write(&path, "<xbel>\n</xbel>\n").unwrap();
        let mut file = BookmarkFile::read(&path).unwrap();
        file.open("file:///mine", &opened("viewer")).unwrap();
        // Someone else writes first.
        fs::write(&path, "<xbel><bookmark href=\"theirs\"/></xbel>").unwrap();
        assert!(matches!(file.write(), Err(WriteError::Conflict)));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "<xbel><bookmark href=\"theirs\"/></xbel>",
            "the destination is untouched"
        );
        // The pending edits are preserved: reload, reapply, retry.
        assert!(file.get("file:///mine").is_some());
        let mut reloaded = file.refresh().expect("it changed").unwrap();
        reloaded.open("file:///mine", &opened("viewer")).unwrap();
        assert!(matches!(reloaded.write().unwrap(), Durability::Synced));
        let settled = BookmarkFile::read(&path).unwrap();
        assert!(settled.get("theirs").is_some());
        assert!(settled.get("file:///mine").is_some());
        // A removed file is a change too.
        let mut gone = BookmarkFile::read(&path).unwrap();
        gone.clear().unwrap();
        fs::remove_file(&path).unwrap();
        assert!(matches!(gone.write(), Err(WriteError::Conflict)));
    }

    #[test]
    fn a_competing_creator_is_a_conflict_and_overwriting_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recently-used.xbel");
        let mut file = BookmarkFile::read(&path).unwrap();
        file.open("file:///mine", &opened("viewer")).unwrap();
        fs::write(&path, "<xbel><bookmark href=\"theirs\"/></xbel>").unwrap();
        assert!(matches!(file.write(), Err(WriteError::Conflict)));
        assert!(fs::read_to_string(&path).unwrap().contains("theirs"));
        // The named escape hatch replaces regardless.
        assert!(matches!(
            file.write_overwriting().unwrap(),
            Durability::Synced
        ));
        let settled = BookmarkFile::read(&path).unwrap();
        assert!(settled.get("theirs").is_none());
        assert!(settled.get("file:///mine").is_some());
    }

    #[test]
    fn an_unedited_absent_file_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recently-used.xbel");
        let mut file = BookmarkFile::read(&path).unwrap();
        assert!(matches!(file.write().unwrap(), Durability::Synced));
        assert!(!path.exists(), "an empty list needs no file");
    }

    #[test]
    fn refresh_reloads_only_when_the_file_changed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recently-used.xbel");
        let absent = BookmarkFile::read(&path).unwrap();
        assert!(absent.refresh().is_none(), "still absent");
        fs::write(&path, "<xbel>\n</xbel>\n").unwrap();
        let appeared = absent.refresh().expect("it appeared").unwrap();
        assert!(appeared.refresh().is_none(), "unchanged since");
        fs::write(&path, "<xbel><bookmark href=\"x\"/></xbel>").unwrap();
        let changed = appeared.refresh().expect("it changed").unwrap();
        assert!(changed.get("x").is_some());
    }

    #[test]
    fn a_document_without_a_file_cannot_be_written() {
        let mut file = BookmarkFile::from_bytes(b"<xbel></xbel>");
        file.open("file:///x", &opened("viewer")).unwrap();
        assert!(matches!(file.write(), Err(WriteError::NoFile)));
        assert!(matches!(file.write_overwriting(), Err(WriteError::NoFile)));
    }

    // FIFOs are made with `mknodat`, which rustix does not have on Apple
    // systems.
    #[cfg(not(target_vendor = "apple"))]
    #[test]
    fn a_fifo_is_refused_not_waited_on() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recently-used.xbel");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &path,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        )
        .unwrap();
        // A FIFO without a writer blocks a plain open; loading must not.
        let (sent, got) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sent.send(BookmarkFile::read(&path).map(|_| ()));
        });
        let read = got
            .recv_timeout(Duration::from_secs(10))
            .expect("loading a FIFO must not block");
        assert_eq!(read.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    /// The limit must bound the read itself: a `/proc` file reports a
    /// length of zero and still serves content.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_size_limit_bounds_the_read_not_the_reported_length() {
        let limits = Limits {
            max_file_len: Some(4),
            ..Limits::default()
        };
        let refused = BookmarkFile::read_with("/proc/self/status", &limits).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::FileTooLarge);
    }

    #[test]
    fn a_dangling_symlink_is_an_absent_target_not_a_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recently-used.xbel");
        std::os::unix::fs::symlink("real.xbel", &path).unwrap();
        let mut file = BookmarkFile::read(&path).unwrap();
        assert_eq!(file.bookmarks().count(), 0);
        file.open("file:///a", &opened("viewer")).unwrap();
        assert!(matches!(file.write().unwrap(), Durability::Synced));
        // The link stays; its target now holds the list.
        assert!(fs::symlink_metadata(&path).unwrap().is_symlink());
        let reread = BookmarkFile::read(dir.path().join("real.xbel")).unwrap();
        assert!(reread.get("file:///a").is_some());
    }

    #[test]
    fn a_result_over_the_documents_own_limits_is_refused_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recently-used.xbel");
        let tight = Limits {
            max_file_len: Some(16),
            ..Limits::default()
        };
        let mut small = BookmarkFile::read_with(&path, &tight).unwrap();
        small.open("file:///a", &opened("viewer")).unwrap();
        assert!(matches!(small.write(), Err(WriteError::OverLimits)));
        assert!(!path.exists(), "nothing was written");
        assert!(small.get("file:///a").is_some(), "the edit is kept");
        let shallow = Limits {
            max_depth: 1,
            ..Limits::default()
        };
        let mut flat = BookmarkFile::read_with(&path, &shallow).unwrap();
        assert!(flat.is_editable());
        flat.open("file:///a", &opened("viewer")).unwrap();
        assert!(matches!(flat.write(), Err(WriteError::OverLimits)));
        assert!(!path.exists());
    }

    #[test]
    fn loading_keeps_to_the_size_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recently-used.xbel");
        fs::write(&path, "<xbel>\n</xbel>\n").unwrap();
        let limits = Limits {
            max_file_len: Some(4),
            ..Limits::default()
        };
        let refused = BookmarkFile::read_with(&path, &limits).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::FileTooLarge);
    }
}
