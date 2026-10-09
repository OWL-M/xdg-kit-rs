//! Writes `mimeinfo.cache`, the file `update-desktop-database`
//! (desktop-file-utils) creates in an `applications` folder to list the
//! types listed by each desktop entry. The file is outside the MIME
//! Applications spec. This crate does not read it; it writes it for other
//! programs, including `GLib`, which uses it for a folder's associations.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags};
use rustix::io::Errno;
use xdg_kit_basedir::MAX_FILE_LEN;
use xdg_kit_desktop_entry::DesktopEntry;

use crate::TypePattern;

/// The `mimeinfo.cache` for the `applications` folder `folder`, as
/// `update-desktop-database` writes it: `[MIME Cache]`, then one
/// `type=id;id;` line per type, both in byte order, for every desktop entry
/// under the folder (sub-folders give IDs joined by `-`) that is not
/// deleted ([`DesktopEntry::hidden`]) and is in UTF-8
/// ([`DesktopEntry::supports_encoding`]), as the tool lists them. Unlike
/// `AppIndex`, entries of no or unknown `Type` are listed, as the tool does.
/// Items of `MimeType=` that are not types or `media/*` wildcards (see
/// [`TypePattern::parse`]) are left out, `*/*` too, as the tool does.
///
/// # Errors
///
/// Calls interrupted by a signal (`EINTR`) are retried. Any other error
/// opening or listing `folder`, listing an opened sub-folder, or reading
/// an opened folder's status fails the walk.
///
/// Errors looking up a name, opening a sub-folder, or opening, checking or
/// reading a desktop entry also fail the walk, except for the errors listed
/// below. This includes reopening a folder whose handle was closed. Failures
/// such as `EMFILE`, `ENFILE`, `ENOMEM`, `EIO` and `EAGAIN` may reflect a
/// system, disk or temporary problem rather than a problem with the name.
///
/// No partial listing is returned. This deliberately differs from
/// `update-desktop-database`, which silently keeps what it listed before
/// an error. An unexpected violation of the walk's internal invariants
/// returns [`io::ErrorKind::Other`].
///
/// For `ENOENT`, `ENOTDIR`, `ELOOP`, `EACCES`, `EPERM`, `ENAMETOOLONG`,
/// `ENXIO` or `ENODEV`, and on Linux and Android `ENOKEY` or `EUCLEAN`, the
/// name cannot be used as written, and would not be on the next run
/// either; it is skipped: a name that cannot be looked at, a folder that
/// cannot be opened (with what is under it), a desktop entry that cannot
/// be opened, checked or read. A folder opened again by name must be the
/// folder listed (the same device and inode): when it cannot be opened
/// again for one of those errors, or another folder was put in its place
/// since, it is skipped as if deleted, with the names left in it and in
/// the folders the walk goes back to through it. Folders read through
/// handles kept open stay as they were first seen; still, a folder moved to
/// another name while the walk is in it may lose the entries not yet read
/// in the folders the walk goes back to by its old name: they are listed
/// under no name. A run after the move lists it fully; going through it
/// again under its new name would list the part read under two names, and
/// is not done. A folder reached again (through a link) is listed once,
/// under its first name in byte order, where `update-desktop-database`
/// lists it under each. The folder a name opens is told by its device and
/// inode once opened; a name that was, when looked at, a folder listed
/// already (a link to it) is skipped without being opened, a change to it
/// since not seen, as for a name the walk has passed. A desktop entry that
/// is not a regular file (also when it is replaced after its type was
/// checked) or is larger than [`MAX_FILE_LEN`] is skipped too, as are the
/// entries left out above.
pub fn mimeinfo_cache(folder: &Path) -> io::Result<Vec<u8>> {
    let mut types: BTreeMap<String, BTreeSet<Vec<u8>>> = BTreeMap::new();
    collect(folder, &mut types)?;
    let mut out = b"[MIME Cache]\n".to_vec();
    for (mime, ids) in types {
        out.extend_from_slice(mime.as_bytes());
        out.push(b'=');
        for id in ids {
            out.extend_from_slice(&id);
            out.push(b';');
        }
        out.push(b'\n');
    }
    Ok(out)
}

