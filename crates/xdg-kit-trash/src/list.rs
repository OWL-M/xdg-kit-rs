//! Listing a trash: every payload in `files`, with its metadata or the
//! reason there is none, then every trash info file without a payload.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;
use std::sync::Arc;

use crate::dir::{Trash, TrashDir};
use crate::info::{self, Field, FieldProblem, HeaderProblem, PathProblem, TrashInfo};
use crate::sys::{self, FileKind, Ident, ReadError};
use crate::time::DeletionDate;

/// Which trash an entry was listed from: its location and the identity of
/// its directory when it was opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashId {
    pub(crate) dir: TrashDir,
    pub(crate) ident: Ident,
}

impl TrashId {
    /// Where the trash is.
    #[must_use]
    pub fn dir(&self) -> &TrashDir {
        &self.dir
    }
}

/// One item of a trash, as listed: a payload, its metadata, or both.
///
/// It is a snapshot. Operations on it ([`Trash::restore`],
/// [`Trash::erase`], [`Trash::discard_metadata`]) first compare it with
/// what the trash holds then; see [`Changed`](crate::Changed) for what
/// that comparison does and does not establish.
#[derive(Debug, Clone)]
pub struct Entry {
    pub(crate) trash: TrashId,
    pub(crate) name: OsString,
    pub(crate) payload: Payload,
    pub(crate) metadata: Metadata,
    pub(crate) origin: Origin,
    pub(crate) seen: Seen,
}

/// The payload in `$trash/files`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Payload {
    /// It exists, and is this kind of file (a symbolic link is not
    /// followed).
    Present(FileKind),
    /// Nothing in `files` has the name: the trash info file stands alone —
    /// a trashing in progress, or one that failed or crashed before its
    /// move.
    Missing,
    /// It could not be inspected.
    Unknown(Arc<io::Error>),
}

/// The trash info file in `$trash/info`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Metadata {
    /// Read: what it says, which may still be malformed.
    Read(Result<TrashInfo, HeaderProblem>),
    /// There is none: an emergency the spec says to present as such. The
    /// payload's name says nothing reliable about where it came from.
    Missing,
    /// It is something other than a regular file.
    NotRegular(FileKind),
    /// It is larger than [`Limits::max_info_len`](crate::Limits::max_info_len)
    /// (its length as far as seen).
    TooLarge(u64),
    /// It could not be read.
    Unreadable(Arc<io::Error>),
}

/// What was seen of an entry's files when listed, for noticing changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Seen {
    /// The payload's identity, when present.
    pub(crate) payload: Option<Ident>,
    /// The trash info file's identity, when it was inspected.
    pub(crate) metadata: Option<Ident>,
    /// The trash info file's bytes, when read.
    pub(crate) bytes: Option<Arc<[u8]>>,
}

/// Where an entry came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// This absolute path, from its trash info file, checked.
    Known(PathBuf),
    /// Not known: never guess it from the payload's name.
    Unknown(UnknownOrigin),
}

/// Why an entry's origin is unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnknownOrigin {
    /// There is no trash info file.
    MetadataMissing,
    /// The trash info file is unusable (not a regular file, too large, or
    /// unreadable).
    MetadataUnusable,
    /// The trash info file does not start with `[Trash Info]`.
    Header,
    /// The trash info file has no `Path`.
    PathMissing,
    /// The first `Path` cannot be decoded.
    PathMalformed(FieldProblem),
    /// The first `Path` decodes to a path that is not used.
    PathRefused(PathProblem),
}

impl fmt::Display for UnknownOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MetadataMissing => f.write_str("there is no trash info file"),
            Self::MetadataUnusable => f.write_str("the trash info file cannot be read"),
            Self::Header => HeaderProblem.fmt(f),
            Self::PathMissing => f.write_str("the trash info file has no Path"),
            Self::PathMalformed(problem) => write!(f, "Path: {problem}"),
            Self::PathRefused(problem) => write!(f, "Path: {problem}"),
        }
    }
}

impl Entry {
    /// The trash it was listed from.
    #[must_use]
    pub fn trash(&self) -> &TrashId {
        &self.trash
    }

    /// Its storage name, in `files` (and, plus `.trashinfo`, in `info`).
    /// Not the original name: never derive one from it.
    #[must_use]
    pub fn name(&self) -> &OsStr {
        &self.name
    }

    /// The payload.
    #[must_use]
    pub fn payload(&self) -> &Payload {
        &self.payload
    }

    /// The trash info file.
    #[must_use]
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    /// Where it came from.
    #[must_use]
    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    /// The parsed trash info, when the file was read and starts right.
    #[must_use]
    pub fn info(&self) -> Option<&TrashInfo> {
        match &self.metadata {
            Metadata::Read(Ok(info)) => Some(info),
            _ => None,
        }
    }

