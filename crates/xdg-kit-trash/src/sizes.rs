//! The size of a trash, and `$trash/directorysizes`: an advisory cache of
//! the sizes of trashed directories, keyed by storage name and checked
//! against the modification time of the entry's trash info file.
//!
//! The cache is never trusted for anything but a size: a missing,
//! malformed, stale or concurrently replaced cache only costs a
//! recalculation, and nothing in it is used to find, keep or remove a
//! payload or its metadata.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use xdg_kit_basedir::Durability;

use crate::dir::Trash;
use crate::encode::{self, EscapeProblem};
use crate::fault::{self, Stage};
use crate::list::info_name;
use crate::sys::{self, FileKind, Ident, Place, ReadError, Sameness};

/// The cache file's name in the trash directory.
const CACHE: &str = "directorysizes";

/// One line of `directorysizes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    /// The directory's allocated size, in bytes.
    pub size: u64,
    /// The modification time of its trash info file, in seconds since the
    /// epoch.
    pub mtime: i64,
    /// Its storage name in `files`.
    pub name: OsString,
}

/// A `directorysizes` line that was not used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheProblem {
    /// The line number, from 1.
    pub line: usize,
    /// What is wrong with it.
    pub kind: CacheProblemKind,
}

/// What is wrong with a `directorysizes` line.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CacheProblemKind {
    /// It is not three fields separated by spaces.
    Fields,
    /// The size is not a decimal number that fits 64 bits.
    Size,
    /// The time is not a decimal number (with an optional `-`) that fits
    /// 64 bits.
    Mtime,
    /// The name's escapes are malformed.
    Escape(EscapeProblem),
    /// The name is empty, `.`, `..`, or holds `/` or NUL (even escaped):
    /// no direct child of `files` has it.
    Name,
    /// An earlier line has the same name; the first is used.
    Duplicate,
}

/// The most [`CacheProblem`]s a [`ParsedSizes`] keeps; the rest are only
/// counted.
pub const MAX_CACHE_PROBLEMS: usize = 64;

/// A `directorysizes` file as read.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ParsedSizes {
    /// The usable entries, in order.
    pub entries: Vec<CacheEntry>,
    /// The first [`MAX_CACHE_PROBLEMS`] lines not used, and why.
    pub problems: Vec<CacheProblem>,
    /// How many lines were not used in all.
    pub rejected: usize,
    /// Whether reading stopped at the entry limit: later lines were not
    /// read (a cache is only advice, so this costs only measurements).
    pub truncated: bool,
}

/// Reads a `directorysizes` file, line by line: at most `max_entries`
/// usable entries are kept, and at most [`MAX_CACHE_PROBLEMS`] problems
/// (all are counted), so memory stays proportional to the entries kept,
/// whatever the file holds. An empty last line (the file's final newline)
/// is not a problem.
#[must_use]
pub fn parse_directory_sizes(bytes: &[u8], max_entries: usize) -> ParsedSizes {
    let mut parsed = ParsedSizes {
        entries: Vec::new(),
        problems: Vec::new(),
        rejected: 0,
        truncated: false,
    };
    let mut names = HashSet::new();
    let mut lines = bytes.split(|&byte| byte == b'\n').enumerate().peekable();
    while let Some((index, line)) = lines.next() {
        if line.is_empty() && lines.peek().is_none() {
            break;
        }
        let kind = match parse_line(line) {
            Ok(entry) if names.contains(&entry.name) => CacheProblemKind::Duplicate,
            Ok(_) if parsed.entries.len() == max_entries => {
                parsed.truncated = true;
                break;
            }
            Ok(entry) => {
                names.insert(entry.name.clone());
                parsed.entries.push(entry);
                continue;
            }
            Err(kind) => kind,
        };
        parsed.rejected += 1;
        if parsed.problems.len() < MAX_CACHE_PROBLEMS {
            parsed.problems.push(CacheProblem {
                line: index + 1,
                kind,
            });
        }
    }
    parsed
}

