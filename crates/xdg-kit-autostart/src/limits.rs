//! Bounds on what discovery reads, so hostile or damaged directories
//! cannot take the process down with them.

/// Bounds for discovering autostart entries. The defaults are generous
/// for real configurations and small against hostile ones.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// The most bytes read from one file; a larger file is unreadable,
    /// which keeps its name claimed (see
    /// [`Ineligible::Unreadable`](crate::Ineligible::Unreadable)). `None`
    /// means any size, and memory use grows with the file.
    pub max_file_len: Option<u64>,
    /// The most directory entries visited in one directory, `.desktop`
    /// names or not; a directory with more cannot be listed in full, so it
    /// contributes no entries and the entries of every less important
    /// directory are uncertain
    /// ([`Ineligible::OverridesUnknown`](crate::Ineligible::OverridesUnknown)).
    pub max_listed: usize,
    /// The most candidate files considered over all directories, shadowed
    /// copies and unusable files included — each retains a path and a
    /// stamp. Once more names than this are collected, less important
    /// directories are not listed at all. Discovery stops there, with one
    /// diagnostic, in directory
    /// precedence and then name order: what is cut is never started, and
    /// nothing it would have shadowed is read, so the cut cannot let a
    /// shadowed entry through. An entry kept may then miss shadowed copies
    /// past the cut ([`Autostart::shadowed`](crate::Autostart::shadowed),
    /// so also a system default an override could be reset to).
    pub max_entries: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_len: Some(xdg_kit_basedir::MAX_FILE_LEN),
            max_listed: 100_000,
            max_entries: 10_000,
        }
    }
}

impl Limits {
    /// No bounds. Memory use grows with the input.
    #[must_use]
    pub fn unlimited() -> Self {
        Self {
            max_file_len: None,
            max_listed: usize::MAX,
            max_entries: usize::MAX,
        }
    }
}