    /// When it was trashed, as a civil time in an unrecorded zone.
    #[must_use]
    pub fn deletion_date(&self) -> Option<DeletionDate> {
        self.info()?.deletion_date().valid().copied()
    }

    /// The payload's path, `$trash/files/<name>`, for display: operations
    /// use the open trash, not this path.
    #[must_use]
    pub fn payload_path(&self) -> PathBuf {
        self.trash.dir.path().join("files").join(&self.name)
    }

    /// The trash info file's path, `$trash/info/<name>.trashinfo`, for
    /// display.
    #[must_use]
    pub fn metadata_path(&self) -> PathBuf {
        self.trash
            .dir
            .path()
            .join("info")
            .join(info_name(&self.name))
    }

    /// Whether the payload is present and the origin is known: the
    /// ordinary case.
    #[must_use]
    pub fn is_intact(&self) -> bool {
        matches!(self.payload, Payload::Present(_)) && matches!(self.origin, Origin::Known(_))
    }
}

/// Whether `name` is a single storage name: one non-empty path component
/// other than `.` and `..`, without NUL. Every directory-relative lookup
/// in `files` and `info` takes only such names, so none leaves them.
pub(crate) fn is_storage_name(name: &OsStr) -> bool {
    let bytes = name.as_bytes();
    !matches!(bytes, b"" | b"." | b"..") && !bytes.contains(&b'/') && !bytes.contains(&0)
}

/// A name given as a storage name that is not one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidName(pub OsString);

impl fmt::Display for InvalidName {
    #[allow(
        clippy::unnecessary_debug_formatting,
        reason = "the name may hold any bytes: Debug escapes them"
    )]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} is not a storage name", self.0)
    }
}

impl std::error::Error for InvalidName {}

/// `<name>.trashinfo`.
pub(crate) fn info_name(name: &OsStr) -> OsString {
    let mut info = name.to_os_string();
    info.push(OsStr::from_bytes(info::SUFFIX));
    info
}

/// Why a listing stopped early: what it returned so far is not the whole
/// trash.
#[derive(Debug)]
#[non_exhaustive]
pub enum ListError {
    /// Reading `files` or `info` failed.
    Io {
        /// `files` or `info`.
        dir: &'static str,
        /// What failed.
        error: io::Error,
    },
    /// [`Limits::max_listed`](crate::Limits::max_listed) directory entries
    /// were visited.
    Limit,
}

impl fmt::Display for ListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { dir, error } => write!(f, "cannot read {dir}: {error}"),
            Self::Limit => f.write_str("the trash has more entries than the listing limit"),
        }
    }
}

impl std::error::Error for ListError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { error, .. } => Some(error),
            Self::Limit => None,
        }
    }
}

/// The entries of a trash, read as iterated: memory does not grow with
/// the trash. Each payload of `files` comes first, then each trash info
/// file without one.
///
/// A [`ListError`] item ends the iteration: the listing is complete only
/// when the iterator ends without one. The listing is not atomic: what
/// changes while it runs may be seen, missed, or (an entry moving from
/// payload-less to whole) seen twice.
#[derive(Debug)]
pub struct Entries<'a> {
    trash: &'a Trash,
    phase: Phase,
    visited: usize,
}

#[derive(Debug)]
enum Phase {
    Start,
    Files(rustix::fs::Dir),
    Info(rustix::fs::Dir),
    Done,
}

