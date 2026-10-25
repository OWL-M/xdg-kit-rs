//! Writes `mimeinfo.cache`, the file `update-desktop-database`
//! (desktop-file-utils) creates in an `applications` folder to list the
//! types listed by each desktop entry. The file is outside the MIME
//! Applications spec. This crate does not read it; it writes it for other
//! programs, including `GLib`, which uses it for a folder's associations.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags};
use rustix::io::Errno;
use xdg_kit_desktop_entry::DesktopEntry;

use crate::{Limits, TypePattern};

/// The `mimeinfo.cache` for the `applications` folder `folder`, as
/// `update-desktop-database` writes it: `[MIME Cache]`, then one
/// `type=id;id;` line per type, both in byte order, for every desktop entry
/// under the folder (sub-folders give IDs joined by `-`) that is not
/// deleted ([`DesktopEntry::hidden`]) and is in UTF-8
/// ([`DesktopEntry::supports_encoding`]), as the tool lists them. Unlike
/// `AppIndex`, entries of no or unknown `Type` are listed, as the tool does.
/// Items of `MimeType=` that are not types or `media/*` wildcards (see
/// [`TypePattern::parse`]) are left out, `*/*` too, as the tool does. A
/// folder reached under two names through a link is listed under both,
/// as the tool and `AppIndex` do. Each ID appears once per type, even if
/// several paths produce it. The tool removes only consecutive repeats,
/// so it repeats an ID when another ID of the same type falls between
/// its paths in folder listing order.
///
/// Each ID is written escaped, as `GLib`'s key-file string lists expect,
/// so that `GLib` reads the same ID back: `\` as `\\`, line feed as `\n`,
/// tab as `\t`, carriage return as `\r`, `;` as `\;`, a leading space as
/// `\s`. IDs are ordered as named, not as escaped. The tool writes the
/// bytes as they are; in `GLib` a line feed then breaks the file, and a
/// backslash or `;` changes the IDs or loses the type's list. An ID
/// `GLib` cannot read back is left out, silently, wherever it would fall
/// in the line: one that is not UTF-8 (`GLib` would lose the type's whole
/// list), and one starting with a form feed (`GLib` skips white space
/// before a value and has no escape for a form feed). The walk still
/// lists and reads its file. A type left with no ID gets no line.
///
/// Links back to any ancestor folder are skipped, as in `AppIndex`.
/// The tool and `GLib` follow these loops until the path is too long:
/// one link produces dozens of IDs, and work grows exponentially with
/// the number of such links.
///
/// # Errors
///
/// Calls interrupted by a signal (`EINTR`) are retried. Interrupted folder
/// listings restart from the beginning. Any other error opening or
/// listing `folder`, listing an opened sub-folder, or reading an opened
/// folder's status fails the walk.
///
/// Errors looking up a name, opening a sub-folder, or opening, checking or
/// reading a desktop entry also fail the walk, except for the errors listed
/// below. This includes reopening a folder whose handle was closed. Failures
/// such as `EMFILE`, `ENFILE`, `ENOMEM`, `EIO` and `EAGAIN` may reflect a
/// system, disk or temporary problem rather than a problem with the name.
///
/// The walk lists at most [`Limits::max_listed`] entries, counted along
/// every path, so links cannot multiply its work without bound; past that
/// it fails with [`io::ErrorKind::Other`], naming the limit. It stops
/// reading a folder at the first entry past the limit, so a wide folder
/// is not read whole: an error its listing would give after that entry is
/// not met, and the walk fails with the limit's error instead (either way
/// there is no cache).
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
/// is not done. The folder a name opens is told by its device and inode
/// once opened; a name that was, when looked at, a folder on the path (a
/// link back to it) is skipped without being opened, a change to it since
/// not seen, as for a name the walk has passed. A desktop entry that
/// is not a regular file (also when it is replaced after its type was
/// checked) or is larger than [`Limits::max_file_len`] is skipped too, as
/// are the entries left out above. Failure to reserve memory for an entry,
/// including one that grows during a read, fails the walk with
/// [`io::ErrorKind::OutOfMemory`], with or without the size limit. On
/// systems that overcommit memory, such as Linux, a reservation may
/// succeed even when there is too little free memory. Reading the entry
/// may still exhaust memory.
///
/// This is [`mimeinfo_cache_with`] within the default [`Limits`].
pub fn mimeinfo_cache(folder: &Path) -> io::Result<Vec<u8>> {
    mimeinfo_cache_with(folder, &Limits::default())
}

/// [`mimeinfo_cache`] within `limits`.
///
/// # Errors
///
/// As [`mimeinfo_cache`].
pub fn mimeinfo_cache_with(folder: &Path, limits: &Limits) -> io::Result<Vec<u8>> {
    let mut types: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    collect(folder, &mut types, limits)?;
    let mut out = b"[MIME Cache]\n".to_vec();
    for (mime, ids) in types {
        out.extend_from_slice(mime.as_bytes());
        out.push(b'=');
        for id in ids {
            escape(&id, &mut out);
            out.push(b';');
        }
        out.push(b'\n');
    }
    Ok(out)
}

