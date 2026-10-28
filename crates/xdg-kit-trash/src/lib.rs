//! The freedesktop.org Trash Specification (version 1.0): trashing files
//! into trash directories, listing them, restoring them, erasing them, and
//! the `directorysizes` cache.
//!
//! ```no_run
//! use xdg_kit_trash::{Context, Origin, RestoreOptions};
//!
//! let context = Context::from_env()?;
//! let trashed = context.trash("notes.txt")?;
//! println!("in {} as {:?}", trashed.trash.path().display(), trashed.name);
//!
//! let trash = context.open(&context.home_trash())?;
//! for entry in trash.entries() {
//!     let entry = entry?;
//!     if let Origin::Known(path) = entry.origin() {
//!         if path.ends_with("notes.txt") {
//!             let restored = trash.restore(&entry, &RestoreOptions::default())?;
//!             println!("back at {}", restored.destination.display());
//!         }
//!     }
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Context and policy
//!
//! A [`Context`] holds everything taken from outside: `$XDG_DATA_HOME`,
//! the user ID, the working directory relative paths are taken from, the
//! time zone and clock of deletion times, the [`Policy`] and the
//! [`Limits`]. [`Context::from_env`] reads the environment (and the
//! system's time zone) once; no operation reads it again.
//!
//! Discovery and operations are separate. [`Context::trash`] chooses a
//! trash directory for a file; [`Context::plan`] says what it would choose,
//! by the same selection, creating nothing (advisory: the trashing still
//! decides); [`Context::trash_into`] uses one the caller
//! chose; [`Context::open`] opens a given one and [`Context::discover`]
//! the home trash plus the per-volume ones of given top directories. Which
//! top directories exist is the caller's to say: [`mounts`] lists them on
//! Linux and Android. Whether a network or removable file system should
//! have per-volume trash directories is the caller's policy too: the crate
//! does not classify file systems.
//!
//! # Trashing
//!
//! A file goes to the home trash when it is on the home trash's mount,
//! otherwise to `$topdir/.Trash/$uid` on its own mount, else
//! `$topdir/.Trash-$uid`. Mounts are told apart by Linux's `statx` mount
//! ID where available; elsewhere only devices are, so a bind mount looks
//! like its source — the rename is then the test, and a rename across
//! mounts fails cleanly ([`AttemptFailure::CrossDevice`]) and the next
//! candidate is tried. The payload is only ever renamed: never copied,
//! and never erased because trashing failed. A candidate that fails before
//! the move gives way to the next; once the move succeeds there is no
//! fallback. A trash directory is never trashed, nor anything inside one:
//! besides the trash directories selection knows, any directory (not a
//! link) named `.Trash` or `.Trash-<digits>` right below a mount's top
//! directory counts as one, another user's included.
//!
//! Each insertion creates the trash info file first with `O_EXCL`, under a
//! storage name cut to fit the reported name limits of `files` and `info`
//! (leaving room for `.trashinfo` and a numeric suffix), writes and syncs
//! it and `info`, then renames the payload into `files` with a rename that
//! refuses to replace anything (`renameat2(RENAME_NOREPLACE)`,
//! `renameatx_np(RENAME_EXCL)`). A name taken in `files` — by an orphaned
//! payload, a directory, a dangling link, or a concurrent trashing — is
//! released and the next one tried. Where no such rename exists, trashing
//! fails as [`Unsupported`]; nothing checks first and replaces after.
//!
//! ## Outcomes of failures and crashes
//!
//! | Step | Failure | Crash after it |
//! |---|---|---|
//! | create the trash info file | next candidate; nothing changed | — |
//! | write it, sync it, sync `info` | reservation removed (if still ours), next candidate | a trash info file without payload |
//! | rename the payload | reservation removed, next candidate (or stop: source gone, unsupported) | the same |
//! | sync `files` and the source's directory | trashed, [`Durability::NotSynced`] | trashed, or (unsynced) back at the source with a trash info file without payload |
//!
//! A payload in `files` without its trash info file never results: the
//! file is synced, with its directory, before the move. A reservation that
//! cannot be removed is reported ([`Leftover`]). Atomicity comes from the
//! rename; durability from the syncs, reported separately.
//!
//! # Listing
//!
//! [`Trash::entries`] reads `files` and `info` as it goes. Every payload is
//! listed, with its metadata or why there is none ([`Metadata`]); then
//! every trash info file without a payload. An [`Entry`]'s
//! [`Origin`] is known only from a valid trash info file: never from the
//! storage name. A listing that could not finish ends with a
//! [`ListError`], never as if complete.
//!
//! # Restoring, erasing, maintenance
//!
//! [`Trash::restore`] moves a payload back without replacing anything
//! ([`RestoreFailure::Conflict`]), creates missing directories only when
//! asked, and removes the trash info file afterwards. [`Trash::erase`]
//! removes a payload for good, never following links or crossing mounts,
//! cancellable, and removes the trash info file only once the payload is
//! confirmed gone. Both first compare the entry with the trash; see
//! [`Changed`] for what that does and does not establish.
//!
//! Trash info files without payloads are never removed automatically, by
//! age or otherwise: a trashing in progress looks exactly like an
//! abandoned one. [`Trash::discard_metadata`] removes one when the caller
//! asserts the trash is quiescent ([`Quiescent`]).
//!
//! # Compliance
//!
//! | Spec | Level | Here |
//! |---|---|---|
//! | Home trash at `$XDG_DATA_HOME/Trash` | MUST | [`Context::home_trash`]; created when needed (SHOULD) |
//! | Files from the home trash's file system go there | SHOULD | yes, by mount (device where mounts are unknown) |
//! | Trash other file systems into the home trash | MAY | no: that is a copy |
//! | Trash into top directories | MAY | yes, [`Policy::volume_trash`] |
//! | Check `$topdir/.Trash` (sticky bit, not a link) | MUST | yes; the sticky check skipped only for [`Policy::unchecked_sticky`] |
//! | A failed check: not used, reported | MUST, SHOULD | yes: [`AttemptFailure::Open`], [`Discovery::problems`] |
//! | Create `$uid`, `.Trash-$uid` without delay | MUST | yes, mode 0700 |
//! | Fall back from (1) to (2) | MUST | yes; [`Policy::private_volume_trash`] off disables (2) |
//! | Both fail: home trash, or refuse and warn, never erase | MUST | refuse: [`TrashError::NoTrash`] |
//! | List both layouts of every known top directory | MUST, SHOULD | [`Context::discover`] with the caller's top directories |
//! | Move into `$trash/files`, never overwrite | MUST | non-replacing rename only |
//! | Keep times, permissions, attributes | SHOULD | a rename keeps them all |
//! | Never derive the original name from the storage name | MUST | [`Origin::Unknown`] |
//! | Missing trash info presented as an emergency | MUST | [`Metadata::Missing`], [`UnknownOrigin::MetadataMissing`] |
//! | `info/<name>.trashinfo`, first line `[Trash Info]` | MUST | yes; another first line: [`UnknownOrigin::Header`] |
//! | `Path`: relative from the base, no `..`; absolute only in the home trash | MUST, SHOULD | written relative when below the base; `..` refused; [`Policy::absolute_paths`] |
//! | `Path` as bytes, URL-escaped | SHOULD | byte-exact both ways |
//! | `DeletionDate` `YYYY-MM-DDThh:mm:ss`, local time | MUST, SHOULD | in the context's zone; read without a zone ([`Instants`]) |
//! | Ignore other lines; the first occurrence wins | MUST | yes, malformed first occurrences included |
//! | Create the trash info file first, atomically | MUST | `O_EXCL`, before the move |
//! | `directorysizes`, `du -B1` sizes, info mtimes, 64-bit times | SHOULD | [`Trash::size`] |
//! | Read fully escaped names; no `/` | MUST | yes |
//! | Update by temporary file and atomic rename | MUST | yes, relative to the open trash directory |
//! | Check the right to delete before trashing | SHOULD | not separately: the rename is the check, and changes nothing when refused |
//! | Restore or erase directories whole | advice | yes |
//!
//! # Platforms
//!
//! | Operation | Linux, Android | Apple systems | FreeBSD, NetBSD, OpenBSD, `DragonFly`, `illumos` |
//! |---|---|---|---|
//! | [`Context::trash`], [`Context::trash_into`] | `renameat2(RENAME_NOREPLACE)`; [`Unsupported`] where the file system refuses it | `renameatx_np(RENAME_EXCL)`; likewise | [`Unsupported`] |
//! | [`Context::plan`] | as trashing (it answers for it) | as trashing | [`Unsupported`], as trashing |
//! | [`Trash::restore`] | as trashing | as trashing | [`Unsupported`] |
//! | [`Context::open`], [`Context::discover`], [`Trash::entries`] | yes | yes | yes |
//! | [`Trash::erase`], [`Trash::discard_metadata`] | yes | yes | yes |
//! | [`Trash::size`] | yes | yes | yes |
//! | Mount boundaries (trash choice, erase, size) | `statx` mount ID (Linux 5.8+), else device | device | device |
//! | [`mounts`] | yes | — | — |
//!
//! Every system: names are resolved relative to open directories without
//! following links in the final component. A directory that is a mount
//! point of the same device (a bind mount) is told apart only where mount
//! IDs are known.

