//! Implements the freedesktop.org MIME Applications Associations spec
//! (version 1.0.1). Finds applications and defaults for a MIME type using
//! every `mimeapps.list` and the installed desktop entries. Also edits
//! `mimeapps.list` without losing its contents or formatting, writes
//! `mimeinfo.cache`, and plans how to open a file or URI.
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use xdg_kit_basedir::BaseDirs;
//! use xdg_kit_desktop_entry::{AppIndex, Desktops, LaunchOptions, Target};
//! use xdg_kit_mime::MimeDatabase;
//! use xdg_kit_mime_apps::{MimeApps, open_plan};
//!
//! let base_dirs = BaseDirs::from_env()?;
//! let apps = MimeApps::load(
//!     &base_dirs,
//!     &Desktops::from_env(),
//!     Arc::new(AppIndex::load(&base_dirs)),
//!     Arc::new(MimeDatabase::load(&base_dirs)),
//! );
//! let plan = open_plan(&Target::Path("notes.txt".into()), &apps, &LaunchOptions::default())?;
//! for mut child in plan.spawn()? {
//!     child.wait()?;
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#[cfg(not(unix))]
compile_error!("xdg-kit-mime-apps supports Unix systems only");

mod apps;
mod cache;
mod error;
#[cfg(fuzzing)]
#[doc(hidden)]
pub mod fuzzing;
mod list;
mod open;
mod pattern;

pub use apps::{AppMatch, Matched, MimeApps};
pub use cache::{mimeinfo_cache, write_mimeinfo_cache};
pub use error::Diagnostic;
pub use list::{EditError, Group, MIMEAPPS_LIST, MimeAppsList};
pub use open::{OpenError, open_plan};
pub use pattern::{MediaType, TypePattern};

/// Steps of work, counted in tests so that they can show the work grows
/// linearly with the input. A step is an item visited (a key, an ID, an
/// entry, a lookup) or a character looked at. Outside tests counting does
/// nothing. Bounds in the tests are stated in items and bytes of input.
///
/// The counts can only show what they are told: on a path a counted test
/// runs, every step that grows with the input counts, each item a loop
/// visits included (each key looked at, each ID checked for repeats or
/// compared, each lookup), and each character a scan of a key or ID looks
/// at, whether the loop is written here or is a library call such as
/// `contains` or `trim` (counted by the bytes it may walk). So does each
/// character of input text copied (`to_owned`, `clone`, `format!`, a
/// media type made from text), hashed (a set or map key inserted or looked
/// up: hashed and compared, two steps per byte) or compared with other
/// input, and each byte of a message that input is written into. Comparing
/// with a fixed name (a literal) is bounded by that name and not counted;
/// nor is file I/O. A new loop over input on such a path must count its
/// steps too.
///
/// There is one counter, the key file's: in tests this crate counts on the
/// counter of `xdg-kit-desktop-entry`, which its dev-dependency compiles in
/// with the hidden `__count-work` feature (not public API). So the steps
/// counted here include those inside the key file (reading, listing keys,
/// splitting values, finding lines, editing, copying), which that crate
/// counts by the same rules (decision 73), and those of
/// [`AppIndex`](xdg_kit_desktop_entry::AppIndex) listing its folders and
/// giving out entries (`dirs`, `get`, `iter`), each entry with its ID and
/// path copied, and their lookups by ID (`get`, `dir_of`; decisions 140
/// and 144); this crate does not count them again.
///
/// This crate counts only its own work (decision 140). `xdg-kit-mime`
/// counts its internals in its own tests: creating and checking types,
/// looking up aliases, finding ancestors and finding a type's media part.
/// Each call into that crate counts as one step here. Processing its
/// results, such as comparing or copying each ancestor, is counted here
/// too. A [`MimeType`](xdg_kit_mime::MimeType) is an `Arc`, so copying one
/// counts as one step regardless of its length.
pub(crate) mod work {
    /// Counts `steps` steps on this thread, on the key file's counter.
    #[inline]
    pub(crate) fn count(steps: usize) {
        #[cfg(test)]
        xdg_kit_desktop_entry::__count_work::count(steps);
        #[cfg(not(test))]
        let _ = steps;
    }

    /// Counts the steps `steps` works out, calling it only in tests, so
    /// that working them out costs nothing elsewhere.
    #[inline]
    pub(crate) fn count_with(steps: impl FnOnce() -> usize) {
        #[cfg(test)]
        count(steps());
        #[cfg(not(test))]
        let _ = steps;
    }

    /// What `f` returns, and the steps it counted on this thread, here and
    /// in the key file.
    #[cfg(test)]
    pub(crate) fn steps<T>(f: impl FnOnce() -> T) -> (T, usize) {
        let before = xdg_kit_desktop_entry::__count_work::total();
        let value = f();
        (value, xdg_kit_desktop_entry::__count_work::total() - before)
    }

    /// Checks the steps counted at a size and at twice that size, each
    /// given with its bound (in items and bytes, about twice the analysed
    /// cost): first that twice the size takes at most 2.5 times the steps
    /// (linear work gives about 2, quadratic about 4), then that each is
    /// within its bound.
    #[cfg(test)]
    #[track_caller]
    pub(crate) fn assert_linear(what: &str, size: (usize, usize), twice: (usize, usize)) {
        let ((steps, bound), (steps_2n, bound_2n)) = (size, twice);
        assert!(
            2 * steps_2n <= 5 * steps,
            "{what}: {steps_2n} steps at twice the size, {}% of the {steps} at the size (bounds {bound_2n}, {bound})",
            100 * steps_2n / steps.max(1)
        );
        assert!(steps <= bound, "{what}: {steps} steps, bound {bound}");
        assert!(
            steps_2n <= bound_2n,
            "{what}: {steps_2n} steps at twice the size, bound {bound_2n}"
        );
    }
}
