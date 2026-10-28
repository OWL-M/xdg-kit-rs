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
//! does not classify file systems. [`Policy::exclusions`] names the places
//! trashing keeps out of — built from a [`mounts`] snapshot
//! ([`Exclusions::from_mounts`]) or the caller's configuration — and is
//! checked from the path alone, before anything touches the file system
//! (see [Exclusions](#exclusions)).
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
//! ## A home trash kept elsewhere
//!
//! `$XDG_DATA_HOME/Trash` may be a symbolic link to a directory of any
//! name ([`Policy::follow_home_link`], on by default): opening, discovery,
//! trashing, planning, listing, restoring, erasing and sizing all use the
//! directory it leads to. The spec forbids a link only at `$topdir/.Trash`;
//! here only that one component is followed, nothing else in any trash.
//! The link is read once and followed from open directories, name by name,
//! each opened without following a link; the directory reached is held
//! open and checked as the home trash always is (the user's, not writable
//! by group or others), and `files` and `info` in it are walked as in any
//! trash. A link that leads to no directory — dangling, to another kind of
//! file, a loop — fails as [`OpenErrorKind::BrokenLink`], is listed among
//! [`Discovery::problems`], and nothing is created through it; `files` and
//! `info` missing in a valid target are created as in any home trash.
//!
//! An operation follows the link once and uses what it opened for the
//! mount comparison, the checks and the move alike; an open [`Trash`]
//! stays on its directory however the link is changed afterwards, while a
//! later open follows the link as it is then ([`Trash::link_target`] says
//! where it led). Changing the link concurrently can therefore make an
//! operation use the directory it led to when read, or fail if a directory
//! on the way is replaced by a link meanwhile; it cannot make an operation
//! use a directory that was not checked, nor follow a link swapped in on
//! the way. The home trash's mount is the target's: files on the link's
//! own mount are then on another mount than the home trash, and go to
//! their volume's trash or are refused — never copied. The link itself,
//! its target, a directory holding the target, and anything inside it are
//! never trashed ([`SourceProblem::HomeTrashLink`],
//! [`SourceProblem::HoldsTrash`], [`SourceProblem::InTrash`]) — a target
//! that fails its checks or cannot be read, or whose `files` or `info` is
//! excluded, included, as it still holds what it holds. A home
//! trash given to [`Context::trash_into`] or [`Context::open`] that is
//! another context's is followed by its own link, not this one's. Relative
//! `Path` values are relative to `$XDG_DATA_HOME`, the home trash's base,
//! wherever the trash is kept.
//!
//! Each insertion creates the trash info file first with `O_EXCL`, under a
//! storage name cut to fit the reported name limits of `files` and `info`
//! (leaving room for `.trashinfo` and a numeric suffix), writes and syncs
//! it and `info`, then renames the payload into `files` with a rename that
//! refuses to replace anything (`renameat2(RENAME_NOREPLACE)`,
//! `renameatx_np(RENAME_EXCL)`). A name taken in `files` — by an orphaned
//! payload, a directory, a dangling link, or a concurrent trashing — is
//! released and the next one tried. Nothing checks first and replaces
//! after.
//!
//! ## Where the native rename is refused
//!
//! Only when that rename is refused as unsupported (the system lacks it, or
//! the file system refuses it) do the fallbacks of [`Policy::fallbacks`]
//! apply; a taken name, another mount, a permission or I/O error is
//! reported as it is. By default ([`Fallbacks::Link`]) a non-directory is
//! hard-linked into `files` under the reserved name — which fails if the
//! name is taken — then `files` is synced, then its source name removed,
//! if still the file inspected: no overwrite, but not atomic
//! ([`MoveMethod::is_atomic`]). The only identity relied on is the
//! source's as inspected: a name holding anything else is never removed.
//! Should the move not complete, the link is removed again — only while
//! the source still holds the payload, so that removing it loses nothing
//! — and the attempt fails as if nothing moved; if the source no longer
//! holds it, the link is its only name and the move counts as done.
//! Anything else stops trashing with [`TrashError::Partial`] and no other
//! trash is tried; its [`PartialState`] says whether both names are
//! verified to hold the payload. Directories need
//! [`Fallbacks::LinkAndPlaceholder`], which renames them over an empty
//! placeholder directory made for them, under a weaker concurrency model
//! spelled out at [`Fallbacks`]. [`Fallbacks::Native`] keeps the strict
//! behavior: [`Unsupported`] wherever the native rename is refused.
//! [`Trashed::method`] says which way was taken. The link fallback needs
//! hard links: where the file system refuses them, the result is
//! [`Unsupported`] with that cause.
//!
//! ## Outcomes of failures and crashes
//!
//! | Step | Failure | Crash after it |
//! |---|---|---|
//! | create the trash info file | next candidate; nothing changed | — |
//! | write it, sync it, sync `info` | reservation removed (if still ours), next candidate | a trash info file without payload |
//! | rename the payload | reservation removed, next candidate (or stop: source gone, unsupported) | the same |
//! | sync `files` and the source's directory | trashed, [`Durability::NotSynced`] | trashed, or (unsynced) back at the source with a trash info file without payload |
//! | link fallback: link into `files`, check it is the payload | as the rename; a name holding anything else: [`TrashError::Partial`] ([`PartialState::Unverified`]), nothing removed | the file under both names, the trash entry complete; nothing lost |
//! | link fallback: sync `files` | link removed again (while the source still holds the payload), next candidate | the same |
//! | link fallback: remove the source name (if still the payload) | link removed again while the source still holds the payload, next candidate; else [`TrashError::Partial`] with its verified or unverified state, no other candidate | trashed (the link synced before); or both names |
//! | placeholder fallback: make the placeholder | name taken: next name; else as the rename | an empty directory with a valid trash info file, listed as an ordinary entry: indistinguishable from a trashed empty directory |
//! | placeholder fallback: rename over it | placeholder removed if still ours and empty (else [`Leftover`] of [`LeftoverKind::Placeholder`]), next name or candidate | trashed, or the placeholder as above |
//!
//! A payload in `files` without its trash info file never results: the
//! file is synced, with its directory, before the move. A reservation or a
//! placeholder that cannot be removed is reported ([`Leftover`]).
//! Atomicity comes from the rename (not from the link fallback);
//! durability from the syncs, reported separately.
//!
//! # Exclusions
//!
//! [`Context::trash`], [`Context::trash_into`] and [`Context::plan`] check
//! the path — as given, anchored at the context's working directory, every
//! place its lookup would visit — against [`Policy::exclusions`] before any
//! system call: an excluded path is refused as [`TrashError::Excluded`]
//! having touched nothing. Excluded trash directories (the home trash, a
//! volume's, an explicit one) are passed over without being probed or
//! created ([`AttemptFailure::Excluded`]). Once the source's real path is
//! known, it is checked again, which catches an alias (a link into an
//! excluded place) — only after resolving it reached that place. A path
//! that passes is trashed as usual; nothing promises it cannot reach a
//! hung volume some other way. A home trash reached through a link is
//! checked before following it, at each link met on the way, with the
//! rest of the way still to go, and at the directory reached — each
//! spelling with its `files` and `info`: one that leads into an excluded
//! place is passed over like an excluded home trash. An excluded `files`
//! or `info` is never entered — whether named through the link's target
//! or as `$XDG_DATA_HOME/Trash/files` and `…/info` — but following goes on
//! to the directory holding it, which then still counts as a trash
//! directory. A relative link's target is checked both
//! from `$XDG_DATA_HOME` as configured and from its resolved path, which
//! resolving it reaches whatever an alias there leads to. Listing,
//! restoring and erasing do not use the exclusions: [`Context::discover`]
//! lists whatever top directories it is given, and opens a linked home
//! trash wherever it leads.
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
//! | A link at `$XDG_DATA_HOME/Trash` | not addressed | followed to a directory of any name, which passes the home trash's checks ([`Policy::follow_home_link`]); never created through |
//! | Files from the home trash's file system go there | SHOULD | yes, by mount (device where mounts are unknown) |
//! | Trash other file systems into the home trash | MAY | no: that is a copy |
//! | Trash into top directories | MAY | yes, [`Policy::volume_trash`] |
//! | Check `$topdir/.Trash` (sticky bit, not a link) | MUST | yes; the sticky check skipped only for [`Policy::unchecked_sticky`] |
//! | A failed check: not used, reported | MUST, SHOULD | yes: [`AttemptFailure::Open`], [`Discovery::problems`] |
//! | Create `$uid`, `.Trash-$uid` without delay | MUST | yes, mode 0700 |
//! | Fall back from (1) to (2) | MUST | yes; [`Policy::private_volume_trash`] off disables (2) |
//! | Both fail: home trash, or refuse and warn, never erase | MUST | refuse: [`TrashError::NoTrash`] |
//! | List both layouts of every known top directory | MUST, SHOULD | [`Context::discover`] with the caller's top directories |
//! | Move into `$trash/files`, never overwrite | MUST | non-replacing rename; where refused, a hard link (never replaces) or, opted in, a rename over a placeholder made for it ([`Fallbacks`]); never a copy |
//! | Keep times, permissions, attributes | SHOULD | a rename and a hard link keep them all (a link changes the status change time) |
//! | Trashing from some volumes refused | MAY | [`Policy::exclusions`], checked before touching them |
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
//! | [`Context::trash`], [`Context::trash_into`] | `renameat2(RENAME_NOREPLACE)`; where the file system refuses it, the [`Fallbacks`] | `renameatx_np(RENAME_EXCL)`; likewise | no native rename: the [`Fallbacks`] always (by default the link fallback, so directories are [`Unsupported`] unless the placeholder fallback is opted in) |
//! | [`Context::plan`] | yes; it does not predict the move's method | yes | yes |
//! | [`Trash::restore`] | as trashing | as trashing | as trashing |
//! | [`Policy::exclusions`] | lexical, no system call | the same | the same |
//! | A link at `$XDG_DATA_HOME/Trash` ([`Policy::follow_home_link`]) | followed name by name from open directories, holding `$XDG_DATA_HOME` and those on the way with `O_PATH` (search permission suffices) | the same, but those directories are opened read-only, which needs read permission too: where one cannot be read, the home trash cannot be used, and its target is located by path alone (search permission suffices) so that what it holds is still recognised as trashed | the same as Apple systems |
//! | [`Context::open`], [`Context::discover`], [`Trash::entries`] | yes | yes | yes |
//! | [`Trash::erase`], [`Trash::discard_metadata`] | yes | yes | yes |
//! | [`Trash::size`] | yes | yes | yes |
//! | Mount boundaries (trash choice, erase, size) | `statx` mount ID (Linux 5.8+), else device | device | device |
//! | [`mounts`] | yes | — | — |
//!
//! Every system: names are resolved relative to open directories without
//! following links in the final component — but for the home trash's own
//! link, where the policy follows it. A directory that is a mount
//! point of the same device (a bind mount) is told apart only where mount
//! IDs are known.

#[cfg(not(unix))]
compile_error!("xdg-kit-trash supports Unix systems only");

mod context;
mod dir;
mod encode;
mod exclude;
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
mod transfer;

pub use context::{Context, ContextError, Policy};
pub use dir::{CheckFailure, Discovery, OpenError, OpenErrorKind, Trash, TrashDir, TrashKind};
pub use encode::EscapeProblem;
pub use exclude::{Excluded, ExclusionReason, ExclusionRule, ExclusionScope, Exclusions};
pub use info::{AbsolutePaths, Field, FieldProblem, HeaderProblem, PathProblem, TrashInfo};
pub use insert::{
    Attempt, AttemptFailure, Leftover, LeftoverKind, PartialTrash, SourceProblem, TrashError,
    Trashed, Unsupported,
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
pub use transfer::{Fallbacks, MoveMethod, PartialState};
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