fn parse_line(line: &[u8]) -> Result<CacheEntry, CacheProblemKind> {
    let mut fields = line.splitn(3, |&byte| byte == b' ');
    let (Some(size), Some(mtime), Some(name)) = (fields.next(), fields.next(), fields.next())
    else {
        return Err(CacheProblemKind::Fields);
    };
    let digits = |text: &[u8]| !text.is_empty() && text.iter().all(u8::is_ascii_digit);
    if !digits(size) {
        return Err(CacheProblemKind::Size);
    }
    let size = std::str::from_utf8(size)
        .ok()
        .and_then(|text| text.parse().ok())
        .ok_or(CacheProblemKind::Size)?;
    if !digits(mtime.strip_prefix(b"-").unwrap_or(mtime)) {
        return Err(CacheProblemKind::Mtime);
    }
    let mtime = std::str::from_utf8(mtime)
        .ok()
        .and_then(|text| text.parse().ok())
        .ok_or(CacheProblemKind::Mtime)?;
    let name = encode::percent_decode(name).map_err(CacheProblemKind::Escape)?;
    if matches!(&*name, b"" | b"." | b"..") || name.contains(&b'/') || name.contains(&0) {
        return Err(CacheProblemKind::Name);
    }
    Ok(CacheEntry {
        size,
        mtime,
        name: OsStr::from_bytes(&name).to_os_string(),
    })
}

/// `entries` as a `directorysizes` file, names fully URL-escaped (as in
/// trash info files), in the order given.
#[must_use]
pub fn format_directory_sizes(entries: &[CacheEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    for entry in entries {
        out.extend_from_slice(format!("{} {} ", entry.size, entry.mtime).as_bytes());
        out.extend_from_slice(&encode::percent_encode(entry.name.as_bytes(), false));
        out.push(b'\n');
    }
    out
}

/// What [`Trash::size`] does with the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheUse {
    /// Neither read nor write it: measure every directory.
    Ignore,
    /// Read it, never write it.
    Read,
    /// Read it, and replace it afterwards with what was used and measured.
    #[default]
    ReadWrite,
}

/// How [`Trash::size`] works.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SizeOptions {
    /// What to do with `directorysizes`.
    pub cache: CacheUse,
}

/// The size of a trash's payloads.
#[derive(Debug)]
#[non_exhaustive]
pub struct SizeReport {
    /// Allocated bytes (512-byte blocks, as `du -B1` counts; a file with
    /// several links within one directory counted once), of what could be
    /// measured.
    pub bytes: u64,
    /// What could not be measured: when not empty, `bytes` is a lower
    /// bound.
    pub problems: Vec<SizeProblem>,
    /// What became of reading the cache.
    pub cache_read: CacheRead,
    /// What became of writing it.
    pub cache_write: CacheWrite,
}

impl SizeReport {
    /// Whether everything was measured.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.problems.is_empty()
    }
}

/// A part of a trash that was not measured.
#[derive(Debug)]
#[non_exhaustive]
pub struct SizeProblem {
    /// Its path inside `files`.
    pub path: PathBuf,
    /// Why.
    pub kind: SizeProblemKind,
}

/// Why a part of a trash was not measured.
#[derive(Debug)]
#[non_exhaustive]
pub enum SizeProblemKind {
    /// A directory on another mount: measuring never crosses mounts.
    MountBoundary,
    /// Deeper than [`Limits::max_depth`](crate::Limits::max_depth).
    TooDeep,
    /// More than [`Limits::max_tree_entries`](crate::Limits::max_tree_entries)
    /// entries in one trashed directory.
    TooMany,
    /// More than [`Limits::max_listed`](crate::Limits::max_listed) entries
    /// in `files`: the rest was not looked at.
    TooManyListed,
    /// A system call failed.
    Io(io::Error),
}