#[cfg(not(unix))]
compile_error!("xdg-kit-trash supports Unix systems only");

mod context;
mod dir;
mod encode;
mod fault;
#[cfg(fuzzing)]
#[doc(hidden)]
pub mod fuzzing;
mod info;
mod insert;
mod limits;
mod list;
mod mounts;
mod ops;
mod plan;
mod sizes;
mod sys;
#[cfg(test)]
mod tests;
mod time;

pub use context::{Context, ContextError, Policy};
pub use dir::{CheckFailure, Discovery, OpenError, OpenErrorKind, Trash, TrashDir, TrashKind};
pub use encode::EscapeProblem;
pub use info::{AbsolutePaths, Field, FieldProblem, HeaderProblem, PathProblem, TrashInfo};
pub use insert::{
    Attempt, AttemptFailure, Leftover, SourceProblem, TrashError, Trashed, Unsupported,
};
pub use limits::Limits;
pub use list::{
    Entries, Entry, InvalidName, ListError, Metadata, Origin, Payload, TrashId, UnknownOrigin,
};
#[cfg(any(target_os = "linux", target_os = "android"))]
pub use mounts::mounts;
pub use mounts::{Mount, MountsError, parse_mountinfo};
pub use ops::{
    Changed, DiscardError, EraseError, EraseFailure, EraseOptions, EraseProblem, EraseProblemKind,
    Erased, MetadataRemoval, Permissions, Quiescent, RestoreError, RestoreFailure, RestoreOptions,
    Restored,
};
pub use plan::{Candidate, CandidateState, TrashPlan};
pub use sizes::{
    CacheEntry, CacheProblem, CacheProblemKind, CacheRead, CacheUse, CacheWrite,
    MAX_CACHE_PROBLEMS, ParsedSizes, SizeError, SizeOptions, SizeProblem, SizeProblemKind,
    SizeReport, format_directory_sizes, parse_directory_sizes,
};
pub use sys::{FileKind, NO_REPLACE_RENAME};
pub use time::{Clock, DeletionDate, Instants, LocalZone, ZoneError};
pub use xdg_kit_basedir::Durability;

// Values are shared between threads; keep them that way.
const _: fn() = || {
    fn shareable<T: Send + Sync>() {}
    shareable::<Context>();
    shareable::<Trash>();
    shareable::<Entry>();
    shareable::<TrashDir>();
    shareable::<Limits>();
};
