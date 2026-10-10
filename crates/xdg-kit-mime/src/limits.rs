//! Limits on reading a MIME database.

/// The deepest rule nesting [`Limits::max_rule_depth`] can allow, even with
/// [`Limits::unlimited`]; larger values count as this one. Rules are checked,
/// copied, matched and dropped recursively; this depth fits a 2 MiB thread
/// stack. On x86-64 with rustc 1.98.1, a 2 MiB stack held rules 1,493 deep in
/// a debug build and 4,656 deep in a release build.
pub const MAX_RULE_DEPTH: usize = 1024;

/// The default of [`Limits::max_diagnostics`].
pub(crate) const DEFAULT_DIAGNOSTICS: usize = 100;

/// Limits on reading a database's files. Functions without `_with` use
/// [`Limits::default`]; `None` means no limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// The largest file read, in bytes:
    /// [`xdg_kit_basedir::MAX_FILE_LEN`] by default. A larger file is
    /// reported as unreadable. `None`: any size, memory growing with the
    /// file.
    pub max_file_len: Option<u64>,
    /// The maximum diagnostics per file, 100 by default, followed by one line
    /// reporting that there are more. With `None`, every problem is listed.
    /// Memory use then grows roughly with the file's size, and can be many
    /// times larger because each diagnostic also stores the file's path.
    pub max_diagnostics: Option<usize>,
    /// The longest string read from a `mime.cache`, in bytes: 4,096 by default.
    /// Longer strings are unreadable; the search for a string's end stops at
    /// this length. With `None`, each search can reach the end of the file, so
    /// reading time can grow with the square of its size.
    pub max_cache_string: Option<usize>,
    /// The bytes a `mime.cache` reader may copy out, per byte of the file:
    /// 8 by default, plus [`Limits::cache_copy_extra`]. Strings are shared
    /// by many entries in a real cache; the budget stops a file that
    /// points many entries at one large region. `None`: no budget, so
    /// memory can grow with the square of the file's size.
    pub cache_copy_per_byte: Option<usize>,
    /// The bytes a `mime.cache` reader may copy out besides
    /// [`Limits::cache_copy_per_byte`]: 64 KiB by default.
    pub cache_copy_extra: usize,
    /// The deepest nesting of `magic` and `treemagic` tests, 64 by
    /// default (the top level is 0). A deeper test is skipped, with a
    /// diagnostic. Values above [`MAX_RULE_DEPTH`] count as that.
    pub max_rule_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_len: Some(xdg_kit_basedir::MAX_FILE_LEN),
            max_diagnostics: Some(DEFAULT_DIAGNOSTICS),
            max_cache_string: Some(4096),
            cache_copy_per_byte: Some(8),
            cache_copy_extra: 64 * 1024,
            max_rule_depth: 64,
        }
    }
}

impl Limits {
    /// Removes all limits except the rule depth ceiling, [`MAX_RULE_DEPTH`].
    /// [`Limits::cache_copy_extra`] keeps its default but is unused until
    /// [`Limits::cache_copy_per_byte`] is set. Memory use grows with the files
    /// read; both time and memory can grow with the square of a `mime.cache`
    /// file's size.
    #[must_use]
    pub fn unlimited() -> Self {
        Self {
            max_file_len: None,
            max_diagnostics: None,
            max_cache_string: None,
            cache_copy_per_byte: None,
            cache_copy_extra: Self::default().cache_copy_extra,
            max_rule_depth: MAX_RULE_DEPTH,
        }
    }

    /// The rule depth allowed: [`Limits::max_rule_depth`], at most
    /// [`MAX_RULE_DEPTH`].
    pub(crate) fn rule_depth(&self) -> usize {
        self.max_rule_depth.min(MAX_RULE_DEPTH)
    }
}
