//! The trash info file: `$trash/info/<name>.trashinfo`.
//!
//! Its first line must be `[Trash Info]`. After it, the first line starting
//! with the key `Path` and the first starting with `DeletionDate` are the
//! only ones read; every other line is ignored, as the spec requires, so
//! a later group header has no effect and a later copy of a key never
//! stands in for a first one that is malformed.

use std::ffi::OsStr;
use std::fmt;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use crate::encode::{self, EscapeProblem};
use crate::time::DeletionDate;

/// The first line of every trash info file.
pub(crate) const HEADER: &[u8] = b"[Trash Info]";

/// The name suffix of trash info files.
pub(crate) const SUFFIX: &[u8] = b".trashinfo";

/// A trash info file as read: its two keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashInfo {
    path: Field<Vec<u8>>,
    deletion_date: Field<DeletionDate>,
}

/// A key's value as read: the first occurrence decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field<T> {
    /// The first occurrence, decoded.
    Valid(T),
    /// No line has the key.
    Missing,
    /// The first occurrence cannot be decoded.
    Invalid(FieldProblem),
}

impl<T> Field<T> {
    /// The value, when valid.
    pub fn valid(&self) -> Option<&T> {
        match self {
            Self::Valid(value) => Some(value),
            Self::Missing | Self::Invalid(_) => None,
        }
    }
}

/// Why a key's first occurrence was not usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FieldProblem {
    /// A string or URL escape is malformed.
    Escape(EscapeProblem),
    /// The `Path` value is empty.
    Empty,
    /// The `DeletionDate` value is not `YYYY-MM-DDThh:mm:ss` with a valid
    /// date and time.
    Date,
}

impl fmt::Display for FieldProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Escape(problem) => problem.fmt(f),
            Self::Empty => f.write_str("the value is empty"),
            Self::Date => f.write_str("the value is not a date and time YYYY-MM-DDThh:mm:ss"),
        }
    }
}

/// The first line is not `[Trash Info]`: the file is not trash info.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HeaderProblem;

impl fmt::Display for HeaderProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the first line is not [Trash Info]")
    }
}

impl std::error::Error for HeaderProblem {}

impl TrashInfo {
    /// Reads a trash info file's bytes. Lines end at `\n`; a `\r` before
    /// it is part of the line. A key line is the key at the very start, then
    /// optional spaces or tabs, `=`, optional spaces or tabs, and the
    /// value up to the end of the line. Values are Desktop Entry strings
    /// (`\s`, `\n`, `\t`, `\r`, `\\`), holding a URL-escaped value; both
    /// escapes are undone, to bytes.
    ///
    /// # Errors
    ///
    /// When the first line is not exactly `[Trash Info]`.
    pub fn parse(bytes: &[u8]) -> Result<Self, HeaderProblem> {
        let mut lines = bytes.split(|&byte| byte == b'\n');
        if lines.next() != Some(HEADER) {
            return Err(HeaderProblem);
        }
        let mut path = None;
        let mut deletion_date = None;
        for line in lines {
            if path.is_none()
                && let Some(value) = value_of(line, b"Path")
            {
                path = Some(decode_path(value));
            } else if deletion_date.is_none()
                && let Some(value) = value_of(line, b"DeletionDate")
            {
                deletion_date = Some(match encode::unescape_string(value) {
                    Ok(text) => DeletionDate::parse(&text)
                        .map_or(Field::Invalid(FieldProblem::Date), Field::Valid),
                    Err(problem) => Field::Invalid(FieldProblem::Escape(problem)),
                });
            }
            if path.is_some() && deletion_date.is_some() {
                break;
            }
        }
        Ok(Self {
            path: path.unwrap_or(Field::Missing),
            deletion_date: deletion_date.unwrap_or(Field::Missing),
        })
    }

    /// The `Path` value: the original location's bytes, absolute or
    /// relative to the trash's base directory, unchecked.
    #[must_use]
    pub fn path(&self) -> &Field<Vec<u8>> {
        &self.path
    }

    /// The `DeletionDate` value.
    #[must_use]
    pub fn deletion_date(&self) -> &Field<DeletionDate> {
        &self.deletion_date
    }
}

