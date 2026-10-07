//! The freedesktop.org Shared MIME-info database (spec version 0.22, whose
//! text still calls itself 0.21): file types from names, content and file
//! kinds, their relationships, icons and descriptions, and content types of
//! mounted volumes.
//!
//! [`MimeDatabase::load`] reads every database folder under the XDG data
//! directories into one immutable snapshot:
//!
//! ```no_run
//! use xdg_kit_basedir::BaseDirs;
//! use xdg_kit_locale::Locales;
//! use xdg_kit_mime::{MimeDatabase, PathOptions};
//!
//! let db = MimeDatabase::load(&BaseDirs::from_env()?);
//! let found = db.type_for_path("report.pdf", &PathOptions::default())?;
//! println!("{} ({})", found.mime, db.description(&found.mime, &Locales::from_env()).unwrap_or_default());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#[cfg(not(unix))]
compile_error!("xdg-kit-mime supports Unix systems only");

mod database;
mod detect;
mod error;
mod fnmatch;
#[cfg(fuzzing)]
#[doc(hidden)]
pub mod fuzzing;
mod merge;
mod mime_type;
mod model;
mod read;
mod tree;
mod xml;

pub use database::MimeDatabase;
pub use detect::{Certainty, DecidedBy, Detection, NameMatch, NameMatches, PathOptions};
pub use error::{Diagnostic, Location};
pub use mime_type::{InvalidMimeType, MimeType};
pub use tree::TreeOptions;
pub use xdg_kit_locale::Locales;
