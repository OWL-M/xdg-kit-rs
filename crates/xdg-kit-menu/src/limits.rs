//! Bounds on what loading reads and resolution does, so hostile or
//! damaged menu configurations cannot take the process down with them.

/// Bounds for loading and resolving a menu. The defaults are generous for
/// real configurations and small against hostile ones.
///
/// Every limit that guards the menu *configuration* — the menu files, their
/// merges and their elements, the `.desktop` scans that fill the pools —
/// fails the whole load when it is hit
/// ([`MenuError::Limit`](crate::MenuError::Limit)): a menu built from part
/// of its configuration could show what an omitted file, or an unread
/// higher-priority desktop entry, was there to hide.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// The most bytes read from one file (menu files, `.directory` and
    /// `.desktop` entries). `None` means any size.
    pub max_file_len: Option<u64>,
    /// The most menu files loaded for one menu, merged ones included, each
    /// time a file is merged counted.
    pub max_files: usize,
    /// The deepest chain of merges (a file merging a file merging …); at
    /// most [`Limits::MAX_DEPTH`].
    pub max_merge_depth: usize,
    /// The deepest nesting of XML elements in one file, and of `<Menu>`
    /// elements in the merged menu (`<Move>`s included); at most
    /// [`Limits::MAX_DEPTH`].
    pub max_depth: usize,
    /// The most XML elements over all files loaded.
    pub max_elements: usize,
    /// The most menus in the merged menu, before deleted ones are dropped.
    pub max_menus: usize,
    /// The most entries listed in one directory enumerated for menu files
    /// or legacy hierarchies, and the limits of each `<AppDir>` scan (as the
    /// Desktop Entry crate's [`Limits::max_listed`](xdg_kit_desktop_entry::Limits::max_listed)).
    pub max_listed: usize,
    /// The most work moving, matching and laying out, over the whole
    /// resolution: each item a `<Move>`'s path search passes (found or
    /// not), each menu its depth check visits, each item it moves or
    /// consolidates again; each rule evaluation (one rule node against one
    /// entry) and each entry a rule pass visits; each entry copied into the
    /// pool of a menu with its own `<AppDir>`s; and each layout node a menu
    /// is laid out by, an inherited `<DefaultLayout>`'s too, and each
    /// candidate a layout's `<Merge>` looks at — the work that
    /// grows with menus × entries × rules, menus × moves, or menus ×
    /// layout.
    pub max_matches: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_len: Some(xdg_kit_basedir::MAX_FILE_LEN),
            max_files: 1_000,
            max_merge_depth: 32,
            max_depth: 64,
            max_elements: 1_000_000,
            max_menus: 10_000,
            max_listed: 100_000,
            max_matches: 100_000_000,
        }
    }
}

impl Limits {
    /// The deepest nesting ever handled, whatever [`Limits::max_depth`] and
    /// [`Limits::max_merge_depth`] say (and in [`Limits::unlimited`]):
    /// reading, merging and resolving recurse over the nesting. At this
    /// depth the work fits a thread spawned with Rust's default stack size
    /// (2 MiB) with room to spare, unoptimized too (the tests check it in
    /// a child process: unoptimized, nesting overflowed that stack past 400
    /// levels); menus in use nest a handful of levels.
    pub const MAX_DEPTH: usize = 128;

    /// No bounds: time and memory grow with the input. Merge cycles are
    /// still detected, and nesting stops at [`Limits::MAX_DEPTH`].
    #[must_use]
    pub fn unlimited() -> Self {
        Self {
            max_file_len: None,
            max_files: usize::MAX,
            max_merge_depth: usize::MAX,
            max_depth: usize::MAX,
            max_elements: usize::MAX,
            max_menus: usize::MAX,
            max_listed: usize::MAX,
            max_matches: usize::MAX,
        }
    }

    /// The Desktop Entry crate's limits for scanning an `<AppDir>`.
    pub(crate) fn for_entries(&self) -> xdg_kit_desktop_entry::Limits {
        let mut limits = xdg_kit_desktop_entry::Limits::default();
        limits.max_file_len = self.max_file_len;
        limits.max_listed = Some(self.max_listed);
        limits.max_depth = Some(self.max_depth);
        limits
    }
}
