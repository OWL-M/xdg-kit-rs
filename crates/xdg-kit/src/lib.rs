//! Freedesktop.org XDG specifications for Rust.
//!
//! Each specification lives in its own crate; this crate re-exports them,
//! one Cargo feature per crate.

#[cfg(feature = "basedir")]
pub use xdg_kit_basedir as basedir;
