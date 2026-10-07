//! Merging folders into one database (spec "Directory layout" and "The
//! glob files").
//!
//! Folders come least important first, as the spec loads them; a later one
//! adds to or overrides what came before. Each rule keeps its folder's rank
//! (0 for the most important folder) and its place within the folder, so
//! ties are broken by importance, then by file order.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use crate::MimeType;
use crate::model::{Folder, Glob, MagicRule, TreeRule};

/// A rule with where it came from.
#[derive(Debug, Clone)]
pub(crate) struct Ranked<T> {
    pub(crate) rule: T,
    /// 0 for the most important folder.
    pub(crate) folder: usize,
    /// Place within its folder's file.
    pub(crate) order: usize,
}

/// Everything the folders say, merged.
#[derive(Debug, Default)]
pub(crate) struct Merged {
    pub(crate) globs: Vec<Ranked<Glob>>,
    pub(crate) magic: Vec<Ranked<MagicRule>>,
    pub(crate) tree: Vec<Ranked<TreeRule>>,
    pub(crate) aliases: HashMap<MimeType, MimeType>,
    pub(crate) parents: HashMap<MimeType, Vec<MimeType>>,
    pub(crate) icons: HashMap<MimeType, String>,
    pub(crate) generic_icons: HashMap<MimeType, String>,
    /// (namespace URI, local name) → type
    pub(crate) namespaces: HashMap<(String, String), MimeType>,
    pub(crate) max_extent: u32,
}

impl Merged {
    /// The type `mime` is an alias of, or `mime` itself.
    pub(crate) fn canonical(&self, mime: &MimeType) -> MimeType {
        let mut current = mime;
        let mut seen = HashSet::new();
        while let Some(target) = self.aliases.get(current) {
            if !seen.insert(current) {
                break;
            }
            current = target;
        }
        current.clone()
    }
}

/// Merges `folders`, least important first.
pub(crate) fn merge<'a>(folders: impl ExactSizeIterator<Item = &'a Folder>) -> Merged {
    let count = folders.len();
    let mut merged = Merged::default();
    for (index, folder) in folders.enumerate() {
        let rank = count - 1 - index;

        // glob-deleteall: this folder's word on these types replaces all
        // earlier folders'.
        merged
            .globs
            .retain(|glob| !folder.no_globs.contains(&glob.rule.mime));
        merged
            .globs
            .extend(folder.globs.iter().enumerate().map(|(order, glob)| Ranked {
                rule: glob.clone(),
                folder: rank,
                order,
            }));

        merged
            .magic
            .retain(|rule| !folder.no_magic.contains(&rule.rule.mime));
        merged
            .magic
            .extend(folder.magic.iter().enumerate().map(|(order, rule)| Ranked {
                rule: rule.clone(),
                folder: rank,
                order,
            }));

        merged
            .tree
            .extend(folder.tree.iter().enumerate().map(|(order, rule)| Ranked {
                rule: rule.clone(),
                folder: rank,
                order,
            }));

        for (alias, target) in &folder.aliases {
            merged.aliases.insert(alias.clone(), target.clone());
        }

        // Add parents from this folder to those from earlier folders,
        // placing this more important folder's parents first. Only
        // glob-deleteall and magic-deleteall remove earlier information
        // (spec "Directory layout").
        let mut parents: HashMap<&MimeType, Vec<MimeType>> = HashMap::new();
        for (mime, parent) in &folder.parents {
            let list = parents.entry(mime).or_default();
            if !list.contains(parent) {
                list.push(parent.clone());
            }
        }
        for (mime, mut list) in parents {
            for earlier in merged.parents.remove(mime).into_iter().flatten() {
                if !list.contains(&earlier) {
                    list.push(earlier);
                }
            }
            merged.parents.insert(mime.clone(), list);
        }

        for (mime, icon) in &folder.icons {
            merged.icons.insert(mime.clone(), icon.clone());
        }
        for (mime, icon) in &folder.generic_icons {
            merged.generic_icons.insert(mime.clone(), icon.clone());
        }
        for (namespace, local, mime) in &folder.namespaces {
            merged
                .namespaces
                .insert((namespace.clone(), local.clone()), mime.clone());
        }

        merged.max_extent = merged.max_extent.max(folder.max_extent);
    }

    // Most important first within each priority; the spec orders the same
    // pattern from several folders by folder.
    let rank = |folder: usize, order: usize| (folder, order);
    merged
        .globs
        .sort_by_key(|glob| rank(glob.folder, glob.order));
    merged.magic.sort_by(|a, b| {
        b.rule
            .priority
            .cmp(&a.rule.priority)
            .then(rank(a.folder, a.order).cmp(&rank(b.folder, b.order)))
    });
    merged.tree.sort_by(|a, b| {
        b.rule
            .priority
            .cmp(&a.rule.priority)
            .then(rank(a.folder, a.order).cmp(&rank(b.folder, b.order)))
    });
    drop_repeats(&mut merged.globs);
    drop_repeats(&mut merged.magic);
    drop_repeats(&mut merged.tree);
    merged
}