/// Writes [`mimeinfo_cache`] of `folder` to `folder/mimeinfo.cache`
/// atomically, by writing and syncing a new file in the folder, then
/// renaming it. Like `update-desktop-database`, this replaces a link named
/// `mimeinfo.cache` without following it and uses the default permissions.
///
/// # Errors
///
/// Returns any error from [`mimeinfo_cache`]: failures to list a folder,
/// read an opened folder's status, look up a name, open or reopen a folder,
/// or open, check or read a desktop entry, unless that error is skipped.
/// These failures, and failures to write, sync or rename the new file,
/// leave an existing `mimeinfo.cache` unchanged.
///
/// If opening or syncing the folder fails after the rename, the error is
/// still returned. The new `mimeinfo.cache` is already in place, but may
/// not survive a crash.
pub fn write_mimeinfo_cache(folder: &Path) -> io::Result<()> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let bytes = mimeinfo_cache(folder)?;
    // A name left over from a crashed writer is skipped, not an error.
    let (temporary, mut file) = loop {
        let temporary = folder.join(format!(
            ".mimeinfo.cache.{}.{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => break (temporary, file),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    };
    let written = (|| {
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, folder.join("mimeinfo.cache"))?;
        // The rename itself is durable once the folder is synced.
        fs::File::open(folder)?.sync_all()
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written
}

/// The most folder handles the walk keeps open at a time, one being opened
/// among them (more only past 2^14 folders deep, see [`Window`]). Besides
/// these: the root's, the folder the walk starts from, held for the whole
/// walk; and one short-lived handle, for a folder's listing or a file being
/// read, closed before the walk goes on.
const MOST_OPEN: usize = 16;

/// A folder on the walk's stack.
struct Level {
    /// The name it was opened by from the folder before it (none for the
    /// first).
    name: OsString,
    /// Its status when first opened, which tells it from a folder put in
    /// its place since ([`reopen`]).
    stat: rustix::fs::Stat,
    /// Its handle, while open. The first folder's is held apart and never
    /// closed.
    handle: Option<OwnedFd>,
    /// Its names left to look at, in byte order, last first.
    left: Vec<OsString>,
    /// The length of the IDs' prefix for its entries.
    prefix: usize,
}

/// Adds the types of every desktop entry under `folder` to `types`.
///
/// The walk keeps its own stack, not the thread's, as folders may nest
/// deeper than a thread's stack allows. It works from folder handles: each
/// name is looked at, opened or read from the handle of its folder, so no
/// path is formed and an entry is listed however deep it is, past
/// `PATH_MAX`. IDs are built from the names. At most [`MOST_OPEN`] handles
/// are kept (more only past 2^14 folders deep, see [`Window`]); going back
/// to a folder whose handle was closed opens it again by name from a
/// handle kept, as a path to it would ([`reopen`]).
///
/// Each folder is listed once, when first opened: a folder is told by the
/// device and inode of its handle once opened, as the name looked at may
/// be another folder by then. A name that was, when looked at, a folder
/// listed already is skipped without being opened: a change to it since is
/// not seen, as for a name the walk has passed. A name that cannot be
/// looked at, a sub-folder that cannot be opened, or opened again, and a
/// desktop entry that cannot be opened, checked or read, for an error
/// saying the name cannot be used as written, is skipped ([`skipped`]). For
/// any other error, or when a sub-folder is opened but cannot be listed,
/// the walk fails with that error, as for `folder`, rather than give a
/// listing with its entries missing (`update-desktop-database` keeps such a
/// partial listing). A call interrupted by a signal is made again. A
/// folder opened again that is not the one listed is skipped as if deleted
/// ([`reopen`]); one read through a handle kept is read as first seen. So a
/// folder moved to another name mid-walk may lose the entries not yet read,
/// listed under no name; a run after the move lists it fully, and going
/// through it again under its new name would list the part read under two
/// names. A desktop entry is also skipped for what it is ([`add`]). An
/// invariant of the walk found broken fails it too ([`invariant`]).
fn collect(folder: &Path, types: &mut BTreeMap<String, BTreeSet<Vec<u8>>>) -> io::Result<()> {
    let first = open_folder(rustix::fs::CWD, folder)?;
    // A folder reached again through a link is listed once, under the first
    // name in byte order, as `AppIndex` gives its files IDs
    // (`update-desktop-database` lists every path). It is told by the
    // device and inode of the handle opened; those of the name looked at
    // only skip a name that was a folder listed already.
    let stat = fstat(first.as_fd())?;
    let mut visited = HashSet::from([(stat.st_dev, stat.st_ino)]);
    let mut id = Vec::new();
    let mut stack = vec![Level {
        name: OsString::new(),
        stat,
        handle: None,
        left: names(first.as_fd())?,
        prefix: 0,
    }];
    let mut window = Window::new(MOST_OPEN);
    while let Some(level) = stack.last_mut() {
        let Some(name) = level.left.pop() else {
            // Every handle kept is that of a folder on the stack, at its
            // depth, and the deepest kept is the last in `window.open`.
            if stack.pop().is_some_and(|level| level.handle.is_some()) {
                let closed = window.open.pop();
                debug_assert_eq!(closed, Some(stack.len()), "the handle kept");
                if closed != Some(stack.len()) {
                    return Err(invariant("the last handle kept is the deepest folder's"));
                }
            }
            continue;
        };
        id.truncate(level.prefix);
        id.extend_from_slice(name.as_bytes());
        let Some(at) = reopen(first.as_fd(), &mut stack, &mut window)? else {
            // A folder on the way is skipped: so are its other names.
            if let Some(level) = stack.last_mut() {
                level.left.clear();
            }
            continue;
        };
        // A name that cannot be looked at is skipped or fails the walk, as a
        // sub-folder that cannot be opened ([`skipping`]).
        let Some(stat) = skipping(stat_at(at, &name).map_err(io::Error::from))? else {
            continue;
        };
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::Directory => {
                id.push(b'-');
                // A name that was a folder listed already (a link to it)
                // when looked at is skipped unopened, a change to it since
                // unseen, as for a name passed. Otherwise the name may be
                // another folder by the time it is opened: checked again.
                if visited.contains(&(stat.st_dev, stat.st_ino)) {
                    continue;
                }
                // Room for one more handle, the deepest folder's kept.
                let depth = stack.len() - 1;
                make_room(&mut window, &mut stack, depth, depth, false)?;
                let from = handle_at(first.as_fd(), &stack, depth)?;
                // A sub-folder that cannot be opened is skipped or fails the
                // walk ([`skipping`]); one opened that then cannot be listed
                // fails the walk, as the first folder does.
                if let Some(handle) = skipping(open_folder(from, &name))? {
                    // A folder listed already, now under this name, is
                    // skipped.
                    let stat = fstat(handle.as_fd())?;
                    if !visited.insert((stat.st_dev, stat.st_ino)) {
                        continue;
                    }
                    let left = names(handle.as_fd())?;
                    window.push(depth + 1);
                    stack.push(Level {
                        name,
                        stat,
                        handle: Some(handle),
                        left,
                        prefix: id.len(),
                    });
                }
            }
            // Only regular files: reading a FIFO would wait for a writer.
            FileType::RegularFile if name.as_bytes().ends_with(b".desktop") => {
                add(at, &name, &id, types)?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// The status of `name` in `dir`, links followed, looked at again when
/// interrupted by a signal. For an error saying the name cannot be used as
/// written ([`skipped`]), the walk skips the name; for any other, it fails.
fn stat_at(dir: BorrowedFd<'_>, name: &OsStr) -> rustix::io::Result<rustix::fs::Stat> {
    #[cfg(test)]
    tests::LOOKING_AT.with_borrow_mut(|looking_at| looking_at.as_mut().map(|hook| hook(name)));
    rustix::io::retry_on_intr(|| {
        #[cfg(test)]
        if let Some(errno) = tests::named_fault(&tests::UNSTATABLE, name) {
            return Err(errno);
        }
        rustix::fs::statat(dir, name, AtFlags::empty())
    })
}

/// Opens the folder `name` in `dir` for listing, links followed, again when
/// interrupted by a signal.
fn open_folder(dir: BorrowedFd<'_>, name: impl AsRef<Path>) -> io::Result<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    #[cfg(test)]
    tests::OPENING
        .with_borrow_mut(|opening| opening.as_mut().map(|hook| hook(name.as_ref().as_os_str())));
    Ok(rustix::io::retry_on_intr(|| {
        let handle = rustix::fs::openat(dir, name.as_ref(), flags, Mode::empty())?;
        #[cfg(test)]
        if let Some(errno) = tests::fault(&tests::UNOPENABLE, handle.as_fd()) {
            return Err(errno);
        }
        Ok(handle)
    })?)
}

/// The status of the folder of the handle `folder`, read again when
/// interrupted by a signal. For any other error, the walk fails: the folder
/// is open, so it does not say its name cannot be used ([`skipped`]).
fn fstat(folder: BorrowedFd<'_>) -> rustix::io::Result<rustix::fs::Stat> {
    rustix::io::retry_on_intr(|| {
        #[cfg(test)]
        if let Some(errno) = tests::fault(&tests::UNFSTATABLE, folder) {
            return Err(errno);
        }
        let stat = rustix::fs::fstat(folder)?;
        #[cfg(test)]
        let stat = tests::seen(stat);
        Ok(stat)
    })
}

/// Opens the folder `name` in `dir` again, links followed, to look names
/// up in it and open them, not to list it (it was listed when first
/// opened). Tries read access first. On `EACCES` or `EPERM`, retries with
/// [`LOOKUPS_ONLY`], so a folder that can no longer be read but can still
/// be searched is still a way through, as it is for a path through it.
/// On systems without a lookup-only flag, gives the original error. Each
/// is tried again when interrupted by a signal.
fn reopen_folder(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<OwnedFd> {
    let open = |access: OFlags| {
        let flags = access | OFlags::DIRECTORY | OFlags::CLOEXEC;
        rustix::io::retry_on_intr(|| {
            let handle = rustix::fs::openat(dir, name, flags, Mode::empty())?;
            #[cfg(test)]
            if let Some(errno) = tests::fault(&tests::UNREOPENABLE, handle.as_fd()) {
                return Err(errno);
            }
            Ok(handle)
        })
    };
    match (open(OFlags::RDONLY), LOOKUPS_ONLY) {
        (Err(Errno::ACCESS | Errno::PERM), Some(lookups_only)) => Ok(open(lookups_only)?),
        (opened, _) => Ok(opened?),
    }
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

/// The names in `folder`, in byte order, last first. Listed from a copy of
/// the handle, closed once they are read; only the names are kept. An error
/// reading them is given, not a shorter listing (a handle opened for
/// lookups only, [`reopen_folder`], cannot be listed), except that a folder
/// deleted while listed ends the listing, as rustix ends it on `ENOENT`.
fn names(folder: BorrowedFd<'_>) -> io::Result<Vec<OsString>> {
    #[cfg(test)]
    if tests::UNLISTABLE.with_borrow(|unlistable| {
        unlistable.as_ref().is_some_and(|unlistable| {
            rustix::fs::fstat(folder).is_ok_and(|stat| same_folder(&stat, unlistable))
        })
    }) {
        return Err(Errno::IO.into());
    }
    let listing = Dir::new(folder.try_clone_to_owned()?)?;
    let mut names = listing
        .map(|entry| Ok(OsStr::from_bytes(entry?.file_name().to_bytes()).to_owned()))
        .filter(|name| !matches!(name, Ok(name) if name == "." || name == ".."))
        .collect::<io::Result<Vec<OsString>>>()?;
    names.sort_unstable_by(|a, b| b.cmp(a));
    Ok(names)
}

/// Opens the deepest folder in `stack` again if its handle was closed, and
/// gives its handle; `None` when a folder on the way is skipped: it cannot
/// be opened again for an error saying its name cannot be used as written
/// (after the retry for lookups only, [`skipped`]), or it is not the folder
/// listed, another folder having been put in its place since (its device
/// and inode are not those of [`Level::stat`]). Either way it is skipped
/// as if deleted.
///
/// The folders are opened again one by one, by name, from the handle
/// [`Window::start`] gives (as a path from there would reach them), for
/// lookups only where they can no longer be read ([`reopen_folder`]); the
/// handles of those covering the deepest folder are kept, the others
/// closed once the next is open. Each is checked to be the folder listed,
/// a handle for lookups only too. Handles already open on the way are used
/// as they are: the folders they were opened on, wherever they are now.
///
/// # Errors
///
/// When a folder on the way cannot be opened again, or its status read, for
/// any other error; the walk then fails with it. Also when a handle the
/// window keeps is not open, or none can be closed to make room for one:
/// the window's invariants are broken ([`invariant`]).
fn reopen<'s>(
    first: BorrowedFd<'s>,
    stack: &'s mut [Level],
    window: &mut Window,
) -> io::Result<Option<BorrowedFd<'s>>> {
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
            make_room(window, stack, depth, at - 1, passing.is_some())?;
            #[cfg(test)]
            tests::REOPENS.with(|reopens| reopens.set(reopens.get() + 1));
            let again = {
                let from = if let Some(passing) = &passing {
                    passing.as_fd()
                } else {
                    debug_assert!(window.is_open(at - 1), "opened from a handle kept");
                    handle_at(first, stack, at - 1)?
                };
                let Some(again) = skipping(reopen_folder(from, &stack[at].name))? else {
                    return Ok(None);
                };
                again
            };
            // Another folder put in its place since is not the one listed:
            // it is skipped, as if the folder were deleted.
            if !same_folder(&fstat(again.as_fd())?, &stack[at].stat) {
                return Ok(None);
            }
            if Window::covers(at, depth) {
                stack[at].handle = Some(again);
                window.insert(at);
                passing = None;
            } else {
                passing = Some(again);
            }
        }
    }
    let stack: &'s [Level] = stack;
    handle_at(first, stack, depth).map(Some)
}

/// Closes handles kept in `window` until there is room for one more, and
/// for `passing`, a handle open besides them; the walk is at, or going
/// back to, `depth` and opens a folder from the handle at `from`
/// ([`Window::close_one`]).
///
/// # Errors
///
/// When no handle can be closed, or one the window keeps is not open: the
/// window's invariants are broken ([`invariant`]).
fn make_room(
    window: &mut Window,
    stack: &mut [Level],
    depth: usize,
    from: usize,
    passing: bool,
) -> io::Result<()> {
    while window.open.len() + usize::from(passing) >= window.most {
        let Some(closed) = window.close_one(depth, from, |at| !stack[at].left.is_empty()) else {
            return Err(invariant("a full window has a handle to close"));
        };
        let kept = stack[closed].handle.take();
        debug_assert!(kept.is_some(), "a handle kept is open");
        if kept.is_none() {
            return Err(invariant("a handle kept is open"));
        }
    }
    Ok(())
}

/// The handle of the folder at `depth` in `stack`, `first` for the first.
///
/// # Errors
///
/// When it is not open: the walk only asks for a handle it keeps
/// ([`invariant`]).
fn handle_at<'a>(
    first: BorrowedFd<'a>,
    stack: &'a [Level],
    depth: usize,
) -> io::Result<BorrowedFd<'a>> {
    if depth == 0 {
        return Ok(first);
    }
    stack[depth]
        .handle
        .as_ref()
        .map(AsFd::as_fd)
        .ok_or_else(|| invariant("a handle opened from or looked in is open"))
}

/// Whether the statuses `a` and `b` are of the same folder: of the same
/// device and inode.
fn same_folder(a: &rustix::fs::Stat, b: &rustix::fs::Stat) -> bool {
    (a.st_dev, a.st_ino) == (b.st_dev, b.st_ino)
}

/// The error for an invariant of the walk, `broken`, found broken: the walk
/// fails rather than skip names or go on from the wrong folder.
fn invariant(broken: &str) -> io::Error {
    io::Error::other(format!("mimeinfo.cache walk: broken invariant: {broken}"))
}

/// Whether the walk skips a name for `errno`, the error looking at, opening
/// or reading it: it says the name cannot be used as written, which comes
/// back on every run. The name is not there (`ENOENT`), not a folder or
/// behind one that is not (`ENOTDIR`), behind too many links (`ELOOP`), not
/// allowed (`EACCES`, `EPERM`), too long or behind a link to a name too
/// long (`ENAMETOOLONG`), or a device or socket that cannot be opened
/// (`ENXIO`, `ENODEV`); on Linux and Android also encrypted with a key that
/// is not loaded (`ENOKEY`), or found corrupt by its file system
/// (`EUCLEAN`). Any other error may come from the system, the disk or the
/// moment (`EMFILE`, `ENFILE`, `ENOMEM`, `EIO`, `EAGAIN`, ...) and fails the
/// walk, as a listing error does: what is there but cannot be reached now
/// is not left out of the cache. `EINTR` does not reach here: an
/// interrupted call is made again.
fn skipped(errno: Errno) -> bool {
    match errno {
        Errno::NOENT
        | Errno::NOTDIR
        | Errno::LOOP
        | Errno::ACCESS
        | Errno::PERM
        | Errno::NAMETOOLONG
        | Errno::NXIO
        | Errno::NODEV => true,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        Errno::NOKEY | Errno::UCLEAN => true,
        _ => false,
    }
}

/// What `done` gives, or `None` when its name is skipped: the error looking
/// at, opening or reading it is one of [`skipped`]. Any other error is
/// given, and fails the walk.
fn skipping<T>(done: io::Result<T>) -> io::Result<Option<T>> {
    match done {
        Err(err) if Errno::from_io_error(&err).is_some_and(skipped) => Ok(None),
        done => done.map(Some),
    }
}

/// The folder handles the walk keeps open, besides the first folder's, and
/// which of them to close when another one is needed: binary
/// checkpointing, as the tree-magic walk of `xdg-kit-mime` keeps its
/// handles (`Window` in its `tree.rs`, with the proof of its bound in the
/// design notes, decision 32).
///
/// Folders are told by their depth in the walk's stack, the first folder at
/// 0. A depth's rank is the number of times 2 divides it; the handle at
/// depth `d` of rank `r` covers the depths `d` to `d + 2^r - 1`. Going back
/// to a folder whose handle was closed opens folders again from the
/// deepest handle covering it whose shallower handles covering it are all
/// open, and keeps the handles of the ones covering it. A handle is closed
/// only to make room for another, and never while near the folder the walk
/// is at ([`Window::near`]); of the others, first one whose folder has no
/// names left, which the walk never goes back to.
///
/// At most `most` handles are open, one being opened among them. `most`
/// starts at [`MOST_OPEN`] and grows by one when the walk first gets
/// `2^(most - 2)` folders deep, so that no depth's rank reaches
/// `most - 2`, the rank tree magic caps: no rank is ever capped. By that
/// proof, in its case with no rank capped, the walk opens folders again at
/// most `G(N)` times for each name looked at or folder listed after the
/// first, `N` the deepest depth reached and `G(N)` as defined there (at
/// most 6 for `N` below 10,000). Once the walk has been `N` folders deep
/// (`N` ≥ 1), `most` is exactly `max(16, ⌊log2 N⌋ + 3)`: at most that many
/// handles are open, besides the first folder's and one more for a moment
/// (a listing's, or an entry's being read).
struct Window {
    /// The depths of the folders whose handles are open, shallowest first.
    open: Vec<usize>,
    /// The most handles open at a time, one being opened among them.
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

    /// The rank at which `most` grows, `most` - 2, which no depth on the
    /// stack reaches once it is pushed ([`Window::push`]).
    fn top_rank(&self) -> u32 {
        u32::try_from(self.most - 2).unwrap_or(u32::MAX)
    }

    /// The rank of `depth`: the number of times 2 divides it. Not capped,
    /// as tree magic's is: [`Window::push`] grows `most` first.
    fn rank(depth: usize) -> u32 {
        depth.trailing_zeros()
    }

    /// Whether the handle at `at` covers `depth`: `depth` is at or after
    /// `at`, by less than 2 to the power of `at`'s rank.
    fn covers(at: usize, depth: usize) -> bool {
        at <= depth && below(depth - at, Self::rank(at))
    }

    /// Whether the handle at `at` is near `depth`: `depth` is at or after
    /// `at` by less than twice what `at` covers.
    fn near(at: usize, depth: usize) -> bool {
        at <= depth && below(depth - at, Self::rank(at) + 1)
    }

    /// Whether the handle at `at` is open; the first folder's always is.
    fn is_open(&self, at: usize) -> bool {
        at == 0 || self.open.binary_search(&at).is_ok()
    }

    /// Where going back to `depth` starts: the deepest handle covering it
    /// such that it and every shallower one covering it are open, else the
    /// first folder (0). The handles covering a depth are at the depth, and
    /// at it with its lowest bit cleared, again and again.
    fn start(&self, depth: usize) -> usize {
        let mut covering = Vec::new();
        let mut at = depth;
        while at != 0 {
            covering.push(at);
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
        #[cfg(test)]
        tests::MOST_OPEN_SEEN.with(|seen| seen.set(seen.get().max(self.open.len())));
    }

    /// Records the handle of a folder just pushed, at `depth`, the deepest,
    /// and grows `most` so that no depth's rank is capped.
    fn push(&mut self, depth: usize) {
        debug_assert!(self.open.last().is_none_or(|&at| at < depth));
        if Self::rank(depth) >= self.top_rank() {
            self.most += 1;
        }
        self.insert(depth);
    }

    /// Picks a handle to close, forgets it and gives its depth: neither
    /// near `depth`, where the walk is or is going back to, nor at `from`,
    /// which a folder is being opened from. Of those, one whose folder has
    /// no names left to look at (`live` false) if any, else the lowest
    /// rank, the shallowest of equals. With `most` handles open there
    /// always is one; `None` if not.
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
            .filter(|&(_, &at)| at != from && !Self::near(at, depth))
            .min_by_key(|&(_, &at)| (live(at), Self::rank(at), at))?;
        Some(self.open.remove(index))
    }
}

/// Adds the types of the desktop entry `name` in `folder`, of ID `id`, to
/// `types`. It is skipped when it cannot be opened, checked or read for an
/// error saying its name cannot be used as written ([`skipped`]), when it
/// is not a regular file (any more) or is larger than [`MAX_FILE_LEN`]
/// ([`read_regular_file`]), and when it is deleted
/// ([`DesktopEntry::hidden`]) or not in UTF-8
/// ([`DesktopEntry::supports_encoding`]).
///
/// # Errors
///
/// When it cannot be opened, checked or read for any other error; the walk
/// then fails with it.
fn add(
    folder: BorrowedFd<'_>,
    name: &OsStr,
    id: &[u8],
    types: &mut BTreeMap<String, BTreeSet<Vec<u8>>>,
) -> io::Result<()> {
    let Some(bytes) = skipping(read_regular_file(folder, name))?.flatten() else {
        return Ok(());
    };
    let entry = DesktopEntry::parse(&bytes);
    // As `update-desktop-database`: not deleted and in UTF-8. Unlike
    // `AppIndex`, entries of no or unknown `Type` are listed.
    if entry.hidden() || !entry.supports_encoding() {
        return Ok(());
    }
    let file = entry.key_file();
    for item in file.list("Desktop Entry", "MimeType").unwrap_or_default() {
        let item = item.trim_end();
        if TypePattern::parse(item).is_some_and(|pattern| pattern != TypePattern::Any) {
            types
                .entry(item.to_owned())
                .or_default()
                .insert(id.to_vec());
        }
    }
    Ok(())
}

/// Reads the regular file `name` in `folder`, as
/// [`xdg_kit_basedir::read_regular_file`] reads one by path: opened without
/// waiting (its type was checked before), checked again to be a regular
/// file, and at most [`MAX_FILE_LEN`] bytes long. Opened again when
/// interrupted by a signal. `None` when it is not a regular file (replaced
/// since its type was checked) or is longer.
///
/// # Errors
///
/// When it cannot be opened, checked or read.
fn read_regular_file(folder: BorrowedFd<'_>, name: &OsStr) -> io::Result<Option<Vec<u8>>> {
    let flags = OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let file = fs::File::from(rustix::io::retry_on_intr(|| {
        #[cfg(test)]
        if let Some(errno) = tests::named_fault(&tests::UNREADABLE, name) {
            return Err(errno);
        }
        rustix::fs::openat(folder, name, flags, Mode::empty())
    })?);
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_LEN {
        return Ok(None);
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    // One byte more, to notice a file grown past the limit since.
    file.take(MAX_FILE_LEN + 1).read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= MAX_FILE_LEN).then_some(bytes))
}

#[cfg(test)]
#[allow(
    clippy::missing_const_for_thread_local,
    reason = "its thread locals are const; clippy flags them on targets without native thread locals (Android, OpenBSD, illumos)"
)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;

    /// A change a test makes to the tree when the walk looks at a name.
    type Change = Box<dyn FnMut(&OsStr)>;

    thread_local! {
        /// Folders opened again by [`reopen`] on this thread.
        pub(super) static REOPENS: Cell<usize> = const { Cell::new(0) };
        /// The most handles [`Window`] has held open on this thread.
        pub(super) static MOST_OPEN_SEEN: Cell<usize> = const { Cell::new(0) };
        /// The folder that [`names`] fails to list on this thread, as if
        /// reading it failed after it was opened.
        pub(super) static UNLISTABLE: RefCell<Option<rustix::fs::Stat>> = const { RefCell::new(None) };
        /// The folder that [`open_folder`] fails to open on this thread,
        /// and the error it gives, as if opening it failed so.
        pub(super) static UNOPENABLE: RefCell<Option<(rustix::fs::Stat, Errno)>> = const { RefCell::new(None) };
        /// The folder that [`reopen_folder`] fails to open again on this
        /// thread, and the error it gives.
        pub(super) static UNREOPENABLE: RefCell<Option<(rustix::fs::Stat, Errno)>> = const { RefCell::new(None) };
        /// The name that [`stat_at`] fails to look at on this thread, in
        /// any folder, and the error it gives.
        pub(super) static UNSTATABLE: RefCell<Option<(OsString, Errno)>> = const { RefCell::new(None) };
        /// The name that [`read_regular_file`] fails to read on this
        /// thread, in any folder, and the error it gives, as if opening the
        /// file failed so.
        pub(super) static UNREADABLE: RefCell<Option<(OsString, Errno)>> = const { RefCell::new(None) };
        /// The folder whose status [`fstat`] fails to read on this
        /// thread, and the error it gives.
        pub(super) static UNFSTATABLE: RefCell<Option<(rustix::fs::Stat, Errno)>> = const { RefCell::new(None) };
        /// The folder whose status [`fstat`] gives on this thread with
        /// another device and inode, and the status they are taken from.
        pub(super) static SEEN_AS: RefCell<Option<(rustix::fs::Stat, rustix::fs::Stat)>> = const { RefCell::new(None) };
        /// What [`stat_at`] calls on this thread with each name, before it
        /// looks at it: a change to the tree in the middle of a walk.
        pub(super) static LOOKING_AT: RefCell<Option<Change>> = const { RefCell::new(None) };
        /// What [`open_folder`] calls on this thread with each name, before
        /// it opens it: a change to the tree once the name was looked at.
        pub(super) static OPENING: RefCell<Option<Change>> = const { RefCell::new(None) };
        /// The calls interrupted by a fault on this thread ([`once`]).
        static INTERRUPTED: Cell<usize> = const { Cell::new(0) };
    }

    /// The error that `fault` ([`UNSTATABLE`] or [`UNREADABLE`]) gives for
    /// `name`, if it names it ([`once`]).
    pub(super) fn named_fault(
        fault: &'static std::thread::LocalKey<RefCell<Option<(OsString, Errno)>>>,
        name: &OsStr,
    ) -> Option<Errno> {
        fault.with_borrow_mut(|fault| {
            let (faulty, errno) = fault.as_ref()?;
            Some(once(fault, (faulty == name).then_some(*errno)?))
        })
    }

    /// The error that `fault` ([`UNOPENABLE`], [`UNREOPENABLE`] or
    /// [`UNFSTATABLE`]) gives for the folder of the handle `opened`, if it
    /// names that folder ([`once`]).
    pub(super) fn fault(
        fault: &'static std::thread::LocalKey<RefCell<Option<(rustix::fs::Stat, Errno)>>>,
        opened: BorrowedFd<'_>,
    ) -> Option<Errno> {
        fault.with_borrow_mut(|fault| {
            let (folder, errno) = fault.as_ref()?;
            let stat = rustix::fs::fstat(opened).ok()?;
            Some(once(fault, same_folder(&stat, folder).then_some(*errno)?))
        })
    }

    /// The status `stat` as [`fstat`] gives it: for the folder that
    /// [`SEEN_AS`] names, with the device and inode it gives.
    pub(super) fn seen(mut stat: rustix::fs::Stat) -> rustix::fs::Stat {
        SEEN_AS.with_borrow(|seen| {
            if let Some((folder, other)) = seen
                && same_folder(&stat, folder)
            {
                (stat.st_dev, stat.st_ino) = (other.st_dev, other.st_ino);
            }
        });
        stat
    }

    /// Gives `errno`, the error of `fault`, and forgets `fault` if it is
    /// `EINTR`: a signal interrupts one call, not the next. Panics once
    /// calls on this thread were interrupted 100 times, so that a test
    /// fails rather than hang if a call interrupted is made again forever.
    fn once<T>(fault: &mut Option<T>, errno: Errno) -> Errno {
        if errno == Errno::INTR {
            *fault = None;
            let interrupted =
                INTERRUPTED.with(|interrupted| interrupted.replace(interrupted.get() + 1));
            assert!(interrupted < 100, "interrupted again and again");
        }
        errno
    }

    /// Makes [`open_folder`] (with `fault` [`UNOPENABLE`]),
    /// [`reopen_folder`] (with [`UNREOPENABLE`]) or [`fstat`] (with
    /// [`UNFSTATABLE`]) fail for `folder` with `errno`; with `None`, for no
    /// folder.
    fn fail(
        fault: &'static std::thread::LocalKey<RefCell<Option<(rustix::fs::Stat, Errno)>>>,
        folder: Option<(&Path, Errno)>,
    ) {
        fault.set(folder.map(|(folder, errno)| {
            let handle = open_at(rustix::fs::CWD, folder);
            (rustix::fs::fstat(&handle).unwrap(), errno)
        }));
    }

    /// The errors for which a name is skipped, and some for which the walk
    /// fails.
    const SKIPPED: &[Errno] = &[
        Errno::ACCESS,
        Errno::PERM,
        Errno::NOENT,
        Errno::NOTDIR,
        Errno::LOOP,
        Errno::NAMETOOLONG,
        Errno::NXIO,
        Errno::NODEV,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        Errno::NOKEY,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        Errno::UCLEAN,
    ];
    const FAILING: [Errno; 5] = [
        Errno::MFILE,
        Errno::NFILE,
        Errno::NOMEM,
        Errno::IO,
        Errno::AGAIN,
    ];

    /// The names in `folder`, in byte order.
    fn listed(folder: &Path) -> Vec<OsString> {
        let mut names = fs::read_dir(folder)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[test]
    fn types_and_ids_in_byte_order() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, text: &str| {
            let path = dir.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        };
        write(
            "b.desktop",
            "[Desktop Entry]\nType=Application\nName=B\nExec=b\nMimeType=text/plain;image/*;*/*;not a type;\n",
        );
        write(
            "kde/a.desktop",
            "[Desktop Entry]\nType=Application\nName=A\nExec=a\nMimeType=text/plain;\n",
        );
        write(
            "hidden.desktop",
            "[Desktop Entry]\nType=Application\nName=H\nExec=h\nHidden=true\nMimeType=text/plain;\n",
        );
        write("notes.txt", "MimeType=text/plain;\n");
        assert_eq!(
            String::from_utf8(mimeinfo_cache(dir.path()).unwrap()).unwrap(),
            "[MIME Cache]\nimage/*=b.desktop;\ntext/plain=b.desktop;kde-a.desktop;\n"
        );
        write_mimeinfo_cache(dir.path()).unwrap();
        assert_eq!(
            fs::read(dir.path().join("mimeinfo.cache")).unwrap(),
            mimeinfo_cache(dir.path()).unwrap()
        );
        assert!(mimeinfo_cache(&dir.path().join("missing")).is_err());
    }

    /// Opens the folder `name` in `dir`.
    fn open_at(dir: impl AsFd, name: impl AsRef<Path>) -> OwnedFd {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        rustix::fs::openat(dir, name.as_ref(), flags, Mode::empty()).unwrap()
    }

    /// Makes `depth` nested folders `name` in `root`, and the desktop entry
    /// `file` in the last one, from folder handles, so that no path need
    /// reach them. Gives the handle of the last folder.
    fn nest(root: &Path, name: &str, depth: usize, file: &str) -> OwnedFd {
        let mut folder = open_at(rustix::fs::CWD, root);
        for _ in 0..depth {
            rustix::fs::mkdirat(&folder, name, Mode::RWXU).unwrap();
            folder = open_at(&folder, name);
        }
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC;
        let entry = rustix::fs::openat(&folder, file, flags, Mode::RUSR | Mode::WUSR).unwrap();
        fs::File::from(entry).write_all(ENTRY.as_bytes()).unwrap();
        folder
    }

    /// Removes what [`nest`] made, from the handle it gave, level by level,
    /// as removing the tree at once may recurse as deep, or need a path.
    fn unnest(mut folder: OwnedFd, name: &str, depth: usize, file: &str) {
        rustix::fs::unlinkat(&folder, file, AtFlags::empty()).unwrap();
        for _ in 0..depth {
            let parent = open_at(&folder, "..");
            rustix::fs::unlinkat(&parent, name, AtFlags::REMOVEDIR).unwrap();
            folder = parent;
        }
    }

    /// The cache of `root`, made on a thread with a small stack, which a
    /// walk going one call deeper per folder would overflow.
    fn on_a_small_stack(root: &Path) -> String {
        let root = root.to_path_buf();
        let cache = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || mimeinfo_cache(&root))
            .unwrap()
            .join()
            .unwrap()
            .unwrap();
        String::from_utf8(cache).unwrap()
    }

    const ENTRY: &str = "[Desktop Entry]\nType=Application\nName=N\nExec=n\nMimeType=text/plain;\n";

    #[test]
    fn entries_deeper_than_path_max_are_listed() {
        let dir = tempfile::tempdir().unwrap();
        // 6,300 bytes of folders: longer than `PATH_MAX` on every system
        // this builds on (4,096 on Linux, 1,024 on Apple's systems and the
        // BSDs), however long the temporary folder's path is.
        let depth = 2_100;
        let bottom = nest(dir.path(), "dd", depth, "a.desktop");
        assert_eq!(
            on_a_small_stack(dir.path()),
            format!(
                "[MIME Cache]\ntext/plain={}a.desktop;\n",
                "dd-".repeat(depth)
            )
        );
        unnest(bottom, "dd", depth, "a.desktop");
    }

    #[test]
    fn going_back_up_a_deep_tree_keeps_few_handles() {
        // 300 levels, each with a folder `d` and, after it, an entry: the
        // walk goes back to every level, most of whose handles it closed.
        let dir = tempfile::tempdir().unwrap();
        let depth = 300;
        let mut path = dir.path().to_path_buf();
        let mut ids = BTreeSet::new();
        for level in 0..depth {
            fs::create_dir(path.join("d")).unwrap();
            fs::write(path.join("e.desktop"), ENTRY).unwrap();
            ids.insert(format!("{}e.desktop", "d-".repeat(level)));
            path.push("d");
        }
        REOPENS.with(|reopens| reopens.set(0));
        MOST_OPEN_SEEN.with(|seen| seen.set(0));
        let cache = String::from_utf8(mimeinfo_cache(dir.path()).unwrap()).unwrap();
        let ids: String = ids.into_iter().map(|id| id + ";").collect();
        assert_eq!(cache, format!("[MIME Cache]\ntext/plain={ids}\n"));
        assert_eq!(MOST_OPEN_SEEN.with(Cell::get), MOST_OPEN, "handles kept");
        // Each name looked at and each folder listed is a step: within the
        // proven bound (decision 32), at most 6 reopens for each step after
        // the first.
        let steps = 2 * depth + (depth + 1);
        let reopens = REOPENS.with(Cell::get);
        assert!(
            reopens > 0 && reopens <= 6 * (steps - 1),
            "{reopens} reopens for {steps} steps"
        );
    }

    #[test]
    fn the_window_grows_before_a_rank_would_be_capped() {
        let mut window = Window::new(MOST_OPEN);
        for depth in 1..=(1 << 15) {
            window.push(depth);
            assert!(Window::rank(depth) < window.top_rank(), "depth {depth}");
            // As the type's doc states.
            let log2 = usize::try_from(depth.ilog2()).unwrap();
            assert_eq!(window.most, MOST_OPEN.max(log2 + 3), "depth {depth}");
            while window.open.len() >= window.most {
                window.close_one(depth, depth, |_| true).unwrap();
            }
        }
        // 2^14 and 2^15 deep: two more.
        assert_eq!(window.most, MOST_OPEN + 2);
    }

    /// The level of a walk's stack for `folder` in `dir` (`dir` itself for
    /// ""), its handle closed and no names left.
    fn closed(dir: &Path, folder: &str) -> Level {
        Level {
            name: Path::new(folder).file_name().unwrap_or_default().into(),
            stat: rustix::fs::stat(dir.join(folder)).unwrap(),
            handle: None,
            left: Vec::new(),
            prefix: 0,
        }
    }

    #[test]
    fn a_folder_that_can_now_only_be_searched_is_still_a_way_through() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        fs::write(dir.path().join("a/b/e.desktop"), ENTRY).unwrap();
        let first = open_folder(rustix::fs::CWD, dir.path()).unwrap();
        // `a` and `b` were listed when first opened, and their handles
        // closed since; the walk is back at `b`.
        let mut stack = ["", "a", "a/b"].map(|folder| closed(dir.path(), folder));
        let mut window = Window::new(MOST_OPEN);
        // Meanwhile `a` can be searched, but no longer read.
        let a = dir.path().join("a");
        fs::set_permissions(&a, fs::Permissions::from_mode(0o111)).unwrap();
        let enforced = fs::read_dir(&a).is_err();
        let found = reopen(first.as_fd(), &mut stack, &mut window)
            .map(|b| b.map(|b| rustix::fs::statat(b, "e.desktop", AtFlags::empty()).is_ok()))
            .map_err(|err| err.raw_os_error());
        fs::set_permissions(&a, fs::Permissions::from_mode(0o755)).unwrap();
        if !enforced {
            eprintln!("folder permissions are not enforced here (root?): not checked");
        } else if LOOKUPS_ONLY.is_some() {
            assert_eq!(found, Ok(Some(true)), "`b` is reached through `a`");
        } else {
            assert_eq!(found, Ok(None), "no lookup-only flag here");
        }
    }

    #[test]
    fn listing_a_handle_for_lookups_only_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("e.desktop"), ENTRY).unwrap();
        let listed = open_folder(rustix::fs::CWD, dir.path()).unwrap();
        assert_eq!(names(listed.as_fd()).unwrap(), ["e.desktop"]);
        let Some(lookups_only) = LOOKUPS_ONLY else {
            return;
        };
        let handle = rustix::fs::openat(
            rustix::fs::CWD,
            dir.path(),
            lookups_only | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        // Not an empty listing, which would hide the misuse.
        let listing = names(handle.as_fd());
        assert!(listing.is_err(), "{listing:?}");
    }

    #[test]
    fn a_folder_opened_but_not_listed_fails_the_walk() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.desktop"), ENTRY).unwrap();
        fs::create_dir_all(dir.path().join("sub/deeper")).unwrap();
        fs::write(dir.path().join("sub/deeper/b.desktop"), ENTRY).unwrap();
        write_mimeinfo_cache(dir.path()).unwrap();
        let cache = dir.path().join("mimeinfo.cache");
        let before = fs::read(&cache).unwrap();
        assert_eq!(
            String::from_utf8(before.clone()).unwrap(),
            "[MIME Cache]\ntext/plain=a.desktop;sub-deeper-b.desktop;\n"
        );
        let files = || {
            let mut names = fs::read_dir(dir.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>();
            names.sort();
            names
        };
        let files_before = files();
        for unlistable in ["sub", "sub/deeper"] {
            let folder = open_folder(rustix::fs::CWD, dir.path().join(unlistable)).unwrap();
            UNLISTABLE.set(Some(rustix::fs::fstat(&folder).unwrap()));
            let listed = mimeinfo_cache(dir.path());
            let written = write_mimeinfo_cache(dir.path());
            UNLISTABLE.set(None);
            // Not a shorter listing, as `update-desktop-database` keeps.
            assert_eq!(
                listed.map_err(|err| err.raw_os_error()),
                Err(Some(Errno::IO.raw_os_error())),
                "{unlistable}"
            );
            assert_eq!(
                written.map_err(|err| err.raw_os_error()),
                Err(Some(Errno::IO.raw_os_error())),
                "{unlistable}"
            );
            // The old cache is left as it is, and no file is left behind.
            assert_eq!(fs::read(&cache).unwrap(), before, "{unlistable}");
            assert_eq!(files(), files_before, "{unlistable}");
        }
        // A folder that cannot be opened is still skipped.
        let sub = dir.path().join("sub");
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o000)).unwrap();
        let enforced = fs::read_dir(&sub).is_err();
        let listed = mimeinfo_cache(dir.path());
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).unwrap();
        if enforced {
            assert_eq!(
                String::from_utf8(listed.unwrap()).unwrap(),
                "[MIME Cache]\ntext/plain=a.desktop;\n"
            );
        } else {
            eprintln!("folder permissions are not enforced here (root?): not checked");
        }
    }

    #[test]
    fn a_sub_folder_that_cannot_be_opened_fails_the_walk_unless_skipped() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.desktop"), ENTRY).unwrap();
        fs::create_dir_all(dir.path().join("sub/deeper")).unwrap();
        fs::write(dir.path().join("sub/deeper/b.desktop"), ENTRY).unwrap();
        write_mimeinfo_cache(dir.path()).unwrap();
        let cache = dir.path().join("mimeinfo.cache");
        let before = fs::read(&cache).unwrap();
        let files_before = listed(dir.path());
        for folder in ["sub", "sub/deeper"] {
            let folder = dir.path().join(folder);
            for errno in FAILING {
                fail(&UNOPENABLE, Some((&folder, errno)));
                let made = mimeinfo_cache(dir.path());
                let written = write_mimeinfo_cache(dir.path());
                fail(&UNOPENABLE, None);
                let error = Some(Some(errno.raw_os_error()));
                assert_eq!(made.err().map(|err| err.raw_os_error()), error, "{errno:?}");
                assert_eq!(
                    written.err().map(|err| err.raw_os_error()),
                    error,
                    "{errno:?}"
                );
                // The old cache is left as it is, and no file is left behind.
                assert_eq!(fs::read(&cache).unwrap(), before, "{errno:?}");
                assert_eq!(listed(dir.path()), files_before, "{errno:?}");
            }
            for &errno in SKIPPED {
                fail(&UNOPENABLE, Some((&folder, errno)));
                let made = mimeinfo_cache(dir.path());
                fail(&UNOPENABLE, None);
                assert_eq!(
                    String::from_utf8(made.unwrap()).unwrap(),
                    "[MIME Cache]\ntext/plain=a.desktop;\n",
                    "{errno:?}"
                );
            }
        }
    }

    #[test]
    fn a_folder_that_cannot_be_opened_again_fails_the_walk_unless_skipped() {
        // As in `going_back_up_a_deep_tree_keeps_few_handles`: the walk
        // goes back to every level, most of whose handles it closed.
        let dir = tempfile::tempdir().unwrap();
        let depth = 300;
        let mut path = dir.path().to_path_buf();
        for _ in 0..depth {
            fs::create_dir(path.join("d")).unwrap();
            fs::write(path.join("e.desktop"), ENTRY).unwrap();
            path.push("d");
        }
        write_mimeinfo_cache(dir.path()).unwrap();
        let cache = dir.path().join("mimeinfo.cache");
        let before = fs::read(&cache).unwrap();
        let files_before = listed(dir.path());
        // A folder whose handle the walk closes and needs again.
        let at = 100;
        let folder = dir.path().join(["d"; 100].join("/"));
        for errno in FAILING {
            fail(&UNREOPENABLE, Some((&folder, errno)));
            let made = mimeinfo_cache(dir.path());
            let written = write_mimeinfo_cache(dir.path());
            fail(&UNREOPENABLE, None);
            let error = Some(Some(errno.raw_os_error()));
            assert_eq!(made.err().map(|err| err.raw_os_error()), error, "{errno:?}");
            assert_eq!(
                written.err().map(|err| err.raw_os_error()),
                error,
                "{errno:?}"
            );
            assert_eq!(fs::read(&cache).unwrap(), before, "{errno:?}");
            assert_eq!(listed(dir.path()), files_before, "{errno:?}");
        }
        for &errno in SKIPPED {
            fail(&UNREOPENABLE, Some((&folder, errno)));
            let made = mimeinfo_cache(dir.path());
            fail(&UNREOPENABLE, None);
            let made = String::from_utf8(made.unwrap()).unwrap();
            let ids = made
                .strip_prefix("[MIME Cache]\ntext/plain=")
                .and_then(|ids| ids.strip_suffix(";\n"))
                .unwrap();
            // Its entry and some under it are skipped, as the folder is
            // where the walk goes back to them; none above it.
            let levels = ids
                .split(';')
                .map(|id| id.matches("d-").count())
                .collect::<BTreeSet<_>>();
            let missing = (0..depth)
                .filter(|level| !levels.contains(level))
                .collect::<Vec<_>>();
            assert_eq!(missing.first(), Some(&at), "{errno:?}: {missing:?}");
            assert!(
                missing.windows(2).all(|pair| pair[1] == pair[0] + 1),
                "{errno:?}: {missing:?}"
            );
        }
    }

    /// The cache of a tree as in `going_back_up_a_deep_tree_keeps_few_handles`,
    /// whose folder at depth 100 is moved away by `change` once the walk is
    /// at the deepest one, having closed most handles on the way.
    fn changed_mid_walk(change: impl FnOnce(&Path, &Path) + 'static) -> String {
        let dir = tempfile::tempdir().unwrap();
        let mut path = dir.path().to_path_buf();
        for _ in 0..300 {
            fs::create_dir(path.join("d")).unwrap();
            fs::write(path.join("e.desktop"), ENTRY).unwrap();
            path.push("d");
        }
        // The last name the deepest folder with names has.
        fs::write(path.parent().unwrap().join("zz"), "").unwrap();
        let away = tempfile::tempdir().unwrap();
        let folder = dir.path().join(["d"; 100].join("/"));
        let moved = away.path().join("moved");
        let mut change = Some(change);
        LOOKING_AT.set(Some(Box::new(move |name| {
            if name == "zz"
                && let Some(change) = change.take()
            {
                change(&folder, &moved);
            }
        })));
        let made = mimeinfo_cache(dir.path());
        LOOKING_AT.set(None);
        String::from_utf8(made.unwrap()).unwrap()
    }

    #[test]
    fn a_folder_replaced_mid_walk_is_skipped_as_if_deleted() {
        let deleted = changed_mid_walk(|folder, moved| fs::rename(folder, moved).unwrap());
        let replaced = changed_mid_walk(|folder, moved| {
            fs::rename(folder, moved).unwrap();
            fs::create_dir(folder).unwrap();
            let other = ENTRY.replace("text/plain", "image/png");
            fs::write(folder.join("e.desktop"), other).unwrap();
            fs::write(folder.join("new.desktop"), ENTRY).unwrap();
        });
        // The entry at depth 100 is skipped with its folder; those above it
        // are listed.
        let ids = deleted
            .strip_prefix("[MIME Cache]\ntext/plain=")
            .unwrap()
            .split(';')
            .collect::<HashSet<_>>();
        assert!(!ids.contains(format!("{}e.desktop", "d-".repeat(100)).as_str()));
        for level in 0..100 {
            assert!(ids.contains(format!("{}e.desktop", "d-".repeat(level)).as_str()));
        }
        // Nothing of the folder put in its place is mixed in.
        assert_eq!(replaced, deleted);
    }

    #[test]
    fn a_folder_opened_again_is_skipped_only_for_some_errors() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        fs::write(dir.path().join("a/b/e.desktop"), ENTRY).unwrap();
        let first = open_folder(rustix::fs::CWD, dir.path()).unwrap();
        // As in `a_folder_that_can_now_only_be_searched_is_still_a_way_through`:
        // the walk is back at `b`, whose handle and that of `a` were closed.
        let back_at_b = || ["", "a", "a/b"].map(|folder| closed(dir.path(), folder));
        let reopened = |mut stack: [Level; 3]| {
            let mut window = Window::new(MOST_OPEN);
            reopen(first.as_fd(), &mut stack, &mut window)
                .map(|b| b.map(|b| rustix::fs::statat(b, "e.desktop", AtFlags::empty()).is_ok()))
                .map_err(|err| err.raw_os_error())
        };
        assert_eq!(reopened(back_at_b()), Ok(Some(true)));
        for folder in ["a", "a/b"] {
            for errno in FAILING {
                fail(&UNREOPENABLE, Some((&dir.path().join(folder), errno)));
                let found = reopened(back_at_b());
                fail(&UNREOPENABLE, None);
                assert_eq!(
                    found,
                    Err(Some(errno.raw_os_error())),
                    "{folder}: {errno:?}"
                );
            }
            for &errno in SKIPPED {
                fail(&UNREOPENABLE, Some((&dir.path().join(folder), errno)));
                let found = reopened(back_at_b());
                fail(&UNREOPENABLE, None);
                assert_eq!(found, Ok(None), "{folder}: {errno:?}");
            }
        }
        // Meanwhile `a` can no longer be searched: `b` cannot be reached
        // (`EACCES`), and is skipped.
        let a = dir.path().join("a");
        let stack = back_at_b();
        fs::set_permissions(&a, fs::Permissions::from_mode(0o000)).unwrap();
        let enforced = fs::read_dir(&a).is_err();
        let found = reopened(stack);
        fs::set_permissions(&a, fs::Permissions::from_mode(0o755)).unwrap();
        if enforced {
            assert_eq!(found, Ok(None));
        } else {
            eprintln!("folder permissions are not enforced here (root?): not checked");
        }
    }

    #[test]
    fn a_folder_put_in_place_of_one_listed_is_not_opened_again() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        let first = open_folder(rustix::fs::CWD, dir.path()).unwrap();
        // The walk is back at `b`, whose handle and that of `a` were closed.
        let back_at_b = || ["", "a", "a/b"].map(|folder| closed(dir.path(), folder));
        let reopened = |mut stack: [Level; 3]| {
            let mut window = Window::new(MOST_OPEN);
            reopen(first.as_fd(), &mut stack, &mut window)
                .map(|b| b.is_some())
                .map_err(|err| err.raw_os_error())
        };
        let (stack, again) = (back_at_b(), back_at_b());
        // Meanwhile `a` is moved away, and another folder with a `b` put in
        // its place: it is skipped, as if `a` were deleted.
        fs::rename(dir.path().join("a"), dir.path().join("old")).unwrap();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        assert_eq!(reopened(stack), Ok(false));
        // Moved back, `a` is the folder it was, and opened again.
        fs::remove_dir_all(dir.path().join("a")).unwrap();
        fs::rename(dir.path().join("old"), dir.path().join("a")).unwrap();
        assert_eq!(reopened(again), Ok(true));
        // A folder of the same inode on another device is not the one
        // listed either.
        let mut other_device = back_at_b();
        other_device[2].stat.st_dev ^= 1;
        assert_eq!(reopened(other_device), Ok(false));
        // `a`, which the walk only passes through on the way to `b`, is
        // checked too: once replaced, with `b` itself moved into the new
        // one, `b` is skipped.
        let stack = back_at_b();
        fs::rename(dir.path().join("a"), dir.path().join("old")).unwrap();
        fs::create_dir(dir.path().join("a")).unwrap();
        fs::rename(dir.path().join("old/b"), dir.path().join("a/b")).unwrap();
        assert_eq!(reopened(stack), Ok(false));
    }

    #[test]
    fn a_folder_whose_status_cannot_be_read_fails_the_walk() {
        let dir = three_entries();
        for folder in ["", "sub", "sub/deeper"] {
            for errno in FAILING.iter().chain(SKIPPED) {
                fail(&UNFSTATABLE, Some((&dir.path().join(folder), *errno)));
                let made = mimeinfo_cache(dir.path());
                fail(&UNFSTATABLE, None);
                assert_eq!(
                    made.map_err(|err| err.raw_os_error()),
                    Err(Some(errno.raw_os_error())),
                    "{folder}: {errno:?}"
                );
            }
        }
        // Also when opened again: the walk is back at `sub/deeper`, whose
        // handle and that of `sub` were closed.
        let first = open_folder(rustix::fs::CWD, dir.path()).unwrap();
        for folder in ["sub", "sub/deeper"] {
            for errno in FAILING.iter().chain(SKIPPED) {
                let mut stack = ["", "sub", "sub/deeper"].map(|at| closed(dir.path(), at));
                let mut window = Window::new(MOST_OPEN);
                fail(&UNFSTATABLE, Some((&dir.path().join(folder), *errno)));
                let found = reopen(first.as_fd(), &mut stack, &mut window)
                    .map(|deeper| deeper.is_some())
                    .map_err(|err| err.raw_os_error());
                fail(&UNFSTATABLE, None);
                assert_eq!(
                    found,
                    Err(Some(errno.raw_os_error())),
                    "{folder}: {errno:?}"
                );
            }
        }
    }

    /// The cache of `dir`, made while [`open_folder`] calls `change` with
    /// each name, before it opens it.
    fn changed_when_opened(dir: &Path, change: impl FnMut(&OsStr) + 'static) -> String {
        OPENING.set(Some(Box::new(change)));
        let made = mimeinfo_cache(dir);
        OPENING.set(None);
        String::from_utf8(made.unwrap()).unwrap()
    }

    #[test]
    fn a_folder_is_listed_once_by_the_folder_opened() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("a")).unwrap();
        fs::write(dir.path().join("a/x.desktop"), ENTRY).unwrap();
        fs::create_dir(dir.path().join("z")).unwrap();
        // `z` is an empty folder when looked at, and a link to `a`, listed
        // already, when opened.
        let z = dir.path().join("z");
        let made = changed_when_opened(dir.path(), {
            let z = z.clone();
            move |name| {
                if name == "z" && z.is_dir() && !z.is_symlink() {
                    fs::remove_dir(&z).unwrap();
                    std::os::unix::fs::symlink("a", &z).unwrap();
                }
            }
        });
        assert!(z.is_symlink(), "`z` was changed when opened");
        assert_eq!(made, "[MIME Cache]\ntext/plain=a-x.desktop;\n");
        // The handle of `a` opened again through `z`, skipped, was closed.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        assert_eq!(open_under(dir.path()), Vec::<std::path::PathBuf>::new());
    }

    /// The files and folders under `dir` that this process has handles
    /// open on.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn open_under(dir: &Path) -> Vec<std::path::PathBuf> {
        let dir = dir.canonicalize().unwrap();
        fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(|fd| fs::read_link(fd.ok()?.path()).ok())
            .filter(|target| target.starts_with(&dir))
            .collect()
    }

    #[test]
    fn a_folder_opened_again_must_be_the_one_opened_not_the_one_looked_at() {
        // `z` is an empty folder when looked at, and a link to a folder
        // outside, deeper than the handles kept (30 deep), when opened: the
        // walk goes back to `z`, whose handle it closed, for `zz.desktop`,
        // and finds the folder opened.
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let mut ids = BTreeSet::from(["z-zz.desktop".to_owned()]);
        let mut path = outside.path().to_path_buf();
        for level in 1..=2 * MOST_OPEN - 2 {
            path.push("d");
            fs::create_dir(&path).unwrap();
            fs::write(path.join("e.desktop"), ENTRY).unwrap();
            ids.insert(format!("z-{}e.desktop", "d-".repeat(level)));
        }
        fs::write(outside.path().join("zz.desktop"), ENTRY).unwrap();
        let z = dir.path().join("z");
        fs::create_dir(&z).unwrap();
        REOPENS.with(|reopens| reopens.set(0));
        let made = changed_when_opened(dir.path(), {
            let (z, outside) = (z.clone(), outside.path().to_path_buf());
            move |name| {
                if name == "z" && !z.is_symlink() {
                    fs::remove_dir(&z).unwrap();
                    std::os::unix::fs::symlink(&outside, &z).unwrap();
                }
            }
        });
        assert!(z.is_symlink(), "`z` was changed when opened");
        assert!(REOPENS.with(Cell::get) > 0, "`z` was opened again");
        let ids: String = ids.into_iter().map(|id| id + ";").collect();
        assert_eq!(made, format!("[MIME Cache]\ntext/plain={ids}\n"));
    }

    #[test]
    fn a_folder_of_the_same_inode_on_another_device_is_another_folder() {
        let dir = tempfile::tempdir().unwrap();
        for folder in ["a", "b"] {
            fs::create_dir(dir.path().join(folder)).unwrap();
            fs::write(dir.path().join(folder).join("x.desktop"), ENTRY).unwrap();
        }
        let [a, b] = ["a", "b"].map(|folder| rustix::fs::stat(dir.path().join(folder)).unwrap());
        let made = |seen_as| {
            SEEN_AS.set(Some((b, seen_as)));
            let made = mimeinfo_cache(dir.path());
            SEEN_AS.set(None);
            String::from_utf8(made.unwrap()).unwrap()
        };
        // Once opened, `b` has the device and inode of `a`, listed already:
        // it is skipped.
        assert_eq!(made(a), "[MIME Cache]\ntext/plain=a-x.desktop;\n");
        // With the inode of `a` but on another device, it is listed too.
        let mut other_device = a;
        other_device.st_dev ^= 1;
        assert_eq!(
            made(other_device),
            "[MIME Cache]\ntext/plain=a-x.desktop;b-x.desktop;\n"
        );
    }

    #[test]
    fn a_folder_moved_mid_walk_loses_the_entries_not_yet_read() {
        // `b`, 30 folders deep with an entry in each and `zz.desktop` at
        // the top, is moved to `c` once the walk is at its deepest entry.
        let dir = tempfile::tempdir().unwrap();
        let (b, c) = (dir.path().join("b"), dir.path().join("c"));
        let mut path = b.clone();
        for _ in 0..30 {
            path.push("d");
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("e.desktop"), ENTRY).unwrap();
        }
        fs::write(b.join("zz.desktop"), ENTRY).unwrap();
        LOOKING_AT.set(Some(Box::new({
            let (b, c) = (b.clone(), c.clone());
            move |name| {
                if name == "e.desktop" && b.exists() {
                    fs::rename(&b, &c).unwrap();
                }
            }
        })));
        let made = mimeinfo_cache(dir.path());
        LOOKING_AT.set(None);
        assert!(c.is_dir() && !b.exists(), "`b` was moved");
        let made = String::from_utf8(made.unwrap()).unwrap();
        let ids = made
            .strip_prefix("[MIME Cache]\ntext/plain=")
            .and_then(|ids| ids.strip_suffix(";\n"))
            .unwrap()
            .split(';')
            .collect::<Vec<_>>();
        // The entry read when it was moved is listed, under its old name,
        // and some read before; `zz.desktop`, not read yet, is listed under
        // no name. Nothing is listed under `c`, which would list what was
        // read under two names.
        let deepest = format!("b-{}e.desktop", "d-".repeat(30));
        assert!(ids.contains(&deepest.as_str()), "{ids:?}");
        assert!(!ids.contains(&"b-zz.desktop"), "{ids:?}");
        assert!(ids.len() < 31, "{ids:?}");
        assert!(ids.iter().all(|id| id.starts_with("b-d-")), "{ids:?}");
        // A run after the move lists it fully, under its new name.
        let after = String::from_utf8(mimeinfo_cache(dir.path()).unwrap()).unwrap();
        let after = after
            .strip_prefix("[MIME Cache]\ntext/plain=")
            .unwrap()
            .split(';')
            .filter(|id| id.starts_with("c-"))
            .count();
        assert_eq!(after, 31);
    }

    #[test]
    fn a_folder_listed_already_is_not_opened_or_listed_again() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        fs::create_dir(&a).unwrap();
        fs::write(a.join("x.desktop"), ENTRY).unwrap();
        std::os::unix::fs::symlink("a", dir.path().join("z")).unwrap();
        // Once `a` is listed, opening it fails: the link `z` to it, looked
        // at, is not opened.
        LOOKING_AT.set(Some(Box::new({
            let a = a.clone();
            move |name| {
                if name == "z" {
                    fail(&UNOPENABLE, Some((&a, Errno::IO)));
                }
            }
        })));
        let made = mimeinfo_cache(dir.path());
        LOOKING_AT.set(None);
        fail(&UNOPENABLE, None);
        assert_eq!(
            String::from_utf8(made.unwrap()).unwrap(),
            "[MIME Cache]\ntext/plain=a-x.desktop;\n"
        );
        // Once `a` is listed, listing it fails: `z`, an empty folder when
        // looked at and a link to `a` when opened, is not listed.
        fs::remove_file(dir.path().join("z")).unwrap();
        fs::create_dir(dir.path().join("z")).unwrap();
        let z = dir.path().join("z");
        let made = changed_when_opened(dir.path(), {
            let z = z.clone();
            move |name| {
                if name == "z" && !z.is_symlink() {
                    fs::remove_dir(&z).unwrap();
                    std::os::unix::fs::symlink("a", &z).unwrap();
                    UNLISTABLE.set(Some(rustix::fs::stat(&a).unwrap()));
                }
            }
        });
        UNLISTABLE.set(None);
        assert!(z.is_symlink(), "`z` was changed when opened");
        assert_eq!(made, "[MIME Cache]\ntext/plain=a-x.desktop;\n");
    }

    #[test]
    fn a_link_changed_once_looked_at_hides_no_folder() {
        for replaced in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir(dir.path().join("b")).unwrap();
            fs::write(dir.path().join("b/x.desktop"), ENTRY).unwrap();
            std::os::unix::fs::symlink("b", dir.path().join("a")).unwrap();
            // The link `a` is looked at, then deleted, or replaced by an
            // empty folder, before it is opened: `b` is still listed.
            let a = dir.path().join("a");
            let mut changed = false;
            let made = changed_when_opened(dir.path(), move |name| {
                if name == "a" && !changed {
                    changed = true;
                    fs::remove_file(&a).unwrap();
                    if replaced {
                        fs::create_dir(&a).unwrap();
                    }
                }
            });
            assert_eq!(
                made, "[MIME Cache]\ntext/plain=b-x.desktop;\n",
                "replaced: {replaced}"
            );
        }
    }

    #[test]
    fn a_link_back_put_in_place_of_a_new_folder_each_time_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/n")).unwrap();
        fs::write(dir.path().join("a/x.desktop"), ENTRY).unwrap();
        // `a/n` is a new folder each time it is looked at, and a link to
        // `a` each time it is opened; the folders replaced are kept, so
        // that each new one is a new inode. Without the check, the walk
        // would go ever deeper: here up to 100 times.
        let n = dir.path().join("a/n");
        LOOKING_AT.set(Some(Box::new({
            let n = n.clone();
            move |name| {
                if name == "n" && n.is_symlink() {
                    fs::remove_file(&n).unwrap();
                    fs::create_dir(&n).unwrap();
                }
            }
        })));
        let kept = tempfile::tempdir().unwrap();
        let opened = std::rc::Rc::new(Cell::new(0));
        let made = changed_when_opened(dir.path(), {
            let opened = std::rc::Rc::clone(&opened);
            move |name| {
                if name == "n" && opened.get() < 100 {
                    opened.set(opened.get() + 1);
                    fs::rename(&n, kept.path().join(opened.get().to_string())).unwrap();
                    std::os::unix::fs::symlink(".", &n).unwrap();
                }
            }
        });
        LOOKING_AT.set(None);
        // The link was put in place once, and not followed.
        assert_eq!(opened.get(), 1);
        assert_eq!(made, "[MIME Cache]\ntext/plain=a-x.desktop;\n");
    }

    #[test]
    fn a_link_back_to_the_first_folder_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.desktop"), ENTRY).unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/b.desktop"), ENTRY).unwrap();
        std::os::unix::fs::symlink("..", dir.path().join("sub/up")).unwrap();
        assert_eq!(
            String::from_utf8(mimeinfo_cache(dir.path()).unwrap()).unwrap(),
            "[MIME Cache]\ntext/plain=a.desktop;sub-b.desktop;\n"
        );
    }

    #[test]
    fn an_interrupted_call_is_made_again() {
        // Each fault gives `EINTR` once, then is forgotten.
        let dir = three_entries();
        let all = fs::read_to_string(dir.path().join("mimeinfo.cache")).unwrap();
        let made = || {
            mimeinfo_cache(dir.path())
                .map(|made| String::from_utf8(made).unwrap())
                .map_err(|err| err.raw_os_error())
        };
        for folder in ["sub", "sub/deeper"] {
            fail(&UNOPENABLE, Some((&dir.path().join(folder), Errno::INTR)));
            assert_eq!(made(), Ok(all.clone()), "{folder}");
            assert!(
                UNOPENABLE.with_borrow(Option::is_none),
                "{folder}: interrupted"
            );
        }
        for (fault, name) in [
            (&UNSTATABLE, "deeper"),
            (&UNSTATABLE, "b.desktop"),
            (&UNREADABLE, "b.desktop"),
        ] {
            fail_name(fault, Some((name, Errno::INTR)));
            assert_eq!(made(), Ok(all.clone()), "{name}");
            assert!(fault.with_borrow(Option::is_none), "{name}: interrupted");
        }
        // The status of the first folder and of each sub-folder opened.
        for folder in ["", "sub", "sub/deeper"] {
            fail(&UNFSTATABLE, Some((&dir.path().join(folder), Errno::INTR)));
            assert_eq!(made(), Ok(all.clone()), "{folder}");
            assert!(
                UNFSTATABLE.with_borrow(Option::is_none),
                "{folder}: interrupted"
            );
        }
        // Opening a folder again, and reading its status. As in
        // `a_folder_opened_again_is_skipped_only_for_some_errors`: the walk
        // is back at `sub/deeper`, whose handle and that of `sub` were
        // closed.
        let first = open_folder(rustix::fs::CWD, dir.path()).unwrap();
        for fault in [&UNREOPENABLE, &UNFSTATABLE] {
            for folder in ["sub", "sub/deeper"] {
                let mut stack = ["", "sub", "sub/deeper"].map(|at| closed(dir.path(), at));
                let mut window = Window::new(MOST_OPEN);
                fail(fault, Some((&dir.path().join(folder), Errno::INTR)));
                let found = reopen(first.as_fd(), &mut stack, &mut window)
                    .map(|deeper| deeper.is_some())
                    .map_err(|err| err.raw_os_error());
                assert_eq!(found, Ok(true), "{folder}");
                assert!(fault.with_borrow(Option::is_none), "{folder}: interrupted");
            }
        }
    }

    #[test]
    fn special_files_and_linked_folders() {
        let dir = tempfile::tempdir().unwrap();
        let entry = "[Desktop Entry]\nType=Application\nName=N\nExec=n\nMimeType=text/plain;\n";
        fs::create_dir_all(dir.path().join("z")).unwrap();
        fs::write(dir.path().join("z/nested.desktop"), entry).unwrap();
        std::os::unix::fs::symlink("z", dir.path().join("a")).unwrap();
        // `mknodat` is not in rustix on Apple systems.
        #[cfg(not(target_vendor = "apple"))]
        rustix::fs::mknodat(
            rustix::fs::CWD,
            dir.path().join("blocked.desktop"),
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(mimeinfo_cache(dir.path()).unwrap()).unwrap(),
            "[MIME Cache]\ntext/plain=a-nested.desktop;\n",
            "the FIFO is skipped, and the folder is listed once, under its first name"
        );
    }

    /// Makes [`stat_at`] (with `fault` [`UNSTATABLE`]) or
    /// [`read_regular_file`] (with [`UNREADABLE`]) fail for `name` with
    /// `errno`; with `None`, for no name.
    fn fail_name(
        fault: &'static std::thread::LocalKey<RefCell<Option<(OsString, Errno)>>>,
        name: Option<(&str, Errno)>,
    ) {
        fault.set(name.map(|(name, errno)| (name.into(), errno)));
    }

    /// A folder with `a.desktop`, `sub/b.desktop` and
    /// `sub/deeper/c.desktop`, and its cache written.
    fn three_entries() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.desktop"), ENTRY).unwrap();
        fs::create_dir_all(dir.path().join("sub/deeper")).unwrap();
        fs::write(dir.path().join("sub/b.desktop"), ENTRY).unwrap();
        fs::write(dir.path().join("sub/deeper/c.desktop"), ENTRY).unwrap();
        write_mimeinfo_cache(dir.path()).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("mimeinfo.cache")).unwrap(),
            "[MIME Cache]\ntext/plain=a.desktop;sub-b.desktop;sub-deeper-c.desktop;\n"
        );
        dir
    }

    /// Checks that the walk of `dir` fails with each of [`FAILING`] when
    /// `fault` fails `name` with it, leaving its cache and files as they
    /// are, and skips `name` for each of [`SKIPPED`], giving `skipped`.
    fn fails_unless_skipped(
        dir: &Path,
        fault: &'static std::thread::LocalKey<RefCell<Option<(OsString, Errno)>>>,
        name: &str,
        skipped: &str,
    ) {
        let cache = dir.join("mimeinfo.cache");
        let before = fs::read(&cache).unwrap();
        let files_before = listed(dir);
        for errno in FAILING {
            fail_name(fault, Some((name, errno)));
            let made = mimeinfo_cache(dir);
            let written = write_mimeinfo_cache(dir);
            fail_name(fault, None);
            let error = Some(Some(errno.raw_os_error()));
            assert_eq!(
                made.err().map(|err| err.raw_os_error()),
                error,
                "{name}: {errno:?}"
            );
            assert_eq!(
                written.err().map(|err| err.raw_os_error()),
                error,
                "{name}: {errno:?}"
            );
            // The old cache is left as it is, and no file is left behind.
            assert_eq!(fs::read(&cache).unwrap(), before, "{name}: {errno:?}");
            assert_eq!(listed(dir), files_before, "{name}: {errno:?}");
        }
        for &errno in SKIPPED {
            fail_name(fault, Some((name, errno)));
            let made = mimeinfo_cache(dir);
            fail_name(fault, None);
            assert_eq!(
                String::from_utf8(made.unwrap()).unwrap(),
                skipped,
                "{name}: {errno:?}"
            );
        }
    }

    #[test]
    fn a_name_that_cannot_be_looked_at_fails_the_walk_unless_skipped() {
        use std::os::unix::fs::PermissionsExt;
        let dir = three_entries();
        fails_unless_skipped(
            dir.path(),
            &UNSTATABLE,
            "b.desktop",
            "[MIME Cache]\ntext/plain=a.desktop;sub-deeper-c.desktop;\n",
        );
        fails_unless_skipped(
            dir.path(),
            &UNSTATABLE,
            "deeper",
            "[MIME Cache]\ntext/plain=a.desktop;sub-b.desktop;\n",
        );
        // A folder that can be listed but not searched: each name in it
        // cannot be looked at (`EACCES`), and is skipped.
        let sub = dir.path().join("sub");
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o400)).unwrap();
        let enforced = fs::metadata(sub.join("b.desktop")).is_err();
        let made = mimeinfo_cache(dir.path());
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).unwrap();
        if enforced {
            assert_eq!(
                String::from_utf8(made.unwrap()).unwrap(),
                "[MIME Cache]\ntext/plain=a.desktop;\n"
            );
        } else {
            eprintln!("folder permissions are not enforced here (root?): not checked");
        }
    }

    #[test]
    fn an_entry_that_cannot_be_read_fails_the_walk_unless_skipped() {
        use std::os::unix::fs::PermissionsExt;
        let dir = three_entries();
        fails_unless_skipped(
            dir.path(),
            &UNREADABLE,
            "b.desktop",
            "[MIME Cache]\ntext/plain=a.desktop;sub-deeper-c.desktop;\n",
        );
        // A file that cannot be opened (`EACCES`) is skipped.
        let b = dir.path().join("sub/b.desktop");
        fs::set_permissions(&b, fs::Permissions::from_mode(0o000)).unwrap();
        let enforced = fs::read(&b).is_err();
        let made = mimeinfo_cache(dir.path());
        fs::set_permissions(&b, fs::Permissions::from_mode(0o644)).unwrap();
        if enforced {
            assert_eq!(
                String::from_utf8(made.unwrap()).unwrap(),
                "[MIME Cache]\ntext/plain=a.desktop;sub-deeper-c.desktop;\n"
            );
        } else {
            eprintln!("file permissions are not enforced here (root?): not checked");
        }
    }

    #[test]
    fn an_entry_too_large_or_not_a_regular_file_is_skipped() {
        let dir = three_entries();
        // Larger than the limit, without taking the room.
        let b = fs::File::options()
            .write(true)
            .open(dir.path().join("sub/b.desktop"))
            .unwrap();
        b.set_len(MAX_FILE_LEN + 1).unwrap();
        assert_eq!(
            String::from_utf8(mimeinfo_cache(dir.path()).unwrap()).unwrap(),
            "[MIME Cache]\ntext/plain=a.desktop;sub-deeper-c.desktop;\n"
        );
        // A FIFO, as if it replaced a regular file once its type was
        // checked: opened without waiting for a writer, and skipped.
        // `mknodat` is not in rustix on Apple systems.
        #[cfg(not(target_vendor = "apple"))]
        {
            let folder = open_at(rustix::fs::CWD, dir.path());
            rustix::fs::mknodat(
                &folder,
                "fifo.desktop",
                FileType::Fifo,
                Mode::RUSR | Mode::WUSR,
                0,
            )
            .unwrap();
            let mut types = BTreeMap::new();
            add(
                folder.as_fd(),
                OsStr::new("fifo.desktop"),
                b"fifo.desktop",
                &mut types,
            )
            .unwrap();
            assert!(types.is_empty(), "{types:?}");
        }
    }

    #[test]
    fn a_link_to_a_name_too_long_is_skipped() {
        let dir = three_entries();
        // A component longer than any system's name limit: looking it up
        // gives `ENAMETOOLONG`, every time.
        std::os::unix::fs::symlink("a".repeat(300), dir.path().join("sub/long.desktop")).unwrap();
        write_mimeinfo_cache(dir.path()).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("mimeinfo.cache")).unwrap(),
            "[MIME Cache]\ntext/plain=a.desktop;sub-b.desktop;sub-deeper-c.desktop;\n"
        );
    }

    /// The stack of a walk back at `a/b/c` in a new folder, whose handles
    /// are all closed but that of `b` when `b` is given, and the handle of
    /// the folder.
    fn back_at_c(b: bool) -> (tempfile::TempDir, OwnedFd, Vec<Level>) {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/b/c")).unwrap();
        let first = open_folder(rustix::fs::CWD, dir.path()).unwrap();
        let mut stack =
            Vec::from(["", "a", "a/b", "a/b/c"].map(|folder| closed(dir.path(), folder)));
        stack[2].handle = b.then(|| open_folder(rustix::fs::CWD, dir.path().join("a/b")).unwrap());
        (dir, first, stack)
    }

    /// Checks that `reopened` is the error of a broken invariant.
    fn broken(reopened: io::Result<Option<BorrowedFd<'_>>>) {
        let err = reopened.map(|_| ()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other, "{err}");
        assert!(err.to_string().contains("invariant"), "{err}");
    }

    #[test]
    fn a_handle_kept_but_closed_fails_the_walk() {
        // The window has the handle of `b` as open, but it is not.
        let (_dir, first, mut stack) = back_at_c(false);
        let mut window = Window::new(MOST_OPEN);
        window.insert(2);
        broken(reopen(first.as_fd(), &mut stack, &mut window));
    }

    #[test]
    fn no_handle_to_close_fails_the_walk() {
        // The only handle open is that of `b`, which `c` is opened from,
        // and there is no room for another.
        let (_dir, first, mut stack) = back_at_c(true);
        let mut window = Window {
            open: vec![2],
            most: 1,
        };
        broken(reopen(first.as_fd(), &mut stack, &mut window));
    }
}