/// What became of reading `directorysizes`.
#[derive(Debug)]
#[non_exhaustive]
pub enum CacheRead {
    /// Not read ([`CacheUse::Ignore`]).
    Ignored,
    /// There is none.
    Missing,
    /// Read: this many usable entries, and what was not used.
    Read {
        /// Usable entries.
        entries: usize,
        /// The first lines not used, and why.
        problems: Vec<CacheProblem>,
        /// How many lines were not used in all.
        rejected: usize,
        /// Whether reading stopped at
        /// [`Limits::max_cache_entries`](crate::Limits::max_cache_entries).
        truncated: bool,
    },
    /// It is not a regular file, too large, or unreadable: not used.
    Unusable(io::Error),
}

/// What became of writing `directorysizes`.
#[derive(Debug)]
#[non_exhaustive]
pub enum CacheWrite {
    /// Not written: not asked to, or the listing of `files` was cut short
    /// (entries not seen might still be valid).
    NotWritten,
    /// Replaced atomically.
    Written(Durability),
    /// Writing failed; the old file, if any, is untouched.
    Failed(io::Error),
}

/// Why [`Trash::size`] stopped.
#[derive(Debug)]
#[non_exhaustive]
pub enum SizeError {
    /// `files` could not be read.
    Io(io::Error),
    /// `cancel` said to stop.
    Cancelled,
}

impl fmt::Display for SizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "cannot read files: {err}"),
            Self::Cancelled => f.write_str("cancelled"),
        }
    }
}

impl std::error::Error for SizeError {}

impl Trash {
    /// Measures the trash, by the spec's suggested algorithm: a file's
    /// size from `stat`, a directory's from the cache when its entry's
    /// trash info file has the cached modification time, else measured
    /// (and cached). A directory whose measurement is incomplete, or
    /// without a trash info file, is not cached. `cancel` is asked before
    /// each payload and each item inside one.
    ///
    /// # Errors
    ///
    /// When `files` cannot be read at all, or `cancel` says to stop.
    pub fn size(
        &self,
        options: &SizeOptions,
        cancel: &dyn Fn() -> bool,
    ) -> Result<SizeReport, SizeError> {
        let mut report = SizeReport {
            bytes: 0,
            problems: Vec::new(),
            cache_read: CacheRead::Ignored,
            cache_write: CacheWrite::NotWritten,
        };
        let mut cached = HashMap::new();
        if options.cache != CacheUse::Ignore {
            (cached, report.cache_read) = self.read_cache();
        }
        let mut seen: Vec<CacheEntry> = Vec::new();
        let mut dir =
            rustix::fs::Dir::read_from(self.files()).map_err(|err| SizeError::Io(err.into()))?;
        let mut listed = 0usize;
        let mut cut_short = false;
        while let Some(item) = dir.read() {
            if cancel() {
                return Err(SizeError::Cancelled);
            }
            let item = item.map_err(|err| SizeError::Io(err.into()))?;
            let name = OsStr::from_bytes(item.file_name().to_bytes());
            if name == "." || name == ".." {
                continue;
            }
            listed += 1;
            if listed > self.limits.max_listed {
                report.problems.push(SizeProblem {
                    path: PathBuf::new(),
                    kind: SizeProblemKind::TooManyListed,
                });
                cut_short = true;
                break;
            }
            let meta = match sys::stat_at(self.files(), name) {
                Ok(meta) => meta,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => {
                    report.problems.push(SizeProblem {
                        path: name.into(),
                        kind: SizeProblemKind::Io(err),
                    });
                    continue;
                }
            };
            if meta.kind != FileKind::Directory {
                report.bytes = report.bytes.saturating_add(meta.allocated);
                continue;
            }
            let mtime = match sys::stat_at(self.info(), &info_name(name)) {
                Ok(info) if info.kind == FileKind::File => Some(info.mtime),
                _ => None,
            };
            if let (Some(mtime), Some(entry)) = (mtime, cached.get(name))
                && entry.mtime == mtime
            {
                report.bytes = report.bytes.saturating_add(entry.size);
                seen.push(entry.clone());
                continue;
            }
            let problems = report.problems.len();
            let size = self.measure(name, &mut report.problems, cancel)?;
            report.bytes = report.bytes.saturating_add(size);
            if let Some(mtime) = mtime
                && report.problems.len() == problems
            {
                seen.push(CacheEntry {
                    size,
                    mtime,
                    name: name.to_os_string(),
                });
            }
        }
        if options.cache == CacheUse::ReadWrite && !cut_short {
            seen.sort_by(|a, b| a.name.cmp(&b.name));
            report.cache_write = match self.write_cache(&format_directory_sizes(&seen)) {
                Ok(durability) => CacheWrite::Written(durability),
                Err(err) => CacheWrite::Failed(err),
            };
        }
        Ok(report)
    }

