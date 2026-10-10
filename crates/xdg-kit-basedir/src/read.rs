//! Reading a whole configuration or data file.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;

use rustix::fs::{Mode, OFlags};

/// The maximum file size for [`read_regular_file`], and the default of
/// [`Limits::max_file_len`]: 64 MiB. This limit does not apply to other
/// readers in xdg-kit, such as content sniffing.
pub const MAX_FILE_LEN: u64 = 64 * 1024 * 1024;

/// Limits on what this crate reads. Functions without `_with` use
/// [`Limits::default`]; `None` means no limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// The largest file read, in bytes: [`MAX_FILE_LEN`] by default. With
    /// `None`, any size is allowed and memory use grows with the file.
    ///
    /// Failure to reserve memory returns [`io::ErrorKind::OutOfMemory`], with
    /// or without a limit. On systems that overcommit memory, such as Linux, a
    /// reservation may succeed even when there is too little free memory.
    /// Reading the file may still exhaust memory.
    pub max_file_len: Option<u64>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_len: Some(MAX_FILE_LEN),
        }
    }
}

impl Limits {
    /// No limit at all: memory grows with the files read.
    #[must_use]
    pub fn unlimited() -> Self {
        Self { max_file_len: None }
    }
}

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
    read_regular_file_with(path, Some(MAX_FILE_LEN))
}

/// Reads a file as [`read_regular_file`] does, rejecting files larger than
/// `max_len` bytes. With `None`, any size is allowed and memory use grows with
/// the file.
///
/// # Errors
///
/// As [`read_regular_file`], using `max_len` as the size limit. Returns
/// [`io::ErrorKind::OutOfMemory`] if memory cannot be reserved, including when
/// the file grows during a read. On systems that overcommit memory, such as
/// Linux, a reservation may succeed even when there is too little free memory.
/// Reading the file may still exhaust memory; a limit above the available
/// memory does not prevent this.
pub fn read_regular_file_with(path: impl AsRef<Path>, max_len: Option<u64>) -> io::Result<Vec<u8>> {
    read_at_most(path.as_ref(), max_len)
}

fn read_at_most(path: &Path, max_len: Option<u64>) -> io::Result<Vec<u8>> {
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
    let Some(max_len) = max_len else {
        let mut bytes = buffer(metadata.len())?;
        read_rest(&mut &file, &mut bytes, grow)?;
        return Ok(bytes);
    };
    if metadata.len() > max_len {
        return Err(too_large());
    }
    let mut bytes = buffer(metadata.len())?;
    // Read one extra byte to detect growth beyond the limit since `fstat`.
    read_rest(&mut file.take(max_len.saturating_add(1)), &mut bytes, grow)?;
    if bytes.len() as u64 > max_len {
        return Err(too_large());
    }
    Ok(bytes)
}

/// Appends data from `reader` to `bytes` until EOF, retrying reads interrupted
/// by a signal. All buffer growth goes through `reserve` ([`grow`] outside
/// tests). Unlike `read_to_end`, this returns an allocation error instead of
/// aborting if the file grows beyond what can be reserved.
///
/// # Errors
///
/// When reading or `reserve` fails.
fn read_rest(
    reader: &mut impl Read,
    bytes: &mut Vec<u8>,
    reserve: impl Fn(&mut Vec<u8>, usize) -> io::Result<()>,
) -> io::Result<()> {
    let mut chunk = [0; 16 * 1024];
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        reserve(bytes, read)?;
        bytes.extend_from_slice(&chunk[..read]);
    }
}

/// Reserves space in `bytes` for `more` bytes, as `Vec::reserve` would.
///
/// # Errors
///
/// [`io::ErrorKind::OutOfMemory`] if the space cannot be reserved.
fn grow(bytes: &mut Vec<u8>, more: usize) -> io::Result<()> {
    bytes.try_reserve(more).map_err(|_| out_of_memory())
}

/// The error returned when memory for a file cannot be reserved.
fn out_of_memory() -> io::Error {
    io::Error::new(io::ErrorKind::OutOfMemory, "too large to hold in memory")
}

