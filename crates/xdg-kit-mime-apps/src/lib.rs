//! Implements the freedesktop.org MIME Applications Associations spec
//! (version 1.0.1). Finds applications and defaults for a MIME type using
//! every `mimeapps.list` and the installed desktop entries. Also edits
//! `mimeapps.list` without losing its contents or formatting, writes
//! `mimeinfo.cache`, and plans how to open a file or URI.
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use xdg_kit_basedir::BaseDirs;
//! use xdg_kit_desktop_entry::{AppIndex, Desktops, LaunchOptions, Target};
//! use xdg_kit_mime::MimeDatabase;
//! use xdg_kit_mime_apps::{MimeApps, open_plan};
//!
//! let base_dirs = BaseDirs::from_env()?;
//! let apps = MimeApps::load(
//!     &base_dirs,
//!     &Desktops::from_env(),
//!     Arc::new(AppIndex::load(&base_dirs)),
//!     Arc::new(MimeDatabase::load(&base_dirs)),
//! );
//! let plan = open_plan(&Target::Path("notes.txt".into()), &apps, &LaunchOptions::default())?;
//! for mut child in plan.spawn()? {
//!     child.wait()?;
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#[cfg(not(unix))]
compile_error!("xdg-kit-mime-apps supports Unix systems only");

mod apps;
mod cache;
mod error;
#[cfg(fuzzing)]
#[doc(hidden)]
pub mod fuzzing;
mod list;
mod open;
mod pattern;

pub use apps::{AppMatch, Matched, MimeApps};
pub use cache::{mimeinfo_cache, write_mimeinfo_cache};
pub use error::Diagnostic;
pub use list::{EditError, Group, MIMEAPPS_LIST, MimeAppsList};
pub use open::{OpenError, open_plan};
pub use pattern::{MediaType, TypePattern};
