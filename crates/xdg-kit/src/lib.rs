//! Freedesktop.org XDG specifications for Rust.
//!
//! Each specification has its own crate, re-exported here through a Cargo
//! feature enabled by default. Features also enable the features that
//! provide types used by their APIs: `desktop-entry` and `mime` enable
//! `basedir`; `mime-apps` enables `desktop-entry`, `mime` and `basedir`.
//! The exception is `locale`: `desktop-entry` and `mime` use only `Locales`
//! from that crate and re-export it.
//!
//! `mime-crate-conversions` adds conversions between `xdg_kit::mime::MimeType`
//! and the `mime` crate's `mime::Mime`. `desktop-entry-dbus` adds D-Bus
//! activation when launching.

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

/// The examples of the workspace's README, run as doctests.
#[cfg(all(
    doctest,
    feature = "basedir",
    feature = "desktop-entry",
    feature = "locale",
    feature = "mime",
    feature = "mime-apps"
))]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;
