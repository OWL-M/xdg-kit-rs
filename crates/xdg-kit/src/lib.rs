//! Freedesktop.org XDG specifications for Rust.
//!
//! Each specification lives in its own crate; this crate re-exports them,
//! one Cargo feature per crate, all on by default. A feature also turns on
//! the features whose types its API takes: `desktop-entry` and `mime` turn
//! on `basedir`, and `mime-apps` turns on `desktop-entry`, `mime` and
//! `basedir`. `mime-crate-conversions` adds conversions between
//! `xdg_kit::mime::MimeType` and the `mime` crate's `mime::Mime`;
//! `desktop-entry-dbus` adds D-Bus activation when launching.

#[cfg(feature = "basedir")]
pub use xdg_kit_basedir as basedir;
#[cfg(feature = "desktop-entry")]
pub use xdg_kit_desktop_entry as desktop_entry;
#[cfg(feature = "locale")]
pub use xdg_kit_locale as locale;
#[cfg(feature = "mime")]
pub use xdg_kit_mime as mime;
#[cfg(feature = "mime-apps")]
pub use xdg_kit_mime_apps as mime_apps;
