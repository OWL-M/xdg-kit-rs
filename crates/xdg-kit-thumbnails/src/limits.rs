//! Bounds on what is read, so hostile or damaged files cannot take the
//! process down with them.

/// Bounds for reading thumbnailer files and thumbnails. The defaults are
/// generous for real files and small against hostile ones.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// The most bytes read from one file; a larger one is unreadable.
    /// `None` means any size, and memory use grows with the file.
    pub max_file_len: Option<u64>,
    /// The most `.thumbnailer` files read; the rest are skipped with a
    /// diagnostic.
    pub max_thumbnailers: usize,
    /// The most chunks walked in one PNG; a longer one is unsound.
    pub max_chunks: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_len: Some(xdg_kit_basedir::MAX_FILE_LEN),
            max_thumbnailers: 10_000,
            max_chunks: 10_000,
        }
    }
}

impl Limits {
    /// No bounds. Memory use grows with the input.
    #[must_use]
    pub fn unlimited() -> Self {
        Self {
            max_file_len: None,
            max_thumbnailers: usize::MAX,
            max_chunks: usize::MAX,
        }
    }
}
