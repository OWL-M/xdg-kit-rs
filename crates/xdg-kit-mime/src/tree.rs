//! Content types of mounted volumes from tree magic (spec "Content types
//! for volumes", "The treemagic files").

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

use rustix::fs::{Access, AtFlags, Dir, DirEntry, FileType, Mode, OFlags};
use rustix::io::Errno;

use crate::model::{TreeKind, TreeMatchlet};
use crate::work::count;
use crate::{MimeDatabase, MimeType};

/// Options for [`MimeDatabase::content_types_for_root`]. Each field limits the
/// search for one rule path. [`TreeOptions::default`] supplies the defaults
/// described below; `None` means no limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct TreeOptions {
    /// The lookups one rule path's search may take, 10,000 by default;
    /// past that the rule path does not match. Listing a folder counts as
    /// one lookup, as does looking for an exact name with `match-case`.
    /// Opening each entry as a folder, or checking it at the last
    /// component, also counts. Opening a folder again on the way back does
    /// not (see [`TreeOptions::max_reopens`]), so the lookups taken do not
    /// depend on how many handles the search keeps open.
    ///
    /// Remembering folders that led nowhere bounds the search by folders ×
    /// path components. This limit also bounds searches without mount IDs
    /// (Linux before 5.8 and other Unixes), where that cannot work, and
    /// searches on volumes made to defeat it. `None`: there, a search can
    /// take time exponential in the rule path's components (each matched
    /// by every spelling that differs only in case).
    pub max_lookups: Option<usize>,
    /// The maximum folder reopens while backtracking along one rule path, after
    /// their handles have been closed. Exceeding it makes the rule path fail to
    /// match. Defaults to 6 per default lookup: the crate's tests check that
    /// searches within the default lookup limit (at most 4,999 folders deep)
    /// need no more, and so find the same paths as one that keeps every
    /// folder open. Raising the lookup limit may exhaust reopens first and
    /// miss matches; `None` removes that restriction.
    pub max_reopens: Option<usize>,
}

impl Default for TreeOptions {
    fn default() -> Self {
        Self {
            max_lookups: Some(MAX_LOOKUPS),
            max_reopens: Some(MAX_REOPENS),
        }
    }
}

impl TreeOptions {
    /// No limit: a search where folders cannot be remembered, or on a
    /// volume made to defeat that, can take time exponential in the rule
    /// path's components.
    #[must_use]
    pub fn unlimited() -> Self {
        Self {
            max_lookups: None,
            max_reopens: None,
        }
    }
}

/// The default of [`TreeOptions::max_lookups`].
const MAX_LOOKUPS: usize = 10_000;

/// The default of [`TreeOptions::max_reopens`]. With its handles kept as
/// [`Window`] says, a search within the default lookups (at most 4,999
/// folders deep, as each folder entered takes two) opens folders again at
/// most 6 times for each lookup after the first. Tests of this bound:
/// `the_reopen_cap_is_the_bound_for_the_default_lookups` (its formula gives
/// 6 at that depth), `small_volumes_stay_within_the_bound_with_few_handles`
/// (every volume of up to 9 folders, with 2 to 6 handles),
/// `the_simulation_reopens_as_the_walk_does` (the simulation those use,
/// against real walks), and, for real walks of up to 1,700 levels,
/// `going_back_past_dead_ends_stays_within_the_bound`,
/// `going_back_past_deep_dead_ends_finds_what_unlimited_handles_find` and
/// `climbing_back_level_by_level_finds_what_unlimited_handles_find`.
const MAX_REOPENS: usize = 6 * MAX_LOOKUPS;

/// The most folder handles one rule path's search holds open at a time,
/// one being opened among them; going back to a folder whose handle was
/// closed opens it again. Besides these: the volume root the search is
/// given, the handle a folder's listing holds while it is read (see
/// [`list_folder`]), and those `accept` opens while it checks an entry (a
/// listing, a file's contents), each closed before the search goes on. The
/// bound on reopening ([`MAX_REOPENS`]) holds within the default lookups
/// with 2 more handles than the most times 2 divides a depth reached (12
/// times, for 4,096): 14.
const MAX_OPEN_FOLDERS: usize = 16;

/// Opens `path` relative to `dir` as a folder, following symbolic links.
/// Tries read access first. On `EACCES` or `EPERM`, retries with
/// [`LOOKUPS_ONLY`] so paths through searchable but unreadable folders
/// still work, as they do when listing fails (see [`find_entries`]).
/// On systems without a lookup-only flag, returns the original error.
fn open_folder(dir: BorrowedFd<'_>, path: &Path) -> io::Result<OwnedFd> {
    let flags = OFlags::DIRECTORY | OFlags::CLOEXEC;
    let opened = rustix::fs::openat(dir, path, OFlags::RDONLY | flags, Mode::empty());
    if let (Err(Errno::ACCESS | Errno::PERM), Some(lookups_only)) = (&opened, LOOKUPS_ONLY) {
        return Ok(rustix::fs::openat(
            dir,
            path,
            lookups_only | flags,
            Mode::empty(),
        )?);
    }
    Ok(opened?)
}

/// The flag that opens a folder for lookups only: `O_PATH` on Linux and
/// Android, which needs no permission on the folder itself.
#[cfg(any(target_os = "linux", target_os = "android"))]
const LOOKUPS_ONLY: Option<OFlags> = Some(OFlags::PATH);

/// The flag that opens a folder for lookups only: `O_SEARCH`, which needs
/// permission to search the folder but not to list it. rustix does not
/// have it; these are the systems the `libc` crate defines it for.
#[cfg(any(
    target_vendor = "apple",
    target_os = "aix",
    target_os = "cygwin",
    target_os = "emscripten",
    target_os = "freebsd",
    target_os = "fuchsia",
    target_os = "illumos",
    target_os = "netbsd",
    target_os = "solaris",
))]
const LOOKUPS_ONLY: Option<OFlags> = Some(OFlags::from_bits_retain(libc::O_SEARCH as u32));

/// Elsewhere there is no such flag: a folder that cannot be listed cannot
/// be opened.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "aix",
    target_os = "cygwin",
    target_os = "emscripten",
    target_os = "freebsd",
    target_os = "fuchsia",
    target_os = "illumos",
    target_os = "netbsd",
    target_os = "solaris",
)))]
const LOOKUPS_ONLY: Option<OFlags> = None;

/// The entries of the folder `path` from `dir`, links followed. Opened for
/// reading, which needs permission to list the folder. With `path` `.`, it
/// lists `dir` itself, whether `dir` was opened for reading or for lookups
/// only.
fn list_folder(dir: BorrowedFd<'_>, path: &Path) -> rustix::io::Result<Dir> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    Dir::new(rustix::fs::openat(dir, path, flags, Mode::empty())?)
}

/// The entries of `folder` itself (see [`list_folder`]).
fn list_here(folder: BorrowedFd<'_>) -> rustix::io::Result<Dir> {
    list_folder(folder, Path::new("."))
}

/// Whether `name` is `.` or `..`, which a listing holds but no path names.
fn is_dot(name: &[u8]) -> bool {
    matches!(name, b"." | b"..")
}

/// Whether `name` in `folder` is a folder that has an entry (see
/// [`lists_an_entry`]).
fn has_entries(folder: BorrowedFd<'_>, name: &OsStr) -> bool {
    fn name_of(entry: &DirEntry) -> &[u8] {
        entry.file_name().to_bytes()
    }
    list_folder(folder, Path::new(name)).is_ok_and(|entries| lists_an_entry(entries, name_of))
}

/// Whether `listing` contains an entry other than `.` and `..`, using
/// `name` to read each item's name. Failed reads do not count as entries:
/// errors below the root do not match, as in `GLib`. A folder whose listing
/// fails therefore does not satisfy `non-empty`.
fn lists_an_entry<T, E>(
    listing: impl IntoIterator<Item = Result<T, E>>,
    name: impl Fn(&T) -> &[u8],
) -> bool {
    listing
        .into_iter()
        .any(|entry| entry.is_ok_and(|entry| !is_dot(name(&entry))))
}

