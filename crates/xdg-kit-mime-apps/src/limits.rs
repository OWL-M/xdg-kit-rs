//! Limits on reading application associations.

/// Limits on reading `mimeapps.list` files and desktop entries, and on the
/// `mimeinfo.cache` walk. Functions without `_with` use [`Limits::default`];
/// `None` means no limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// The largest file read, in bytes:
    /// [`xdg_kit_basedir::MAX_FILE_LEN`] by default. A larger
    /// `mimeapps.list` cannot be read; a larger desktop entry is skipped
    /// by the `mimeinfo.cache` walk and not followed as a link by
    /// [`open_plan`](crate::open_plan). `None`: any size, memory growing
    /// with the file.
    pub max_file_len: Option<u64>,
    /// The maximum entries listed by one `mimeinfo.cache` walk, 100,000 by
    /// default. Counts each name in each folder along every path. Exceeding the
    /// limit fails the walk, bounding the work caused by folder links. With
    /// `None`, time and memory can grow exponentially with the nesting of
    /// folder links. Loops are still skipped.
    pub max_listed: Option<usize>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_len: Some(xdg_kit_basedir::MAX_FILE_LEN),
            max_listed: Some(100_000),
        }
    }
}

impl Limits {
    /// No limit at all: memory grows with the files read, and folder
    /// links can make the `mimeinfo.cache` walk take exponential time and
    /// memory.
    #[must_use]
    pub fn unlimited() -> Self {
        Self {
            max_file_len: None,
            max_listed: None,
        }
    }

    /// The desktop entry crate's limits with this file size limit.
    pub(crate) fn for_entries(&self) -> xdg_kit_desktop_entry::Limits {
        let mut limits = xdg_kit_desktop_entry::Limits::default();
        limits.max_file_len = self.max_file_len;
        limits
    }
}