    /// The usable entries of `directorysizes` by name, and what became of
    /// reading it.
    fn read_cache(&self) -> (HashMap<OsString, CacheEntry>, CacheRead) {
        let unusable = |why: String| CacheRead::Unusable(io::Error::other(why));
        match sys::read_bounded(
            self.root.as_fd(),
            OsStr::new(CACHE),
            self.limits.max_cache_len,
        ) {
            Ok((bytes, _)) => {
                let parsed = parse_directory_sizes(&bytes, self.limits.max_cache_entries);
                let read = CacheRead::Read {
                    entries: parsed.entries.len(),
                    problems: parsed.problems,
                    rejected: parsed.rejected,
                    truncated: parsed.truncated,
                };
                let cached = parsed
                    .entries
                    .into_iter()
                    .map(|entry| (entry.name.clone(), entry))
                    .collect();
                (cached, read)
            }
            Err(ReadError::Missing) => (HashMap::new(), CacheRead::Missing),
            Err(ReadError::NotRegular(meta)) => (
                HashMap::new(),
                unusable(format!("it is {:?}, not a regular file", meta.kind)),
            ),
            Err(ReadError::TooLarge(meta)) => (
                HashMap::new(),
                unusable(format!("it is larger than the limit ({} bytes)", meta.len)),
            ),
            Err(ReadError::Io(err)) => (HashMap::new(), CacheRead::Unusable(err)),
        }
    }

    /// The allocated size of the directory `name` in `files` and all it
    /// holds, without crossing mounts; what could not be measured is
    /// added to `problems`.
    fn measure(
        &self,
        name: &OsStr,
        problems: &mut Vec<SizeProblem>,
        cancel: &dyn Fn() -> bool,
    ) -> Result<u64, SizeError> {
        let mut problem = |path: &Path, kind| {
            problems.push(SizeProblem {
                path: path.to_path_buf(),
                kind,
            });
        };
        let root = match sys::open_dir_at(self.files(), name) {
            Ok(fd) => fd,
            Err(err) => {
                problem(Path::new(name), SizeProblemKind::Io(err));
                return Ok(0);
            }
        };
        let Some((place, mut total)) = self.measure_root(name, &root, &mut problem) else {
            return Ok(0);
        };
        let mut links: HashSet<Ident> = HashSet::new();
        let mut visited = 0u64;
        let open = |fd: OwnedFd| rustix::fs::Dir::read_from(&fd).map(|dir| (fd, dir));
        let mut stack: Vec<(OwnedFd, rustix::fs::Dir, PathBuf)> = Vec::new();
        match open(root) {
            Ok((fd, dir)) => stack.push((fd, dir, PathBuf::from(name))),
            Err(err) => problem(Path::new(name), SizeProblemKind::Io(err.into())),
        }
        while let Some(last) = stack.len().checked_sub(1) {
            if cancel() {
                return Err(SizeError::Cancelled);
            }
            let item = match stack[last].1.read() {
                None => {
                    stack.pop();
                    continue;
                }
                Some(Err(err)) => {
                    let (_, _, path) = stack.pop().expect("a frame");
                    problem(&path, SizeProblemKind::Io(err.into()));
                    continue;
                }
                Some(Ok(item)) => item,
            };
            let child = OsStr::from_bytes(item.file_name().to_bytes()).to_os_string();
            if child == "." || child == ".." {
                continue;
            }
            let path = stack[last].2.join(&child);
            visited += 1;
            if visited > self.limits.max_tree_entries {
                problem(&path, SizeProblemKind::TooMany);
                break;
            }
            let fd: BorrowedFd<'_> = stack[last].0.as_fd();
            let meta = match sys::stat_at(fd, &child) {
                Ok(meta) => meta,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => {
                    problem(&path, SizeProblemKind::Io(err));
                    continue;
                }
            };
            if meta.kind != FileKind::Directory {
                if meta.nlink <= 1 || links.insert(meta.ident) {
                    total = total.saturating_add(meta.allocated);
                }
                continue;
            }
            if stack.len() >= self.limits.max_depth {
                problem(&path, SizeProblemKind::TooDeep);
                continue;
            }
            let opened = match sys::open_dir_at(fd, &child) {
                Ok(opened) => opened,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => {
                    problem(&path, SizeProblemKind::Io(err));
                    continue;
                }
            };
            match Place::of(opened.as_fd()) {
                Ok(other) if other.same(place) != Sameness::Different => {}
                Ok(_) => {
                    problem(&path, SizeProblemKind::MountBoundary);
                    continue;
                }
                Err(err) => {
                    problem(&path, SizeProblemKind::Io(err));
                    continue;
                }
            }
            total = total.saturating_add(meta.allocated);
            match open(opened) {
                Ok((fd, dir)) => stack.push((fd, dir, path)),
                Err(err) => problem(&path, SizeProblemKind::Io(err.into())),
            }
        }
        Ok(total)
    }

