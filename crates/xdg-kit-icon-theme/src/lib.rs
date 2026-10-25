//! The freedesktop.org Icon Theme spec: `index.theme` inheritance and
//! the spec's exact lookup algorithm, served from an immutable in-memory
//! index — built from validated GTK `icon-theme.cache` files where they
//! are sound and fresh, and from directory scans otherwise — plus the
//! Icon Naming spec's fallback chains as explicit conveniences.
//!
//! Lookups read the snapshot and do no filesystem I/O; a returned path
//! is a candidate from that snapshot, not a guarantee the file still
//! exists — the caller opens it and handles that race, which no
//! stat-walk could eliminate either. `refresh` alone checks the
//! filesystem and returns a replacement snapshot; the Icon Theme spec's
//! implementation note (recheck theme directories at most every five
//! seconds) is met by calling it at the application's own cadence —
//! nothing here polls behind the caller's back.
//!
//! # Cost
//!
//! Loading is linear in the icon files of the requested theme's
//! inheritance chain: one enumeration pass per theme directory per
//! search root, through a validated `icon-theme.cache` where one is
//! sound and fresh, by `readdir` otherwise. A lookup touches only the
//! name's candidate records. Indicatively (one 2026 dev machine, warm
//! page cache, release build, the Adwaita + hicolor chain): a cold load
//! took about 18 ms through caches and 21 ms scanning; lookups averaged
//! about 130 ns, hits and misses alike. Measure your own chain; these
//! are not promises.
//!
//! ```no_run
//! use xdg_kit_basedir::BaseDirs;
//! use xdg_kit_icon_theme::{Formats, IconRequest, IconThemes};
//!
//! let base_dirs = BaseDirs::from_env()?;
//! let themes = IconThemes::load(&base_dirs, "Adwaita");
//! let request = IconRequest {
//!     size: 16,
//!     scale: 1,
//!     formats: Formats::default(),
//! };
//! if let Some(icon) = themes.lookup_with_fallbacks("network-wired-disconnected", &request) {
//!     println!("{} from {:?}", icon.path().display(), icon.theme());
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#[cfg(not(unix))]
compile_error!("xdg-kit-icon-theme supports Unix systems only");

mod cache;
mod error;
mod index;
mod limits;
mod lookup;
mod size;
#[cfg(test)]
mod tests;
mod theme;
mod work;

pub use error::Diagnostic;
pub use index::{IconThemes, LoadOptions};
pub use limits::Limits;
pub use lookup::{Formats, Icon, IconFormat, IconRequest};
pub use size::{RuleKind, SizingRule};
pub use theme::{ThemeListing, themes};
pub use xdg_kit_locale::Locales;

// Snapshots are shared between threads; keep them that way.
const _: fn() = || {
    fn shareable<T: Send + Sync>() {}
    shareable::<IconThemes>();
    shareable::<Icon>();
    shareable::<ThemeListing>();
    shareable::<Limits>();
};
