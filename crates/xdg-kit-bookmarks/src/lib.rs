//! The freedesktop.org Desktop Bookmark spec: the XBEL file GTK and
//! others keep at `$XDG_DATA_HOME/recently-used.xbel`, read leniently and
//! edited losslessly.
//!
//! Reading tolerates whatever is in the file, with diagnostics. Editing
//! has a higher bar — the document must be well-formed XML end to end —
//! and preserves everything this crate does not understand, byte for
//! byte: other applications share the file. Writes are atomic and detect
//! stale baselines; they refuse with a conflict instead of overwriting
//! another writer's changes, where that is observable (the final check
//! and the rename are still two steps, so detection, not a guarantee —
//! [`BookmarkLock`] serializes writers that cooperate, and GTK does not).
//!
//! ```no_run
//! use xdg_kit_basedir::BaseDirs;
//! use xdg_kit_bookmarks::{BookmarkFile, Opened, WriteError};
//!
//! let dirs = BaseDirs::from_env()?;
//! let record = Opened {
//!     name: "Foo Viewer".into(),
//!     exec: "'fooview %u'".into(),
//!     mime: "text/plain".parse()?,
//!     groups: vec![],
//!     private: false,
//!     when: std::time::SystemTime::now(),
//! };
//! // Writes conflict instead of overwriting someone else's update:
//! // reload, reapply, retry.
//! loop {
//!     let mut recent = BookmarkFile::load_recently_used(&dirs)?;
//!     recent.open("file:///home/u/notes.txt", &record)?;
//!     match recent.write() {
//!         Ok(_durability) => break,
//!         Err(WriteError::Conflict) => continue,
//!         Err(err) => return Err(err.into()),
//!     }
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#[cfg(not(unix))]
compile_error!("xdg-kit-bookmarks supports Unix systems only");

mod bookmark;
mod document;
mod edit;
mod error;
#[cfg(fuzzing)]
#[doc(hidden)]
pub mod fuzzing;
mod limits;
mod lock;
mod serialize;
#[cfg(test)]
mod tests;
mod time;
mod work;
mod write;
mod xml;

pub use bookmark::{Application, Bookmark};
pub use document::BookmarkFile;
pub use edit::Opened;
pub use error::{Diagnostic, EditError, WriteError};
pub use limits::Limits;
pub use lock::BookmarkLock;
pub use xdg_kit_basedir::Durability;

// Snapshots are shared between threads; keep them that way.
const _: fn() = || {
    fn shareable<T: Send + Sync>() {}
    shareable::<BookmarkFile>();
    shareable::<BookmarkLock>();
    shareable::<Limits>();
    shareable::<Opened>();
};