/// Finds entries in `folder` named `name`. With `match_case`, looks up the
/// exact name. Otherwise, uses `list` to compare names after lowercasing each
/// UTF-8 character independently (`ΑΣ` becomes `ασ`, never `ας`), or
/// lowercasing ASCII for non-UTF-8 names. Both `DCIM` and `dcim` can match. If
/// the folder is searchable but not readable (`EACCES` or `EPERM`), looks up
/// `name` as written because other spellings cannot be listed. Other listing
/// errors return no entries.
fn find_entries(
    folder: BorrowedFd<'_>,
    name: &OsStr,
    match_case: bool,
    list: &dyn Fn(BorrowedFd<'_>) -> rustix::io::Result<Dir>,
) -> Vec<OsString> {
    let look_up = || {
        rustix::fs::statat(folder, name, AtFlags::SYMLINK_NOFOLLOW)
            .is_ok()
            .then(|| name.to_owned())
            .into_iter()
            .collect()
    };
    if match_case {
        return look_up();
    }
    let entries = match list(folder) {
        Ok(entries) => entries,
        Err(Errno::ACCESS | Errno::PERM) => return look_up(),
        Err(_) => return Vec::new(),
    };
    let wanted = crate::database::lowercase(name.as_bytes());
    entries
        .flatten()
        .filter_map(|entry| {
            let entry = entry.file_name().to_bytes();
            (crate::database::lowercase(entry) == wanted)
                .then(|| OsStr::from_bytes(entry).to_owned())
        })
        .collect()
}

/// Returns whether any entry named by the rule path under `root` passes
/// `accept`. The callback receives the entry's name and its open parent
/// folder. If a component matches several entries case-insensitively,
/// tries each in turn, as `GLib` does, until one leads to an accepted entry.
/// Rule paths with `..` or a leading `/` never match. An empty rule path
/// names the root itself; `accept` receives `root_path` relative to the
/// current folder.
///
/// Each step looks up one name in the folder the step before opened, links
/// followed as in a path, so neither the limit on links followed in one
/// lookup nor the limit on path length depends on the way to a folder.
/// The search from a folder depends only on that folder within its mount:
/// on what the mount shows under it, and on `accept` judging one name in
/// one folder, which may depend on the mount (its `noexec` flag, for an
/// executable check) but not on the way there. A folder reached again on
/// the same mount (through a link, or a name differing only in case) at a
/// position where it already led nowhere is not searched again, so work
/// stays bounded by folders × path components. This holds for the folder
/// holding the last component too, as `accept` looks at a name in it, not
/// at a path. A lookup that failed with an error is remembered like any
/// other: by another way it would fail the same, short of a passing error
/// such as running out of file descriptors. A folder whose mount cannot be
/// told is not remembered (see [`Identity::place`]). After
/// [`TreeOptions::max_lookups`] lookups (listings, and entries opened or
/// checked) the search stops and the rule path does not match.
///
/// The search holds at most [`MAX_OPEN_FOLDERS`] handles regardless of
/// depth. While backtracking, it reopens closed folders from their
/// ancestors ([`Window`]). Reopening does not count as a lookup and has
/// its own limit, [`TreeOptions::max_reopens`]. Within the default
/// lookups, the handles kept stay within the default reopens as far as
/// the tests named at [`MAX_REOPENS`] check, so the search then finds
/// every path that a search with unlimited handles would find within
/// the same lookups. If reopening finds a different folder (see
/// [`Walk::from`]), the rule path does not match.
/// This works with or without mount IDs.
fn any_path(
    root: BorrowedFd<'_>,
    root_path: &Path,
    rule_path: &str,
    match_case: bool,
    options: &TreeOptions,
    accept: &mut impl FnMut(BorrowedFd<'_>, &OsStr) -> bool,
) -> bool {
    let mut components: Vec<&OsStr> = Vec::new();
    for component in Path::new(rule_path).components() {
        match component {
            Component::Normal(name) => components.push(name),
            Component::CurDir => {}
            Component::RootDir | Component::ParentDir | Component::Prefix(_) => return false,
        }
    }
    if components.is_empty() {
        return accept(rustix::fs::CWD, root_path.as_os_str());
    }
    // No limit is as good as one that cannot be reached.
    Walk {
        lookups: options.max_lookups.unwrap_or(usize::MAX),
        reopens: options.max_reopens.unwrap_or(usize::MAX),
        ..Walk::new(&components, match_case, accept)
    }
    .from(root, 0)
}

/// Where a folder is: (mount ID, device, inode).
type Place = (u64, rustix::fs::Dev, u64);

/// Which folder an open handle is: its device and inode, and its birth
/// time and mount ID where `statx` reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Identity {
    device: rustix::fs::Dev,
    inode: u64,
    /// Seconds and nanoseconds.
    born: Option<(i64, u32)>,
    mount: Option<u64>,
}

impl Identity {
    /// Checks whether the reopened folder `self` may be the original
    /// `first`. Compares the device and, when both report it, the mount ID.
    /// If the file system keeps inode numbers (`numbers`), also compares
    /// the inode and, when both report it, the birth time. Opening it as a
    /// folder already guarantees its type.
    ///
    /// On file systems that make up inode numbers, skips the birth time
    /// too. exFAT keeps a new folder's birth time at nanosecond precision
    /// until its inode is released, but stores it at 10 ms precision on
    /// disk. Comparing those values would reject an unchanged folder.
    /// These checks are best effort: without a birth time, a folder removed
    /// and recreated with the same inode is accepted as the original. Where
    /// inode numbers are made up, any folder on the same mount is accepted.
    fn may_be(&self, first: &Self, numbers: InodeNumbers) -> bool {
        fn agree<T: PartialEq>(one: Option<T>, other: Option<T>) -> bool {
            one.zip(other).is_none_or(|(one, other)| one == other)
        }
        self.device == first.device
            && agree(self.mount, first.mount)
            && (numbers == InodeNumbers::MadeUp
                || (self.inode == first.inode && agree(self.born, first.born)))
    }

    /// Where the folder is, to remember it by, if its mount is known. The
    /// device and inode alone are not enough: a bind mount shows the folder
    /// of its source with other mounts under it, or none, and with its own
    /// flags (`noexec`). The device stays, as one btrfs mount shows each
    /// subvolume as a device of its own, with inodes repeated across them.
    ///
    /// The memo built on it assumes inode numbers unique within a mount: a
    /// FUSE driver whose inode numbers are not stable could make two folders
    /// look like one, and a path that is there be missed. Where the kernel
    /// makes them up ([`InodeNumbers::MadeUp`]), a folder may get a new
    /// number once its handles are closed, and so be searched again; a
    /// number given up is given again only once a 32-bit counter has come
    /// round.
    ///
    /// `None` when `statx` does not report the mount ID (Linux before 5.8)
    /// or fails, and on other Unixes.
    fn place(&self) -> Option<Place> {
        self.mount.map(|mount| (mount, self.device, self.inode))
    }
}

/// Which folder `folder` is: from one `statx` on Linux and Android where it
/// works, else from `fstat`, which every Unix has (device and inode only).
/// The inode, mount and birth time do not change, so attributes a network
/// file system has cached are good enough.
fn folder_identity(folder: BorrowedFd<'_>) -> Option<Identity> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use rustix::fs::StatxFlags;
        // rustix's libc backend has the flag only on linux-gnu.
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        let flags = AtFlags::EMPTY_PATH | AtFlags::STATX_DONT_SYNC;
        #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
        let flags = AtFlags::EMPTY_PATH;
        let wanted = StatxFlags::INO | StatxFlags::BTIME | StatxFlags::MNT_ID;
        if let Ok(found) = rustix::fs::statx(folder, "", flags, wanted)
            && let reported = StatxFlags::from_bits_retain(found.stx_mask)
            && reported.contains(StatxFlags::INO)
        {
            let born = (found.stx_btime.tv_sec, found.stx_btime.tv_nsec);
            return Some(Identity {
                device: rustix::fs::makedev(found.stx_dev_major, found.stx_dev_minor),
                inode: found.stx_ino,
                born: reported.contains(StatxFlags::BTIME).then_some(born),
                mount: reported
                    .contains(StatxFlags::MNT_ID)
                    .then_some(found.stx_mnt_id),
            });
        }
    }
    let stat = rustix::fs::fstat(folder).ok()?;
    Some(Identity {
        device: stat.st_dev,
        inode: stat.st_ino,
        born: None,
        mount: None,
    })
}

/// Whether a file system keeps the inode number of a folder that does not
/// change, which tells a folder opened again ([`Identity::may_be`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InodeNumbers {
    /// A folder's number is its own for as long as it is there.
    Kept,
    /// A folder's number is made up each time the kernel builds its inode,
    /// so it changes once the folder's handles are closed and the inode is
    /// let go, though the folder does not.
    MadeUp,
}

/// Whether the file system of type `fs_type` (`f_type` from `fstatfs`,
/// widened to be one type on every target) keeps inode numbers. Linux
/// (7.2) makes them up on FAT (`vfat`, `msdos`) and exFAT, for every file
/// and folder (`iunique` in `fat_build_inode`, `exfat_build_inode`); on a
/// virtual machine's shared folders, `vboxsf` (`vboxsf_new_inode`, at each
/// lookup); and in `/proc`, for the folders of processes and those under
/// `/proc/sys` (`get_next_ino`). An SMB mount with `noserverino` makes them
/// up too, but its type does not tell, so it keeps the full check.
#[cfg(any(target_os = "linux", target_os = "android", test))]
fn inode_numbers(fs_type: i128) -> InodeNumbers {
    /// `MSDOS_SUPER_MAGIC`, `EXFAT_SUPER_MAGIC`, `VBOXSF_SUPER_MAGIC`,
    /// `PROC_SUPER_MAGIC`.
    const MADE_UP: [i128; 4] = [0x4d44, 0x2011_bab0, 0x786f_4256, 0x9fa0];
    if MADE_UP.contains(&fs_type) {
        InodeNumbers::MadeUp
    } else {
        InodeNumbers::Kept
    }
}

/// Whether the file system `folder` is on keeps inode numbers, from one
/// `fstatfs` ([`inode_numbers`]); kept where that fails.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn file_system_inode_numbers(folder: BorrowedFd<'_>) -> InodeNumbers {
    rustix::fs::fstatfs(folder).map_or(InodeNumbers::Kept, |found| {
        inode_numbers(i128::from(found.f_type))
    })
}

/// Whether the file system `folder` is on keeps inode numbers: taken as
/// kept, as the numbers [`inode_numbers`] knows are Linux's, so the full
/// check stays.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn file_system_inode_numbers(_folder: BorrowedFd<'_>) -> InodeNumbers {
    InodeNumbers::Kept
}

/// One rule path's search.
struct Walk<'a, F> {
    components: &'a [&'a OsStr],
    match_case: bool,
    /// Which folder a handle is ([`folder_identity`] but in tests).
    identity: &'a dyn Fn(BorrowedFd<'_>) -> Option<Identity>,
    /// Where a folder is, given its identity ([`Identity::place`] but in
    /// tests).
    place: &'a dyn Fn(BorrowedFd<'_>, Option<&Identity>) -> Option<Place>,
    /// A folder's entries ([`list_here`] but in tests).
    list: &'a dyn Fn(BorrowedFd<'_>) -> rustix::io::Result<Dir>,
    /// Whether the file system a folder is on keeps inode numbers
    /// ([`file_system_inode_numbers`] but in tests).
    file_system: &'a dyn Fn(BorrowedFd<'_>) -> InodeNumbers,
    /// What `file_system` gave for each device, read when a folder on it
    /// is first opened again.
    volumes: HashMap<rustix::fs::Dev, InodeNumbers>,
    /// Places and component indexes of folders whose search from that
    /// component found nothing.
    failed: HashSet<(Place, usize)>,
    /// Lookups left ([`TreeOptions::max_lookups`]; `usize::MAX` for no
    /// limit).
    lookups: usize,
    /// Folders left to open again ([`TreeOptions::max_reopens`];
    /// `usize::MAX` for no limit).
    reopens: usize,
    /// The most handles open at a time ([`MAX_OPEN_FOLDERS`] but in
    /// tests).
    most_open: usize,
    accept: &'a mut F,
}

/// A folder on the way down one rule path's search.
struct Level {
    /// The index of the component looked up in it.
    index: usize,
    /// The name it was opened by from the folder before it (none for the
    /// first).
    name: OsString,
    /// Where it is, if that can be told.
    place: Option<Place>,
    /// Which folder it is, recorded when it was first opened, to check it
    /// when it is opened again.
    identity: Option<Identity>,
    /// Its handle, while open. The first folder's is the one the search
    /// was given, never closed, and not held here.
    handle: Option<OwnedFd>,
    /// The entries found for the component still to try, last first.
    left: Vec<OsString>,
}

/// What came of entering a folder.
enum Entered {
    /// Its entries for the component, last first, where it is and which
    /// folder it is.
    Listed(Option<Place>, Option<Identity>, Vec<OsString>),
    /// It already led nowhere from that component.
    Failed,
    /// No lookup was left.
    Stopped,
}

/// The folder handles one search holds open, besides the first folder's,
/// and which of them to close when another one is needed.
///
/// Folders are told by their depth in the search's stack, the first folder
/// at 0. A depth's rank is the number of times 2 divides it, at most
/// [`Window::top_rank`]; the handle at depth `d` of rank `r` covers the
/// depths `d` to `d + 2^r - 1` (it is a checkpoint for them). Going back to
/// a folder whose handle was closed opens folders again from the deepest
/// handle covering it whose shallower handles covering it are all open,
/// and keeps the handles of the ones covering it ([`Walk::reopen`]). A handle
/// is closed only to make room for another, and never while near the
/// folder the search is at ([`Window::near`]); of the others, first one
/// whose folder has no names left to try, which the search never goes back
/// to. This bounds the folders opened again by the lookups (see
/// [`MAX_REOPENS`]).
struct Window {
    /// The depths of the folders whose handles are open, shallowest first.
    open: Vec<usize>,
    /// The most handles open at a time, one being opened among them; at
    /// least 2.
    most: usize,
}

