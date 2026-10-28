//! Directory-relative file system primitives: every operation on a trash
//! names one component relative to an open directory, without following a
//! symbolic link in that component.

use std::ffi::OsStr;
use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;

use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat};

#[cfg(test)]
thread_local! {
    /// What this thread's calls here touched: a path for path-based calls,
    /// `None` for calls on an open directory. Tests read it to show that a
    /// refused path reached nothing.
    #[allow(
        clippy::missing_const_for_thread_local,
        reason = "the initializer is const already; the lint still fires for Android targets"
    )]
    static PROBES: std::cell::RefCell<Vec<Option<std::path::PathBuf>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Notes a call touching the file system (tests only).
#[inline]
fn probe(path: Option<&Path>) {
    #[cfg(test)]
    PROBES.with(|probes| probes.borrow_mut().push(path.map(Path::to_path_buf)));
    #[cfg(not(test))]
    let _ = path;
}

/// The calls noted on this thread since the last take (tests only).
#[cfg(test)]
pub(crate) fn take_probes() -> Vec<Option<std::path::PathBuf>> {
    PROBES.with(|probes| std::mem::take(&mut *probes.borrow_mut()))
}

/// Creates `dir` and its missing ancestors (mode 0700, less the umask),
/// syncing each parent a directory was made in.
pub(crate) fn create_dir_all(dir: &Path) -> io::Result<xdg_kit_basedir::Durability> {
    probe(Some(dir));
    xdg_kit_basedir::create_dir_all_synced(dir, 0o700)
}

/// `realpath`: `path` with every symbolic link resolved.
pub(crate) fn canonicalize(path: &Path) -> io::Result<std::path::PathBuf> {
    probe(Some(path));
    std::fs::canonicalize(path)
}

/// What kind of file a name is, without following a symbolic link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileKind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// A symbolic link (itself, not what it points to).
    Symlink,
    /// Anything else: a FIFO, a socket, a device.
    Other,
}

impl FileKind {
    fn of(file_type: FileType) -> Self {
        match file_type {
            FileType::RegularFile => Self::File,
            FileType::Directory => Self::Directory,
            FileType::Symlink => Self::Symlink,
            _ => Self::Other,
        }
    }
}

/// A file's device and inode numbers: which file it is, while it exists.
/// Equal identities do not mean equal contents: a file can be rewritten in
/// place, and a freed inode number can be reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Ident {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
}

/// What `stat` says, in portable types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Meta {
    pub(crate) ident: Ident,
    pub(crate) kind: FileKind,
    /// The permission bits, set-id and sticky bits included.
    pub(crate) mode: u32,
    pub(crate) uid: u32,
    pub(crate) nlink: u64,
    /// Bytes allocated: 512-byte blocks, as `du -B1` counts.
    pub(crate) allocated: u64,
    pub(crate) len: u64,
    /// Seconds since the epoch.
    pub(crate) mtime: i64,
}

impl Meta {
    // `stat`'s field types differ between systems: some are already the
    // types used here, some are narrower, some signed.
    #[allow(
        clippy::unnecessary_cast,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        clippy::cast_lossless,
        clippy::useless_conversion,
        reason = "stat's field types vary by system"
    )]
    fn of(stat: &Stat) -> Self {
        Self {
            ident: Ident {
                dev: stat.st_dev as u64,
                ino: stat.st_ino as u64,
            },
            kind: FileKind::of(FileType::from_raw_mode(stat.st_mode)),
            mode: (stat.st_mode as u32) & 0o7777,
            uid: stat.st_uid as u32,
            nlink: stat.st_nlink as u64,
            allocated: (stat.st_blocks as u64).saturating_mul(512),
            len: stat.st_size as u64,
            mtime: stat.st_mtime as i64,
        }
    }
}