    /// The place of `files` — the boundary a payload's measurement does not
    /// cross, the payload itself included (a payload that is a mount point
    /// is not entered) — and the payload directory's own allocated size.
    fn measure_root(
        &self,
        name: &OsStr,
        root: &OwnedFd,
        problem: &mut impl FnMut(&Path, SizeProblemKind),
    ) -> Option<(Place, u64)> {
        match (
            Place::of(self.files()),
            Place::of(root.as_fd()),
            sys::fstat(root),
        ) {
            (Ok(place), Ok(own), Ok(meta)) if own.same(place) != Sameness::Different => {
                Some((place, meta.allocated))
            }
            (Ok(_), Ok(_), Ok(_)) => {
                problem(Path::new(name), SizeProblemKind::MountBoundary);
                None
            }
            (Err(err), _, _) | (_, Err(err), _) | (_, _, Err(err)) => {
                problem(Path::new(name), SizeProblemKind::Io(err));
                None
            }
        }
    }

    /// Replaces `directorysizes` with `bytes`: a new file in the trash
    /// directory, synced, renamed over the old one, the directory synced —
    /// all relative to the open trash directory.
    fn write_cache(&self, bytes: &[u8]) -> io::Result<Durability> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let root = self.root.as_fd();
        let mut attempt = 0;
        let (temp, fd) = loop {
            let temp: OsString = format!(
                ".{CACHE}.{}.{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            )
            .into();
            match sys::create_exclusive(root, &temp, 0o600) {
                Ok(fd) => break (temp, fd),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists && attempt < 16 => {
                    attempt += 1;
                }
                Err(err) => return Err(err),
            }
        };
        let written = fault::check(Stage::WriteCache)
            .and_then(|()| sys::write_all(&fd, bytes))
            .and_then(|()| sys::sync(&fd))
            .and_then(|()| Ok(rustix::fs::renameat(root, &temp, root, CACHE)?));
        drop(fd);
        if let Err(err) = written {
            let _ = rustix::fs::unlinkat(root, &temp, rustix::fs::AtFlags::empty());
            return Err(err);
        }
        Ok(match sys::sync(root) {
            Ok(()) => Durability::Synced,
            Err(err) => Durability::NotSynced(err),
        })
    }
}
