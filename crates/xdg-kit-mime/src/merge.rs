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
use crate::model::{Folder, Glob, MagicRule, Matchlet, TreeMatchlet, TreeRule};
use crate::work;

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
    /// Each alias and the type it resolves to (see [`Merged::canonical`]).
    pub(crate) aliases: HashMap<MimeType, MimeType>,
    /// Each type aliases resolve to, other than themselves, and those
    /// aliases, sorted.
    pub(crate) aliases_of: HashMap<MimeType, Vec<MimeType>>,
    /// Each type's explicit parents, each canonical and listed once.
    pub(crate) parents: HashMap<MimeType, Vec<MimeType>>,
    pub(crate) icons: HashMap<MimeType, String>,
    pub(crate) generic_icons: HashMap<MimeType, String>,
    /// (namespace URI, local name) → type
    pub(crate) namespaces: HashMap<(String, String), MimeType>,
    pub(crate) max_extent: u32,
}

impl Merged {
    /// The type `mime` aliases, or `mime` itself. Follows alias chains
    /// until a type repeats. A type in a loop therefore resolves to itself;
    /// a type leading into a loop resolves to the first loop member it
    /// reaches.
    pub(crate) fn canonical(&self, mime: &MimeType) -> MimeType {
        // The lookup: the type hashed and compared.
        work::count(1 + 2 * mime.as_str().len());
        self.aliases.get(mime).unwrap_or(mime).clone()
    }
}

