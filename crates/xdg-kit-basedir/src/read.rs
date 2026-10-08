//! Reading a whole configuration or data file.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;

use rustix::fs::{Mode, OFlags};

/// The maximum file size for [`read_regular_file`]: 64 MiB. This limit
/// does not apply to other readers in xdg-kit, such as content sniffing.
pub const MAX_FILE_LEN: u64 = 64 * 1024 * 1024;

/// Reads the regular file at `path`, following symbolic links.
///
/// Rejects non-regular files before opening them to avoid device side
/// effects. Opens the file in non-blocking mode and checks its type again
/// in case it was replaced. This avoids waiting on a FIFO or reading a
/// device such as `/dev/zero` indefinitely. A conflicting file lease,
/// which can occur on network shares, returns
/// [`io::ErrorKind::WouldBlock`] without waiting.
///
/// # Errors
///
/// When the file cannot be opened or read, is not a regular file
/// ([`io::ErrorKind::InvalidInput`]) or is larger than [`MAX_FILE_LEN`]
/// ([`io::ErrorKind::FileTooLarge`]).
pub fn read_regular_file(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    read_at_most(path.as_ref(), MAX_FILE_LEN)
}

fn read_at_most(path: &Path, max_len: u64) -> io::Result<Vec<u8>> {
    let not_regular = || io::Error::new(io::ErrorKind::InvalidInput, "not a regular file");
    if !fs::metadata(path)?.is_file() {
        return Err(not_regular());
    }
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let file = File::from(fd);
    // The path may have been replaced; check the file we actually opened.
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(not_regular());
    }
    let too_large = || io::Error::new(io::ErrorKind::FileTooLarge, "larger than the size limit");
    if metadata.len() > max_len {
        return Err(too_large());
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    // Read one extra byte to detect growth beyond the limit since `fstat`.
    file.take(max_len + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_len {
        return Err(too_large());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn regular_files_and_links_to_them_are_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        fs::write(&path, "contents").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert_eq!(read_regular_file(&path).unwrap(), b"contents");
        assert_eq!(read_regular_file(&link).unwrap(), b"contents");
        let missing = read_regular_file(dir.path().join("missing")).unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    }

    #[cfg(not(target_vendor = "apple"))]
    #[test]
    fn fifos_are_refused_not_waited_on() {
        use crate::testing::{fifo, within};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fifo");
        fifo(&path);
        let read = within(std::time::Duration::from_secs(10), move || {
            read_regular_file(&path).map(|_| ())
        });
        assert_eq!(read.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn folders_and_devices_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        for path in [dir.path(), Path::new("/dev/zero")] {
            let err = read_regular_file(path).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{path:?}");
        }
    }

    #[test]
    fn other_files_are_refused_without_being_opened() {
        // Opening a socket would fail with ENXIO. Refused on its metadata,
        // it is never opened, so the error is InvalidInput.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("socket");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let err = read_regular_file(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
    }

    #[test]
    fn files_larger_than_the_limit_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        fs::write(&path, "1234").unwrap();
        assert_eq!(read_at_most(&path, 4).unwrap(), b"1234");
        let err = read_at_most(&path, 3).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn files_longer_than_their_size_are_refused_past_the_limit() {
        // A regular file whose size says 0 but whose reads give more, as a
        // file that grew after `fstat` would.
        let path = Path::new("/proc/self/status");
        assert_eq!(fs::metadata(path).unwrap().len(), 0);
        let err = read_at_most(path, 8).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);
    }
}