/// Writes `id` to `out` as an item of a `GLib` key-file string list: `\`,
/// line feed, tab, carriage return and `;` escaped, a leading space as
/// `\s`, as `xdg-kit-desktop-entry` escapes list items. Unescaped by
/// `g_key_file_get_string_list`, it is `id` again.
///
/// Written here: that crate's escaper is not public API.
fn escape(id: &str, out: &mut Vec<u8>) {
    for (at, byte) in id.bytes().enumerate() {
        match byte {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b';' => out.extend_from_slice(b"\\;"),
            b' ' if at == 0 => out.extend_from_slice(b"\\s"),
            _ => out.push(byte),
        }
    }
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
/// The returned [`Durability`](xdg_kit_basedir::Durability) says whether
/// the completed replacement is known to survive a crash:
/// [`Durability::NotSynced`](xdg_kit_basedir::Durability::NotSynced)
/// means the new `mimeinfo.cache` *is* in place but syncing the folder
/// afterwards failed — an outcome, not an error, since redoing the write
/// would overwrite whatever happened since.
pub fn write_mimeinfo_cache(folder: &Path) -> io::Result<xdg_kit_basedir::Durability> {
    write_mimeinfo_cache_with(folder, &Limits::default())
}

/// Writes the cache as [`write_mimeinfo_cache`] does, using `limits` for
/// [`mimeinfo_cache_with`].
///
/// # Errors
///
/// As [`write_mimeinfo_cache`].
pub fn write_mimeinfo_cache_with(
    folder: &Path,
    limits: &Limits,
) -> io::Result<xdg_kit_basedir::Durability> {
    use xdg_kit_basedir::{
        CreateMode, SymlinkPolicy, WriteFileError, WriteOptions, write_regular_file,
    };
    let bytes = mimeinfo_cache_with(folder, limits)?;
    let committed = write_regular_file(
        &folder.join("mimeinfo.cache"),
        &bytes,
        &WriteOptions {
            // The default permissions: `0666` as the umask leaves it.
            create: CreateMode::Masked(0o666),
            preserve_mode: false,
            symlinks: SymlinkPolicy::ReplaceLink,
        },
    )
    .map_err(|err| match err {
        WriteFileError::Before(err) => err,
        // Links are replaced and nothing is installed with no-replace
        // semantics, so nothing else arises.
        other => io::Error::other(other.to_string()),
    })?;
    Ok(committed.durability)
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
/// Uses an explicit stack because folders may nest deeper than a thread stack
/// allows. Looks up, opens and reads each name through its parent folder's
/// handle, without forming a path. This allows entries beyond `PATH_MAX`; IDs
/// are built from the names. Keeps at most [`MOST_OPEN`] handles (more only
/// beyond 2^14 folders deep, see [`Window`]). When returning to a folder whose
/// handle was closed, reopens it by name from a retained handle, as a path
/// lookup would ([`reopen`]).
///
/// Lists each folder when opened, through every path to it, but skips loops
/// back to ancestors. Identifies opened folders by their handle's device and
/// inode because a name may point elsewhere between lookup and opening. If
/// lookup already identifies an ancestor, skips the name without opening it.
/// Later changes to that name go unseen, as with names the walk has passed.
/// Exceeding [`Limits::max_listed`] entries fails the walk ([`names`]).
///
/// Skips names that cannot be looked up, sub-folders that cannot be opened or
/// reopened, and desktop entries that cannot be opened, checked or read, when
/// the error means the name cannot be used as written ([`skipped`]). Other
/// errors fail the walk. Listing failures also fail it, both for `folder` and
/// for opened sub-folders, rather than return an incomplete listing
/// (`update-desktop-database` keeps partial listings). Interrupted calls are
/// retried.
///
/// A reopened folder that differs from the one listed is skipped as if deleted
/// ([`reopen`]); a folder read through a retained handle stays as first seen.
/// Moving a folder during the walk may therefore leave unread entries unlisted.
/// A run after the move lists it fully. Revisiting it under the new name would
/// list previously read entries under both names. Desktop entries can also be
/// skipped for their file type, size or contents ([`add`]). A broken walk
/// invariant fails the walk ([`invariant`]).
fn collect(
    folder: &Path,
    types: &mut BTreeMap<String, BTreeSet<String>>,
    limits: &Limits,
) -> io::Result<()> {
    let first = open_folder(rustix::fs::CWD, folder)?;
    // Every path is listed, a folder reached under two names (a link) under
    // each, as `update-desktop-database` and `AppIndex` do; a link back to a
    // folder on the path (a loop) is not followed. A folder is told by the
    // device and inode of the handle opened; those of the name looked at
    // only skip a name that was a folder on the path.
    let stat = fstat(first.as_fd())?;
    let mut ancestors = HashSet::from([(stat.st_dev, stat.st_ino)]);
    let left = names(first.as_fd(), 0, limits.max_listed)?;
    let mut listed = left.len();
    let mut id = Vec::new();
    let mut stack = vec![Level {
        name: OsString::new(),
        stat,
        handle: None,
        left,
        prefix: 0,
    }];
    let mut window = Window::new(MOST_OPEN);
    while let Some(level) = stack.last_mut() {
        let Some(name) = level.left.pop() else {
            // Every handle kept is that of a folder on the stack, at its
            // depth, and the deepest kept is the last in `window.open`.
            let popped = stack.pop();
            if let Some(level) = &popped
                && !stack.is_empty()
            {
                ancestors.remove(&(level.stat.st_dev, level.stat.st_ino));
            }
            if popped.is_some_and(|level| level.handle.is_some()) {
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
                // A name that was a folder on the path (a link back to it)
                // when looked at is skipped unopened, a change to it since
                // unseen, as for a name passed. Otherwise the name may be
                // another folder by the time it is opened: checked again.
                if ancestors.contains(&(stat.st_dev, stat.st_ino)) {
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
                    // A folder on the path, now under this name, is skipped.
                    let stat = fstat(handle.as_fd())?;
                    if !ancestors.insert((stat.st_dev, stat.st_ino)) {
                        continue;
                    }
                    let left = names(handle.as_fd(), listed, limits.max_listed)?;
                    listed += left.len();
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
                add(at, &name, &id, types, limits.max_file_len)?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// The error of a walk past `max` ([`Limits::max_listed`]) entries listed:
/// it fails, so no cache is made.
fn too_many(max: usize) -> io::Error {
    io::Error::other(format!(
        "mimeinfo.cache walk: more than {max} entries to list; no cache made"
    ))
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

/// Returns names in `folder` in reverse byte order. Lists through a copy of the
/// handle, then closes it and keeps only the names. An interrupted listing is
/// rewound and restarted after discarding the names read so far. Other read
/// errors are returned instead of a partial listing. A handle opened for
/// lookups only ([`reopen_folder`]) cannot be listed. If the folder is deleted
/// during listing, rustix ends the listing on `ENOENT`.
///
/// # Errors
///
/// Also when the names, added to the `listed` entries the walk listed
/// before, pass `max` ([`Limits::max_listed`]): the listing stops at the
/// first name past it ([`too_many`]), so a wide folder is not read whole.
/// An error the listing would give after that name is not met: the cap's
/// error is returned instead (either way the walk fails).
fn names(folder: BorrowedFd<'_>, listed: usize, max: Option<usize>) -> io::Result<Vec<OsString>> {
    #[cfg(test)]
    if tests::UNLISTABLE.with_borrow(|unlistable| {
        unlistable.as_ref().is_some_and(|unlistable| {
            rustix::fs::fstat(folder).is_ok_and(|stat| same_folder(&stat, unlistable))
        })
    }) {
        return Err(Errno::IO.into());
    }
    let mut listing = Dir::new(folder.try_clone_to_owned()?)?;
    // On interruption, rewind and restart, discarding the partial listing.
    // Retry as often as needed, as `retry_on_intr` does.
    // The most names this listing may hold; `None`: no limit.
    let most = max.map(|max| max.saturating_sub(listed));
    let mut names = loop {
        match read_names(&mut listing, folder, most) {
            Err(Errno::INTR) => listing.rewind(),
            names => break names?,
        }
    };
    if let Some(max) = max
        && listed + names.len() > max
    {
        return Err(too_many(max));
    }
    // Sorted only once within the cap.
    names.sort_unstable_by(|a, b| b.cmp(a));
    Ok(names)
}

/// Reads the remaining names from `listing` (of `folder`), excluding `.` and
/// `..`. Stops once it holds one more than `most`.
fn read_names(
    listing: &mut Dir,
    folder: BorrowedFd<'_>,
    most: Option<usize>,
) -> rustix::io::Result<Vec<OsString>> {
    #[cfg(not(test))]
    let _ = folder;
    let mut names = Vec::new();
    while let Some(entry) = listing.read() {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(OsStr::from_bytes(name).to_owned());
            #[cfg(test)]
            tests::NAMES_READ.with(|read| read.set(read.get() + 1));
            if most.is_some_and(|most| names.len() > most) {
                break;
            }
        }
        // Interrupted once a name is kept, so that the listing is partial.
        #[cfg(test)]
        if !names.is_empty()
            && let Some(errno) = tests::fault(&tests::INTERRUPTED_LISTING, folder)
        {
            return Err(errno);
        }
    }
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
/// handles (`Window` in its `tree.rs`).
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
/// `2^(most - 2)` folders deep, so that no depth's rank reaches `most - 2`,
/// the rank tree magic caps: no rank is ever capped
/// (`the_window_grows_before_a_rank_would_be_capped` checks this, and the
/// formula below, to 2^15 folders deep). Once the walk has been `N` folders
/// deep (`N` ≥ 1), `most` is exactly `max(16, ⌊log2 N⌋ + 3)`: at most that
/// many handles are open, besides the first folder's and one more for a
/// moment (a listing's, or an entry's being read).
///
/// The walk opens folders again at most 6 times for each name looked at
/// or folder listed after the first, as far as the tests check: here
/// `going_back_up_a_deep_tree_keeps_few_handles`, 300 folders deep; for
/// tree magic's handles, its tests (named at `MAX_REOPENS` in its
/// `tree.rs`), to the 4,999 folders deep its default lookups allow. The
/// default [`Limits::max_listed`] lets this walk go deeper than either.
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
/// is not a regular file (any more) or is larger than `max_len`
/// ([`read_regular_file`]), when its ID is one `GLib` cannot read back
/// (not UTF-8, or starting with a form feed), and when it is deleted
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
    types: &mut BTreeMap<String, BTreeSet<String>>,
    max_len: Option<u64>,
) -> io::Result<()> {
    let Some(bytes) = skipping(read_regular_file(folder, name, max_len))?.flatten() else {
        return Ok(());
    };
    // IDs `GLib` cannot read back are left out: not UTF-8, or starting
    // with a form feed (skipped before a value, with no escape).
    let Ok(id) = std::str::from_utf8(id) else {
        return Ok(());
    };
    if id.starts_with('\x0c') {
        return Ok(());
    }
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
                .insert(id.to_owned());
        }
    }
    Ok(())
}

/// The status of the opened file `file` (named `name`), read again when
/// interrupted by a signal, as [`fstat`] reads a folder's.
fn fstat_file(file: BorrowedFd<'_>, name: &OsStr) -> rustix::io::Result<rustix::fs::Stat> {
    #[cfg(not(test))]
    let _ = name;
    rustix::io::retry_on_intr(|| {
        #[cfg(test)]
        if let Some(errno) = tests::named_fault(&tests::UNFSTATABLE_FILE, name) {
            return Err(errno);
        }
        rustix::fs::fstat(file)
    })
}

/// Reads the regular file `name` in `folder`, as
/// [`xdg_kit_basedir::read_regular_file`] reads one by path. Opens without
/// waiting after a type check, checks again that the opened file is regular,
/// and rejects files larger than `max_len` bytes (`None` allows any size).
/// Retries opening and checking after signal interruptions. Returns `None`
/// if the file is no longer regular or exceeds the size limit.
///
/// # Errors
///
/// When the file cannot be opened, checked or read, or memory cannot be
/// reserved for it ([`io::ErrorKind::OutOfMemory`]).
fn read_regular_file(
    folder: BorrowedFd<'_>,
    name: &OsStr,
    max_len: Option<u64>,
) -> io::Result<Option<Vec<u8>>> {
    let flags = OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let file = fs::File::from(rustix::io::retry_on_intr(|| {
        #[cfg(test)]
        if let Some(errno) = tests::named_fault(&tests::UNREADABLE, name) {
            return Err(errno);
        }
        rustix::fs::openat(folder, name, flags, Mode::empty())
    })?);
    let stat = fstat_file(file.as_fd(), name)?;
    let len = u64::try_from(stat.st_size).unwrap_or(0);
    let fits = |len: u64| max_len.is_none_or(|max| len <= max);
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile || !fits(len) {
        return Ok(None);
    }
    let mut bytes = buffer(len)?;
    // One byte more, to notice a file grown past the limit since.
    let most = max_len.map_or(u64::MAX, |max| max.saturating_add(1));
    read_rest(&mut file.take(most), &mut bytes, grow)?;
    Ok(fits(bytes.len() as u64).then_some(bytes))
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
#[allow(
    clippy::missing_const_for_thread_local,
    reason = "its thread locals are const; clippy flags them on targets without native thread locals (Android, OpenBSD, illumos)"
)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::io::Write;

    use super::*;

    #[test]
    fn a_buffer_too_large_for_memory_is_an_error_not_an_abort() {
        // Larger than any address space, whatever the machine's memory.
        let err = buffer(1 << 60).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::OutOfMemory);
        assert_eq!(buffer(4).unwrap().capacity(), 4);
    }

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
        assert_eq!(bytes, []);
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

    #[cfg(target_os = "linux")]
    #[test]
    fn a_file_longer_than_its_size_is_skipped_past_the_limit() {
        // A regular file whose size says 0 but whose reads give more, as a
        // file that grew after `fstat` would.
        let proc = open_at(rustix::fs::CWD, "/proc/self");
        let name = OsStr::new("status");
        assert_eq!(
            rustix::fs::statat(&proc, name, AtFlags::empty())
                .unwrap()
                .st_size,
            0
        );
        assert_eq!(
            read_regular_file(proc.as_fd(), name, Some(8)).unwrap(),
            None
        );
        let whole = read_regular_file(proc.as_fd(), name, None)
            .unwrap()
            .unwrap();
        assert!(whole.len() > 8, "{}", whole.len());
    }

    /// A change a test makes to the tree when the walk looks at a name.
    type Change = Box<dyn FnMut(&OsStr)>;

    /// Temporarily changes a file or folder's mode and restores its usual
    /// mode on drop, including during a panic. Restoration happens before
    /// the containing temporary folder is removed, so failed tests leave
    /// no files that cannot be removed.
    struct TempMode<'a> {
        path: &'a Path,
        usual: u32,
    }

    impl<'a> TempMode<'a> {
        /// Gives `path` the mode `mode`, and `usual` back when dropped.
        fn set(path: &'a Path, mode: u32, usual: u32) -> Self {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
            Self { path, usual }
        }
    }

    impl Drop for TempMode<'_> {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(self.path, fs::Permissions::from_mode(self.usual));
        }
    }

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
        /// The name whose opened file [`read_regular_file`] fails to read
        /// the status of on this thread, in any folder, and the error it
        /// gives.
        pub(super) static UNFSTATABLE_FILE: RefCell<Option<(OsString, Errno)>> = const { RefCell::new(None) };
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
        /// The folder whose listing [`names`] reads is interrupted on this
        /// thread once a name was kept, and the error it gives.
        pub(super) static INTERRUPTED_LISTING: RefCell<Option<(rustix::fs::Stat, Errno)>> =
            const { RefCell::new(None) };
        /// The names [`names`] read on this thread, `.` and `..` left out.
        pub(super) static NAMES_READ: Cell<usize> = const { Cell::new(0) };
        /// The calls interrupted by a fault on this thread ([`once`]).
        static INTERRUPTED: Cell<usize> = const { Cell::new(0) };
    }

    /// The error that `fault` ([`UNSTATABLE`], [`UNREADABLE`] or
    /// [`UNFSTATABLE_FILE`]) gives for `name`, if it names it ([`once`]).
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
        let _ = write_mimeinfo_cache(dir.path()).unwrap();
        assert_eq!(
            fs::read(dir.path().join("mimeinfo.cache")).unwrap(),
            mimeinfo_cache(dir.path()).unwrap()
        );
        assert!(mimeinfo_cache(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn the_cache_respects_the_umask() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.desktop"),
            "[Desktop Entry]\nType=Application\nName=A\nExec=a\nMimeType=text/plain;\n",
        )
        .unwrap();
        // The umask is process-global, hence the restore however it ends.
        let before = rustix::process::umask(rustix::fs::Mode::from_bits_truncate(0o077));
        let written = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            write_mimeinfo_cache(dir.path())
        }));
        rustix::process::umask(before);
        let _ = written.unwrap().unwrap();
        let mode = fs::metadata(dir.path().join("mimeinfo.cache"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "0666 as the umask leaves it");
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
        // bound of `Window`, at most 6 reopens for each step after the
        // first.
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
        let search_only = TempMode::set(&a, 0o111, 0o755);
        let enforced = fs::read_dir(&a).is_err();
        let found = reopen(first.as_fd(), &mut stack, &mut window)
            .map(|b| b.map(|b| rustix::fs::statat(b, "e.desktop", AtFlags::empty()).is_ok()))
            .map_err(|err| err.raw_os_error());
        drop(search_only);
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
        assert_eq!(names(listed.as_fd(), 0, None).unwrap(), ["e.desktop"]);
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
        let listing = names(handle.as_fd(), 0, None);
        assert!(listing.is_err(), "{listing:?}");
    }

    #[test]
    fn a_folder_opened_but_not_listed_fails_the_walk() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.desktop"), ENTRY).unwrap();
        fs::create_dir_all(dir.path().join("sub/deeper")).unwrap();
        fs::write(dir.path().join("sub/deeper/b.desktop"), ENTRY).unwrap();
        let _ = write_mimeinfo_cache(dir.path()).unwrap();
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
                written.map(|_| ()).map_err(|err| err.raw_os_error()),
                Err(Some(Errno::IO.raw_os_error())),
                "{unlistable}"
            );
            // The cache already there is kept, and no file is left behind.
            assert_eq!(fs::read(&cache).unwrap(), before, "{unlistable}");
            assert_eq!(files(), files_before, "{unlistable}");
        }
        // A folder that cannot be opened is still skipped.
        let sub = dir.path().join("sub");
        let closed = TempMode::set(&sub, 0o000, 0o755);
        let enforced = fs::read_dir(&sub).is_err();
        let listed = mimeinfo_cache(dir.path());
        drop(closed);
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
        let _ = write_mimeinfo_cache(dir.path()).unwrap();
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
                // The cache already there is kept, and no file is left behind.
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
        let _ = write_mimeinfo_cache(dir.path()).unwrap();
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
        let closed = TempMode::set(&a, 0o000, 0o755);
        let enforced = fs::read_dir(&a).is_err();
        let found = reopened(stack);
        drop(closed);
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
    fn a_folder_is_listed_as_the_folder_opened() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("a")).unwrap();
        fs::write(dir.path().join("a/x.desktop"), ENTRY).unwrap();
        fs::create_dir(dir.path().join("z")).unwrap();
        // `z` is an empty folder when looked at, and a link to `a` when
        // opened: what is listed is `a`.
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
        assert_eq!(made, "[MIME Cache]\ntext/plain=a-x.desktop;z-x.desktop;\n");
        // Every handle opened was closed.
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
        fs::write(dir.path().join("top.desktop"), ENTRY).unwrap();
        fs::create_dir(dir.path().join("a")).unwrap();
        fs::write(dir.path().join("a/x.desktop"), ENTRY).unwrap();
        let [top, a] = ["", "a"].map(|folder| rustix::fs::stat(dir.path().join(folder)).unwrap());
        let made = |seen_as| {
            SEEN_AS.set(Some((a, seen_as)));
            let made = mimeinfo_cache(dir.path());
            SEEN_AS.set(None);
            String::from_utf8(made.unwrap()).unwrap()
        };
        // Once opened, `a` has the device and inode of the first folder, on
        // the path: a loop, not listed.
        assert_eq!(made(top), "[MIME Cache]\ntext/plain=top.desktop;\n");
        // With that inode but on another device, it is listed.
        let mut other_device = top;
        other_device.st_dev ^= 1;
        assert_eq!(
            made(other_device),
            "[MIME Cache]\ntext/plain=a-x.desktop;top.desktop;\n"
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
    fn a_folder_on_the_path_is_not_opened_or_listed_again() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        fs::create_dir(&a).unwrap();
        fs::write(a.join("x.desktop"), ENTRY).unwrap();
        std::os::unix::fs::symlink("..", a.join("up")).unwrap();
        // Once the walk is in `a`, opening the first folder fails: the link
        // `up` to it, looked at, is not opened.
        LOOKING_AT.set(Some(Box::new({
            let top = dir.path().to_owned();
            move |name| {
                if name == "up" {
                    fail(&UNOPENABLE, Some((&top, Errno::IO)));
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
        // Once the walk is in `a`, listing the first folder fails: `z`, an
        // empty folder when looked at and a link to it when opened, is not
        // listed.
        fs::remove_file(a.join("up")).unwrap();
        let z = a.join("z");
        fs::create_dir(&z).unwrap();
        let made = changed_when_opened(dir.path(), {
            let (z, top) = (z.clone(), dir.path().to_owned());
            move |name| {
                if name == "z" && !z.is_symlink() {
                    fs::remove_dir(&z).unwrap();
                    std::os::unix::fs::symlink("..", &z).unwrap();
                    UNLISTABLE.set(Some(rustix::fs::stat(&top).unwrap()));
                }
            }
        });
        UNLISTABLE.set(None);
        assert!(z.is_symlink(), "`z` was changed when opened");
        assert_eq!(made, "[MIME Cache]\ntext/plain=a-x.desktop;\n");
    }

    #[test]
    fn ids_not_in_utf8_are_left_out() {
        // `GLib` cannot read them, and loses the type's whole list. A type
        // left with no ID gets no line.
        let dir = tempfile::tempdir().unwrap();
        let raw = |relative: &[u8]| dir.path().join(OsStr::from_bytes(relative));
        fs::write(raw(b"bad\x80.desktop"), ENTRY).unwrap();
        fs::write(dir.path().join("bad\u{fffd}.desktop"), ENTRY).unwrap();
        fs::create_dir(raw(b"d\xff")).unwrap();
        fs::write(raw(b"d\xff/a.desktop"), ENTRY).unwrap();
        fs::write(
            raw(b"only\xfe.desktop"),
            ENTRY.replace("text/plain", "text/only"),
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(mimeinfo_cache(dir.path()).unwrap()).unwrap(),
            "[MIME Cache]\ntext/plain=bad\u{fffd}.desktop;\n"
        );
    }

    #[test]
    fn ids_starting_with_a_form_feed_are_left_out() {
        // `GLib` drops white space before a value and cannot escape a form
        // feed: left out wherever they would fall in the line.
        let dir = tempfile::tempdir().unwrap();
        for name in ["\x0cff", "z\x0c", "\x0c"] {
            fs::write(dir.path().join(format!("{name}.desktop")), ENTRY).unwrap();
        }
        fs::create_dir(dir.path().join("\x0cd")).unwrap();
        fs::write(dir.path().join("\x0cd/a.desktop"), ENTRY).unwrap();
        fs::create_dir(dir.path().join("e")).unwrap();
        fs::write(dir.path().join("e/\x0cb.desktop"), ENTRY).unwrap();
        fs::write(dir.path().join("~.desktop"), ENTRY).unwrap();
        let cache = mimeinfo_cache(dir.path()).unwrap();
        assert_eq!(
            String::from_utf8(cache.clone()).unwrap(),
            "[MIME Cache]\ntext/plain=e-\x0cb.desktop;z\x0c.desktop;~.desktop;\n"
        );
        assert_eq!(
            glib_string_list(&cache, "text/plain"),
            Some(vec![
                "e-\x0cb.desktop".to_owned(),
                "z\x0c.desktop".to_owned(),
                "~.desktop".to_owned()
            ])
        );
    }

    /// The items `GLib` reads from `key` in `[MIME Cache]` of `cache`, as
    /// `g_key_file_load_from_data` and `g_key_file_get_string_list` do
    /// (glib/gkeyfile.c, 2.88.3); `None` where `GLib` gives an error. A
    /// little stricter: a key with `[` or `]` (a locale) refuses the file.
    fn glib_string_list(cache: &[u8], key: &str) -> Option<Vec<String>> {
        // `g_ascii_isspace`: no vertical tab.
        let space = |byte: &u8| matches!(byte, b' ' | b'\t' | b'\n' | b'\x0c' | b'\r');
        let mut group: Option<&[u8]> = None;
        let mut value = None;
        for line in cache.split_inclusive(|&byte| byte == b'\n') {
            // A carriage return goes only before a line feed.
            let line = line
                .strip_suffix(b"\r\n")
                .or_else(|| line.strip_suffix(b"\n"))
                .unwrap_or(line);
            let start = line.iter().position(|byte| !space(byte));
            let Some(line) = start.map(|start| &line[start..]) else {
                continue;
            };
            if line[0] == b'#' {
                continue;
            }
            let end = line.iter().position(|&byte| byte == b']');
            if line[0] == b'['
                && let Some(end) = end
                && line[end + 1..]
                    .iter()
                    .all(|&byte| matches!(byte, b' ' | b'\t'))
            {
                let name = &line[1..end];
                if name.is_empty()
                    || name
                        .iter()
                        .any(|&byte| byte == b'[' || byte < 0x20 || byte == 0x7f)
                {
                    return None;
                }
                group = Some(name);
                continue;
            }
            // Not an entry, group or comment, or before the first group: the
            // whole file is refused.
            let equals = line
                .iter()
                .position(|&byte| byte == b'=')
                .filter(|&at| at > 0)?;
            let name = &line[..equals];
            let name = &name[..=name.iter().rposition(|byte| !space(byte))?];
            if name.iter().any(|&byte| byte == b'[' || byte == b']') {
                return None;
            }
            let rest = &line[equals + 1..];
            let rest = &rest[rest
                .iter()
                .position(|byte| !space(byte))
                .unwrap_or(rest.len())..];
            if group? == b"MIME Cache" && name == key.as_bytes() {
                value = Some(rest);
            }
        }
        let value = std::str::from_utf8(value?).ok()?;
        let (mut items, mut item) = (Vec::new(), String::new());
        let mut chars = value.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => item.push(match chars.next()? {
                    's' => ' ',
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    '\\' => '\\',
                    ';' => ';',
                    _ => return None,
                }),
                ';' => items.push(std::mem::take(&mut item)),
                _ => item.push(c),
            }
        }
        if !item.is_empty() {
            items.push(item);
        }
        Some(items)
    }

    #[test]
    fn glib_reads_every_id_back() {
        let names = [
            "bad\nname",
            "a;b",
            "x\\y",
            "s\\sx",
            "n\\nn",
            "p\\;q",
            "w\\\\w",
            " lead",
            "\tlead",
            "tab\tx",
            "cr\rx",
            "a=b",
            "[x]",
            "#c",
        ];
        let all = tempfile::tempdir().unwrap();
        let mut every = vec!["d\\-z.desktop".to_owned()];
        for name in names {
            let dir = tempfile::tempdir().unwrap();
            let id = format!("{name}.desktop");
            fs::write(dir.path().join(&id), ENTRY).unwrap();
            fs::write(all.path().join(&id), ENTRY).unwrap();
            let cache = mimeinfo_cache(dir.path()).unwrap();
            assert_eq!(
                glib_string_list(&cache, "text/plain"),
                Some(vec![id.clone()]),
                "{id:?} in {:?}",
                String::from_utf8_lossy(&cache)
            );
            every.push(id);
        }
        // A folder's name, ending in an escape character.
        fs::create_dir(all.path().join("d\\")).unwrap();
        fs::write(all.path().join("d\\/z.desktop"), ENTRY).unwrap();
        every.sort();
        let cache = mimeinfo_cache(all.path()).unwrap();
        assert_eq!(
            glib_string_list(&cache, "text/plain"),
            Some(every),
            "{:?}",
            String::from_utf8_lossy(&cache)
        );
    }

    #[test]
    fn ids_are_written_escaped_as_glib_lists_expect() {
        let dir = tempfile::tempdir().unwrap();
        for name in [" a;b\\c\n", "\td\r e"] {
            fs::write(dir.path().join(format!("{name}.desktop")), ENTRY).unwrap();
        }
        assert_eq!(
            String::from_utf8(mimeinfo_cache(dir.path()).unwrap()).unwrap(),
            "[MIME Cache]\ntext/plain=\\td\\r e.desktop;\\sa\\;b\\\\c\\n.desktop;\n"
        );
    }

    #[test]
    fn a_folder_linked_under_two_names_is_listed_under_each() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("b")).unwrap();
        fs::write(dir.path().join("b/foo.desktop"), ENTRY).unwrap();
        std::os::unix::fs::symlink("b", dir.path().join("a")).unwrap();
        // A loop: back to the first folder, and to `b` itself.
        std::os::unix::fs::symlink("..", dir.path().join("b/self")).unwrap();
        std::os::unix::fs::symlink(".", dir.path().join("b/here")).unwrap();
        assert_eq!(
            String::from_utf8(mimeinfo_cache(dir.path()).unwrap()).unwrap(),
            "[MIME Cache]\ntext/plain=a-foo.desktop;b-foo.desktop;\n"
        );
    }

    #[test]
    fn one_file_under_two_paths_of_one_id_is_listed_once() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("x/y")).unwrap();
        fs::write(dir.path().join("x/y/foo.desktop"), ENTRY).unwrap();
        std::os::unix::fs::symlink("x/y", dir.path().join("x-y")).unwrap();
        assert_eq!(
            String::from_utf8(mimeinfo_cache(dir.path()).unwrap()).unwrap(),
            "[MIME Cache]\ntext/plain=x-y-foo.desktop;\n"
        );
    }

    #[test]
    fn a_wide_folder_is_read_only_until_the_cap_is_passed() {
        // A folder of 1,000 names, as the first folder and under it.
        let dir = tempfile::tempdir().unwrap();
        let wide = dir.path().join("wide");
        for at in 0..1000 {
            fs::create_dir_all(wide.join(format!("w/{at}"))).unwrap();
        }
        for (folder, names) in [(wide.join("w"), 1000), (wide, 1001)] {
            for max in [0, 1, 10, 500, 999] {
                let limits = Limits {
                    max_listed: Some(max),
                    ..Limits::default()
                };
                NAMES_READ.set(0);
                let err = mimeinfo_cache_with(&folder, &limits).unwrap_err();
                assert_eq!(
                    err.to_string(),
                    format!("mimeinfo.cache walk: more than {max} entries to list; no cache made"),
                    "{max}"
                );
                // Names read stop at the first past the cap.
                assert_eq!(NAMES_READ.get(), max + 1, "{folder:?} {max}");
            }
            // Within the cap, all is listed.
            let limits = Limits {
                max_listed: Some(names),
                ..Limits::default()
            };
            NAMES_READ.set(0);
            let made = mimeinfo_cache_with(&folder, &limits).unwrap();
            assert_eq!(made, b"[MIME Cache]\n");
            assert_eq!(NAMES_READ.get(), names, "{folder:?}");
        }
    }

    #[test]
    fn fan_out_through_links_fails_at_the_cap() {
        // Folders `d1` to `d17` outside, each but the last with two links to
        // the next, and two links to `d1`: about 2^18 entries along every
        // path, more than the cap.
        let dir = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.desktop"), ENTRY).unwrap();
        let _ = write_mimeinfo_cache(dir.path()).unwrap();
        let before = fs::read(dir.path().join("mimeinfo.cache")).unwrap();
        let levels = 17;
        for level in 1..=levels {
            let folder = ext.path().join(format!("d{level}"));
            fs::create_dir(&folder).unwrap();
            if level < levels {
                for name in ["l0", "l1"] {
                    let next = ext.path().join(format!("d{}", level + 1));
                    std::os::unix::fs::symlink(next, folder.join(name)).unwrap();
                }
            }
        }
        for name in ["l0", "l1"] {
            std::os::unix::fs::symlink(ext.path().join("d1"), dir.path().join(name)).unwrap();
        }
        let err = mimeinfo_cache(dir.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other, "{err}");
        assert_eq!(
            err.to_string(),
            "mimeinfo.cache walk: more than 100000 entries to list; no cache made"
        );
        // No cache is written; the one written before stays.
        assert!(write_mimeinfo_cache(dir.path()).is_err());
        assert_eq!(fs::read(dir.path().join("mimeinfo.cache")).unwrap(), before);
        assert_eq!(
            listed(dir.path()),
            ["a.desktop", "l0", "l1", "mimeinfo.cache"]
        );
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
    fn a_link_to_an_ancestor_further_up_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/b/c/d/e")).unwrap();
        fs::write(dir.path().join("a/b/x.desktop"), ENTRY).unwrap();
        fs::write(dir.path().join("a/b/c/d/e/f.desktop"), ENTRY).unwrap();
        // At depth 5, a link to `a/b`, three levels up.
        std::os::unix::fs::symlink("../../..", dir.path().join("a/b/c/d/e/up3")).unwrap();
        assert_eq!(
            String::from_utf8(mimeinfo_cache(dir.path()).unwrap()).unwrap(),
            "[MIME Cache]\ntext/plain=a-b-c-d-e-f.desktop;a-b-x.desktop;\n"
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
            (&UNFSTATABLE_FILE, "b.desktop"),
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
        // Listing a folder: listed again from the start, the partial
        // listing dropped.
        for folder in ["", "sub", "sub/deeper"] {
            let path = dir.path().join(folder);
            fail(&INTERRUPTED_LISTING, Some((&path, Errno::INTR)));
            let handle = open_folder(rustix::fs::CWD, &path).unwrap();
            let mut names = names(handle.as_fd(), 0, None).map_err(|err| err.raw_os_error());
            if let Ok(names) = &mut names {
                names.reverse();
            }
            assert_eq!(names, Ok(listed(&path)), "{folder}");
            assert!(
                INTERRUPTED_LISTING.with_borrow(Option::is_none),
                "{folder}: interrupted"
            );
            fail(&INTERRUPTED_LISTING, Some((&path, Errno::INTR)));
            assert_eq!(made(), Ok(all.clone()), "{folder}");
            assert!(
                INTERRUPTED_LISTING.with_borrow(Option::is_none),
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
            "[MIME Cache]\ntext/plain=a-nested.desktop;z-nested.desktop;\n",
            "the FIFO is skipped, and the folder is listed under each name"
        );
    }

    /// Makes [`stat_at`] (with `fault` [`UNSTATABLE`]) or
    /// [`read_regular_file`] (with [`UNREADABLE`], or [`UNFSTATABLE_FILE`]
    /// for the status of the file opened) fail for `name` with `errno`;
    /// with `None`, for no name.
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
        let _ = write_mimeinfo_cache(dir.path()).unwrap();
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
            // The cache already there is kept, and no file is left behind.
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
        let list_only = TempMode::set(&sub, 0o400, 0o755);
        let enforced = fs::metadata(sub.join("b.desktop")).is_err();
        let made = mimeinfo_cache(dir.path());
        drop(list_only);
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
        let dir = three_entries();
        fails_unless_skipped(
            dir.path(),
            &UNREADABLE,
            "b.desktop",
            "[MIME Cache]\ntext/plain=a.desktop;sub-deeper-c.desktop;\n",
        );
        // A file opened but whose status cannot be read.
        fails_unless_skipped(
            dir.path(),
            &UNFSTATABLE_FILE,
            "b.desktop",
            "[MIME Cache]\ntext/plain=a.desktop;sub-deeper-c.desktop;\n",
        );
        // A file that cannot be opened (`EACCES`) is skipped.
        let b = dir.path().join("sub/b.desktop");
        let closed = TempMode::set(&b, 0o000, 0o644);
        let enforced = fs::read(&b).is_err();
        let made = mimeinfo_cache(dir.path());
        drop(closed);
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
        b.set_len(xdg_kit_basedir::MAX_FILE_LEN + 1).unwrap();
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
                Limits::default().max_file_len,
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
        let _ = write_mimeinfo_cache(dir.path()).unwrap();
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
