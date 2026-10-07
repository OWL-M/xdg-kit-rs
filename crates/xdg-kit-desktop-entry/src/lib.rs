//! The freedesktop.org Desktop Entry spec (version 1.5, with the latest
//! text of xdg-specs). Reads and writes `.desktop` and `.directory` files
//! without losing their contents or formatting. Provides typed keys,
//! translations, actions, validation, and application lookup by desktop
//! file ID, plus support for launching applications.
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
mod registry;
mod validate;

pub use entry::{
    Action, DESKTOP_ACTION, DESKTOP_ENTRY, DesktopEntry, Desktops, EntryType, KDE_DESKTOP_ENTRY,
};
pub use error::Diagnostic;
pub use index::{AppEntry, AppIndex};
pub use key_file::{EditError, KeyFile, KeyNames};
pub use launch::{
    DBusCall, DBusMethod, Launch, LaunchError, LaunchOptions, LaunchPlan, PlannedCommand, Target,
    prepare_launch,
};
pub use validate::{Issue, Level, validate};
pub use xdg_kit_locale::Locales;
