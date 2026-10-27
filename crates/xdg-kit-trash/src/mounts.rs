//! The mounts of this process, for finding the top directories whose trash
//! directories a listing should include. Separate from every trash
//! operation: callers pass the top directories they want.

use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;

/// One line of `/proc/self/mountinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Mount {
    /// The mount's ID.
    pub id: u64,
    /// Its parent's ID.
    pub parent: u64,
    /// The directory it is mounted on: a top directory.
    pub mount_point: PathBuf,
    /// The file system type (`ext4`, `nfs4`, `fuse.sshfs`, …).
    pub fs_type: String,
    /// The mount source (a device, a remote, or anything).
    pub source: PathBuf,
}

/// Why the mount list could not be read: a partial list is never returned.
#[derive(Debug)]
#[non_exhaustive]
pub enum MountsError {
    /// Reading failed.
    Io(io::Error),
    /// The file is larger than
    /// [`Limits::max_mountinfo_len`](crate::Limits::max_mountinfo_len).
    TooLarge,
    /// A line is malformed (its number, from 1).
    Malformed(usize),
}

impl fmt::Display for MountsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "cannot read the mount list: {err}"),
            Self::TooLarge => f.write_str("the mount list is larger than the limit"),
            Self::Malformed(line) => write!(f, "line {line} of the mount list is malformed"),
        }
    }
}

impl std::error::Error for MountsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::TooLarge | Self::Malformed(_) => None,
        }
    }
}

/// Parses the format of `/proc/<pid>/mountinfo` (Linux's
/// `proc_pid_mountinfo(5)`): space-separated fields, optional fields up to
/// a lone `-`, and `\ooo` octal escapes in paths. Any malformed line fails
/// the whole list.
///
/// # Errors
///
/// [`MountsError::Malformed`] with the first bad line's number.
pub fn parse_mountinfo(bytes: &[u8]) -> Result<Vec<Mount>, MountsError> {
    let mut mounts = Vec::new();
    for (index, line) in bytes.split(|&byte| byte == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let bad = || MountsError::Malformed(index + 1);
        let fields: Vec<&[u8]> = line.split(|&byte| byte == b' ').collect();
        let separator = fields
            .iter()
            .skip(6)
            .position(|&field| field == b"-")
            .ok_or_else(bad)?
            + 6;
        let number = |field: &[u8]| std::str::from_utf8(field).ok()?.parse::<u64>().ok();
        let (Some(id), Some(parent), Some(mount_point), Some(fs_type), Some(source)) = (
            fields.first().copied().and_then(number),
            fields.get(1).copied().and_then(number),
            fields.get(4).copied().and_then(unescape),
            fields.get(separator + 1).copied().and_then(unescape),
            fields.get(separator + 2).copied().and_then(unescape),
        ) else {
            return Err(bad());
        };
        if mount_point.first() != Some(&b'/') {
            return Err(bad());
        }
        mounts.push(Mount {
            id,
            parent,
            mount_point: OsStr::from_bytes(&mount_point).into(),
            fs_type: String::from_utf8(fs_type).map_err(|_| bad())?,
            source: OsStr::from_bytes(&source).into(),
        });
    }
    Ok(mounts)
}

/// Undoes `\ooo` escapes (three octal digits).
fn unescape(field: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(field.len());
    let mut at = 0;
    while let Some(&byte) = field.get(at) {
        if byte == b'\\' {
            let digits = field.get(at + 1..at + 4)?;
            let mut value = 0u16;
            for &digit in digits {
                if !(b'0'..=b'7').contains(&digit) {
                    return None;
                }
                value = value * 8 + u16::from(digit - b'0');
            }
            out.push(u8::try_from(value).ok()?);
            at += 4;
        } else {
            out.push(byte);
            at += 1;
        }
    }
    Some(out)
}

/// The mounts of this process, from `/proc/self/mountinfo`, at most
/// `limits.max_mountinfo_len` bytes of it. Linux and Android only: other
/// systems have no such file, and callers there supply top directories
/// themselves.
///
/// # Errors
///
/// When the file cannot be read in full, or a line is malformed.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn mounts(limits: &crate::Limits) -> Result<Vec<Mount>, MountsError> {
    use std::io::Read as _;
    // `/proc` files report no size: read up to one byte past the limit.
    let file = std::fs::File::open("/proc/self/mountinfo").map_err(MountsError::Io)?;
    let mut bytes = Vec::new();
    file.take(limits.max_mountinfo_len.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(MountsError::Io)?;
    if bytes.len() as u64 > limits.max_mountinfo_len {
        return Err(MountsError::TooLarge);
    }
    parse_mountinfo(&bytes)
}