/// Opens the directory at `path`, following symbolic links on the way:
/// paths given by the caller (a base directory, a mount's top directory, a
/// destination's parent) mean what path lookup makes of them.
pub(crate) fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    probe(Some(path));
    Ok(rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

/// Opens the directory `name` in `dir`, refusing a symbolic link.
pub(crate) fn open_dir_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<OwnedFd> {
    probe(None);
    Ok(rustix::fs::openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )?)
}

/// The target of the symbolic link `name` in `dir`, as stored.
pub(crate) fn read_link_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<std::ffi::OsString> {
    use std::os::unix::ffi::OsStringExt as _;
    probe(None);
    Ok(std::ffi::OsString::from_vec(
        rustix::fs::readlinkat(dir, name, Vec::new())?.into_bytes(),
    ))
}

/// Opens the directory `name` in `dir` for looking up names in it, refusing
/// a symbolic link: on Linux and Android with `O_PATH`, which needs only
/// the search permission a path lookup needs; elsewhere read-only, which
/// needs read permission too. Not for reading the directory: see
/// [`reopen_dir`].
pub(crate) fn open_search_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<OwnedFd> {
    probe(None);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let access = OFlags::PATH;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let access = OFlags::RDONLY;
    Ok(rustix::fs::openat(
        dir,
        name,
        access | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )?)
}