impl Trash {
    /// Lists the trash.
    #[must_use]
    pub fn entries(&self) -> Entries<'_> {
        Entries {
            trash: self,
            phase: Phase::Start,
            visited: 0,
        }
    }

    pub(crate) fn trash_id(&self) -> TrashId {
        TrashId {
            dir: self.dir.clone(),
            ident: self.id,
        }
    }

    /// The entry with the storage name `name` as it is now; `None` when
    /// neither a payload nor a trash info file has the name.
    ///
    /// # Errors
    ///
    /// [`InvalidName`] when `name` is not one storage name: empty, `.`,
    /// `..`, or holding `/` or NUL. Nothing is looked up then.
    pub fn entry(&self, name: &OsStr) -> Result<Option<Entry>, InvalidName> {
        if !is_storage_name(name) {
            return Err(InvalidName(name.to_os_string()));
        }
        let entry = self.read_entry(name);
        Ok((!matches!(entry.payload, Payload::Missing)
            || !matches!(entry.metadata, Metadata::Missing))
        .then_some(entry))
    }

    /// The entry `name` (one storage name: listed, or checked by
    /// [`Trash::entry`]) as it is now, whatever is there.
    pub(crate) fn read_entry(&self, name: &OsStr) -> Entry {
        debug_assert!(is_storage_name(name), "a name that is no storage name");
        let mut seen = Seen {
            payload: None,
            metadata: None,
            bytes: None,
        };
        let payload = match sys::stat_at(self.files(), name) {
            Ok(meta) => {
                seen.payload = Some(meta.ident);
                Payload::Present(meta.kind)
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Payload::Missing,
            Err(err) => Payload::Unknown(Arc::new(err)),
        };
        let metadata =
            match sys::read_bounded(self.info(), &info_name(name), self.limits.max_info_len) {
                Ok((bytes, meta)) => {
                    seen.metadata = Some(meta.ident);
                    let info = TrashInfo::parse(&bytes);
                    seen.bytes = Some(bytes.into());
                    Metadata::Read(info)
                }
                Err(ReadError::Missing) => Metadata::Missing,
                Err(ReadError::NotRegular(meta)) => {
                    seen.metadata = Some(meta.ident);
                    Metadata::NotRegular(meta.kind)
                }
                Err(ReadError::TooLarge(meta)) => {
                    seen.metadata = Some(meta.ident);
                    Metadata::TooLarge(meta.len)
                }
                Err(ReadError::Io(err)) => {
                    seen.metadata = sys::stat_at(self.info(), &info_name(name))
                        .ok()
                        .map(|meta| meta.ident);
                    Metadata::Unreadable(Arc::new(err))
                }
            };
        let origin = self.origin_of(&metadata);
        Entry {
            trash: self.trash_id(),
            name: name.to_os_string(),
            payload,
            metadata,
            origin,
            seen,
        }
    }

    fn origin_of(&self, metadata: &Metadata) -> Origin {
        let info = match metadata {
            Metadata::Read(Ok(info)) => info,
            Metadata::Read(Err(_)) => return Origin::Unknown(UnknownOrigin::Header),
            Metadata::Missing => return Origin::Unknown(UnknownOrigin::MetadataMissing),
            Metadata::NotRegular(_) | Metadata::TooLarge(_) | Metadata::Unreadable(_) => {
                return Origin::Unknown(UnknownOrigin::MetadataUnusable);
            }
        };
        match info.path() {
            Field::Valid(raw) => match info::resolve(raw, self.dir.base(), self.absolute_paths) {
                Ok(path) => Origin::Known(path),
                Err(problem) => Origin::Unknown(UnknownOrigin::PathRefused(problem)),
            },
            Field::Missing => Origin::Unknown(UnknownOrigin::PathMissing),
            Field::Invalid(problem) => Origin::Unknown(UnknownOrigin::PathMalformed(*problem)),
        }
    }
}

impl Iterator for Entries<'_> {
    type Item = Result<Entry, ListError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (dir, label) = match &mut self.phase {
                Phase::Done => return None,
                Phase::Start => {
                    match rustix::fs::Dir::read_from(self.trash.files()) {
                        Ok(dir) => self.phase = Phase::Files(dir),
                        Err(err) => return Some(self.fail("files", err.into())),
                    }
                    continue;
                }
                Phase::Files(dir) => (dir, "files"),
                Phase::Info(dir) => (dir, "info"),
            };
            let Some(item) = dir.read() else {
                if label == "files" {
                    match rustix::fs::Dir::read_from(self.trash.info()) {
                        Ok(dir) => self.phase = Phase::Info(dir),
                        Err(err) => return Some(self.fail("info", err.into())),
                    }
                } else {
                    self.phase = Phase::Done;
                }
                continue;
            };
            let item = match item {
                Ok(item) => item,
                Err(err) => return Some(self.fail(label, err.into())),
            };
            let name = OsStr::from_bytes(item.file_name().to_bytes());
            if name == "." || name == ".." {
                continue;
            }
            self.visited += 1;
            if self.visited > self.trash.limits.max_listed {
                self.phase = Phase::Done;
                return Some(Err(ListError::Limit));
            }
            if label == "files" {
                return Some(Ok(self.trash.read_entry(name)));
            }
            // In `info`: only `<name>.trashinfo` files with no payload.
            let Some(stem) = name.as_bytes().strip_suffix(info::SUFFIX) else {
                continue;
            };
            let stem = OsStr::from_bytes(stem);
            // `.trashinfo`, `..trashinfo`, `...trashinfo` name no entry.
            if !is_storage_name(stem) {
                continue;
            }
            // A payload that exists was listed with `files`; one that
            // cannot be inspected is listed here too (as unknown), as it
            // may not have been.
            let entry = self.trash.read_entry(stem);
            if !matches!(entry.payload, Payload::Present(_)) {
                return Some(Ok(entry));
            }
        }
    }
}

impl Entries<'_> {
    fn fail(&mut self, dir: &'static str, error: io::Error) -> Result<Entry, ListError> {
        self.phase = Phase::Done;
        Err(ListError::Io { dir, error })
    }
}
