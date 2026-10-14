//! The XDG Base Directory spec (version 0.8) and the XDG user folders.
//!
//! ```
//! use xdg_kit_basedir::BaseDirs;
//!
//! let vars = |name: &str| match name {
//!     "HOME" => Some("/home/u".into()),
//!     "XDG_DATA_DIRS" => Some("/opt/share:/usr/share".into()),
//!     _ => None,
//! };
//! let dirs = BaseDirs::from_vars(vars)?;
//! assert_eq!(dirs.config_home(), std::path::Path::new("/home/u/.config"));
//! assert_eq!(dirs.data_dirs().len(), 2);
//! # Ok::<(), xdg_kit_basedir::Error>(())
//! ```
//!
//! [`BaseDirs::find_config`] and [`BaseDirs::find_data`] give the most
//! important file that exists; [`BaseDirs::read_config`] and
//! [`BaseDirs::read_data`] (and their `_with` forms, taking [`Limits`]) read
//! the most important one that can be read, skipping missing files and
//! files that may not be read, as the spec says.

#[cfg(not(unix))]
compile_error!("xdg-kit-basedir supports Unix systems only");

// Not public API (see the module's docs).
#[doc(hidden)]
#[path = "hashed.rs"]
pub mod __hashed;
mod base_dirs;
mod error;
mod paths;
mod read;
mod runtime;
mod stamp;
// FIFOs are made with `mknodat`, which rustix does not have on Apple systems.
#[cfg(all(test, not(target_vendor = "apple")))]
mod testing;
pub mod user_dirs;
mod work;

pub use base_dirs::{BaseDirs, ReadFile};
pub use error::{Diagnostic, DiagnosticSource, Error};
pub use read::{Limits, MAX_FILE_LEN, read_regular_file, read_regular_file_with};
pub use runtime::RuntimeDirProblem;
pub use stamp::FileStamp;
pub use user_dirs::{UserDirKind, UserDirs};

// Values are shared between threads; keep them that way.
const _: fn() = || {
    fn shareable<T: Send + Sync>() {}
    shareable::<BaseDirs>();
    shareable::<FileStamp>();
    shareable::<Limits>();
    shareable::<UserDirs>();
};