/// Opens the directory at `path`, following symbolic links on the way as
/// [`open_dir`] does, for looking up names in it: with `O_PATH` on Linux
/// and Android (search permission suffices), elsewhere read-only.
pub(crate) fn open_search(path: &Path) -> io::Result<OwnedFd> {
    probe(Some(path));
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let access = OFlags::PATH;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let access = OFlags::RDONLY;
    Ok(rustix::fs::open(
        path,
        access | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

/// The directory open as `dir` (perhaps only for lookups), opened again
/// for reading: the same directory, found by no name.
pub(crate) fn reopen_dir(dir: BorrowedFd<'_>) -> io::Result<OwnedFd> {
    probe(None);
    Ok(rustix::fs::openat(
        dir,
        ".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

/// The target of the symbolic link at `path`, as stored (links before the
/// last component followed).
pub(crate) fn read_link(path: &Path) -> io::Result<std::ffi::OsString> {
    probe(Some(path));
    Ok(std::fs::read_link(path)?.into_os_string())
}

/// `stat` of `name` in `dir`, not following a symbolic link.
pub(crate) fn stat_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<Meta> {
    probe(None);
    Ok(Meta::of(&rustix::fs::statat(
        dir,
        name,
        AtFlags::SYMLINK_NOFOLLOW,
    )?))
}

/// `stat` of `name` in `dir`: `None` when nothing has that name.
pub(crate) fn lookup_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<Option<Meta>> {
    match stat_at(dir, name) {
        Ok(meta) => Ok(Some(meta)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// `stat` of an open file.
pub(crate) fn fstat(fd: impl AsFd) -> io::Result<Meta> {
    probe(None);
    Ok(Meta::of(&rustix::fs::fstat(fd)?))
}

/// The mount an open file is on (`at` with an empty path), or the file at
/// `at` as a path (symbolic links followed), where the system reports
/// mounts (Linux's `statx` mount ID, since 5.8). `None` means unknown,
/// never "the same".
#[cfg(any(target_os = "linux", target_os = "android"))]
fn mount_id(at: Result<BorrowedFd<'_>, &Path>) -> Option<u64> {
    use rustix::fs::StatxFlags;
    let found = match at {
        Ok(fd) => rustix::fs::statx(fd, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID),
        Err(path) => rustix::fs::statx(rustix::fs::CWD, path, AtFlags::empty(), StatxFlags::MNT_ID),
    }
    .ok()?;
    StatxFlags::from_bits_retain(found.stx_mask)
        .contains(StatxFlags::MNT_ID)
        .then_some(found.stx_mnt_id)
}

/// The mount a file is on: never known on this system.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn mount_id(_at: Result<BorrowedFd<'_>, &Path>) -> Option<u64> {
    None
}

/// Where an open directory sits among mounts, as far as can be told: its
/// device, and its mount where the system reports mounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Place {
    pub(crate) dev: u64,
    pub(crate) mount: Option<u64>,
}

impl Place {
    pub(crate) fn of(fd: BorrowedFd<'_>) -> io::Result<Self> {
        Ok(Self {
            dev: fstat(fd)?.ident.dev,
            mount: mount_id(Ok(fd)),
        })
    }

    /// The place of the directory at `path` (symbolic links followed),
    /// looked at with `stat`: this needs search permission on the way
    /// there, not read permission on the directory itself.
    pub(crate) fn of_path(path: &Path) -> io::Result<Self> {
        probe(Some(path));
        Ok(Self {
            dev: Meta::of(&rustix::fs::stat(path)?).ident.dev,
            mount: mount_id(Err(path)),
        })
    }

    /// Whether two places are on the same mount: certain when both mounts
    /// are known, otherwise only that the devices match — a bind mount of
    /// the same device then looks the same, and a rename is the only test.
    pub(crate) fn same(self, other: Self) -> Sameness {
        match (self.mount, other.mount) {
            (Some(a), Some(b)) if a == b && self.dev == other.dev => Sameness::Same,
            (Some(_), Some(_)) => Sameness::Different,
            _ if self.dev != other.dev => Sameness::Different,
            _ => Sameness::SameDevice,
        }
    }
}

/// How sure [`Place::same`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sameness {
    /// The same mount.
    Same,
    /// The same device; the mounts are unknown.
    SameDevice,
    /// Different devices or different mounts.
    Different,
}

/// Whether this system has a native rename that refuses to replace its
/// target. Where it has not, or a file system refuses it, moves use the
/// policy's [`Fallbacks`](crate::Fallbacks).
pub const NO_REPLACE_RENAME: bool = cfg!(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple"
));

/// Why a non-replacing rename did not happen. The source is untouched in
/// every case.
#[cfg_attr(
    not(any(target_os = "linux", target_os = "android", target_vendor = "apple")),
    allow(
        dead_code,
        reason = "without a native rename, only `Unsupported` is reported"
    )
)]
#[derive(Debug)]
pub(crate) enum RenameError {
    /// The target name exists.
    Exists,
    /// Source and target are on different mounts.
    CrossDevice,
    /// The system or the file system cannot rename without replacing.
    Unsupported(Option<io::Error>),
    /// Anything else.
    Other(io::Error),
}

/// Renames `from` in `from_dir` to `to` in `to_dir`, failing if `to`
/// exists: `renameat2(RENAME_NOREPLACE)` on Linux, `renameatx_np`
/// (`RENAME_EXCL`) on Apple systems. Nothing checks first and replaces
/// after: where that call is missing, this fails as unsupported.
pub(crate) fn rename_no_replace(
    from_dir: BorrowedFd<'_>,
    from: &OsStr,
    to_dir: BorrowedFd<'_>,
    to: &OsStr,
) -> Result<(), RenameError> {
    probe(None);
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    {
        use rustix::io::Errno;
        match rustix::fs::renameat_with(
            from_dir,
            from,
            to_dir,
            to,
            rustix::fs::RenameFlags::NOREPLACE,
        ) {
            Ok(()) => Ok(()),
            Err(Errno::EXIST) => Err(RenameError::Exists),
            Err(Errno::XDEV) => Err(RenameError::CrossDevice),
            // Linux: a kernel without renameat2 (ENOSYS), or a file system
            // without RENAME_NOREPLACE (EINVAL). Apple: a file system
            // without RENAME_EXCL (ENOTSUP). Moving a directory into
            // itself is also EINVAL; callers rule that out first.
            Err(err @ (Errno::NOSYS | Errno::INVAL | Errno::NOTSUP)) => {
                Err(RenameError::Unsupported(Some(err.into())))
            }
            #[allow(
                unreachable_patterns,
                reason = "EOPNOTSUPP equals ENOTSUP on some systems"
            )]
            Err(err @ Errno::OPNOTSUPP) => Err(RenameError::Unsupported(Some(err.into()))),
            Err(err) => Err(RenameError::Other(err.into())),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    {
        let _ = (from_dir, from, to_dir, to);
        Err(RenameError::Unsupported(None))
    }
}

/// The kind a directory entry reports for itself: `Unknown` where the
/// system's entries carry no type (Solaris and illumos), and wherever the
/// file system leaves it out; the caller then looks with `stat`.
pub(crate) fn entry_type(item: &rustix::fs::DirEntry) -> FileType {
    #[cfg(not(any(target_os = "illumos", target_os = "solaris")))]
    {
        item.file_type()
    }
    #[cfg(any(target_os = "illumos", target_os = "solaris"))]
    {
        let _ = item;
        FileType::Unknown
    }
}

/// Permission bits as a [`Mode`]. Modes are 16 bits wide on some systems;
/// the bits used here (at most `0o7777`) fit.
#[allow(
    clippy::cast_possible_truncation,
    clippy::unnecessary_cast,
    reason = "a mode's bits fit every system's mode type"
)]
pub(crate) fn mode_bits(bits: u32) -> Mode {
    Mode::from_raw_mode((bits & 0o7777) as rustix::fs::RawMode)
}

/// Creates `name` in `dir` exclusively, for writing, refusing a symbolic
/// link (`O_EXCL` already fails on any existing name, a dangling link
/// included).
pub(crate) fn create_exclusive(
    dir: BorrowedFd<'_>,
    name: &OsStr,
    mode: u32,
) -> io::Result<OwnedFd> {
    probe(None);
    Ok(rustix::fs::openat(
        dir,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        mode_bits(mode),
    )?)
}

/// `accessat`'s flag for judging by the effective user and groups, as an
/// actual operation is judged. Android's C library has no `AT_EACCESS`:
/// there the real ones are used, which differ only in a set-ID program.
#[cfg(not(target_os = "android"))]
const EFFECTIVE_IDS: AtFlags = AtFlags::EACCESS;
#[cfg(target_os = "android")]
const EFFECTIVE_IDS: AtFlags = AtFlags::empty();

/// Whether this process may create entries in the open directory `dir`:
/// write and search permission, by the effective user and groups (the
/// real ones on Android), as the kernel judges them now (ACLs and
/// read-only mounts included). Advisory: only an actual creation decides.
pub(crate) fn may_create_in(dir: BorrowedFd<'_>) -> io::Result<()> {
    use rustix::fs::Access;
    probe(None);
    Ok(rustix::fs::accessat(
        dir,
        ".",
        Access::WRITE_OK | Access::EXEC_OK,
        EFFECTIVE_IDS,
    )?)
}

/// [`may_create_in`] for the directory at `path` (symbolic links
/// followed), which needs search permission on the way, not read
/// permission on the directory.
pub(crate) fn may_create_at(path: &Path) -> io::Result<()> {
    use rustix::fs::Access;
    probe(Some(path));
    Ok(rustix::fs::accessat(
        rustix::fs::CWD,
        path,
        Access::WRITE_OK | Access::EXEC_OK,
        EFFECTIVE_IDS,
    )?)
}

/// `stat` of the file at `path`, symbolic links followed: `None` when
/// nothing has that path.
pub(crate) fn lookup_path(path: &Path) -> io::Result<Option<Meta>> {
    probe(Some(path));
    match rustix::fs::stat(path) {
        Ok(stat) => Ok(Some(Meta::of(&stat))),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// `lstat` of the file at `path`: the final component is not followed (a
/// symbolic link is itself). `None` when nothing has that path.
pub(crate) fn lookup_path_nofollow(path: &Path) -> io::Result<Option<Meta>> {
    probe(Some(path));
    match rustix::fs::lstat(path) {
        Ok(stat) => Ok(Some(Meta::of(&stat))),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// Creates the directory `name` in `dir`.
pub(crate) fn mkdir_at(dir: BorrowedFd<'_>, name: &OsStr, mode: u32) -> io::Result<()> {
    probe(None);
    Ok(rustix::fs::mkdirat(dir, name, mode_bits(mode))?)
}

/// Writes all of `bytes` to `fd`.
pub(crate) fn write_all(fd: &OwnedFd, bytes: &[u8]) -> io::Result<()> {
    let mut file = std::fs::File::from(fd.try_clone()?);
    file.write_all(bytes)
}

/// `fsync`.
pub(crate) fn sync(fd: impl AsFd) -> io::Result<()> {
    Ok(rustix::fs::fsync(fd)?)
}

/// Why [`read_bounded`] read nothing.
#[derive(Debug)]
pub(crate) enum ReadError {
    /// Nothing has that name.
    Missing,
    /// The name is not a regular file.
    NotRegular(Meta),
    /// The file is larger than the limit (its length).
    TooLarge(Meta),
    /// Opening, inspecting or reading failed.
    Io(io::Error),
}

/// Reads the regular file `name` in `dir`, not following a symbolic link,
/// and its metadata, if it holds at most `max` bytes. A file that grows
/// while read counts as too large.
pub(crate) fn read_bounded(
    dir: BorrowedFd<'_>,
    name: &OsStr,
    max: u64,
) -> Result<(Vec<u8>, Meta), ReadError> {
    probe(None);
    // Non-blocking: opening a FIFO must not wait for a writer.
    let fd = match rustix::fs::openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Err(ReadError::Missing),
        Err(err) => {
            // A symbolic link, or a FIFO that cannot be opened: say what
            // it is when that can be told.
            return match stat_at(dir, name) {
                Ok(meta) if meta.kind != FileKind::File => Err(ReadError::NotRegular(meta)),
                _ => Err(ReadError::Io(err.into())),
            };
        }
    };
    let meta = fstat(&fd).map_err(ReadError::Io)?;
    if meta.kind != FileKind::File {
        return Err(ReadError::NotRegular(meta));
    }
    if meta.len > max {
        return Err(ReadError::TooLarge(meta));
    }
    let mut bytes = Vec::new();
    let file = std::fs::File::from(fd);
    file.take(max.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(ReadError::Io)?;
    if bytes.len() as u64 > max {
        return Err(ReadError::TooLarge(Meta {
            len: bytes.len() as u64,
            ..meta
        }));
    }
    Ok((bytes, meta))
}

/// Removes `name` from `dir` only if it is still the file `ident` (a
/// directory when `dir_entry`). `Ok(false)` when it is gone already;
/// an error of kind `AlreadyExists` when another file has the name now.
/// Between the check and the removal another process can still swap the
/// name; nothing short of the file system's cooperation closes that.
pub(crate) fn remove_if_same(
    dir: BorrowedFd<'_>,
    name: &OsStr,
    ident: Ident,
    dir_entry: bool,
) -> io::Result<bool> {
    match lookup_at(dir, name)? {
        None => Ok(false),
        Some(meta) if meta.ident == ident => {
            let flags = if dir_entry {
                AtFlags::REMOVEDIR
            } else {
                AtFlags::empty()
            };
            match rustix::fs::unlinkat(dir, name, flags) {
                Ok(()) => Ok(true),
                Err(rustix::io::Errno::NOENT) => Ok(false),
                Err(err) => Err(err.into()),
            }
        }
        Some(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "another file has taken the name",
        )),
    }
}

/// The longest file name a directory takes, in bytes: `None` when the
/// system does not say.
pub(crate) fn name_max(fd: BorrowedFd<'_>) -> Option<usize> {
    probe(None);
    let found = rustix::fs::fstatvfs(fd).ok()?;
    usize::try_from(found.f_namemax).ok().filter(|&max| max > 0)
}
