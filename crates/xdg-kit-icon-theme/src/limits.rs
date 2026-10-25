//! Bounds on what is read, so hostile or damaged files cannot take the
//! process down with them.

/// Bounds for loading themes. The defaults are generous for real themes
/// and small against hostile ones.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// The most bytes read from one file (`index.theme` or a cache); a
    /// larger one is unreadable. `None` means any size, and memory use
    /// grows with the file.
    pub max_file_len: Option<u64>,
    /// The most icon files indexed across the loaded chain; the rest are
    /// skipped with a diagnostic.
    pub max_icons: usize,
    /// The most themes a chain may reach through `Inherits`.
    pub max_chain: usize,
    /// The most entries walked in one GTK cache; a longer one is refused
    /// (and that directory scanned instead).
    pub max_cache_entries: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_len: Some(xdg_kit_basedir::MAX_FILE_LEN),
            max_icons: 1_000_000,
            max_chain: 32,
            max_cache_entries: 1_000_000,
        }
    }
}

impl Limits {
    /// No bounds. Memory use grows with the input.
    #[must_use]
    pub fn unlimited() -> Self {
        Self {
            max_file_len: None,
            max_icons: usize::MAX,
            max_chain: usize::MAX,
            max_cache_entries: usize::MAX,
        }
    }
}
