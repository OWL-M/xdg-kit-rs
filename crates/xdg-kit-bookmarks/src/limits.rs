//! Bounds on what is read, so hostile or damaged files cannot take the
//! process down with them.

/// Bounds for reading a bookmark file. The defaults are generous for real
/// files and small against hostile ones.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// The most bytes read from a file; a larger one is unreadable.
    /// `None` means any size, and memory use grows with the file.
    pub max_file_len: Option<u64>,
    /// The most `<bookmark>` elements read; the rest are skipped with a
    /// diagnostic, and the document is not editable.
    pub max_bookmarks: usize,
    /// The deepest element nesting followed; a deeper document is not
    /// well-formed for this crate's purposes.
    pub max_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_len: Some(xdg_kit_basedir::MAX_FILE_LEN),
            max_bookmarks: 100_000,
            max_depth: 64,
        }
    }
}

impl Limits {
    /// No bounds. Memory use grows with the file.
    #[must_use]
    pub fn unlimited() -> Self {
        Self {
            max_file_len: None,
            max_bookmarks: usize::MAX,
            max_depth: usize::MAX,
        }
    }
}