/// Whether `distance` is less than 2 to the power `bits`.
fn below(distance: usize, bits: u32) -> bool {
    distance.checked_shr(bits).is_none_or(|high| high == 0)
}

impl Window {
    fn new(most: usize) -> Self {
        Window {
            open: Vec::new(),
            most: most.max(2),
        }
    }

    /// The highest rank, `most` - 2: at most one handle of each rank is
    /// near a depth, so with the one a folder is being opened from, those
    /// near it leave room for one more.
    fn top_rank(&self) -> u32 {
        u32::try_from(self.most - 2).unwrap_or(u32::MAX)
    }

    /// The rank of `depth`.
    fn rank(&self, depth: usize) -> u32 {
        depth.trailing_zeros().min(self.top_rank())
    }

    /// Whether the handle at `at` covers `depth`: `depth` is at or after
    /// `at`, by less than 2 to the power of `at`'s rank.
    fn covers(&self, at: usize, depth: usize) -> bool {
        at <= depth && below(depth - at, self.rank(at))
    }

    /// Whether the handle at `at` is near `depth`: `depth` is at or after
    /// `at` by less than twice what `at` covers (once, for the top rank).
    fn near(&self, at: usize, depth: usize) -> bool {
        let rank = self.rank(at);
        let reach = if rank < self.top_rank() {
            rank + 1
        } else {
            rank
        };
        at <= depth && below(depth - at, reach)
    }

    /// Whether the handle at `at` is open; the first folder's always is.
    fn is_open(&self, at: usize) -> bool {
        at == 0 || self.open.binary_search(&at).is_ok()
    }

    /// Where going back to `depth` starts: the deepest handle covering it
    /// such that it and every shallower one covering it are open, else the
    /// first folder (0). The handles covering a depth are at the depth, and
    /// at it with its lowest bit cleared, again and again, down to one of
    /// the top rank.
    fn start(&self, depth: usize) -> usize {
        let mut covering = Vec::new();
        let mut at = depth;
        while at != 0 {
            covering.push(at);
            if at.trailing_zeros() >= self.top_rank() {
                break;
            }
            at &= at - 1;
        }
        covering
            .into_iter()
            .rev()
            .take_while(|&at| self.is_open(at))
            .last()
            .unwrap_or(0)
    }

    /// Records the handle at `at` as open.
    fn insert(&mut self, at: usize) {
        if let Err(index) = self.open.binary_search(&at) {
            self.open.insert(index, at);
        }
    }

    /// Picks a handle to close, forgets it and gives its depth: neither
    /// near `depth`, where the search is, nor at `from`, which a folder is
    /// being opened from. Of those, one whose folder has no names left to
    /// try (`live` false) if any, else the lowest rank, the shallowest of
    /// equals. With `most` handles open there always is one; `None` if not.
    fn close_one(
        &mut self,
        depth: usize,
        from: usize,
        live: impl Fn(usize) -> bool,
    ) -> Option<usize> {
        let (index, _) = self
            .open
            .iter()
            .enumerate()
            .filter(|&(_, &at)| at != from && !self.near(at, depth))
            .min_by_key(|&(_, &at)| (live(at), self.rank(at), at))?;
        Some(self.open.remove(index))
    }
}

impl<'a, F: FnMut(BorrowedFd<'_>, &OsStr) -> bool> Walk<'a, F> {
    /// The search for `components`, compared as `match_case` says, that
    /// gives the entries it finds to `accept`.
    fn new(components: &'a [&'a OsStr], match_case: bool, accept: &'a mut F) -> Self {
        Walk {
            components,
            match_case,
            identity: &folder_identity,
            place: &|_, identity| identity.and_then(Identity::place),
            list: &list_here,
            file_system: &file_system_inode_numbers,
            volumes: HashMap::new(),
            failed: HashSet::new(),
            lookups: MAX_LOOKUPS,
            reopens: MAX_REOPENS,
            most_open: MAX_OPEN_FOLDERS,
            accept,
        }
    }

    /// Takes one lookup; `false` when none is left.
    fn look(&mut self) -> bool {
        let left = self.lookups > 0;
        self.lookups = self.lookups.saturating_sub(1);
        left
    }

    /// Enters `folder` for the component at `index`: tells which folder it
    /// is, checks the memo, then finds the entries the component names in
    /// it (one lookup).
    fn enter(&mut self, folder: BorrowedFd<'_>, index: usize) -> Entered {
        let identity = (self.identity)(folder);
        let place = (self.place)(folder, identity.as_ref());
        if place.is_some_and(|place| self.failed.contains(&(place, index))) {
            return Entered::Failed;
        }
        if !self.look() {
            return Entered::Stopped;
        }
        let mut entries = find_entries(folder, self.components[index], self.match_case, self.list);
        entries.reverse();
        Entered::Listed(place, identity, entries)
    }

    /// Whether the components from `index` on (at least one), looked up
    /// in `folder`, lead to an entry that passes.
    ///
    /// Depth first, on a stack of [`Level`]s rather than of calls, so
    /// that a rule path of any depth needs neither a deep call stack nor
    /// more than [`MAX_OPEN_FOLDERS`] handles besides `folder`: going one
    /// folder deeper with that many open closes one ([`Window`]). Going
    /// back to a folder whose handle was closed opens it again
    /// ([`Walk::reopen`]), with folders before it, name by name, each open
    /// counted against [`MAX_REOPENS`] (not the lookups), and checks that
    /// each is the same folder as when it was first opened
    /// ([`Identity::may_be`]). Each is opened by the same names from the
    /// same folder, so device and inode tell it, whether or not the mount
    /// can be told; the birth time and mount ID are compared too where
    /// they are known, and otherwise matter only to remember failures
    /// across different ways ([`Identity::place`]). On a file system that
    /// makes up inode numbers ([`InodeNumbers`]), compares only the device
    /// and mount ID, skipping both the inode and birth time. If a reopened
    /// folder differs from the original, the volume changed during the
    /// search. This or a missing name makes the rule path fail to match.
    /// Keeping these folders' handles open instead would require an
    /// unbounded number of handles.
    fn from(&mut self, folder: BorrowedFd<'_>, index: usize) -> bool {
        let (place, left) = match self.enter(folder, index) {
            Entered::Listed(place, _, left) => (place, left),
            Entered::Failed | Entered::Stopped => return false,
        };
        let mut stack = vec![Level {
            index,
            name: OsString::new(),
            place,
            identity: None,
            handle: None,
            left,
        }];
        let mut window = Window::new(self.most_open);
        while let Some(level) = stack.last_mut() {
            let Some(entry) = level.left.pop() else {
                if let Some(place) = level.place {
                    self.failed.insert((place, level.index));
                }
                // Every handle kept is the handle of a folder on the stack,
                // at its depth, and the deepest kept is the last in
                // `window.open`.
                if stack.pop().is_some_and(|level| level.handle.is_some()) {
                    let closed = window.open.pop();
                    debug_assert_eq!(closed, Some(stack.len()), "the handle kept");
                } else {
                    debug_assert!(stack.is_empty() || !window.is_open(stack.len()));
                }
                continue;
            };
            let index = level.index + 1;
            let last = index == self.components.len();
            if !self.look() {
                return false;
            }
            let Some(at) = self.reopen(folder, &mut stack, &mut window, !last) else {
                return false;
            };
            if last {
                if (self.accept)(at, &entry) {
                    return true;
                }
                continue;
            }
            let Ok(next) = open_folder(at, Path::new(&entry)) else {
                continue;
            };
            match self.enter(next.as_fd(), index) {
                Entered::Listed(place, identity, left) => {
                    debug_assert!(window.open.last().is_none_or(|&at| at < stack.len()));
                    window.open.push(stack.len());
                    stack.push(Level {
                        index,
                        name: entry,
                        place,
                        identity,
                        handle: Some(next),
                        left,
                    });
                }
                Entered::Failed => {}
                Entered::Stopped => return false,
            }
        }
        false
    }

    /// Opens the deepest folder in `stack` again if its handle was closed,
    /// and with `room`, leaves room for one more handle. Gives the deepest
    /// folder's handle; `None` when it cannot be opened again: no reopen
    /// left, a name gone, a folder opened again that cannot be told (no
    /// identity, then or when first opened) or is another, or no handle
    /// that may be closed (which [`Window`] rules out).
    ///
    /// The folders are opened again one by one from the handle
    /// [`Window::start`] gives; the handles of those covering the deepest
    /// folder are kept, the others closed once the next is open. Handles
    /// already open on the way are used as they are.
    fn reopen<'s>(
        &mut self,
        first: BorrowedFd<'s>,
        stack: &'s mut [Level],
        window: &mut Window,
        room: bool,
    ) -> Option<BorrowedFd<'s>> {
        let depth = stack.len() - 1;
        if depth > 0 && stack[depth].handle.is_none() {
            // The handle of the folder just opened again when not kept: the
            // next is opened from it.
            let mut passing: Option<OwnedFd> = None;
            for at in window.start(depth) + 1..=depth {
                if stack[at].handle.is_some() {
                    passing = None;
                    continue;
                }
                while window.open.len() + usize::from(passing.is_some()) >= window.most {
                    let closed =
                        window.close_one(depth, at - 1, |at| !stack[at].left.is_empty())?;
                    let kept = stack[closed].handle.take();
                    debug_assert!(kept.is_some(), "a handle kept is open");
                }
                self.reopens = self.reopens.checked_sub(1)?;
                let again = {
                    let from = match (&passing, at - 1) {
                        (Some(passing), _) => passing.as_fd(),
                        (None, 0) => first,
                        (None, before) => {
                            debug_assert!(window.is_open(before), "opened from a handle kept");
                            stack[before].handle.as_ref()?.as_fd()
                        }
                    };
                    open_folder(from, Path::new(&stack[at].name)).ok()?
                };
                let identity = (self.identity)(again.as_fd())?;
                let numbers = *self
                    .volumes
                    .entry(identity.device)
                    .or_insert_with(|| (self.file_system)(again.as_fd()));
                if !stack[at]
                    .identity
                    .is_some_and(|recorded| identity.may_be(&recorded, numbers))
                {
                    return None;
                }
                if window.covers(at, depth) {
                    stack[at].handle = Some(again);
                    window.insert(at);
                    passing = None;
                } else {
                    passing = Some(again);
                }
            }
        }
        while room && window.open.len() >= window.most {
            let closed = window.close_one(depth, depth, |at| !stack[at].left.is_empty())?;
            let kept = stack[closed].handle.take();
            debug_assert!(kept.is_some(), "a handle kept is open");
        }
        let stack: &'s [Level] = stack;
        Some(stack[depth].handle.as_ref().map_or(first, AsFd::as_fd))
    }
}

