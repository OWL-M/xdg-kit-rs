//! Freedesktop.org XDG specifications for Rust.
//!
//! Each specification has its own crate, re-exported here through a Cargo
//! feature enabled by default. Features also enable the features that
//! provide types used by their APIs: `desktop-entry` and `mime` enable
//! `basedir`; `mime-apps` enables `desktop-entry`, `mime` and `basedir`;
//! `bookmarks` enables `mime` and `basedir`; `thumbnails` enables
//! `desktop-entry`, `mime` and `basedir`; `icon-theme` enables
//! `desktop-entry`, `locale` and `basedir`; `autostart` enables
//! `desktop-entry` and `basedir`; `menu` enables `desktop-entry`, `locale`
//! and `basedir`; `trash` enables `basedir`.
//! The exception is `locale`: `desktop-entry` and `mime` use only `Locales`
//! from that crate and re-export it.
//!
//! `desktop-entry-dbus` adds D-Bus activation when launching, autostart
//! plans included.

#[cfg(feature = "autostart")]
pub use xdg_kit_autostart as autostart;
#[cfg(feature = "basedir")]
pub use xdg_kit_basedir as basedir;
#[cfg(feature = "bookmarks")]
pub use xdg_kit_bookmarks as bookmarks;
#[cfg(feature = "desktop-entry")]
pub use xdg_kit_desktop_entry as desktop_entry;
#[cfg(feature = "icon-theme")]
pub use xdg_kit_icon_theme as icon_theme;
#[cfg(feature = "locale")]
pub use xdg_kit_locale as locale;
#[cfg(feature = "menu")]
pub use xdg_kit_menu as menu;
#[cfg(feature = "mime")]
pub use xdg_kit_mime as mime;
#[cfg(feature = "mime-apps")]
pub use xdg_kit_mime_apps as mime_apps;
#[cfg(feature = "thumbnails")]
pub use xdg_kit_thumbnails as thumbnails;
#[cfg(feature = "trash")]
pub use xdg_kit_trash as trash;

/// The examples of the workspace's README, run as doctests.
#[cfg(all(
    doctest,
    feature = "autostart",
    feature = "basedir",
    feature = "bookmarks",
    feature = "desktop-entry",
    feature = "icon-theme",
    feature = "locale",
    feature = "menu",
    feature = "mime",
    feature = "mime-apps",
    feature = "thumbnails",
    feature = "trash"
))]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;