/// Merges `folders`, least important first.
pub(crate) fn merge<'a>(folders: impl ExactSizeIterator<Item = &'a Folder>) -> Merged {
    let folders: Vec<&Folder> = folders.inspect(|_| work::count(1)).collect();
    let count = folders.len();
    let mut merged = Merged::default();
    let mut aliases = HashMap::new();
    // glob-deleteall and magic-deleteall: a folder's word on these types
    // replaces all earlier folders'. Each type whose globs (or magic) a
    // folder deletes, with the rank of the last, most important, such
    // folder.
    let mut no_globs: HashMap<&MimeType, usize> = HashMap::new();
    let mut no_magic: HashMap<&MimeType, usize> = HashMap::new();
    for (index, folder) in folders.iter().enumerate() {
        let rank = count - 1 - index;

        for (marks, types) in [
            (&mut no_globs, &folder.no_globs),
            (&mut no_magic, &folder.no_magic),
        ] {
            for mime in types {
                // The type hashed and compared.
                work::count(1 + 2 * mime.as_str().len());
                marks.insert(mime, rank);
            }
        }

        merged.globs.extend(ranked(&folder.globs, rank));
        merged.magic.extend(ranked(&folder.magic, rank));
        merged.tree.extend(ranked(&folder.tree, rank));

        for (alias, target) in &folder.aliases {
            // The alias hashed and compared.
            work::count(1 + 2 * alias.as_str().len());
            aliases.insert(alias.clone(), target.clone());
        }
        for (mime, icon) in &folder.icons {
            // The type hashed and compared, and each byte of the icon copied.
            work::count(1 + 2 * mime.as_str().len() + icon.len());
            merged.icons.insert(mime.clone(), icon.clone());
        }
        for (mime, icon) in &folder.generic_icons {
            work::count(1 + 2 * mime.as_str().len() + icon.len());
            merged.generic_icons.insert(mime.clone(), icon.clone());
        }
        for (namespace, local, mime) in &folder.namespaces {
            // Each byte of the key copied, hashed and compared.
            work::count(1 + 3 * (namespace.len() + local.len()));
            merged
                .namespaces
                .insert((namespace.clone(), local.clone()), mime.clone());
        }

        merged.max_extent = merged.max_extent.max(folder.max_extent);
    }

    // A rule goes when a more important folder deletes its type's rules.
    let kept = |marks: &HashMap<&MimeType, usize>, mime: &MimeType, folder: usize| {
        work::count(1 + 2 * mime.as_str().len());
        marks.get(mime).is_none_or(|&marker| marker >= folder)
    };
    merged
        .globs
        .retain(|glob| kept(&no_globs, &glob.rule.mime, glob.folder));
    merged
        .magic
        .retain(|rule| kept(&no_magic, &rule.rule.mime, rule.folder));

    merged.aliases = resolve(&aliases);
    merged.aliases_of = aliases_of(&aliases, &merged.aliases);
    merged.parents = parents(&folders, &merged);

    // Most important first within each priority; the spec orders the same
    // pattern from several folders by folder.
    let rank = |folder: usize, order: usize| {
        // Each comparison a sort makes.
        work::count(1);
        (folder, order)
    };
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

/// `rules` of the folder of rank `folder`, copied, with their places.
fn ranked<T: Clone + Bytes>(rules: &[T], folder: usize) -> impl Iterator<Item = Ranked<T>> {
    rules.iter().enumerate().map(move |(order, rule)| {
        // Each byte copied.
        work::count_by(|| 1 + rule.bytes());
        Ranked {
            rule: rule.clone(),
            folder,
            order,
        }
    })
}

/// Resolves every alias once, as [`Merged::canonical`] describes: to the
/// first type its chain reaches that is not an alias, or, when the chain
/// comes back to a type it already passed, to that type. Each resolved
/// type is spelled as the alias entry that led to it.
fn resolve(aliases: &HashMap<MimeType, MimeType>) -> HashMap<MimeType, MimeType> {
    let mut resolved: HashMap<MimeType, MimeType> = HashMap::with_capacity(aliases.len());
    // Where on its chain each alias was met. An alias met and not yet
    // resolved is on the current chain: a chain resolves all its aliases
    // before the next one starts.
    let mut met: HashMap<&MimeType, usize> = HashMap::new();
    let mut chain: Vec<&MimeType> = Vec::new();
    for start in aliases.keys() {
        chain.clear();
        let mut current = start;
        // What the chain resolves to, and where on it a loop starts.
        let (end, looped) = loop {
            // The type looked up, hashed and compared up to four times.
            work::count(1 + 8 * current.as_str().len());
            if let Some(end) = resolved.get(current) {
                break (end.clone(), chain.len());
            }
            if let Some(&at) = met.get(current) {
                break (current.clone(), at);
            }
            let Some(target) = aliases.get(current) else {
                break (current.clone(), chain.len());
            };
            met.insert(current, chain.len());
            chain.push(current);
            current = target;
        };
        for (at, alias) in chain.iter().enumerate() {
            // The alias hashed and compared.
            work::count(2 * alias.as_str().len());
            // Past the start of a loop, each alias is the type the alias
            // before it leads to, so it resolves to itself.
            let end = if at > looped {
                (*alias).clone()
            } else {
                end.clone()
            };
            resolved.insert((*alias).clone(), end);
        }
    }
    resolved
}

/// Each type `aliases` resolve to, other than themselves, and those
/// aliases, sorted.
fn aliases_of(
    aliases: &HashMap<MimeType, MimeType>,
    resolved: &HashMap<MimeType, MimeType>,
) -> HashMap<MimeType, Vec<MimeType>> {
    let mut aliases_of: HashMap<MimeType, Vec<MimeType>> = HashMap::new();
    for alias in aliases.keys() {
        let target = &resolved[alias];
        // The alias looked up and compared, its type hashed and compared.
        work::count(1 + 3 * alias.as_str().len() + 2 * target.as_str().len());
        if alias != target {
            aliases_of
                .entry(target.clone())
                .or_default()
                .push(alias.clone());
        }
    }
    for list in aliases_of.values_mut() {
        // Each list visited.
        work::count(1);
        list.sort_by(|a, b| {
            // Each comparison, by the bytes it compares.
            work::count(1 + a.as_str().len().min(b.as_str().len()));
            a.cmp(b)
        });
    }
    aliases_of
}

/// Each type's parents in `folders` (least important first), resolved
/// through `merged`'s aliases. Parents are added to, never replaced: only
/// glob-deleteall and magic-deleteall remove earlier information (spec
/// "Directory layout"). The more important folder's parents come first,
/// then file order, and each parent is kept once, as the type it resolves
/// to.
fn parents(folders: &[&Folder], merged: &Merged) -> HashMap<MimeType, Vec<MimeType>> {
    let mut parents: HashMap<MimeType, Vec<MimeType>> = HashMap::new();
    let mut listed = HashSet::new();
    for folder in folders.iter().rev() {
        for (mime, parent) in &folder.parents {
            let parent = merged.canonical(parent);
            // The pair hashed and compared, and the type again.
            work::count(1 + 4 * mime.as_str().len() + 2 * parent.as_str().len());
            if listed.insert((mime, parent.clone())) {
                parents.entry(mime.clone()).or_default().push(parent);
            }
        }
    }
    parents
}

/// Drops rules that repeat an earlier one exactly, keeping the first (the
/// most important): the same package installed in several folders gives
/// the same patterns and magic again (xdgmime's `filter_out_dupes`).
fn drop_repeats<T: Eq + Hash + Bytes>(rules: &mut Vec<Ranked<T>>) {
    let keep: Vec<bool> = {
        let mut seen = HashSet::new();
        rules
            .iter()
            .map(|ranked| {
                // The rule hashed and compared.
                work::count_by(|| 1 + 2 * ranked.rule.bytes());
                seen.insert(&ranked.rule)
            })
            .collect()
    };
    let mut keep = keep.into_iter();
    rules.retain(|_| {
        // Each rule kept or dropped.
        work::count(1);
        keep.next().unwrap_or(true)
    });
}

/// The bytes of input a rule holds, nested parts included, for counting.
pub(crate) trait Bytes {
    fn bytes(&self) -> usize;
}

impl Bytes for Glob {
    fn bytes(&self) -> usize {
        self.pattern.len() + self.mime.as_str().len()
    }
}

impl Bytes for MagicRule {
    fn bytes(&self) -> usize {
        self.mime.as_str().len() + self.matchlets.iter().map(Bytes::bytes).sum::<usize>()
    }
}

impl Bytes for Matchlet {
    fn bytes(&self) -> usize {
        1 + self.value.len()
            + self.mask.as_ref().map_or(0, Vec::len)
            + self.children.iter().map(Bytes::bytes).sum::<usize>()
    }
}

impl Bytes for TreeRule {
    fn bytes(&self) -> usize {
        self.mime.as_str().len() + self.matchlets.iter().map(Bytes::bytes).sum::<usize>()
    }
}

impl Bytes for TreeMatchlet {
    fn bytes(&self) -> usize {
        1 + self.path.len()
            + self.mimetype.as_ref().map_or(0, |mime| mime.as_str().len())
            + self.children.iter().map(Bytes::bytes).sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// A folder of `count` globs and magic rules, of types `text/x-t0` on.
    fn rules(count: usize) -> Folder {
        Folder {
            globs: (0..count)
                .map(|at| glob(&format!("*.e{at}"), &format!("text/x-t{at}")))
                .collect(),
            magic: (0..count)
                .map(|at| magic(50, &format!("text/x-t{at}"), b"ab"))
                .collect(),
            ..Folder::default()
        }
    }

    /// A folder that deletes the globs and magic of types `text/x-t0` to
    /// `text/x-t{count - 1}`, and has a glob of `text/x-t0` itself.
    fn markers(count: usize) -> Folder {
        let types: Vec<MimeType> = (0..count).map(|at| m(&format!("text/x-t{at}"))).collect();
        Folder {
            globs: vec![glob("*.kept", "text/x-t0")],
            no_globs: types.clone(),
            no_magic: types,
            ..Folder::default()
        }
    }

    /// The rules and markers of `folders`, and their bytes.
    fn size(folders: &[&Folder]) -> (usize, usize) {
        let mut items = 0;
        let mut bytes = 0;
        for folder in folders {
            items += folder.globs.len()
                + folder.magic.len()
                + folder.no_globs.len()
                + folder.no_magic.len();
            bytes += folder.globs.iter().map(Bytes::bytes).sum::<usize>()
                + folder.magic.iter().map(Bytes::bytes).sum::<usize>()
                + folder
                    .no_globs
                    .iter()
                    .chain(&folder.no_magic)
                    .map(|mime| mime.as_str().len())
                    .sum::<usize>();
        }
        (items, bytes)
    }

    /// The bound for merging `items` rules, markers and parents of
    /// `bytes` bytes: twice the analysed cost. Per item up to three steps
    /// (a marker noted; a rule copied and checked against the markers; a
    /// parent resolved and noted), and per byte about two (a type hashed
    /// and compared, a pattern or value copied).
    fn markers_bound(items: usize, bytes: usize) -> usize {
        2 * (3 * items + 2 * bytes)
    }

    #[test]
    fn deletion_markers_are_applied_in_linear_time() {
        // n markers against n earlier globs and magic rules.
        work::assert_linear(
            1000,
            |n| {
                let (system, user) = (rules(n), markers(n));
                let (merged, steps) = work::steps(|| merge([&system, &user].into_iter()));
                assert_eq!(merged.globs.len(), 1);
                assert!(merged.magic.is_empty());
                (steps, size(&[&system, &user]).1)
            },
            |items, bytes| markers_bound(4 * items, bytes),
        );
    }

    #[test]
    fn many_deletion_markers_against_a_system_folder_are_applied_in_linear_time() {
        // The usual shape: a user folder of n markers over a system folder
        // of a fixed size.
        let system = rules(1000);
        work::assert_linear(
            1000,
            |n| {
                let user = markers(n);
                let (merged, steps) = work::steps(|| merge([&system, &user].into_iter()));
                assert_eq!(merged.globs.len(), 1);
                assert!(merged.magic.is_empty());
                (steps, size(&[&system, &user]).1)
            },
            |items, bytes| markers_bound(2 * items + 2 * 1000, bytes),
        );
    }

    #[test]
    fn many_folders_are_merged_in_linear_time() {
        // n folders, each with a glob and a magic rule of its own type,
        // markers for the previous folder's type, and a parent of one
        // shared type.
        work::assert_linear(
            1000,
            |n| {
                let folders: Vec<Folder> = (0..n)
                    .map(|at| {
                        let own = format!("text/x-t{at}");
                        let previous = m(&format!("text/x-t{}", at.saturating_sub(1)));
                        Folder {
                            globs: vec![glob(&format!("*.e{at}"), &own)],
                            magic: vec![magic(50, &own, b"ab")],
                            no_globs: vec![previous.clone()],
                            no_magic: vec![previous],
                            parents: vec![(m("text/x-a"), m(&format!("text/x-p{at}")))],
                            ..Folder::default()
                        }
                    })
                    .collect();
                let (merged, steps) = work::steps(|| merge(folders.iter()));
                // Only the last folder's rules are left; all parents are.
                assert_eq!(merged.globs.len(), 1);
                assert_eq!(merged.magic.len(), 1);
                assert_eq!(merged.parents[&m("text/x-a")].len(), n);
                let refs: Vec<&Folder> = folders.iter().collect();
                let parents: usize = folders
                    .iter()
                    .flat_map(|folder| &folder.parents)
                    .map(|(mime, parent)| mime.as_str().len() + parent.as_str().len())
                    .sum();
                (steps, size(&refs).1 + parents)
            },
            // Twice the analysed cost. Per folder about ten steps (the
            // folder; two rules, two markers and a parent, a step or two
            // each), and per byte up to four, about three overall: a
            // marker's type hashed and compared (two), a rule copied and
            // its type hashed and compared against the markers (up to
            // three), a parent made canonical, then the pair hashed and
            // compared and its type again (four).
            |items, bytes| 2 * (10 * items + 3 * bytes),
        );
    }

    /// What `canonical` gave when it walked the chain on each call, before
    /// aliases were resolved when merging: the reference for `resolve`.
    fn walked(aliases: &HashMap<MimeType, MimeType>, mime: &MimeType) -> MimeType {
        let mut current = mime;
        let mut seen = HashSet::new();
        while let Some(target) = aliases.get(current) {
            if !seen.insert(current) {
                break;
            }
            current = target;
        }
        current.clone()
    }

    /// A small pseudo-random generator (xorshift64).
    struct Random(u64);

    impl Random {
        fn below(&mut self, bound: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            usize::try_from(self.0 % bound as u64).unwrap()
        }
    }

    #[test]
    fn no_alias_resolves_to_another_alias() {
        // A chain, a branch onto it, a loop, a chain into the loop and an
        // alias of itself.
        let folder = Folder {
            aliases: vec![
                (m("a/chain0"), m("a/chain1")),
                (m("a/chain1"), m("A/End")),
                (m("a/branch"), m("a/chain1")),
                (m("a/loop0"), m("a/loop1")),
                (m("a/loop1"), m("a/loop2")),
                (m("a/loop2"), m("A/LOOP0")),
                (m("a/into"), m("a/loop1")),
                (m("a/self"), m("a/self")),
            ],
            ..Folder::default()
        };
        let merged = merge([&folder].into_iter());
        let canonical = |text: &str| merged.canonical(&m(text)).as_str().to_owned();
        for alias in ["a/chain0", "a/chain1", "a/branch"] {
            assert_eq!(canonical(alias), "A/End");
        }
        // A type on a loop resolves to itself, spelled as the alias that
        // leads to it.
        assert_eq!(canonical("a/loop0"), "A/LOOP0");
        assert_eq!(canonical("a/loop1"), "a/loop1");
        assert_eq!(canonical("a/loop2"), "a/loop2");
        assert_eq!(
            canonical("a/into"),
            "a/loop1",
            "the first loop type reached"
        );
        assert_eq!(canonical("a/self"), "a/self");
        assert_eq!(canonical("A/Other"), "A/Other");
        for (alias, target) in &merged.aliases {
            assert!(
                merged
                    .aliases
                    .get(target)
                    .is_none_or(|again| again == target),
                "{alias} resolves to {target}, which resolves further"
            );
        }
        assert_eq!(
            merged.aliases_of[&m("a/end")],
            [m("a/branch"), m("a/chain0"), m("a/chain1")]
        );
        assert_eq!(merged.aliases_of[&m("a/loop1")], [m("a/into")]);
        assert!(!merged.aliases_of.contains_key(&m("a/self")));
    }

    #[test]
    fn resolved_aliases_match_walking_each_chain() {
        let mut random = Random(0x9e37_79b9_7f4a_7c15);
        let spelled = |random: &mut Random, at: usize| {
            let text = format!("a/t{at}");
            m(&if random.below(3) == 0 {
                text.to_uppercase()
            } else {
                text
            })
        };
        for _ in 0..3000 {
            // Random alias graphs over a few types, spread over up to
            // three folders: chains, branches, loops and chains into
            // loops, spelled in either case.
            let types = 1 + random.below(12);
            let folders: Vec<Folder> = (0..=random.below(3))
                .map(|_| Folder {
                    aliases: (0..random.below(2 * types + 1))
                        .map(|_| {
                            let (from, to) = (random.below(types), random.below(types));
                            (spelled(&mut random, from), spelled(&mut random, to))
                        })
                        .collect(),
                    ..Folder::default()
                })
                .collect();
            let merged = merge(folders.iter());
            let mut aliases = HashMap::new();
            for (alias, target) in folders.iter().flat_map(|folder| &folder.aliases) {
                aliases.insert(alias.clone(), target.clone());
            }
            let texts = |types: &[MimeType]| -> Vec<String> {
                types.iter().map(|mime| mime.as_str().to_owned()).collect()
            };
            // Every type and one that is never an alias, in both cases.
            for at in 0..=types {
                for mime in [m(&format!("a/t{at}")), m(&format!("A/T{at}"))] {
                    let expected = walked(&aliases, &mime);
                    let canonical = merged.canonical(&mime);
                    assert_eq!(
                        canonical.as_str(),
                        expected.as_str(),
                        "{mime} in {aliases:?}"
                    );
                    let mut listed: Vec<MimeType> = aliases
                        .keys()
                        .filter(|alias| walked(&aliases, alias) == expected && **alias != expected)
                        .cloned()
                        .collect();
                    listed.sort();
                    let got = merged
                        .aliases_of
                        .get(&canonical)
                        .cloned()
                        .unwrap_or_default();
                    assert_eq!(texts(&got), texts(&listed), "{mime} in {aliases:?}");
                }
            }
            for (alias, target) in &merged.aliases {
                assert!(
                    merged
                        .aliases
                        .get(target)
                        .is_none_or(|again| again == target),
                    "{alias}"
                );
            }
        }
    }
}
