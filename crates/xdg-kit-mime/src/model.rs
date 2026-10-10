//! What one database folder holds, whichever files it was read from.

use crate::MimeType;

/// The deletion marker for globs (`glob-deleteall`), written as a pattern.
pub(crate) const NO_GLOBS: &str = "__NOGLOBS__";
/// The deletion marker for magic (`magic-deleteall`), written as a value.
pub(crate) const NO_MAGIC: &[u8] = b"__NOMAGIC__";

/// A file name pattern.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Glob {
    pub(crate) weight: u32,
    pub(crate) pattern: String,
    pub(crate) mime: MimeType,
    pub(crate) case_sensitive: bool,
}

/// One magic test, with the tests that must also hold when it matches
/// (any one of them).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Matchlet {
    pub(crate) start: u32,
    pub(crate) range: u32,
    pub(crate) word_size: u32,
    pub(crate) value: Vec<u8>,
    pub(crate) mask: Option<Vec<u8>>,
    pub(crate) children: Vec<Matchlet>,
}

/// The magic of one `<magic>` element: any of its matchlets may match.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct MagicRule {
    pub(crate) priority: u32,
    pub(crate) mime: MimeType,
    pub(crate) matchlets: Vec<Matchlet>,
}

/// The kind a tree magic path must have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum TreeKind {
    Any,
    File,
    Directory,
    Link,
}

/// One tree magic test, with nested tests that must also hold (any one).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct TreeMatchlet {
    pub(crate) path: String,
    pub(crate) kind: TreeKind,
    pub(crate) match_case: bool,
    pub(crate) executable: bool,
    pub(crate) non_empty: bool,
    pub(crate) mimetype: Option<MimeType>,
    pub(crate) children: Vec<TreeMatchlet>,
}

/// The tree magic of one `<treemagic>` element.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct TreeRule {
    pub(crate) priority: u32,
    pub(crate) mime: MimeType,
    pub(crate) matchlets: Vec<TreeMatchlet>,
}

/// Everything one `<MIME>` folder says, in file order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Folder {
    pub(crate) globs: Vec<Glob>,
    /// Types whose globs from earlier folders are discarded.
    pub(crate) no_globs: Vec<MimeType>,
    pub(crate) magic: Vec<MagicRule>,
    /// Types whose magic from earlier folders is discarded.
    pub(crate) no_magic: Vec<MimeType>,
    /// (alias, type)
    pub(crate) aliases: Vec<(MimeType, MimeType)>,
    /// (type, parent)
    pub(crate) parents: Vec<(MimeType, MimeType)>,
    pub(crate) icons: Vec<(MimeType, String)>,
    pub(crate) generic_icons: Vec<(MimeType, String)>,
    /// (namespace URI, local name, type)
    pub(crate) namespaces: Vec<(String, String, MimeType)>,
    pub(crate) tree: Vec<TreeRule>,
    /// The `types` file, when there is one.
    pub(crate) types: Option<Vec<MimeType>>,
    /// The bytes magic can look at: the largest `start + range + value
    /// length` over every matchlet, nested ones included.
    pub(crate) max_extent: u32,
}

impl Matchlet {
    fn extent(&self) -> u32 {
        let own = self
            .start
            .saturating_add(self.range)
            .saturating_add(u32::try_from(self.value.len()).unwrap_or(u32::MAX));
        self.children
            .iter()
            .map(Matchlet::extent)
            .fold(own, u32::max)
    }
}

impl Folder {
    /// The largest extent over all magic, as `update-mime-database` writes
    /// it into `mime.cache`.
    pub(crate) fn magic_extent(&self) -> u32 {
        self.magic
            .iter()
            .flat_map(|rule| &rule.matchlets)
            .map(Matchlet::extent)
            .max()
            .unwrap_or(0)
    }

    /// The parts `mime.cache` holds, in a fixed order, so a folder read from
    /// the cache and the same folder read from the text files compare equal.
    /// Repeats are dropped: `globs2` can list a line twice (the reference
    /// database does, for `*.par2`, `*.pl` and `*.mc2`), the cache once.
    #[cfg(test)]
    pub(crate) fn comparable(&self) -> Self {
        fn sorted<T: Ord + Clone>(items: &[T]) -> Vec<T> {
            let mut items = items.to_vec();
            items.sort();
            items.dedup();
            items
        }
        fn sorted_rule(rule: &MagicRule) -> MagicRule {
            fn sort(matchlets: &[Matchlet]) -> Vec<Matchlet> {
                let mut matchlets: Vec<Matchlet> = matchlets
                    .iter()
                    .map(|matchlet| Matchlet {
                        children: sort(&matchlet.children),
                        ..matchlet.clone()
                    })
                    .collect();
                matchlets.sort();
                matchlets
            }
            MagicRule {
                matchlets: sort(&rule.matchlets),
                ..rule.clone()
            }
        }
        let magic: Vec<MagicRule> = self.magic.iter().map(sorted_rule).collect();
        Self {
            globs: sorted(&self.globs),
            no_globs: sorted(&self.no_globs),
            magic: sorted(&magic),
            no_magic: sorted(&self.no_magic),
            aliases: sorted(&self.aliases),
            parents: sorted(&self.parents),
            icons: sorted(&self.icons),
            generic_icons: sorted(&self.generic_icons),
            namespaces: sorted(&self.namespaces),
            tree: Vec::new(),
            types: None,
            max_extent: self.max_extent,
        }
    }
}
