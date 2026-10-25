//! The freedesktop.org Desktop Entry spec (version 1.5, with the latest
//! text of xdg-specs). Reads and writes `.desktop` and `.directory` files
//! without losing their contents or formatting. Provides typed keys,
//! translations, actions, validation, and lookup of installed entries by
//! desktop file ID, plus support for launching applications.
//!
//! ```no_run
//! use xdg_kit_basedir::BaseDirs;
//! use xdg_kit_desktop_entry::{AppIndex, LaunchOptions, Target, prepare_launch};
//!
//! let apps = AppIndex::load(&BaseDirs::from_env()?);
//! if let Some(app) = apps.get("org.example.FooViewer.desktop") {
//!     let plan = prepare_launch(app.entry(), &[Target::Path("notes.txt".into())], &LaunchOptions::default())?;
//!     for mut child in plan.spawn()? {
//!         child.wait()?;
//!     }
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#[cfg(not(unix))]
compile_error!("xdg-kit-desktop-entry supports Unix systems only");

mod entry;
mod error;
pub mod exec;
#[cfg(fuzzing)]
#[doc(hidden)]
pub mod fuzzing;
mod index;
mod key_file;
mod keys;
mod launch;
mod limits;
mod registry;
mod validate;

pub use entry::{
    Action, DESKTOP_ACTION, DESKTOP_ENTRY, DesktopEntry, Desktops, EntryType, KDE_DESKTOP_ENTRY,
};
pub use error::Diagnostic;
pub use index::{AppEntry, AppIndex};
pub use key_file::{EditError, KeyFile, KeyNames};
pub use launch::{
    DBusCall, DBusError, DBusMethod, Launch, LaunchError, LaunchOptions, LaunchPlan,
    PlannedCommand, Target, prepare_launch,
};
pub use limits::Limits;
pub use validate::{Issue, Level, validate};
pub use xdg_kit_basedir::Durability;
pub use xdg_kit_locale::Locales;

// Snapshots and parsed entries are shared between threads; keep them
// that way.
const _: fn() = || {
    fn shareable<T: Send + Sync>() {}
    shareable::<AppIndex>();
    shareable::<AppEntry>();
    shareable::<DesktopEntry>();
    shareable::<KeyFile>();
    shareable::<LaunchPlan>();
    shareable::<Limits>();
};

/// Not public API: the work counter of this crate's counted tests, compiled
/// in by the hidden `__count-work` feature for the workspace's own tests
/// (`xdg-kit-mime-apps` counts on the same counter, so that its counted
/// tests include the work done here). It may change or go at any time.
#[cfg(feature = "__count-work")]
#[doc(hidden)]
pub mod __count_work {
    /// Counts `steps` steps on this thread's counter.
    pub fn count(steps: usize) {
        crate::key_file::work::count(steps);
    }

    /// The steps counted on this thread so far, here and by [`count`].
    #[must_use]
    pub fn total() -> usize {
        crate::key_file::work::total()
    }
}