impl MimeDatabase {
    fn tree_matchlet_matches(
        &self,
        root: BorrowedFd<'_>,
        root_path: &Path,
        matchlet: &TreeMatchlet,
        options: &TreeOptions,
    ) -> bool {
        // The rule path, split into its components; the lookups themselves
        // are file I/O, bounded per rule path by `options.max_lookups`.
        count(1 + matchlet.path.len());
        any_path(
            root,
            root_path,
            &matchlet.path,
            matchlet.match_case,
            options,
            &mut |folder, name| self.entry_passes(folder, name, matchlet),
        ) && (matchlet.children.is_empty()
            || matchlet
                .children
                .iter()
                .any(|child| self.tree_matchlet_matches(root, root_path, child, options)))
    }

    /// Whether `name` in `folder` has the kind, permission, content and
    /// type `matchlet` asks for.
    fn entry_passes(&self, folder: BorrowedFd<'_>, name: &OsStr, matchlet: &TreeMatchlet) -> bool {
        let Ok(link) = rustix::fs::statat(folder, name, AtFlags::SYMLINK_NOFOLLOW) else {
            return false;
        };
        let target_is = |kind| {
            rustix::fs::statat(folder, name, AtFlags::empty())
                .is_ok_and(|target| FileType::from_raw_mode(target.st_mode) == kind)
        };
        let kind_matches = match matchlet.kind {
            TreeKind::Any => true,
            TreeKind::Link => FileType::from_raw_mode(link.st_mode) == FileType::Symlink,
            TreeKind::File => target_is(FileType::RegularFile),
            TreeKind::Directory => target_is(FileType::Directory),
        };
        if !kind_matches {
            return false;
        }
        if matchlet.executable
            && rustix::fs::accessat(folder, name, Access::EXEC_OK, AtFlags::empty()).is_err()
        {
            return false;
        }
        if matchlet.non_empty && !has_entries(folder, name) {
            return false;
        }
        if let Some(wanted) = &matchlet.mimetype {
            let found = self.type_in(folder, name);
            if !found.is_ok_and(|found| found.mime == self.canonical(wanted)) {
                return false;
            }
        }
        true
    }