/// The value of `line` when it holds `key`.
fn value_of<'a>(line: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let rest = line.strip_prefix(key)?;
    let blank = |byte: &u8| *byte == b' ' || *byte == b'\t';
    let at = rest.iter().position(|byte| !blank(byte))?;
    let rest = rest[at..].strip_prefix(b"=")?;
    let at = rest
        .iter()
        .position(|byte| !blank(byte))
        .unwrap_or(rest.len());
    Some(&rest[at..])
}

fn decode_path(value: &[u8]) -> Field<Vec<u8>> {
    let decoded = encode::unescape_string(value).and_then(|text| encode::percent_decode(&text));
    match decoded {
        Ok(bytes) if bytes.is_empty() => Field::Invalid(FieldProblem::Empty),
        Ok(bytes) => Field::Valid(bytes),
        Err(problem) => Field::Invalid(FieldProblem::Escape(problem)),
    }
}

/// A trash info file's bytes: the header, `Path` (URL-escaped, which
/// leaves nothing a Desktop Entry string must escape) and `DeletionDate`.
pub(crate) fn format(path: &[u8], date: DeletionDate) -> Vec<u8> {
    let mut out = Vec::with_capacity(path.len() * 3 + 64);
    out.extend_from_slice(HEADER);
    out.extend_from_slice(b"\nPath=");
    out.extend_from_slice(&encode::percent_encode(path, true));
    out.extend_from_slice(b"\nDeletionDate=");
    out.extend_from_slice(date.to_string().as_bytes());
    out.push(b'\n');
    out
}

/// How absolute `Path` values in a per-volume trash are read. The spec
/// says such trash directories SHOULD hold relative paths only; a
/// volume's trash is written by whoever can write to the volume, and an
/// absolute path in it can name anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AbsolutePaths {
    /// Never: the origin of such an entry is unknown.
    Reject,
    /// Only a path below the volume's top directory.
    #[default]
    UnderTopdir,
    /// Any absolute path.
    Accept,
}

/// Why a decoded `Path` is not used as an origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PathProblem {
    /// It holds a NUL byte, which no path can.
    Nul,
    /// It has a `..` component.
    ParentComponent,
    /// It does not end in a file name (it is `/`, or ends in `/`, `.` or
    /// `/.`).
    NoFileName,
    /// It is absolute in a per-volume trash, which the policy refuses.
    AbsoluteRefused,
    /// It is absolute in a per-volume trash and not below its top
    /// directory, which the policy refuses.
    OutsideTopdir,
}

impl fmt::Display for PathProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Nul => "the path holds a NUL byte",
            Self::ParentComponent => "the path has a '..' component",
            Self::NoFileName => "the path does not end in a file name",
            Self::AbsoluteRefused => "absolute paths are refused in a per-volume trash",
            Self::OutsideTopdir => "the absolute path is outside the volume's top directory",
        })
    }
}

/// The original location `raw` names: absolute as it is, relative from
/// `base`. `volume` is the per-volume policy, `None` for the home trash.
pub(crate) fn resolve(
    raw: &[u8],
    base: &Path,
    volume: Option<AbsolutePaths>,
) -> Result<PathBuf, PathProblem> {
    if raw.contains(&0) {
        return Err(PathProblem::Nul);
    }
    let absolute = raw.first() == Some(&b'/');
    let mut path = if absolute {
        PathBuf::from("/")
    } else {
        base.to_path_buf()
    };
    let mut last = None;
    for part in raw.split(|&byte| byte == b'/') {
        last = Some(part);
        match part {
            b"" | b"." => {}
            b".." => return Err(PathProblem::ParentComponent),
            name => path.push(OsStr::from_bytes(name)),
        }
    }
    if matches!(last, None | Some(b"" | b".")) {
        return Err(PathProblem::NoFileName);
    }
    if absolute {
        match volume {
            None | Some(AbsolutePaths::Accept) => {}
            Some(AbsolutePaths::Reject) => return Err(PathProblem::AbsoluteRefused),
            Some(AbsolutePaths::UnderTopdir) => {
                if !path.starts_with(base) || path == base {
                    return Err(PathProblem::OutsideTopdir);
                }
            }
        }
    }
    Ok(path)
}
