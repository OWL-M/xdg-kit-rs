//! The freedesktop.org Desktop Application Autostart spec: the `.desktop`
//! files a session starts after login — which ones, why the others do
//! not, how to launch them — and the user's overrides that enable or
//! disable them.
//!
//! The crate never starts anything: discovery and planning only read,
//! and [`Autostart::prepare`] returns a
//! [`LaunchPlan`](xdg_kit_desktop_entry::LaunchPlan) the caller spawns.
//!
//! ```no_run
//! use xdg_kit_autostart::{AutostartEntries, Context};
//! use xdg_kit_basedir::BaseDirs;
//! use xdg_kit_desktop_entry::LaunchOptions;
//!
//! let entries = AutostartEntries::load(&BaseDirs::from_env()?);
//! let context = Context::from_env();
//! for entry in entries.iter() {
//!     match entry.prepare(&context, &LaunchOptions::default()) {
//!         Ok(plan) => {
//!             plan.spawn()?;
//!         }
//!         Err(why) => eprintln!("{}: {why}", entry.name().display()),
//!     }
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # What the spec leaves open, decided
//!
//! - **Precedence** is by file name, `$XDG_CONFIG_HOME/autostart` first,
//!   then each `$XDG_CONFIG_DIRS/autostart`; only the most important
//!   file of a name is used, so its `Hidden=true` hides every other copy.
//!   Shadowed copies stay inspectable ([`Autostart::shadowed`]).
//! - **Broken overrides fail closed.** A regular file that cannot be read
//!   (permissions, the size limit, an I/O error) still claims its name:
//!   it is not started and hides less important copies, so a user's
//!   damaged override never lets the entry it hides start. A `Hidden`
//!   value that is not a boolean counts as hidden, and a file with a line
//!   that is not UTF-8 (which reading would drop) is not started. A directory that
//!   cannot be listed in full makes every entry of less important
//!   directories uncertain, and those are not started. Non-files
//!   (directories, dangling links) and hidden names claim nothing.
//! - **Eligibility** uses exactly the spec's keys — `Hidden`,
//!   `OnlyShowIn`, `NotShowIn`, `TryExec` — in the effective file;
//!   `NoDisplay` does not stop an entry. Every failing check is reported
//!   ([`Eligibility::reasons`]), apart from failures to build a command
//!   ([`PrepareError::Launch`]).
//! - **`TryExec`** follows this spec, which differs from the `GLib`
//!   lookup [`AppEntry::try_exec`](xdg_kit_desktop_entry::AppEntry::try_exec)
//!   reproduces for menus: see [`Autostart::eligibility`].
//! - **Extensions are not interpreted**: desktop-specific switches
//!   (`X-GNOME-Autostart-enabled`), delays and startup phases, and
//!   removable-media autorun are out of scope. Every unknown key is kept
//!   by overrides and readable through [`Autostart::entry`].
//!
//! # Overrides
//!
//! [`AutostartEntries::edit`] gives an [`Override`] of one entry in the
//! user's directory: [`Override::disable`], [`Override::enable`] (an
//! explicit `Hidden=false`) and [`Override::reset`] (removing the user's
//! file, back to the system default). Writes are atomic and checked
//! against the snapshot; system directories are never written.
//!
//! # Reference
//!
//! The tests compare discovery and eligibility with systemd's
//! `systemd-xdg-autostart-generator` and record each deliberate
//! difference with its reason.

#[cfg(not(unix))]
compile_error!("xdg-kit-autostart supports Unix systems only");

mod discovery;
mod edit;
mod eligibility;
mod error;
#[cfg(fuzzing)]
#[doc(hidden)]
pub mod fuzzing;
mod limits;
#[cfg(test)]
mod tests;
mod work;

pub use discovery::{Autostart, AutostartEntries, Origin, ReadProblem, SourceFile};
pub use edit::{EditError, Override, WriteError};
pub use eligibility::{Context, Eligibility, Ineligible, PrepareError, TryExecProblem};
pub use error::Diagnostic;
pub use limits::Limits;
pub use xdg_kit_basedir::Durability;
pub use xdg_kit_desktop_entry::Desktops;

// Snapshots are shared between threads; keep them that way.
const _: fn() = || {
    fn shareable<T: Send + Sync>() {}
    shareable::<AutostartEntries>();
    shareable::<Autostart>();
    shareable::<Context>();
    shareable::<Eligibility>();
    shareable::<Override>();
    shareable::<Limits>();
};