    /// The `x-content/*` types of the volume mounted at `root`, highest
    /// priority first (ties: the more important folder, then file order),
    /// each once.
    ///
    /// Each rule's paths are looked up under `root` one component at a time,
    /// case-insensitively unless the rule says `match-case`. Rule paths with
    /// `..` or a leading `/` never match (the database reports them), but
    /// symbolic links on the volume are followed like any other path. A
    /// rule path whose search takes more than [`TreeOptions::max_lookups`]
    /// lookups (folder listings, and entries opened or checked) does not
    /// match. A search keeps at most 16 folders open, and opens a closed one
    /// again when it goes back to it, which is not counted as a lookup but
    /// against [`TreeOptions::max_reopens`]: it chooses which to keep so
    /// that, within the default limits, it finds what it would find with
    /// every folder kept open. A rule path whose folders change while it is
    /// searched does not match.
    ///
    /// A path is matched where it is present, whether or not the folders
    /// on the way can be listed: in a folder that can only be searched, a
    /// component is looked up as the rule spells it, and other spellings
    /// are not seen. That is on Linux and Android (`O_PATH`), and on the
    /// systems that have `O_SEARCH` (FreeBSD, NetBSD, macOS and Apple's
    /// other systems, illumos, Solaris, AIX, Cygwin, Fuchsia, Emscripten);
    /// elsewhere the folders on the way must be readable.
    ///
    /// # Errors
    ///
    /// When `root` cannot be opened as a folder. A root that can be
    /// searched but not listed is opened all the same where folders on the
    /// way can be (above); elsewhere it is an error.
    pub fn content_types_for_root(
        &self,
        root: impl AsRef<Path>,
        options: &TreeOptions,
    ) -> io::Result<Vec<MimeType>> {
        let root_path = root.as_ref();
        let root = open_folder(rustix::fs::CWD, root_path)?;
        let mut types: Vec<MimeType> = Vec::new();
        let mut kept: HashSet<MimeType> = HashSet::new();
        for ranked in &self.merged.tree {
            let rule = &ranked.rule;
            let mime = self.canonical(&rule.mime);
            // Hashed and compared.
            count(1 + 2 * mime.as_str().len());
            if kept.contains(&mime) {
                continue;
            }
            if rule.matchlets.iter().any(|matchlet| {
                self.tree_matchlet_matches(root.as_fd(), root_path, matchlet, options)
            }) {
                // Copied, hashed and compared.
                count(3 * mime.as_str().len());
                kept.insert(mime.clone());
                types.push(mime);
            }
        }
        Ok(types)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::path::PathBuf;

    use super::*;

    /// A folder or file by device and inode.
    type Id = (rustix::fs::Dev, u64);

    fn id(stat: &rustix::fs::Stat) -> Id {
        (stat.st_dev, stat.st_ino)
    }

    fn open(path: &Path) -> OwnedFd {
        open_folder(rustix::fs::CWD, path).unwrap()
    }

    /// The entries at `paths` as `accept` is given them, sorted: the folder
    /// each is in (links followed) and its name.
    fn entries(paths: &[&Path]) -> Vec<(Id, OsString)> {
        let mut entries: Vec<_> = paths
            .iter()
            .map(|path| {
                let folder = rustix::fs::stat(path.parent().unwrap()).unwrap();
                (id(&folder), path.file_name().unwrap().to_owned())
            })
            .collect();
        entries.sort();
        entries
    }

    /// The entries `accept` is given, sorted.
    fn paths(root: &Path, rule_path: &str, match_case: bool) -> Vec<(Id, OsString)> {
        let mut found = Vec::new();
        any_path(
            open(root).as_fd(),
            root,
            rule_path,
            match_case,
            &TreeOptions::default(),
            &mut |folder, name| {
                found.push((id(&rustix::fs::fstat(folder).unwrap()), name.to_owned()));
                false
            },
        );
        found.sort();
        found
    }

    #[test]
    fn components_are_found_case_insensitively_unless_asked() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("DCIM/Photos")).unwrap();
        assert_eq!(
            paths(root.path(), "dcim/photos", false),
            entries(&[&root.path().join("DCIM/Photos")])
        );
        assert_eq!(paths(root.path(), "dcim", true), []);
        assert_eq!(
            paths(root.path(), "DCIM", true),
            entries(&[&root.path().join("DCIM")])
        );
        assert_eq!(paths(root.path(), "../etc", false), []);
        assert_eq!(paths(root.path(), "/etc", false), []);
        assert_eq!(paths(root.path(), "missing", false), []);
    }

    #[test]
    fn components_are_lowercased_per_character() {
        // Lowercased with context, `ΑΣ` would end in a final `ς`.
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("ΑΣ")).unwrap();
        assert_eq!(
            paths(root.path(), "ασ", false),
            entries(&[&root.path().join("ΑΣ")])
        );
        assert_eq!(paths(root.path(), "ας", false), []);
    }

    #[test]
    fn every_entry_differing_only_in_case_is_tried() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("DCIM")).unwrap();
        fs::create_dir_all(root.path().join("dcim/photos")).unwrap();
        assert_eq!(
            paths(root.path(), "dcim/photos", false),
            entries(&[&root.path().join("dcim/photos")]),
            "the empty DCIM does not hide dcim/photos"
        );
        assert_eq!(
            paths(root.path(), "DCIM", false),
            entries(&[&root.path().join("DCIM"), &root.path().join("dcim")])
        );
        // A constraint that the first entry fails is tried on the next.
        assert!(any_path(
            open(root.path()).as_fd(),
            root.path(),
            "DCIM",
            false,
            &TreeOptions::default(),
            &mut |folder, name| has_entries(folder, name)
        ));
    }

    /// Two links to the volume root that differ only in case: each `a/`
    /// component doubles the ways down, all to the same folder.
    fn root_reached_both_ways(depth: usize) -> (tempfile::TempDir, String) {
        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(".", root.path().join("a")).unwrap();
        std::os::unix::fs::symlink(".", root.path().join("A")).unwrap();
        (root, "a/".repeat(depth))
    }

    /// The most folders the memo lets the search for the 22 `a/`
    /// components place: each of the 23 positions is reached by two ways
    /// (one for the root), and searched from once; without the memo, 2^23
    /// times. A folder opened again on the way back is not placed again:
    /// it is checked by device and inode.
    const MOST_PLACED: usize = 2 * 23;

    /// Where a folder is: [`Identity::place`] where it tells, else the
    /// device and inode alone, all on one mount, so that a test runs on any
    /// Unix, with or without `statx`.
    fn any_place(folder: BorrowedFd<'_>, identity: Option<&Identity>) -> Option<Place> {
        let identity = identity.copied().or_else(|| folder_identity(folder))?;
        Some((identity.mount.unwrap_or(0), identity.device, identity.inode))
    }

    #[test]
    fn a_folder_searched_in_vain_is_not_searched_again() {
        let (root, prefix) = root_reached_both_ways(22);
        let rule_path = format!("{prefix}missing");
        let components: Vec<&OsStr> = Path::new(&rule_path).iter().collect();
        let mut accept = |_: BorrowedFd<'_>, _: &OsStr| true;
        // Where a folder is, failing as soon as more folders are placed
        // than the memo allows.
        let placed = Cell::new(0);
        let counted_place = |folder: BorrowedFd<'_>, identity: Option<&Identity>| {
            placed.set(placed.get() + 1);
            assert!(
                placed.get() <= MOST_PLACED,
                "{} folders placed",
                placed.get()
            );
            any_place(folder, identity)
        };
        let mut walk = Walk {
            place: &counted_place,
            ..Walk::new(&components, false, &mut accept)
        };
        assert!(!walk.from(open(root.path()).as_fd(), 0));
    }

    #[test]
    fn one_folder_on_two_mounts_is_searched_on_each() {
        // A bind mount shows its source's folder (same device and inode)
        // with other entries under it. `bind` and `source` stand for the
        // two mounts: different mount IDs, the same device and inodes.
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("bind/sub")).unwrap();
        fs::create_dir_all(root.path().join("source/sub")).unwrap();
        fs::write(root.path().join("source/sub/file"), "").unwrap();
        let inode = |path: &str| rustix::fs::stat(root.path().join(path)).unwrap().st_ino;
        let on_bind = [inode("bind"), inode("bind/sub")];
        let subs = [inode("bind/sub"), inode("source/sub")];
        let inode_of = |folder: BorrowedFd<'_>| rustix::fs::fstat(folder).unwrap().st_ino;
        let mount = |folder: BorrowedFd<'_>| u64::from(on_bind.contains(&inode_of(folder)));
        let inode = |folder: BorrowedFd<'_>| u64::from(subs.contains(&inode_of(folder)));
        let components = [OsStr::new("sub"), OsStr::new("file")];
        let search = |place: &dyn Fn(BorrowedFd<'_>, Option<&Identity>) -> Option<Place>| {
            let mut accept = |_: BorrowedFd<'_>, _: &OsStr| true;
            let mut walk = Walk {
                place,
                ..Walk::new(&components, true, &mut accept)
            };
            (
                walk.from(open(&root.path().join("bind")).as_fd(), 0),
                walk.from(open(&root.path().join("source")).as_fd(), 0),
            )
        };
        assert_eq!(
            search(&|folder, _| Some((mount(folder), 1, inode(folder)))),
            (false, true)
        );
        // Told apart by device and inode alone, the source would not be
        // searched again.
        assert_eq!(
            search(&|folder, _| Some((0, 1, inode(folder)))),
            (false, false)
        );
        // A folder whose mount cannot be told is not remembered.
        assert_eq!(search(&|_, _| None), (false, true));
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn folders_are_placed_on_linux() {
        use std::os::unix::fs::PermissionsExt;

        /// Gives the folder back its usual mode when dropped (before
        /// `root`, also on a panic), so that `root` can be removed.
        struct Readable<'a>(&'a Path);
        impl Drop for Readable<'_> {
            fn drop(&mut self) {
                let _ = fs::set_permissions(self.0, fs::Permissions::from_mode(0o755));
            }
        }

        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(".", root.path().join("link")).unwrap();
        // A folder that can be read is opened for reading.
        let folder = open(root.path());
        let flags = rustix::fs::fcntl_getfl(&folder).unwrap();
        assert!(!flags.contains(OFlags::PATH), "{flags:?}");
        // One that can only be searched, for lookups only.
        let search_only = root.path().join("search-only");
        fs::create_dir(&search_only).unwrap();
        let _readable = Readable(&search_only);
        fs::set_permissions(&search_only, fs::Permissions::from_mode(0o111)).unwrap();
        if fs::read_dir(&search_only).is_ok() {
            eprintln!("folder permissions are not enforced here (root?): O_PATH not checked");
        } else {
            let flags = rustix::fs::fcntl_getfl(open(&search_only)).unwrap();
            assert!(flags.contains(OFlags::PATH), "{flags:?}");
        }
        // `statx` works on either handle.
        let place =
            |folder: BorrowedFd<'_>| folder_identity(folder).and_then(|found| found.place());
        let Some(placed) = place(folder.as_fd()) else {
            eprintln!("statx reports no mount ID here (Linux before 5.8?): not checked");
            return;
        };
        assert_eq!(place(open(&root.path().join("link")).as_fd()), Some(placed));
        // Opened again, a folder has the same identity, the mount ID the
        // place has.
        let identity = folder_identity(folder.as_fd()).unwrap();
        assert_eq!(identity.mount, Some(placed.0));
        assert_eq!(
            folder_identity(open(&root.path().join("link")).as_fd()),
            Some(identity)
        );
    }

    #[test]
    fn matches_through_links_and_case_variants_are_still_found() {
        let (root, prefix) = root_reached_both_ways(22);
        fs::write(root.path().join("Target"), "").unwrap();
        // Any of the routes will do: which comes first is the file
        // system's order.
        assert!(any_path(
            open(root.path()).as_fd(),
            root.path(),
            &format!("{prefix}target"),
            false,
            &TreeOptions::default(),
            &mut |_, name| name == "Target"
        ));
        // One case variant leads to an empty folder, the other through a
        // link to the one that holds the path. File systems list entries
        // by name or by age, so both are swapped: in some of the four, the
        // empty one is tried first.
        let cases = [("DCIM", "dcim"), ("dcim", "DCIM")]
            .into_iter()
            .flat_map(|names| [(names, true), (names, false)]);
        for ((empty, linked), empty_first) in cases {
            let root = tempfile::tempdir().unwrap();
            fs::create_dir_all(root.path().join("full/photos")).unwrap();
            let make_empty = || fs::create_dir(root.path().join(empty)).unwrap();
            if empty_first {
                make_empty();
            }
            std::os::unix::fs::symlink("full", root.path().join(linked)).unwrap();
            if !empty_first {
                make_empty();
            }
            let is_folder = |folder: BorrowedFd<'_>, name: &OsStr| {
                rustix::fs::statat(folder, name, AtFlags::empty()).is_ok_and(|found| {
                    FileType::from_raw_mode(found.st_mode) == FileType::Directory
                })
            };
            assert!(
                any_path(
                    open(root.path()).as_fd(),
                    root.path(),
                    "dcim/photos",
                    false,
                    &TreeOptions::default(),
                    &mut |folder, name| is_folder(folder, name)
                ),
                "{linked} links to the photos"
            );
            assert_eq!(
                paths(root.path(), "dcim/photos", false),
                entries(&[&root.path().join(linked).join("photos")])
            );
        }
    }

    #[test]
    fn an_empty_rule_path_names_the_root_by_its_path() {
        let volume = tempfile::tempdir().unwrap();
        let root = volume.path().join("link");
        std::os::unix::fs::symlink(".", &root).unwrap();
        for rule_path in ["", "."] {
            let mut given = Vec::new();
            any_path(
                open(&root).as_fd(),
                &root,
                rule_path,
                false,
                &TreeOptions::default(),
                &mut |folder, name| {
                    given.push(name.to_owned());
                    rustix::fs::statat(folder, name, AtFlags::SYMLINK_NOFOLLOW).is_ok_and(|found| {
                        FileType::from_raw_mode(found.st_mode) == FileType::Symlink
                    })
                },
            );
            assert_eq!(given, [root.as_os_str()], "{rule_path:?}");
        }
    }

    #[test]
    fn the_types_of_a_root_are_gathered_in_linear_work() {
        // `n` rules of their own types, each matching `f` at the root.
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("f"), "").unwrap();
        let run = |n: usize| {
            let mut text = b"MIME-TreeMagic\0\n".to_vec();
            for at in 0..n {
                text.extend_from_slice(format!("[50:x-content/t{at}]\n>\"f\"=file\n").as_bytes());
            }
            let db = MimeDatabase::from_files(&[vec![("treemagic", &text)]]);
            let (found, steps) = crate::work::steps(|| {
                db.content_types_for_root(root.path(), &TreeOptions::default())
            });
            assert_eq!(found.unwrap().len(), n);
            (steps, text.len())
        };
        // Per rule: a few steps and its path, and its type made canonical
        // (hashed and compared), looked up among the kept, then kept
        // (copied, hashed and compared): seven steps per byte of the type,
        // under half of each rule's bytes.
        crate::work::assert_linear(1_000, run, |items, bytes| 2 * (3 * items + 7 * bytes / 2));
    }

    #[test]
    fn a_search_stops_after_its_lookups() {
        // `x/y/z/file` takes eight lookups: each folder's listing, and the
        // entry found there.
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("x/y/z")).unwrap();
        fs::write(root.path().join("x/y/z/file"), "").unwrap();
        let components = ["x", "y", "z", "file"].map(OsStr::new);
        let search = |lookups| {
            let mut accept = |_: BorrowedFd<'_>, _: &OsStr| true;
            let mut walk = Walk {
                lookups,
                ..Walk::new(&components, false, &mut accept)
            };
            let found = walk.from(open(root.path()).as_fd(), 0);
            (found, walk.lookups)
        };
        assert_eq!(search(8), (true, 0));
        assert_eq!(search(7), (false, 0));
        assert_eq!(search(MAX_LOOKUPS), (true, MAX_LOOKUPS - 8));

        // Without the memo, the 2^23 ways down 22 `a/` components stop
        // when the lookups run out, and nothing is found.
        let (root, prefix) = root_reached_both_ways(22);
        let rule_path = format!("{prefix}missing");
        let components: Vec<&OsStr> = Path::new(&rule_path).iter().collect();
        let mut accept = |_: BorrowedFd<'_>, _: &OsStr| true;
        let mut walk = Walk {
            place: &|_, _| None,
            lookups: 1_000,
            ..Walk::new(&components, false, &mut accept)
        };
        assert!(!walk.from(open(root.path()).as_fd(), 0));
        assert_eq!(walk.lookups, 0);
    }

    /// How many open file descriptors refer to `root` or something under
    /// it.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn open_under(root: &Path) -> usize {
        fs::read_dir("/proc/self/fd")
            .unwrap()
            .flatten()
            .filter(|fd| fs::read_link(fd.path()).is_ok_and(|target| target.starts_with(root)))
            .count()
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn a_deep_rule_path_needs_neither_a_deep_stack_nor_many_handles() {
        // 100 real folders `n`, then `a`, a link to the last of them, and
        // `f` in it.
        let volume = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(volume.path()).unwrap();
        let nested = "n/".repeat(100);
        let bottom = root.join(&nested);
        fs::create_dir_all(&bottom).unwrap();
        std::os::unix::fs::symlink(".", bottom.join("a")).unwrap();
        fs::write(bottom.join("f"), "").unwrap();
        let search = |links: usize| {
            let rule_path = format!("{nested}{}f", "a/".repeat(links));
            let root = root.clone();
            // A small stack, which a walk going one call deeper per
            // component would overflow.
            std::thread::Builder::new()
                .stack_size(256 * 1024)
                .spawn(move || {
                    let components: Vec<&OsStr> = Path::new(&rule_path).iter().collect();
                    // Handles open when a folder is placed (just opened)
                    // or an entry checked, and while a folder is listed.
                    let peak = Cell::new(0);
                    let listing_peak = Cell::new(0);
                    let sample = |peak: &Cell<usize>| peak.set(peak.get().max(open_under(&root)));
                    let place = |folder: BorrowedFd<'_>, identity: Option<&Identity>| {
                        sample(&peak);
                        any_place(folder, identity)
                    };
                    let list = |folder: BorrowedFd<'_>| {
                        let listing = list_here(folder);
                        sample(&listing_peak);
                        listing
                    };
                    let mut accept = |folder: BorrowedFd<'_>, name: &OsStr| {
                        sample(&peak);
                        rustix::fs::statat(folder, name, AtFlags::empty()).is_ok()
                    };
                    let mut walk = Walk {
                        place: &place,
                        list: &list,
                        ..Walk::new(&components, false, &mut accept)
                    };
                    let found = walk.from(open(&root).as_fd(), 0);
                    (found, walk.lookups, peak.get(), listing_peak.get())
                })
                .unwrap()
                .join()
                .unwrap()
        };
        // At most the root and the handles kept, the one just opened
        // among them; while a folder is listed, also the listing's own
        // handle, closed when the listing ends.
        let most = (MAX_OPEN_FOLDERS + 1, MAX_OPEN_FOLDERS + 2);
        // 3,001 components, two lookups each (the listing, the one entry).
        let (found, left, peak, listing_peak) = search(2_900);
        assert_eq!((found, left), (true, MAX_LOOKUPS - 6_002));
        assert_eq!((peak, listing_peak), most, "handles open");
        // 6,001 components: the lookups run out on the way down.
        let (found, left, peak, listing_peak) = search(5_900);
        assert_eq!((found, left), (false, 0));
        assert_eq!((peak, listing_peak), most, "handles open");
    }

    /// `top/s/` then 30 `a/` then `f`, on a volume where `top` holds `s`
    /// and `S`: the one listed first leads, through `a` (a link to
    /// itself), to no `f`; the other to one. So the walk goes back to
    /// `top`, whose handle was closed if it keeps two (see
    /// [`two_handles`]).
    fn branching_volume() -> (tempfile::TempDir, String, Vec<u64>) {
        let root = tempfile::tempdir().unwrap();
        let top = root.path().join("top");
        fs::create_dir_all(top.join("s")).unwrap();
        fs::create_dir_all(top.join("S")).unwrap();
        let listed: Vec<_> = fs::read_dir(&top)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        for path in &listed {
            std::os::unix::fs::symlink(".", path.join("a")).unwrap();
        }
        fs::write(listed[1].join("f"), "").unwrap();
        let inode = |path: &Path| rustix::fs::stat(path).unwrap().st_ino;
        let inodes = vec![inode(&top), inode(&listed[0]), inode(&listed[1])];
        (root, format!("top/s/{}f", "a/".repeat(30)), inodes)
    }

    /// How a test walk differs from [`Walk::new`]'s.
    struct Setup<'a> {
        place: &'a dyn Fn(BorrowedFd<'_>, Option<&Identity>) -> Option<Place>,
        identity: &'a dyn Fn(BorrowedFd<'_>) -> Option<Identity>,
        file_system: &'a dyn Fn(BorrowedFd<'_>) -> InodeNumbers,
        reopens: usize,
        most_open: usize,
    }

    impl Default for Setup<'_> {
        fn default() -> Self {
            Setup {
                place: &any_place,
                identity: &folder_identity,
                file_system: &file_system_inode_numbers,
                reopens: MAX_REOPENS,
                most_open: MAX_OPEN_FOLDERS,
            }
        }
    }

    /// A walk that keeps two handles: the one of the folder it is at, and
    /// one being opened. Each step back opens a folder again.
    fn two_handles<'a>() -> Setup<'a> {
        Setup {
            most_open: 2,
            ..Setup::default()
        }
    }

    /// The walk of `rule_path` from `root` set up as `setup` says, for an
    /// entry that is there: whether it found one, the lookups it took and
    /// the folders it opened again.
    fn walk_with(root: &Path, rule_path: &str, setup: &Setup<'_>) -> (bool, usize, usize) {
        let components: Vec<&OsStr> = Path::new(rule_path).iter().collect();
        let mut accept = |folder: BorrowedFd<'_>, name: &OsStr| {
            rustix::fs::statat(folder, name, AtFlags::empty()).is_ok()
        };
        let mut walk = Walk {
            place: setup.place,
            identity: setup.identity,
            file_system: setup.file_system,
            reopens: setup.reopens,
            most_open: setup.most_open,
            ..Walk::new(&components, false, &mut accept)
        };
        let found = walk.from(open(root).as_fd(), 0);
        (
            found,
            MAX_LOOKUPS - walk.lookups,
            setup.reopens - walk.reopens,
        )
    }

    #[test]
    fn going_back_to_a_closed_folder_reopens_it() {
        let (root, rule_path, inodes) = branching_volume();
        let top_placed = Cell::new(0);
        let place = |folder: BorrowedFd<'_>, identity: Option<&Identity>| {
            if rustix::fs::fstat(folder).unwrap().st_ino == inodes[0] {
                top_placed.set(top_placed.get() + 1);
            }
            any_place(folder, identity)
        };
        // Two lookups for each of the 33 components on the way down the
        // first branch but the last (one listing, nothing found), then the
        // second branch: its entry, and two for each of its 31 components.
        // `top`, whose handle was closed, is opened again from the root,
        // which is not a lookup.
        let setup = Setup {
            place: &place,
            ..two_handles()
        };
        assert_eq!(
            walk_with(root.path(), &rule_path, &setup),
            (true, 2 * 32 + 1 + 1 + 2 * 31, 1)
        );
        assert_eq!(
            top_placed.get(),
            1,
            "top placed when first opened, not when reopened"
        );
    }

    #[test]
    fn a_closed_folder_is_reopened_where_mounts_cannot_be_told() {
        // No folder placed (no mount ID: Linux before 5.8, other Unixes):
        // the walk goes back to `top` all the same, as in
        // `going_back_to_a_closed_folder_reopens_it`.
        let (root, rule_path, _) = branching_volume();
        let setup = Setup {
            place: &|_, _| None,
            ..two_handles()
        };
        assert_eq!(
            walk_with(root.path(), &rule_path, &setup),
            (true, 2 * 32 + 1 + 1 + 2 * 31, 1)
        );
    }

    #[test]
    fn a_folder_reopened_elsewhere_ends_the_walk() {
        let (root, rule_path, inodes) = branching_volume();
        let top = root.path().join("top");
        // While the walk is down the first branch, `top` is moved away and
        // another put in its place, with `f` under both branches: going
        // back, the walk opens the new `top` by the same name, another
        // folder, and stops there.
        let swapped = Cell::new(false);
        let place = |folder: BorrowedFd<'_>, identity: Option<&Identity>| {
            if !swapped.get() && rustix::fs::fstat(folder).unwrap().st_ino == inodes[1] {
                swapped.set(true);
                fs::rename(&top, root.path().join("old")).unwrap();
                for branch in ["s", "S"] {
                    fs::create_dir_all(top.join(branch)).unwrap();
                    std::os::unix::fs::symlink(".", top.join(branch).join("a")).unwrap();
                    fs::write(top.join(branch).join("f"), "").unwrap();
                }
            }
            any_place(folder, identity)
        };
        let setup = Setup {
            place: &place,
            ..two_handles()
        };
        assert!(!walk_with(root.path(), &rule_path, &setup).0);
        assert!(swapped.get());
    }

    #[test]
    fn a_folder_gone_on_the_way_back_ends_the_walk() {
        let (root, rule_path, inodes) = branching_volume();
        // While the walk is down the first branch, `top` is moved away:
        // going back, the walk cannot open it again, and stops there. The
        // links at the root lead to the moved branches, so that a walk
        // going on from the root instead would find `f`.
        let moved = Cell::new(false);
        let place = |folder: BorrowedFd<'_>, identity: Option<&Identity>| {
            if !moved.get() && rustix::fs::fstat(folder).unwrap().st_ino == inodes[1] {
                moved.set(true);
                fs::rename(root.path().join("top"), root.path().join("old")).unwrap();
                for branch in ["s", "S"] {
                    std::os::unix::fs::symlink(
                        Path::new("old").join(branch),
                        root.path().join(branch),
                    )
                    .unwrap();
                }
            }
            any_place(folder, identity)
        };
        let setup = Setup {
            place: &place,
            ..two_handles()
        };
        assert!(!walk_with(root.path(), &rule_path, &setup).0);
        assert!(moved.get());
    }

    /// Makes `s` and `S` in `folder`; gives them in the order the folder
    /// lists them.
    fn two(folder: &Path) -> (PathBuf, PathBuf) {
        for name in ["s", "S"] {
            fs::create_dir(folder.join(name)).unwrap();
        }
        let first = fs::read_dir(folder).unwrap().next().unwrap().unwrap();
        let other = if first.file_name() == "s" { "S" } else { "s" };
        (first.path(), folder.join(other))
    }

    /// `levels` levels, each holding `s` and `S`: the one listed first
    /// starts a dead end `dead` folders deep (`s/s/…`), the other holds the
    /// next level, and the last level holds `f`. The rule path, `s/`
    /// `levels` times then `f`, goes down each dead end before the way on.
    fn dead_ends(root: &Path, levels: usize, dead: usize) -> String {
        let mut level = root.to_owned();
        for _ in 0..levels {
            let (first, next) = two(&level);
            fs::create_dir_all(first.join("s/".repeat(dead - 1))).unwrap();
            level = next;
        }
        fs::write(level.join("f"), "").unwrap();
        format!("{}f", "s/".repeat(levels))
    }

    /// The root holds `s` and `S`. The one listed first starts a chain of
    /// `levels` folders, each but the last holding the next one and,
    /// listed after it, an empty folder; the other starts a plain chain of
    /// `levels` folders whose last holds `f`. The rule path, `s/` `levels`
    /// times then `f`, goes down the first chain, then back up it one level
    /// at a time, trying each empty folder on the way, and then down the
    /// second.
    fn climb(root: &Path, levels: usize) -> String {
        let (mut level, mut target) = two(root);
        for _ in 1..levels {
            level = two(&level).0;
            target = target.join("s");
            fs::create_dir(&target).unwrap();
        }
        fs::write(target.join("f"), "").unwrap();
        format!("{}f", "s/".repeat(levels))
    }

    /// Lets this process open at least `wanted` files at once, if the
    /// system allows it; whether it does.
    fn allow_open(wanted: u64) -> bool {
        use rustix::process::{Resource, getrlimit, setrlimit};
        let limit = getrlimit(Resource::Nofile);
        if limit.current.is_none_or(|current| current >= wanted) {
            return true;
        }
        let raised = rustix::process::Rlimit {
            current: limit.maximum,
            ..limit
        };
        setrlimit(Resource::Nofile, raised).is_ok()
            && limit.maximum.is_none_or(|most| most >= wanted)
    }

    /// Serializes tests that open many handles or would fail if none were
    /// available, preventing parallel tests from exhausting the shared
    /// open-file limit. Acquired before creating the test volume: removing
    /// a deep volume also needs a handle for each level.
    static MANY_HANDLES: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn many_handles() -> std::sync::MutexGuard<'static, ()> {
        MANY_HANDLES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The walk of the rule path that `volume` makes in a new temporary
    /// folder, `levels` levels then `f` (as [`dead_ends`] and [`climb`]
    /// do), with [`MAX_OPEN_FOLDERS`] handles and with as many as it
    /// wants: they find the same and take the same lookups, and the first
    /// stays within the bound on reopens ([`MAX_REOPENS`]). Gives whether
    /// it found the path, its lookups and its reopens; `None` where the
    /// system does not allow the handles the second needs, and then the
    /// volume is not made (it could not be removed either: removing it
    /// holds a handle for each level). Callers hold [`many_handles`].
    fn against_unlimited(
        levels: usize,
        volume: impl FnOnce(&Path, usize) -> String,
    ) -> Option<(bool, usize, usize)> {
        let components = levels + 1;
        if !allow_open(u64::try_from(components).unwrap() + 64) {
            eprintln!("cannot open {components} files at once here: not compared");
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let rule_path = volume(root, levels);
        let rule_path = rule_path.as_str();
        assert_eq!(Path::new(rule_path).iter().count(), components);
        let unlimited = Setup {
            most_open: usize::MAX,
            ..two_handles()
        };
        let (found, lookups, reopens) = walk_with(root, rule_path, &unlimited);
        assert_eq!(reopens, 0);
        let limited = walk_with(root, rule_path, &Setup::default());
        assert_eq!(
            (limited.0, limited.1),
            (found, lookups),
            "as with unlimited handles"
        );
        let bound = MAX_REOPENS / MAX_LOOKUPS * (lookups - 1);
        assert!(
            limited.2 <= bound,
            "{} reopens, more than {bound}",
            limited.2
        );
        Some(limited)
    }

    #[test]
    fn a_handle_whose_folder_has_no_names_left_is_closed_first() {
        // Handles at 1 (rank 0) and 2 (rank 1), neither near 16. Of two
        // folders with names left, or two without, the lower rank is
        // closed; else the one with none left, whatever its rank.
        for (names_left, closed) in [
            ([true, true], 1),
            ([false, false], 1),
            ([true, false], 2),
            ([false, true], 1),
        ] {
            let mut window = Window::new(MAX_OPEN_FOLDERS);
            window.insert(1);
            window.insert(2);
            let live = |at: usize| names_left[at - 1];
            assert_eq!(
                window.close_one(16, 16, live),
                Some(closed),
                "{names_left:?}"
            );
            assert_eq!(window.open, [3 - closed]);
        }
    }

    #[test]
    fn going_back_past_deep_dead_ends_finds_what_unlimited_handles_find() {
        // 1,600 levels with a dead end two folders deep before each: six
        // lookups a level. The walk never goes back to a folder whose
        // handle it closed: leaving a dead end, it goes back to the level
        // it started from, near and kept, and the levels before have no
        // names left. (Which handles are closed first is checked in
        // `a_handle_whose_folder_has_no_names_left_is_closed_first`.)
        let _many = many_handles();
        let two_deep = |root: &Path, levels| dead_ends(root, levels, 2);
        let found = against_unlimited(1_600, two_deep);
        assert!(
            found.is_none_or(|found| found == (true, 9_600, 0)),
            "{found:?}"
        );
        // 1,700 levels: the lookups run out, with unlimited handles too.
        let found = against_unlimited(1_700, two_deep);
        assert!(
            found.is_none_or(|(found, lookups, _)| !found && lookups == MAX_LOOKUPS),
            "{found:?}"
        );
    }

    #[test]
    fn climbing_back_level_by_level_finds_what_unlimited_handles_find() {
        // Every level on the way down has a name left to try, so the walk
        // goes back to each, with at most 16 handles for 1,600 levels.
        let _many = many_handles();
        let found = against_unlimited(1_600, climb);
        assert!(found.is_none_or(|(found, _, _)| found), "{found:?}");
        let found = against_unlimited(1_700, climb);
        assert!(
            found.is_none_or(|(found, lookups, _)| !found && lookups == MAX_LOOKUPS),
            "{found:?}"
        );
    }

    #[test]
    fn going_back_past_dead_ends_stays_within_the_bound() {
        // 150 levels with a dead end 20 folders deep before each. (Closing
        // the handle leaving the smallest gap would take 403 reopens here,
        // and closing the shallowest 9,045.)
        let _many = many_handles();
        let found = against_unlimited(150, |root, levels| dead_ends(root, levels, 20));
        assert!(found.is_none_or(|(found, _, _)| found));
    }

    #[test]
    fn a_search_stops_after_its_reopens() {
        // `top` is opened again once (see
        // `going_back_to_a_closed_folder_reopens_it`).
        let (root, rule_path, _) = branching_volume();
        let search = |reopens| {
            let setup = Setup {
                reopens,
                ..two_handles()
            };
            let (found, _, reopened) = walk_with(root.path(), &rule_path, &setup);
            (found, reopened)
        };
        assert_eq!(search(1), (true, 1));
        assert_eq!(search(0), (false, 0));
    }

    #[test]
    fn a_folder_reopened_with_another_birth_time_or_mount_ends_the_walk() {
        let (root, rule_path, inodes) = branching_volume();
        let born = Some((1, 0));
        // What `top` gives when first opened, then when opened again, and
        // whether the walk goes on.
        let cases = [
            ((born, Some(7)), (born, Some(7)), true),
            ((born, Some(7)), (Some((2, 0)), Some(7)), false),
            ((born, Some(7)), (born, Some(8)), false),
            // Not told on either side: device and inode alone.
            ((born, Some(7)), (None, None), true),
            ((None, None), (Some((2, 0)), Some(8)), true),
        ];
        for (first, again, goes_on) in cases {
            let opened = Cell::new(0);
            let identity = |folder: BorrowedFd<'_>| {
                let mut identity = folder_identity(folder)?;
                if identity.inode == inodes[0] {
                    opened.set(opened.get() + 1);
                    (identity.born, identity.mount) = if opened.get() == 1 { first } else { again };
                }
                Some(identity)
            };
            let setup = Setup {
                identity: &identity,
                ..two_handles()
            };
            let (found, _, reopened) = walk_with(root.path(), &rule_path, &setup);
            assert_eq!((found, reopened), (goes_on, 1), "{first:?} then {again:?}");
            assert_eq!(opened.get(), 2);
        }
    }

    #[test]
    fn only_file_systems_known_to_make_up_inode_numbers_are_told_apart() {
        // FAT (`vfat`, `msdos`), exFAT, VirtualBox shared folders, `/proc`.
        for fs_type in [0x4d44, 0x2011_bab0, 0x786f_4256, 0x9fa0] {
            assert_eq!(inode_numbers(fs_type), InodeNumbers::MadeUp, "{fs_type:#x}");
        }
        // ext2/3/4, btrfs, XFS, tmpfs, ISO 9660, UDF, NTFS (ntfs3), FUSE,
        // SMB, NFS, and a number no file system has.
        for fs_type in [
            0xef53,
            0x9123_683e,
            0x5846_5342,
            0x0102_1994,
            0x9660,
            0x1501_3346,
            0x7366_746e,
            0x6573_5546,
            0xfe53_4d42,
            0x6969,
            -1,
        ] {
            assert_eq!(inode_numbers(fs_type), InodeNumbers::Kept, "{fs_type:#x}");
        }
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn the_file_system_type_is_read_on_linux() {
        // `/proc` makes up the numbers of its folders; a temporary folder's
        // file system (tmpfs, ext4, btrfs…) keeps them.
        assert_eq!(
            file_system_inode_numbers(open(Path::new("/proc")).as_fd()),
            InodeNumbers::MadeUp
        );
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            file_system_inode_numbers(open(root.path()).as_fd()),
            InodeNumbers::Kept
        );
    }

    #[test]
    fn a_folder_with_a_made_up_inode_number_is_told_by_device_and_mount() {
        let first = Identity {
            device: 1,
            inode: 10,
            born: Some((1, 123_456_789)),
            mount: Some(7),
        };
        let renumbered = Identity { inode: 11, ..first };
        assert!(renumbered.may_be(&first, InodeNumbers::MadeUp));
        assert!(!renumbered.may_be(&first, InodeNumbers::Kept));
        assert!(first.may_be(&first, InodeNumbers::Kept));
        // exFAT keeps a new folder's birth time at nanosecond precision
        // until its inode is released, but stores it at 10 ms precision on
        // disk. Skip the comparison so this rounding does not cause a mismatch.
        let reread = Identity {
            born: Some((1, 120_000_000)),
            ..renumbered
        };
        assert!(reread.may_be(&first, InodeNumbers::MadeUp));
        // Where inode numbers are kept, the birth time is still compared.
        let same_inode = Identity {
            inode: 10,
            ..reread
        };
        assert!(!same_inode.may_be(&first, InodeNumbers::Kept));
        // The device and mount ID are still compared.
        for other in [
            Identity {
                device: 2,
                ..renumbered
            },
            Identity {
                mount: Some(8),
                ..renumbered
            },
        ] {
            assert!(!other.may_be(&first, InodeNumbers::MadeUp), "{other:?}");
        }
    }

    #[test]
    fn a_folder_reopened_with_a_made_up_inode_number_goes_on() {
        // `top` gets another inode number when opened again, as on FAT once
        // its inode was let go: the walk goes on on a file system known to
        // make them up (FAT, exFAT), and stops elsewhere (ext4).
        let (root, rule_path, inodes) = branching_volume();
        for (fs_type, goes_on) in [(0x4d44, true), (0x2011_bab0, true), (0xef53, false)] {
            let opened = Cell::new(0);
            let identity = |folder: BorrowedFd<'_>| {
                let mut identity = folder_identity(folder)?;
                if identity.inode == inodes[0] {
                    opened.set(opened.get() + 1);
                    identity.inode += 1_000_000 * opened.get();
                }
                Some(identity)
            };
            // Read once, when `top` is opened again.
            let asked = Cell::new(0);
            let file_system = |_: BorrowedFd<'_>| {
                asked.set(asked.get() + 1);
                inode_numbers(fs_type)
            };
            let setup = Setup {
                identity: &identity,
                file_system: &file_system,
                ..two_handles()
            };
            let (found, _, reopened) = walk_with(root.path(), &rule_path, &setup);
            assert_eq!((found, reopened), (goes_on, 1), "{fs_type:#x}");
            assert_eq!((opened.get(), asked.get()), (2, 1));
        }
    }

    #[test]
    fn only_a_folder_that_may_not_be_listed_is_looked_up_as_written() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("DCIM")).unwrap();
        let folder = open(root.path());
        for (error, found) in [
            (Errno::ACCESS, &["DCIM"][..]),
            (Errno::PERM, &["DCIM"]),
            (Errno::IO, &[]),
            (Errno::NOMEM, &[]),
        ] {
            assert_eq!(
                find_entries(folder.as_fd(), OsStr::new("DCIM"), false, &|_| Err(error)),
                found,
                "{error:?}"
            );
        }
        // Read, the listing finds every spelling.
        assert_eq!(
            find_entries(folder.as_fd(), OsStr::new("dcim"), false, &list_here),
            ["DCIM"]
        );
    }

    #[test]
    fn a_listing_that_fails_holds_no_entry() {
        fn name<'a>(entry: &'a &[u8]) -> &'a [u8] {
            entry
        }
        let listing = |items: Vec<Result<&'static [u8], Errno>>| items;
        assert!(!lists_an_entry(listing(vec![Err(Errno::IO)]), name));
        assert!(!lists_an_entry(
            listing(vec![Ok(b"."), Err(Errno::IO), Ok(b"..")]),
            name
        ));
        assert!(!lists_an_entry(listing(vec![Ok(b"."), Ok(b"..")]), name));
        assert!(lists_an_entry(
            listing(vec![Err(Errno::IO), Ok(b"a")]),
            name
        ));
    }

    /// A made-up folder, for [`simulate`]: the entries a component names
    /// in it, in the order listed, each a folder or (`None`) a name that is
    /// no folder.
    #[derive(Debug, Default)]
    struct Made {
        entries: Vec<Option<std::rc::Rc<Made>>>,
    }

    impl Made {
        /// How deep its deepest folder is below it.
        fn depth(&self) -> usize {
            self.entries
                .iter()
                .flatten()
                .map(|folder| folder.depth() + 1)
                .max()
                .unwrap_or(0)
        }
    }

    /// The made-up volume of the folder `folder` as it lists its entries.
    fn listed(folder: &Path) -> Made {
        let entries = fs::read_dir(folder)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                entry
                    .file_type()
                    .unwrap()
                    .is_dir()
                    .then(|| std::rc::Rc::new(listed(&entry.path())))
            })
            .collect();
        Made { entries }
    }

    /// What [`simulate`] gives: the depth of each step, the lookups taken,
    /// the folders opened again, and the most handles open at once.
    type Simulated = (Vec<usize>, usize, usize, usize);

    /// Whether the handle at `at` was far enough from `depth` to be closed:
    /// `depth` is at least twice what it covers after it, once at the top
    /// rank. Apart from [`Window::near`], which it checks.
    fn far(window: &Window, at: usize, depth: usize) -> bool {
        let rank = window.rank(at);
        let past = if rank < window.top_rank() { 2 } else { 1 };
        depth >= at && (depth - at).checked_shr(rank).unwrap_or(0) >= past
    }

    /// The walk of [`Walk::from`] over `volume`, for a rule path one
    /// component longer than the volume is deep, with handles kept as
    /// [`Window`] says for `most`, but no files: each step is one lookup,
    /// each folder entered one more. Checks where going back starts and what
    /// it costs, and that a handle closed is far from the walk, as it goes.
    fn simulate(volume: &Made, most: usize, lookups: usize) -> Simulated {
        struct Level<'v> {
            folder: &'v Made,
            next: usize,
            open: bool,
        }
        let live = |level: &Level<'_>| level.next < level.folder.entries.len();
        let components = volume.depth() + 1;
        let mut stack = vec![Level {
            folder: volume,
            next: 0,
            open: false,
        }];
        let mut window = Window::new(most);
        let (mut steps, mut used, mut reopens, mut peak) = (Vec::new(), 1, 0, 0);
        while let Some(level) = stack.last_mut() {
            let folder: &Made = level.folder;
            let Some(entry) = folder.entries.get(level.next) else {
                if stack.pop().is_some_and(|level| level.open) {
                    window.open.pop();
                }
                continue;
            };
            level.next += 1;
            let depth = stack.len() - 1;
            let last = depth + 1 == components;
            if used == lookups {
                break;
            }
            used += 1;
            steps.push(depth);
            // As `Walk::reopen`.
            if depth > 0 && !stack[depth].open {
                // The depths covering this one, the shallowest closed among
                // them, and the one before it, where going back starts.
                let covering: Vec<usize> =
                    (1..=depth).filter(|&at| window.covers(at, depth)).collect();
                let closed = covering.iter().position(|&at| !stack[at].open).unwrap();
                let start = closed.checked_sub(1).map_or(0, |before| covering[before]);
                assert_eq!(window.start(depth), start, "where going back starts");
                let (shallowest, rank) = (covering[closed], window.rank(covering[closed]));
                let cost = if rank < window.top_rank() {
                    (2 << rank) - 1
                } else {
                    shallowest + (1 << rank) - 1
                };
                let before = reopens;
                let mut passing = false;
                for at in window.start(depth) + 1..=depth {
                    if stack[at].open {
                        passing = false;
                        continue;
                    }
                    while window.open.len() + usize::from(passing) >= window.most {
                        let closed = window
                            .close_one(depth, at - 1, |at| live(&stack[at]))
                            .unwrap();
                        assert!(far(&window, closed, depth), "closed far from the walk");
                        stack[closed].open = false;
                    }
                    reopens += 1;
                    peak = peak.max(window.open.len() + usize::from(passing) + 1);
                    if window.covers(at, depth) {
                        stack[at].open = true;
                        window.insert(at);
                        passing = false;
                    } else {
                        passing = true;
                    }
                }
                assert!(reopens - before <= cost, "what going back costs");
            }
            while !last && window.open.len() >= window.most {
                let closed = window
                    .close_one(depth, depth, |at| live(&stack[at]))
                    .unwrap();
                assert!(far(&window, closed, depth), "closed far from the walk");
                stack[closed].open = false;
            }
            let (false, Some(folder)) = (last, entry) else {
                continue;
            };
            if used == lookups {
                break;
            }
            used += 1;
            window.open.push(stack.len());
            stack.push(Level {
                folder: folder.as_ref(),
                next: 0,
                open: true,
            });
            peak = peak.max(window.open.len());
        }
        (steps, used, reopens, peak)
    }

    /// The most crossing points a depth from 1 to `deepest` has: the depths
    /// `z` that are a multiple of `2^(r + 1)`, for a rank `r` below `top`,
    /// with `z - 2^r` at least `2^r`, the depth at most `2^r` after `z`,
    /// and `z + 2^r` at most `deepest`.
    fn crossing_points(deepest: usize, top: u32) -> usize {
        (1..=deepest)
            .map(|depth| {
                let mut points: Vec<usize> = (0..top.min(usize::BITS - 2))
                    .filter_map(|rank| {
                        let (half, step) = (1 << rank, 2 << rank);
                        let point = (depth - 1) / step * step;
                        (point >= step && depth - point <= half && point + half <= deepest)
                            .then_some(point)
                    })
                    .collect();
                points.dedup();
                points.len()
            })
            .max()
            .unwrap_or(0)
    }

    /// Twice the most folders opened again for each lookup after the
    /// first, with `most` handles and depths up to `deepest`: twice the
    /// crossing points, plus, where the ranks are capped below the deepest
    /// folders, the deepest multiple of `2^top` less one.
    fn twice_per_lookup(most: usize, deepest: usize) -> usize {
        let top = Window::new(most).top_rank();
        let twice = 2 * crossing_points(deepest, top);
        match top.checked_add(1).and_then(|bits| 1usize.checked_shl(bits)) {
            Some(capped) if capped <= deepest => twice + (deepest >> top << top) - 1,
            _ => twice,
        }
    }

    #[test]
    fn the_reopen_cap_is_the_bound_for_the_default_lookups() {
        // A walk within its lookups goes at most this deep: the first
        // folder's listing is one lookup, and each folder entered two
        // (the step to it and its listing).
        let deepest = (MAX_LOOKUPS - 1) / 2;
        let top = Window::new(MAX_OPEN_FOLDERS).top_rank();
        // No rank is capped at that depth.
        assert!(deepest < 2 << top);
        assert_eq!(crossing_points(deepest, top), 6);
        assert_eq!(MAX_REOPENS, 6 * MAX_LOOKUPS);
        assert_eq!(twice_per_lookup(MAX_OPEN_FOLDERS, deepest), 12);
    }

    /// The made-up volumes of `folders` folders (counting the first), each
    /// holding at most `most` others and, after them, a name that is no
    /// folder or not (a folder holding no folders is entered and left at
    /// once either way, so it has none).
    fn volumes(folders: usize, most: usize) -> Vec<std::rc::Rc<Made>> {
        /// The ways to put `folders` folders in at most `most` rows, made
        /// from the volumes of each size in `made`.
        fn rows(
            folders: usize,
            most: usize,
            made: &[Vec<std::rc::Rc<Made>>],
        ) -> Vec<Vec<std::rc::Rc<Made>>> {
            if folders == 0 {
                return vec![Vec::new()];
            }
            if most == 0 {
                return Vec::new();
            }
            let mut all = Vec::new();
            for first in 1..=folders.min(made.len() - 1) {
                for folder in &made[first] {
                    for rest in rows(folders - first, most - 1, made) {
                        let mut row = vec![folder.clone()];
                        row.extend(rest);
                        all.push(row);
                    }
                }
            }
            all
        }
        let mut made = vec![Vec::new()];
        for size in 1..=folders {
            let mut all = Vec::new();
            for row in rows(size - 1, most, &made) {
                let tails: &[bool] = if row.is_empty() {
                    &[false]
                } else {
                    &[false, true]
                };
                for &tail in tails {
                    let mut entries: Vec<_> = row.iter().cloned().map(Some).collect();
                    if tail {
                        entries.push(None);
                    }
                    all.push(std::rc::Rc::new(Made { entries }));
                }
            }
            made.push(all);
        }
        made.pop().unwrap()
    }

    #[test]
    fn small_volumes_stay_within_the_bound_with_few_handles() {
        // Every volume of up to 9 folders, each holding up to 3, walked
        // with 2 to 6 handles: the same steps and lookups as with every
        // handle kept, the lookups cut short or not, never more handles
        // than allowed, and the reopens within the bound for that many
        // handles and that depth.
        let mut walked = 0;
        for folders in 1..=9 {
            for volume in volumes(folders, 3) {
                let deepest = volume.depth();
                let (steps, lookups, _, _) = simulate(&volume, usize::MAX, usize::MAX);
                for most in 2..=6 {
                    let (with_most, taken, reopens, peak) = simulate(&volume, most, usize::MAX);
                    assert_eq!((&with_most, taken), (&steps, lookups), "{volume:?}");
                    assert!(peak <= most);
                    assert!(
                        2 * reopens <= twice_per_lookup(most, deepest) * (lookups - 1),
                        "{reopens} reopens in {lookups} lookups with {most} handles: {volume:?}"
                    );
                    let cut = lookups / 2 + 1;
                    let short = simulate(&volume, most, cut);
                    assert_eq!(short.0, simulate(&volume, usize::MAX, cut).0);
                }
                walked += 1;
            }
        }
        assert_eq!(walked, 43_839);
    }

    #[test]
    fn the_simulation_reopens_as_the_walk_does() {
        // Random volumes on disk, their folders named `kk` in nine
        // spellings (`K` is also the Kelvin sign), names that are no
        // folders among them: the walk and its simulation over the same
        // listings open the same folders again, with 2, 3, 5 or 16 handles.
        let _many = many_handles();
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut random = |below: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % below
        };
        let spellings: Vec<String> = ["k", "K", "\u{212a}"]
            .iter()
            .flat_map(|one| ["k", "K", "\u{212a}"].map(|other| format!("{one}{other}")))
            .collect();
        for _ in 0..30 {
            let root = tempfile::tempdir().unwrap();
            // Grow the volume, depth first: each folder gets up to 3
            // names, folders mostly, down to a random depth, 150 folders
            // at most.
            let deepest = 3 + random(40);
            let mut budget = 150;
            let mut todo = vec![(root.path().to_owned(), 0)];
            while let Some((folder, depth)) = todo.pop() {
                let names = if depth < deepest {
                    1 + random(3)
                } else {
                    random(2)
                };
                for name in spellings.iter().take(usize::try_from(names).unwrap()) {
                    let path = folder.join(name);
                    if depth < deepest && budget > 0 && random(5) != 0 {
                        budget -= 1;
                        fs::create_dir(&path).unwrap();
                        todo.push((path, depth + 1));
                    } else {
                        fs::write(&path, "").unwrap();
                    }
                }
            }
            let volume = listed(root.path());
            let rule_path = vec!["kk"; volume.depth() + 1].join("/");
            let components: Vec<&OsStr> = Path::new(&rule_path).iter().collect();
            for most in [2, 3, 5, 16] {
                let mut accept = |_: BorrowedFd<'_>, _: &OsStr| false;
                let mut walk = Walk {
                    most_open: most,
                    ..Walk::new(&components, false, &mut accept)
                };
                assert!(!walk.from(open(root.path()).as_fd(), 0));
                let (_, lookups, reopens, _) = simulate(&volume, most, MAX_LOOKUPS);
                assert_eq!(
                    (MAX_LOOKUPS - walk.lookups, MAX_REOPENS - walk.reopens),
                    (lookups, reopens),
                    "{most} handles"
                );
            }
        }
    }
}
