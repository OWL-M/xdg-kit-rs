//! Bounds on the work that trash contents can cause. Trash directories are
//! written by every program a user runs and, on removable or shared
//! volumes, by anyone with the volume: their contents are input.

/// Bounds for every operation. The defaults are generous for real trash
/// directories and small against hostile ones. Reaching one is always
/// reported — as [`ListError::Limit`](crate::ListError::Limit),
/// [`EraseProblemKind::TooMany`](crate::EraseProblemKind::TooMany),
/// [`SizeProblemKind::TooMany`](crate::SizeProblemKind::TooMany) and the
/// like: no result that a limit cut short looks complete.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// The most bytes read from one trash info file. A larger one is
    /// unreadable metadata: the entry is listed, its origin unknown.
    pub max_info_len: u64,
    /// The most directory entries one listing visits in `files` and
    /// `info` together.
    pub max_listed: usize,
    /// The most storage names tried when inserting before giving up.
    pub max_name_attempts: u32,
    /// The longest name assumed for `files` and `info` when the system
    /// does not report one; `None` refuses to insert then. The actual
    /// limit is enforced by the file system either way: a name it refuses
    /// as too long is shortened and tried again.
    pub name_max_fallback: Option<usize>,
    /// The most bytes read from a `directorysizes` file; a larger one is
    /// ignored as a cache and replaced when the cache is written.
    pub max_cache_len: u64,
    /// The most `directorysizes` entries kept; later lines are not read.
    pub max_cache_entries: usize,
    /// The deepest a directory tree is descended when erasing or sizing an
    /// entry; each level holds an open directory.
    pub max_depth: usize,
    /// The most directory entries visited when erasing or sizing one
    /// trashed directory.
    pub max_tree_entries: u64,
    /// The most bytes read from `/proc/self/mountinfo`.
    pub max_mountinfo_len: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_info_len: 64 * 1024,
            max_listed: 1_000_000,
            max_name_attempts: 1_000,
            name_max_fallback: Some(255),
            max_cache_len: 16 * 1024 * 1024,
            max_cache_entries: 100_000,
            max_depth: 256,
            max_tree_entries: 100_000_000,
            max_mountinfo_len: 16 * 1024 * 1024,
        }
    }
}