/// Creates an empty buffer with capacity for `len` bytes.
///
/// # Errors
///
/// [`io::ErrorKind::OutOfMemory`] if the space cannot be reserved.
fn buffer(len: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    usize::try_from(len)
        .ok()
        .and_then(|len| bytes.try_reserve_exact(len).ok())
        .ok_or_else(out_of_memory)?;
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
        assert_eq!(read_at_most(&path, Some(4)).unwrap(), b"1234");
        let err = read_at_most(&path, Some(3)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);
    }

    /// A sparse file of `len` bytes, on a file system that allows it
    /// (`tmpfs` in `/dev/shm` allows any `i64` length), if one does.
    fn sparse(len: u64) -> Option<(tempfile::TempDir, std::path::PathBuf)> {
        let dir = tempfile::tempdir_in("/dev/shm")
            .or_else(|_| tempfile::tempdir())
            .ok()?;
        let path = dir.path().join("sparse");
        fs::File::create(&path).ok()?.set_len(len).ok()?;
        Some((dir, path))
    }

    /// Larger than any address space, so no buffer can hold it, regardless of
    /// the machine's memory.
    const UNREADABLE_LEN: u64 = 1 << 60;

    // The tests below exercise the loop with custom readers. They deliberately
    // leave its call sites untested: proving that a call cannot abort would
    // require a file to grow beyond reservable memory while being read.

    /// The bytes each read of a [`Drip`] gives.
    const DRIP: usize = 7;

    /// Gives `left` bytes, [`DRIP`] at a time, after failing once with
    /// `hiccup`, if any.
    struct Drip {
        left: usize,
        hiccup: Option<io::ErrorKind>,
    }

    impl Drip {
        fn new(left: usize, hiccup: Option<io::ErrorKind>) -> Self {
            Self { left, hiccup }
        }
    }

    impl Read for Drip {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if let Some(kind) = self.hiccup.take() {
                return Err(io::Error::from(kind));
            }
            let n = self.left.min(buf.len()).min(DRIP);
            buf[..n].fill(b'x');
            self.left -= n;
            Ok(n)
        }
    }

    /// [`grow`], checked to have made room for the [`DRIP`] bytes just
    /// read.
    fn checked_grow(bytes: &mut Vec<u8>, more: usize) -> io::Result<()> {
        grow(bytes, more)?;
        let room = bytes.capacity() - bytes.len();
        assert!(room >= DRIP, "room for {room} bytes, {DRIP} read");
        Ok(())
    }

    /// A whole number of reads.
    const DRIPPED: usize = 14_286 * DRIP;

    #[test]
    fn a_reader_giving_more_than_reserved_is_read_whole() {
        let mut bytes = buffer(4).unwrap();
        read_rest(&mut Drip::new(DRIPPED, None), &mut bytes, checked_grow).unwrap();
        assert_eq!(bytes, vec![b'x'; DRIPPED]);
    }

    #[test]
    fn an_interrupted_read_is_made_again() {
        let mut bytes = buffer(4).unwrap();
        let mut reader = Drip::new(DRIPPED, Some(io::ErrorKind::Interrupted));
        read_rest(&mut reader, &mut bytes, checked_grow).unwrap();
        assert_eq!(bytes, vec![b'x'; DRIPPED]);
    }

    #[test]
    fn a_failed_read_is_an_error() {
        let mut bytes = buffer(4).unwrap();
        let mut reader = Drip::new(DRIPPED, Some(io::ErrorKind::PermissionDenied));
        let err = read_rest(&mut reader, &mut bytes, checked_grow).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(bytes.is_empty());
    }

    #[test]
    fn growth_that_cannot_be_reserved_is_an_error_not_an_abort() {
        // Past 64 bytes, each growth asks for more than any address space.
        let greedy = |bytes: &mut Vec<u8>, more: usize| {
            grow(bytes, if bytes.len() < 64 { more } else { usize::MAX })
        };
        let mut bytes = buffer(4).unwrap();
        let err = read_rest(&mut Drip::new(DRIPPED, None), &mut bytes, greedy).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::OutOfMemory);
        assert!(bytes.len() <= 64 + DRIP, "{}", bytes.len());
    }

    #[test]
    fn a_file_too_large_for_memory_is_an_error_not_an_abort() {
        let err = buffer(UNREADABLE_LEN).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::OutOfMemory);
        let Some((_dir, path)) = sparse(UNREADABLE_LEN) else {
            eprintln!("file part skipped: no file system here takes a sparse file of 2^60 bytes");
            return;
        };
        let err = read_regular_file_with(&path, None).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::OutOfMemory);
        // The limit refuses it first.
        let err = read_regular_file(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);
    }

    #[test]
    fn the_size_limit_can_be_set_or_lifted() {
        assert_eq!(Limits::default().max_file_len, Some(MAX_FILE_LEN));
        assert_eq!(Limits::unlimited().max_file_len, None);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        fs::write(&path, "1234").unwrap();
        assert_eq!(read_regular_file_with(&path, Some(4)).unwrap(), b"1234");
        let err = read_regular_file_with(&path, Some(3)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);
        // Past the default: refused by it, read without a limit.
        let large = dir.path().join("large");
        fs::File::create(&large)
            .unwrap()
            .set_len(MAX_FILE_LEN + 1)
            .unwrap();
        let err = read_regular_file(&large).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);
        let read = read_regular_file_with(&large, None).unwrap();
        assert_eq!(read.len() as u64, MAX_FILE_LEN + 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn files_longer_than_their_size_are_refused_past_the_limit() {
        // A regular file whose size says 0 but whose reads give more, as a
        // file that grew after `fstat` would.
        let path = Path::new("/proc/self/status");
        assert_eq!(fs::metadata(path).unwrap().len(), 0);
        let err = read_at_most(path, Some(8)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);
    }
}
