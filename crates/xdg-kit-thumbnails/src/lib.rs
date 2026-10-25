//! The freedesktop.org Thumbnail Managing Standard: discovering
//! `.thumbnailer` files and building their commands, and the thumbnail
//! cache — keys, freshness, failure markers — including the shared
//! per-directory repositories.
//!
//! The crate never generates a thumbnail itself: it finds the right
//! thumbnailer, builds its command (no shell is ever involved), and the
//! caller spawns it. Cached thumbnails are served only while the
//! original stays readable, and their freshness is reported, never
//! assumed: a thumbnail without timestamps is `Unknown`, not fresh.
//!
//! ```no_run
//! use xdg_kit_basedir::BaseDirs;
//! use xdg_kit_mime::{MimeDatabase, MimeType};
//! use xdg_kit_thumbnails::{
//!     SourceInfo, ThumbnailCache, ThumbnailRequest, ThumbnailSize, ThumbnailSource, Thumbnailers,
//! };
//!
//! let base_dirs = BaseDirs::from_env()?;
//! let cache = ThumbnailCache::new(&base_dirs);
//! let source = ThumbnailSource::local("photos/me.png")?;
//! let info = SourceInfo::of_local("photos/me.png")?;
//! if cache.find(&source, ThumbnailSize::Normal, &info)?.is_none() {
//!     let database = MimeDatabase::load(&base_dirs);
//!     let thumbnailers = Thumbnailers::load(&base_dirs);
//!     let png = MimeType::new("image/png")?;
//!     if let Some(thumbnailer) = thumbnailers.for_type(&database, &png).first() {
//!         let output = std::env::temp_dir().join("made.png");
//!         let command = thumbnailer.command(&ThumbnailRequest {
//!             source: &source,
//!             output: &output,
//!             size: ThumbnailSize::Normal.max_dimension(),
//!         })?;
//!         // The caller spawns and supervises; the crate never does.
//!         if command.to_command().status()?.success() {
//!             cache.install(&output, &source, &info, ThumbnailSize::Normal)?;
//!         }
//!     }
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#[cfg(not(unix))]
compile_error!("xdg-kit-thumbnails supports Unix systems only");

mod cache;
mod command;
mod crc32;
mod error;
#[cfg(fuzzing)]
#[doc(hidden)]
pub mod fuzzing;
mod limits;
mod md5;
mod png;
mod source;
mod thumbnailer;
mod work;

pub use cache::{Candidate, Freshness, Provenance, Thumbnail, ThumbnailCache, ThumbnailSize};
pub use command::{ThumbnailRequest, ThumbnailerCommand};
pub use error::{CommandError, Diagnostic};
pub use limits::Limits;
pub use source::{Application, InvalidApplication, InvalidUri, SourceInfo, ThumbnailSource};
pub use thumbnailer::{Thumbnailer, Thumbnailers};
pub use xdg_kit_basedir::Durability;

// Snapshots are shared between threads; keep them that way.
const _: fn() = || {
    fn shareable<T: Send + Sync>() {}
    shareable::<Thumbnailers>();
    shareable::<ThumbnailCache>();
    shareable::<ThumbnailerCommand>();
    shareable::<Thumbnail>();
    shareable::<Limits>();
};