/// Drops rules that repeat an earlier one exactly, keeping the first (the
/// most important): the same package installed in several folders gives
/// the same patterns and magic again (xdgmime's `filter_out_dupes`).
fn drop_repeats<T: Eq + Hash>(rules: &mut Vec<Ranked<T>>) {
    let keep: Vec<bool> = {
        let mut seen = HashSet::new();
        rules
            .iter()
            .map(|ranked| seen.insert(&ranked.rule))
            .collect()
    };
    let mut keep = keep.into_iter();
    rules.retain(|_| keep.next().unwrap_or(true));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Matchlet;

    fn m(text: &str) -> MimeType {
        MimeType::new(text).unwrap()
    }

    fn glob(pattern: &str, mime: &str) -> Glob {
        Glob {
            weight: 50,
            pattern: pattern.into(),
            mime: m(mime),
            case_sensitive: false,
        }
    }

    fn magic(priority: u32, mime: &str, value: &[u8]) -> MagicRule {
        MagicRule {
            priority,
            mime: m(mime),
            matchlets: vec![Matchlet {
                start: 0,
                range: 1,
                word_size: 1,
                value: value.to_vec(),
                mask: None,
                children: Vec::new(),
            }],
        }
    }

    #[test]
    fn deletion_markers_discard_only_earlier_folders() {
        let system = Folder {
            globs: vec![
                glob("*.diff", "text/x-diff"),
                glob("*.patch", "text/x-diff"),
                glob("*.txt", "text/plain"),
            ],
            magic: vec![magic(50, "text/x-diff", b"diff")],
            ..Folder::default()
        };
        let user = Folder {
            globs: vec![glob("*.diff", "text/x-diff")],
            no_globs: vec![m("text/x-diff")],
            no_magic: vec![m("TEXT/X-DIFF")],
            ..Folder::default()
        };
        let merged = merge([&system, &user].into_iter());
        let patterns: Vec<_> = merged
            .globs
            .iter()
            .map(|g| (g.rule.pattern.as_str(), g.folder))
            .collect();
        assert_eq!(patterns, [("*.diff", 0), ("*.txt", 1)]);
        assert!(merged.magic.is_empty());
    }

    #[test]
    fn later_folders_override_by_key_and_rank_first() {
        let system = Folder {
            aliases: vec![(m("a/old"), m("a/one"))],
            parents: vec![
                (m("a/x"), m("a/p1")),
                (m("a/x"), m("a/p2")),
                (m("a/y"), m("a/p")),
            ],
            icons: vec![(m("a/x"), "sys".into())],
            magic: vec![magic(50, "a/x", b"1"), magic(80, "a/y", b"2")],
            max_extent: 10,
            ..Folder::default()
        };
        let user = Folder {
            aliases: vec![(m("a/old"), m("a/two"))],
            parents: vec![(m("a/x"), m("a/p3"))],
            icons: vec![(m("a/x"), "user".into())],
            magic: vec![magic(50, "a/z", b"3")],
            max_extent: 4,
            ..Folder::default()
        };
        let merged = merge([&system, &user].into_iter());
        assert_eq!(merged.aliases[&m("a/old")], m("a/two"));
        assert_eq!(
            merged.parents[&m("a/x")],
            [m("a/p3"), m("a/p1"), m("a/p2")],
            "added to, the more important folder's first"
        );
        assert_eq!(merged.parents[&m("a/y")], [m("a/p")]);
        assert_eq!(merged.icons[&m("a/x")], "user");
        let order: Vec<_> = merged
            .magic
            .iter()
            .map(|r| (r.rule.priority, r.rule.mime.as_str(), r.folder))
            .collect();
        // Highest priority first; equal priority: the more important folder.
        assert_eq!(order, [(80, "a/y", 1), (50, "a/z", 0), (50, "a/x", 1)]);
        assert_eq!(merged.max_extent, 10);
    }

    #[test]
    fn rules_repeated_across_folders_are_kept_once_from_the_most_important() {
        let mut heavier = glob("*.C", "text/x-c++src");
        heavier.weight = 60;
        let mut sensitive = glob("*.C", "text/x-c++src");
        sensitive.case_sensitive = true;
        let system = Folder {
            globs: vec![glob("*.C", "text/x-c++src"), heavier.clone()],
            magic: vec![magic(50, "a/x", b"1"), magic(50, "a/x", b"2")],
            ..Folder::default()
        };
        let local = system.clone();
        let user = Folder {
            globs: vec![glob("*.C", "TEXT/X-C++SRC"), sensitive.clone()],
            magic: vec![magic(50, "a/x", b"1")],
            ..Folder::default()
        };
        let merged = merge([&system, &local, &user].into_iter());
        let globs: Vec<_> = merged
            .globs
            .iter()
            .map(|g| (g.rule.weight, g.rule.case_sensitive, g.folder))
            .collect();
        assert_eq!(globs, [(50, false, 0), (50, true, 0), (60, false, 1)]);
        let magic: Vec<_> = merged
            .magic
            .iter()
            .map(|r| (r.rule.matchlets[0].value.clone(), r.folder))
            .collect();
        assert_eq!(magic, [(b"1".to_vec(), 0), (b"2".to_vec(), 1)]);
    }
}
