//! Freedesktop.org XDG specifications for Rust.
//!
//! Each specification lives in its own crate; this crate re-exports them,
//! one Cargo feature per crate, all on by default. `mime-crate-conversions`
//! adds conversions between `xdg_kit::mime::MimeType` and the `mime` crate's
//! `mime::Mime`.

#[cfg(feature = "basedir")]
pub use xdg_kit_basedir as basedir;
#[cfg(feature = "locale")]
pub use xdg_kit_locale as locale;
#[cfg(feature = "mime")]
pub use xdg_kit_mime as mime;
