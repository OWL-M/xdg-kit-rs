//! Limits on reading and indexing desktop entries.

/// Limits on reading desktop entries and on the [`AppIndex`](crate::AppIndex)
/// walk. Functions without `_with` (or `_with_limits`) use [`Limits::default`];
/// `None` means no limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// The largest file read, in bytes:
    /// [`xdg_kit_basedir::MAX_FILE_LEN`] by default. A larger file cannot
    /// be read. `None`: any size, memory growing with the file.
    pub max_file_len: Option<u64>,
    /// The deepest folder nesting an [`AppIndex`](crate::AppIndex) walks below
    /// an `applications` folder, 32 by default. Deeper folders are skipped with
    /// a diagnostic. Setting `None` removes this limit at little cost: the walk
    /// uses an explicit stack and holds no folder open. Regardless of this
    /// setting, paths longer than the system allows (`PATH_MAX`) are skipped
    /// without a diagnostic.
    pub max_depth: Option<usize>,
    /// The maximum entries listed by an [`AppIndex`](crate::AppIndex) walk of
    /// one `applications` folder, 100,000 by default. Counts each name in each
    /// folder along every path. Past the limit, the walk stops with a
    /// diagnostic, bounding the work caused by folder links. With `None`, time
    /// and memory can grow exponentially with the nesting of folder links.
    /// Loops are still skipped.
    pub max_listed: Option<usize>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_len: Some(xdg_kit_basedir::MAX_FILE_LEN),
            max_depth: Some(32),
            max_listed: Some(100_000),
        }
    }
}

impl Limits {
    /// No limit at all: memory grows with the files read, and folder links
    /// can make an [`AppIndex`](crate::AppIndex) walk take exponential time
    /// and memory.
    #[must_use]
    pub fn unlimited() -> Self {
        Self {
            max_file_len: None,
            max_depth: None,
            max_listed: None,
        }
    }
}
