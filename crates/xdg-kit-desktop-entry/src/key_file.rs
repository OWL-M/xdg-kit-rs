//! Key files (Desktop Entry spec "Basic format of the file"), read and
//! written without losing anything.
//!
//! Every line is kept as it was written: comments, blank lines, group
//! headers and entries, and the bytes of a line that is not UTF-8. A
//! comment may hold any bytes: one that is not UTF-8 gets no diagnostic. A
//! file written back unchanged is byte-identical; changing a value rewrites
//! only the value part of its line.
//!
//! The spec forbids duplicate groups and keys. Reading follows `GLib`'s
//! `gkeyfile.c`: a repeated group continues the first one, and a repeated
//! key's last value counts. Both produce diagnostics.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::hash::Hash;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use xdg_kit_locale::Locales;

use crate::Diagnostic;
use crate::error::quoted;

/// What one line is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Blank,
    /// A line whose first byte after spaces and tabs is `#`, whatever its
    /// other bytes are: they need not be UTF-8.
    Comment,
    Group(String),
    /// `key[locale]=value`; the value starts at byte `value_start`.
    Entry {
        key: String,
        locale: Option<String>,
        value_start: usize,
        /// The entry's id, which it keeps while it is in the file, so that
        /// the index finds its place by it ([`Index::places`]).
        id: u64,
    },
    /// Not UTF-8, or neither of the above.
    Invalid,
    /// A line deleted by [`KeyFile::remove`]. Skipped everywhere but kept
    /// in place so later positions in the block remain valid.
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Line {
    /// The line as written, without its line feed.
    raw: Vec<u8>,
    kind: Kind,
    /// What is wrong with the line itself, found when it was read.
    problem: Option<String>,
}

impl Line {
    /// The line without a carriage return before its line feed, which
    /// reading treats as part of the line ending (`validate` reports it).
    fn content(&self) -> &[u8] {
        self.raw.strip_suffix(b"\r").unwrap_or(&self.raw)
    }

    /// The steps of copying the line: one, and one per byte of it, of its
    /// key and locale or group name, and of its problem.
    fn copy_steps(&self) -> usize {
        let kind = match &self.kind {
            Kind::Group(name) => name.len(),
            Kind::Entry { key, locale, .. } => key.len() + locale.as_ref().map_or(0, String::len),
            Kind::Blank | Kind::Comment | Kind::Invalid | Kind::Removed => 0,
        };
        1 + self.raw.len() + kind + self.problem.as_ref().map_or(0, String::len)
    }

    fn value(&self) -> Option<&str> {
        match &self.kind {
            Kind::Entry { value_start, .. } => {
                let value = self.content().get(*value_start..)?;
                // A step per byte checked for UTF-8.
                work::count(value.len());
                std::str::from_utf8(value).ok()
            }
            _ => None,
        }
    }
}

/// A line's block, by its number ([`Block::number`]), and its position
/// within that block.
type Place = (u64, usize);

/// A group header and the lines up to the next header, or the lines
/// before the first header.
///
/// Only the first block, which holds lines before any header, and the
/// blocks of removed groups may be empty. Every other block starts with a
/// header and contains no other headers. Trailing comments, blank lines and
/// invalid lines are stored separately so an entry can be inserted before
/// them without moving existing lines. A line removed from `lines` stays
/// there as [`Kind::Removed`], so that no later place in the block moves,
/// until more of them are removed than not and the block is compacted; the
/// last two of `lines` are never removed ones.
#[derive(Debug, Clone, Default)]
struct Block {
    /// The block's number, which it keeps while it is in the file: blocks
    /// are numbered in file order, and the places of lines name blocks by
    /// it, so that letting go of removed groups' blocks moves no place
    /// (see [`KeyFile::reclaim`]). The first block's is 0.
    number: u64,
    /// The lines up to the last entry, or the header when there is none.
    lines: Vec<Line>,
    /// How many of `lines` are removed ones.
    removed: usize,
    /// The lines after them.
    tail: VecDeque<Line>,
    /// Problems in `lines`, with their positions, in line order. Those of
    /// removed lines are skipped.
    problems: Vec<(usize, String)>,
    /// Problems in `tail`, in line order, with their positions within it
    /// less `tail_front` (wrapping), which stays right as lines are put in
    /// front of the tail.
    tail_problems: VecDeque<(usize, String)>,
    /// How many lines were put in front of the tail.
    tail_front: usize,
    /// Which of `lines` are not removed ones, kept as they change, for
    /// counting the lines before a place.
    live: Counts,
    /// The number of the nearest earlier block that is not a removed
    /// group's.
    previous: Option<u64>,
    /// The number of the nearest later block that is not a removed group's.
    next: Option<u64>,
}

impl Block {
    /// How many lines the block has, removed ones not counted.
    fn len(&self) -> usize {
        self.lines.len() - self.removed + self.tail.len()
    }

    /// How many places the block has: its lines, removed ones included.
    fn places(&self) -> usize {
        self.lines.len() + self.tail.len()
    }

    /// The line at `at`, counted over `lines` and then `tail`.
    fn line_mut(&mut self, at: usize) -> &mut Line {
        match at.checked_sub(self.lines.len()) {
            Some(in_tail) => &mut self.tail[in_tail],
            None => &mut self.lines[at],
        }
    }

    fn last_line(&self) -> Option<&Line> {
        self.tail.back().or_else(|| self.lines.last())
    }

    /// How many lines come before the place `at` in the block, removed ones
    /// not counted.
    fn rank(&self, at: usize) -> usize {
        if self.removed == 0 {
            return at;
        }
        match at.checked_sub(self.lines.len()) {
            Some(in_tail) => self.lines.len() - self.removed + in_tail,
            None => self.live.before(at),
        }
    }

    /// The steps of copying the block: one, its lines', and one per problem,
    /// per byte of one and per node of the line counts.
    fn copy_steps(&self) -> usize {
        let lines: usize = self
            .lines
            .iter()
            .chain(&self.tail)
            .map(Line::copy_steps)
            .sum();
        let problems: usize = self
            .problems
            .iter()
            .chain(&self.tail_problems)
            .map(|(_, problem)| 1 + problem.len())
            .sum();
        1 + lines + problems + self.live.tree.len()
    }

    /// Adds `line` at the end of `lines`.
    fn push(&mut self, line: Line) {
        self.live.push(usize::from(line.kind != Kind::Removed));
        self.lines.push(line);
    }

    /// Takes the last of `lines` off.
    fn pop(&mut self) -> Option<Line> {
        self.live.pop();
        self.lines.pop()
    }
}

/// Stores a count per list item in a Fenwick tree. Summing the first `n`
/// counts takes a step per bit of `n`; changing a count takes a step per
/// bit of the list length. Adding or dropping the last item takes constant
/// amortized work.
#[derive(Debug, Clone, Default)]
struct Counts {
    /// Node `n` (stored at `n - 1`) holds the count of the items after
    /// `n - n.isolate_lowest_one()` up to `n`, counted from 1.
    tree: Vec<usize>,
}

impl Counts {
    /// Adds an item with `count` at the end.
    fn push(&mut self, count: usize) {
        // The new node sums the item and the nodes it covers: a step per
        // node, one on average.
        let n = self.tree.len() + 1;
        let mut sum = count;
        let mut covered = n - 1;
        while covered > n - n.isolate_lowest_one() {
            work::count(1);
            sum += self.tree[covered - 1];
            covered -= covered.isolate_lowest_one();
        }
        self.tree.push(sum);
    }

    /// Drops the last item: no other node covers it.
    fn pop(&mut self) {
        self.tree.pop();
    }

    /// Changes the count of item `at` (counted from 0) by `change`, in
    /// each node that covers it: a step per node.
    fn change(&mut self, at: usize, change: impl Fn(usize) -> usize) {
        let mut n = at + 1;
        while n <= self.tree.len() {
            work::count(1);
            self.tree[n - 1] = change(self.tree[n - 1]);
            n += n.isolate_lowest_one();
        }
    }

    /// Adds `count` to item `at` (counted from 0).
    fn add(&mut self, at: usize, count: usize) {
        self.change(at, |sum| sum + count);
    }

    /// Takes `count` off item `at` (counted from 0), which has at least that.
    fn sub(&mut self, at: usize, count: usize) {
        self.change(at, |sum| sum - count);
    }

    /// The sum of the counts of the first `at` items.
    fn before(&self, at: usize) -> usize {
        let mut n = at;
        let mut sum = 0;
        while n > 0 {
            work::count(1);
            sum += self.tree[n - 1];
            n -= n.isolate_lowest_one();
        }
        sum
    }
}

/// A step per level of a set of `len` items walked: the steps of finding,
/// adding or removing one.
fn depth(len: usize) -> usize {
    usize::try_from(usize::BITS - len.leading_zeros()).unwrap_or(usize::MAX)
}

/// The lines of one key in a group, found by locale: finding, adding or
/// removing the lines of one locale is constant work for distinct locales,
/// and log n among the n variants that share a postfix once their
/// `.ENCODING` parts are stripped.
///
/// Lines are identified by entry IDs; [`Index::places`] tracks their
/// positions, so moving a line needs no change here. A key's IDs stay in
/// file order: reading assigns them in that order, new entries follow the
/// key's other lines with an ID larger than any previous one, and lines
/// move only past removed lines.
#[derive(Debug, Clone, Default)]
struct KeyLines {
    /// The ids of the lines of each locale (`None`: untranslated), in file
    /// order; as a repeated key's last value counts, the last one is the
    /// one read.
    lines: HashMap<Option<String>, Vec<u64>>,
    /// The ids of the lines of each locale postfix with an `.ENCODING`
    /// part, under the postfix without it.
    encoded: HashMap<String, BTreeSet<u64>>,
}

impl KeyLines {
    /// Adds the line of the entry `id` for `locale`; it comes after the
    /// key's other lines.
    fn add(&mut self, locale: Option<&str>, id: u64) {
        // Looked through for a `.`, then copied, hashed and compared as a
        // key (see [`work`]).
        work::count(4 * locale.map_or(0, str::len));
        if let Some(locale) = locale.filter(|locale| locale.contains('.')) {
            let stripped = strip_encoding(locale).into_owned();
            // The postfix without its encoding hashed and compared as a key,
            // and the place added to its set.
            work::count(2 * stripped.len());
            let ids = self.encoded.entry(stripped).or_default();
            work::count(depth(ids.len()));
            ids.insert(id);
        }
        self.lines
            .entry(locale.map(str::to_owned))
            .or_default()
            .push(id);
    }

    /// The id of the last line of `locale`.
    fn last(&self, locale: Option<&str>) -> Option<u64> {
        // Copied, hashed and compared.
        work::count(3 * locale.map_or(0, str::len));
        self.lines
            .get(&locale.map(str::to_owned))
            .and_then(|ids| ids.last())
            .copied()
    }

    /// The id of the line holding the value of `locale`: the last one, as
    /// a repeated key's last value counts.
    fn find(&self, locale: Option<&str>) -> Option<u64> {
        let id = self.last(locale);
        work::count(1);
        id
    }

    /// The id of the last line whose locale postfix is `wanted` once its
    /// `.ENCODING` part is stripped, `wanted` having none.
    fn last_encoded(&self, wanted: &str) -> Option<u64> {
        // Hashed and compared.
        work::count(2 * wanted.len());
        let ids = self.encoded.get(wanted)?;
        work::count(depth(ids.len()));
        ids.last().copied()
    }

    /// The steps of copying them: one per locale or postfix, per byte of
    /// one and per id.
    fn copy_steps(&self) -> usize {
        let lines: usize = self
            .lines
            .iter()
            .map(|(locale, ids)| 1 + locale.as_ref().map_or(0, String::len) + ids.len())
            .sum();
        let encoded: usize = self
            .encoded
            .iter()
            .map(|(postfix, ids)| 1 + postfix.len() + ids.len())
            .sum();
        lines + encoded
    }

    /// Takes out the lines of `locale` and returns their ids, in file
    /// order.
    fn remove(&mut self, locale: Option<&str>) -> Vec<u64> {
        // Copied, hashed and compared.
        work::count(3 * locale.map_or(0, str::len));
        let ids = self
            .lines
            .remove(&locale.map(str::to_owned))
            .unwrap_or_default();
        if let Some(locale) = locale.filter(|locale| locale.contains('.')) {
            let stripped = strip_encoding(locale);
            // Looked through for a `.`; then the postfix without its
            // encoding hashed and compared, twice when its set empties.
            work::count(locale.len() + 4 * stripped.len());
            if let Some(set) = self.encoded.get_mut(stripped.as_ref()) {
                for id in &ids {
                    work::count(depth(set.len()));
                    set.remove(id);
                }
                if set.is_empty() {
                    self.encoded.remove(stripped.as_ref());
                }
            }
        }
        ids
    }

    /// Every id of the key's lines.
    fn ids(&self) -> impl Iterator<Item = u64> {
        self.lines.values().flatten().copied()
    }
}

/// How many bytes an item of a [`Listed`] has, for [`work`].
trait Bytes {
    fn bytes(&self) -> usize;
}

impl Bytes for String {
    fn bytes(&self) -> usize {
        self.len()
    }
}

impl Bytes for (String, Option<String>) {
    fn bytes(&self) -> usize {
        self.0.len() + self.1.as_ref().map_or(0, String::len)
    }
}

/// Items in the order they were added, each once. Removing one leaves a
/// gap, and the gaps are dropped when they outnumber the items, so adding
/// and removing take constant amortized work. Each item's position is
/// tracked by its number, so removing gaps can move items without hashing
/// them again.
#[derive(Debug, Clone)]
struct Listed<T> {
    /// The items with their numbers, and gaps.
    items: Vec<Option<(u64, T)>>,
    /// Each item's number.
    numbers: HashMap<T, u64>,
    /// Where each item is in `items`, by its number.
    at: HashMap<u64, usize>,
    /// The number the next item gets. Numbers are never reused. Running
    /// out of them would take `u64::MAX` items added in the life of the
    /// list, which cannot happen; it would panic rather than wrap.
    next: u64,
}

impl<T> Default for Listed<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            numbers: HashMap::new(),
            at: HashMap::new(),
            next: 0,
        }
    }
}

impl<T: Bytes + Clone + Eq + Hash> Listed<T> {
    /// Adds `item` at the end unless it is listed; whether it was new.
    fn insert(&mut self, item: T) -> bool {
        // Hashed and compared; when new, copied, hashed and compared again,
        // and its number hashed.
        work::count(2 * item.bytes());
        if self.numbers.contains_key(&item) {
            return false;
        }
        work::count(3 * item.bytes() + 1);
        let number = self.next;
        self.next = number
            .checked_add(1)
            .expect("the key file's list numbers are exhausted");
        self.numbers.insert(item.clone(), number);
        self.at.insert(number, self.items.len());
        self.items.push(Some((number, item)));
        true
    }

    fn remove(&mut self, item: &T) {
        // Hashed and compared.
        work::count(2 * item.bytes());
        let Some(number) = self.numbers.remove(item) else {
            return;
        };
        // Its number hashed.
        work::count(1);
        if let Some(at) = self.at.remove(&number) {
            self.items[at] = None;
        }
        if 2 * self.at.len() < self.items.len() {
            // A step per item or gap, and per item kept for its number
            // hashed to note its new place.
            work::count(self.items.len() + self.at.len());
            self.items.retain(Option::is_some);
            for (at, (number, _)) in self.items.iter().flatten().enumerate() {
                self.at.insert(*number, at);
            }
        }
    }

    /// The steps of copying them: one per item or gap, per item and byte of
    /// one again for its number, and one per item for where it is.
    fn copy_steps(&self) -> usize {
        self.items.len()
            + self
                .numbers
                .keys()
                .map(|item| 2 + 2 * item.bytes())
                .sum::<usize>()
    }

    /// The items in order, with a step per item or gap.
    fn iter(&self) -> impl Iterator<Item = &T> {
        work::count(self.items.len());
        self.items.iter().flatten().map(|(_, item)| item)
    }

    fn is_empty(&self) -> bool {
        self.numbers.is_empty()
    }
}

/// An index of positions within the blocks, built when the file is read and
/// updated by each edit.
///
/// Past `headers`, a group is named by the number of its first block, which
/// it keeps while it is in the file (block numbers are never reused), so
/// that the work per entry does not grow with the group's name. Past
/// `entries`, an entry is named by its id, so that moving its line does
/// not grow with its key or locale.
#[derive(Debug, Clone, Default)]
struct Index {
    /// Group names, each once, in the order they first appear.
    groups: Listed<String>,
    /// The numbers of the blocks of each group's headers, in file order. The
    /// first names the group; a new entry goes at the end of the `lines` of
    /// the last.
    headers: HashMap<String, Vec<u64>>,
    /// Each group's keys with their locales, each once, in order.
    keys: HashMap<u64, Listed<(String, Option<String>)>>,
    /// The lines of each (group, key), by locale.
    entries: HashMap<(u64, String), KeyLines>,
    /// The place of each indexed entry, by its id ([`Kind::Entry`]):
    /// moving a line updates its place here in constant work, without its
    /// key or locale.
    places: HashMap<u64, Place>,
    /// The id the next entry gets. Ids are never reused. Running out of
    /// them would take `u64::MAX` entries read or added in the life of the
    /// file, which cannot happen; it would panic rather than wrap.
    next_id: u64,
}

impl Index {
    /// A new entry's id: larger than any given before.
    fn new_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = id
            .checked_add(1)
            .expect("the key file's entry ids are exhausted");
        id
    }

    /// The place of the entry `id`, with a step for the number hashed.
    fn place(&self, id: u64) -> Place {
        work::count(1);
        self.places[&id]
    }

    /// Notes that the entry `id` is at `place`, with a step for the number
    /// hashed.
    fn set_place(&mut self, id: u64, place: Place) {
        work::count(1);
        self.places.insert(id, place);
    }

    /// Forgets the entry `id` and returns its place, with a step for the
    /// number hashed.
    fn take_place(&mut self, id: u64) -> Option<Place> {
        work::count(1);
        self.places.remove(&id)
    }

    /// The steps of copying it: one per group, key and entry, per byte of
    /// their names and per block number, the key lists' and key lines', and
    /// one per entry's place.
    fn copy_steps(&self) -> usize {
        let headers: usize = self
            .headers
            .iter()
            .map(|(name, numbers)| 1 + name.len() + numbers.len())
            .sum();
        let keys: usize = self.keys.values().map(|keys| 1 + keys.copy_steps()).sum();
        let entries: usize = self
            .entries
            .iter()
            .map(|((_, key), lines)| 1 + key.len() + lines.copy_steps())
            .sum();
        self.groups.copy_steps() + headers + keys + entries + self.places.len()
    }
}

/// A key file: groups of `key=value` entries, with comments.
///
/// Two key files are equal when their text is.
#[derive(Debug)]
pub struct KeyFile {
    /// The lines before the first group header, then one block per header,
    /// in file order, with the emptied blocks of removed groups among them
    /// until they are let go ([`KeyFile::reclaim`]). Positions are kept
    /// within a block, counted over its `lines` and then its `tail`, so
    /// that adding an entry moves no line, and removing one moves only
    /// lines of its own block, now and then ([`KeyFile::tidy`]).
    blocks: Vec<Block>,
    /// Where in `blocks` each block is, by its number.
    slots: HashMap<u64, usize>,
    /// The number the next block added gets. Numbers are never reused.
    /// Running out of them would take `u64::MAX` blocks read or added in
    /// the life of the file, which cannot happen; it would panic rather
    /// than wrap.
    next_number: u64,
    /// How many of `blocks` are removed groups' blocks.
    dead: usize,
    /// Whether the text ended with a line feed.
    final_newline: bool,
    path: Option<PathBuf>,
    key_names: KeyNames,
    index: Index,
    /// How many lines each of `blocks` has (removed ones not counted),
    /// kept as they change, for the line each block starts on.
    sizes: Counts,
    /// Problems with line numbers, cached on first access after a change.
    diagnostics: OnceLock<Vec<Diagnostic>>,
}

impl PartialEq for KeyFile {
    fn eq(&self, other: &Self) -> bool {
        let (count, final_newline) = self.canonical();
        let (other_count, other_final_newline) = other.canonical();
        final_newline == other_final_newline
            && count == other_count
            && self.lines().zip(other.lines()).take(count).all(|(a, b)| {
                // A step per byte compared.
                work::count(a.raw.len().min(b.raw.len()));
                a.raw == b.raw
            })
    }
}

impl Clone for KeyFile {
    fn clone(&self) -> Self {
        // A step per item and per byte copied: the blocks with their lines
        // (removed ones too), the slots and line counts, the path, the
        // index, and the diagnostics when they were worked out.
        work::count_with(|| {
            let path =
                |path: &Option<PathBuf>| path.as_ref().map_or(0, |path| path.as_os_str().len());
            let blocks: usize = self.blocks.iter().map(Block::copy_steps).sum();
            let diagnostics: usize = self.diagnostics.get().map_or(0, |found| {
                found
                    .iter()
                    .map(|diagnostic| 1 + diagnostic.message.len() + path(&diagnostic.path))
                    .sum()
            });
            blocks
                + self.slots.len()
                + self.sizes.tree.len()
                + path(&self.path)
                + self.index.copy_steps()
                + diagnostics
        });
        Self {
            blocks: self.blocks.clone(),
            slots: self.slots.clone(),
            next_number: self.next_number,
            dead: self.dead,
            final_newline: self.final_newline,
            path: self.path.clone(),
            key_names: self.key_names,
            index: self.index.clone(),
            sizes: self.sizes.clone(),
            diagnostics: self.diagnostics.clone(),
        }
    }
}

impl Eq for KeyFile {}

impl Default for KeyFile {
    fn default() -> Self {
        Self {
            blocks: vec![Block::default()],
            slots: HashMap::from([(0, 0)]),
            next_number: 1,
            dead: 0,
            final_newline: true,
            path: None,
            key_names: KeyNames::default(),
            index: Index::default(),
            sizes: {
                // The lines before the first group: none.
                let mut sizes = Counts::default();
                sizes.push(0);
                sizes
            },
            diagnostics: OnceLock::new(),
        }
    }
}

/// The key names a key file accepts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeyNames {
    /// The Desktop Entry spec's: `A-Za-z0-9-`.
    #[default]
    DesktopEntry,
    /// Names other key files use, such as `mimeapps.list` with its MIME
    /// type keys: not empty, no `=`, `[`, `]` or control character, no
    /// space or tab at either end, and not starting with `#` (a comment).
    /// A little stricter than `GLib`'s key files, so that every name reads
    /// back as written.
    Any,
}

impl KeyNames {
    /// Whether `name` may name a key.
    #[must_use]
    pub fn allows(self, name: &str) -> bool {
        match self {
            Self::DesktopEntry => valid_key_name(name),
            Self::Any => {
                !name.is_empty()
                    && !name.starts_with([' ', '\t', '#'])
                    && !name.ends_with([' ', '\t'])
                    && !name
                        .chars()
                        .inspect(|_| work::count(1))
                        .any(|c| matches!(c, '=' | '[' | ']') || c.is_control())
            }
        }
    }
}

/// Why an edit was refused: it would change the file's structure (a line
/// feed in a value, a `]` in a group name) or break its grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EditError {
    /// Not a group name: printable ASCII without `[` and `]`.
    GroupName(String),
    /// Not a key name under the file's [`KeyNames`].
    KeyName(String),
    /// Not a locale postfix: printable ASCII without `[`, `]` and `=`.
    Locale(String),
    /// A written value with a line feed or carriage return in it, or one
    /// starting with a space or tab (reading skips those; escape them).
    Value,
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GroupName(name) => write!(f, "\"{}\" is not a group name", quoted(name)),
            Self::KeyName(name) => write!(f, "\"{}\" is not a key name", quoted(name)),
            Self::Locale(locale) => write!(f, "\"{}\" is not a locale", quoted(locale)),
            Self::Value => write!(
                f,
                "a written value cannot hold a line break or start with a space or tab"
            ),
        }
    }
}

impl std::error::Error for EditError {}

/// Whether `locale` may follow a key in brackets.
fn valid_locale(locale: &str) -> bool {
    !locale.is_empty()
        && locale
            .bytes()
            .inspect(|_| work::count(1))
            .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b'[' | b']' | b'='))
}

/// Whether `name` may name a group: printable ASCII except `[` and `]`.
pub(crate) fn valid_group_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .inspect(|_| work::count(1))
            .all(|byte| (0x20..0x7f).contains(&byte) && byte != b'[' && byte != b']')
}

/// Whether `name` may name a key: `A-Za-z0-9-`.
pub(crate) fn valid_key_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .inspect(|_| work::count(1))
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// Splits `Name[de_AT]` into `Name` and `de_AT`.
fn split_key(text: &str) -> (&str, Option<&str>) {
    work::count(text.len());
    match text.strip_suffix(']').and_then(|rest| rest.split_once('[')) {
        Some((key, locale)) => (key, Some(locale)),
        None => (text, None),
    }
}

/// The problem of a key repeated in its group. Its line is given with it,
/// so the group is not named: the message stays short however long the
/// group's name is.
fn repeated_key(key: &str, locale: Option<&str>) -> String {
    let shown = match locale {
        Some(locale) => format!("{}[{}]", quoted(key), quoted(locale)),
        None => quoted(key),
    };
    let problem = format!("key {shown} appears again in this group; the last value counts");
    // The message written.
    work::count(problem.len());
    problem
}

/// A locale postfix without its `.ENCODING` part: `de_DE.UTF-8@euro` →
/// `de_DE@euro`.
fn strip_encoding(locale: &str) -> std::borrow::Cow<'_, str> {
    // The `.`, then the `@` after it: each character looked at once; and
    // copied.
    work::count(2 * locale.len());
    match locale.find('.') {
        None => locale.into(),
        Some(dot) => {
            let modifier = locale[dot..].find('@').map_or("", |at| &locale[dot + at..]);
            format!("{}{modifier}", &locale[..dot]).into()
        }
    }
}

/// Undoes the string escapes `\s \n \t \r \\`; any other backslash is kept
/// as written.
pub(crate) fn unescape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().inspect(|_| work::count(1));
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            // `\\` and a backslash at the end both give one backslash.
            Some('\\') | None => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    out
}

/// Splits a list value at `;` (not `\;`) and unescapes each item. A final
/// `;` ends the last item rather than starting an empty one.
pub(crate) fn split_list(raw: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut chars = raw.chars().inspect(|_| work::count(1));
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(';') => current.push(';'),
                Some(other) => {
                    current.push('\\');
                    current.push(other);
                }
                None => current.push('\\'),
            },
            ';' => items.push(unescape(&std::mem::take(&mut current))),
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        items.push(unescape(&current));
    }
    work::count(items.len());
    items
}

/// Escapes `value` for writing: backslash, line feed, tab, carriage return,
/// a leading space, and in a list item `;`.
pub(crate) fn escape(value: &str, list_item: bool) -> String {
    let mut out = String::with_capacity(value.len());
    for (at, c) in value.chars().enumerate() {
        work::count(1);
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            ' ' if at == 0 => out.push_str("\\s"),
            ';' if list_item => out.push_str("\\;"),
            _ => out.push(c),
        }
    }
    out
}

impl KeyFile {
    /// An empty key file.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads the key file at `path`.
    ///
    /// # Errors
    ///
    /// When the file cannot be read, is not a regular file or is larger
    /// than [`xdg_kit_basedir::MAX_FILE_LEN`]. Problems in its text are
    /// diagnostics.
    pub fn read(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::read_with(path, KeyNames::DesktopEntry)
    }

    /// Reads the key file at `path`, whose keys follow `key_names`.
    ///
    /// # Errors
    ///
    /// When the file cannot be read, is not a regular file or is larger
    /// than [`xdg_kit_basedir::MAX_FILE_LEN`]. Problems in its text are
    /// diagnostics.
    pub fn read_with(path: impl AsRef<Path>, key_names: KeyNames) -> io::Result<Self> {
        let path = path.as_ref();
        let bytes = xdg_kit_basedir::read_regular_file(path)?;
        Ok(Self::parse_from(&bytes, Some(path.to_owned()), key_names))
    }

    /// Parses `bytes` as a key file. Problems are diagnostics; nothing fails.
    #[must_use]
    pub fn parse(bytes: &[u8]) -> Self {
        Self::parse_from(bytes, None, KeyNames::DesktopEntry)
    }

    /// Parses `bytes` as a key file whose keys follow `key_names`.
    #[must_use]
    pub fn parse_with(bytes: &[u8], key_names: KeyNames) -> Self {
        Self::parse_from(bytes, None, key_names)
    }

    /// The key names this file accepts.
    #[must_use]
    pub fn key_names(&self) -> KeyNames {
        self.key_names
    }

    fn parse_from(bytes: &[u8], path: Option<PathBuf>, key_names: KeyNames) -> Self {
        let mut file = Self {
            final_newline: bytes.last() == Some(&b'\n') || bytes.is_empty(),
            path,
            key_names,
            ..Self::default()
        };
        if !bytes.is_empty() {
            let body = bytes.strip_suffix(b"\n").unwrap_or(bytes);
            for raw in body.split(|&byte| {
                work::count(1);
                byte == b'\n'
            }) {
                work::count(1);
                let line = classify(raw, key_names);
                if matches!(line.kind, Kind::Group(_)) {
                    file.blocks.push(Block::default());
                }
                if let Some(block) = file.blocks.last_mut() {
                    block.lines.push(line);
                }
            }
        }
        file.reindex();
        file
    }

    /// Splits each block of freshly read lines into its lines and its tail,
    /// and builds the blocks' numbers, problems, their links and the index.
    fn reindex(&mut self) {
        let mut index = Index::default();
        let count = self.blocks.len();
        self.slots = HashMap::with_capacity(count);
        for (slot, (number, block)) in (0..).zip(self.blocks.iter_mut()).enumerate() {
            // A step per block.
            work::count(1);
            block.number = number;
            self.slots.insert(number, slot);
            let Block {
                lines,
                tail,
                problems,
                tail_problems,
                live,
                previous,
                next,
                ..
            } = block;
            *previous = number.checked_sub(1);
            *next = (slot + 1 < count).then_some(number + 1);
            // The tail starts after the last entry, or after the header.
            let split = lines
                .iter()
                .inspect(|_| work::count(1))
                .rposition(|line| matches!(line.kind, Kind::Entry { .. }))
                .map_or(usize::from(number > 0), |last| last + 1);
            if split < lines.len() {
                tail.extend(lines.drain(split..));
            }
            for _ in lines.iter() {
                live.push(1);
            }
            for (at, line) in tail.iter().enumerate() {
                work::count(1);
                if let Some(problem) = &line.problem {
                    work::count(problem.len());
                    tail_problems.push_back((at, problem.clone()));
                }
            }
            let mut group = None;
            for (at, line) in lines.iter_mut().enumerate() {
                work::count(1);
                if let Some(problem) = &line.problem {
                    work::count(problem.len());
                    problems.push((at, problem.clone()));
                }
                match &mut line.kind {
                    Kind::Group(name) => {
                        // Hashed and compared; when new, copied, hashed and
                        // compared again (the group list counts its own).
                        work::count(2 * name.len());
                        if let Some(found) = index.headers.get_mut(name) {
                            found.push(number);
                            group = Some(found[0]);
                            let problem = format!(
                                "group [{}] appears again; its entries join the first one",
                                quoted(name)
                            );
                            work::count(problem.len());
                            problems.push((at, problem));
                        } else {
                            work::count(3 * name.len());
                            index.headers.insert(name.clone(), vec![number]);
                            index.groups.insert(name.clone());
                            group = Some(number);
                        }
                    }
                    Kind::Entry {
                        key, locale, id, ..
                    } => {
                        // Every entry gets an id, so that ids are unique.
                        *id = index.new_id();
                        let Some(group) = group else {
                            problems.push((at, "entry before the first group; ignored".to_owned()));
                            continue;
                        };
                        // The key and locale copied for the key list (which
                        // counts the rest), and the key copied, hashed and
                        // compared for the entries.
                        work::count(4 * key.len() + locale.as_ref().map_or(0, String::len));
                        let new = index
                            .keys
                            .entry(group)
                            .or_default()
                            .insert((key.clone(), locale.clone()));
                        if !new {
                            problems.push((at, repeated_key(key, locale.as_deref())));
                        }
                        index
                            .entries
                            .entry((group, key.clone()))
                            .or_default()
                            .add(locale.as_deref(), *id);
                        index.set_place(*id, (number, at));
                    }
                    Kind::Blank | Kind::Comment | Kind::Invalid | Kind::Removed => {}
                }
            }
        }
        self.index = index;
        self.next_number = self.blocks.last().map_or(0, |block| block.number + 1);
        self.dead = 0;
        self.sizes = Counts::default();
        for block in &self.blocks {
            self.sizes.push(block.len());
        }
        self.changed();
    }

    /// Where in `blocks` the block numbered `number` is.
    fn slot(&self, number: u64) -> usize {
        self.slots[&number]
    }

    /// The block numbered `number`.
    fn block(&self, number: u64) -> &Block {
        &self.blocks[self.slot(number)]
    }

    /// The block numbered `number`.
    fn block_mut(&mut self, number: u64) -> &mut Block {
        let slot = self.slot(number);
        &mut self.blocks[slot]
    }

    /// Clears cached line numbers and diagnostics after a change.
    fn changed(&mut self) {
        self.diagnostics.take();
    }

    /// The line number of `place`, counted from 1.
    fn line_number(&self, (number, at): Place) -> usize {
        let slot = self.slot(number);
        self.sizes.before(slot) + self.blocks[slot].rank(at) + 1
    }

    fn line(&self, (number, at): Place) -> &Line {
        let block = self.block(number);
        match at.checked_sub(block.lines.len()) {
            Some(in_tail) => &block.tail[in_tail],
            None => &block.lines[at],
        }
    }

    /// Every line, in file order, with a step per block and per line,
    /// removed ones included. The blocks of removed groups are at most
    /// as many as the others ([`KeyFile::reclaim`]).
    fn lines(&self) -> impl Iterator<Item = &Line> {
        self.blocks.iter().flat_map(|block| {
            work::count(1);
            block
                .lines
                .iter()
                .chain(&block.tail)
                .inspect(|_| work::count(1))
                .filter(|line| line.kind != Kind::Removed)
        })
    }

    /// The last line, with a step per block looked at: the last block is
    /// never a removed group's, so only the first can be empty.
    fn last_line(&self) -> Option<&Line> {
        self.blocks
            .iter()
            .rev()
            .inspect(|_| work::count(1))
            .find_map(Block::last_line)
    }

    /// The place after the last line.
    fn end(&self) -> Place {
        let last = &self.blocks[self.blocks.len() - 1];
        (last.number, last.places())
    }

    /// The place of the line before `place`, if there is one. That line is
    /// never a removed one: the last two of a block's `lines` never are, and
    /// earlier places are not asked about.
    fn before(&self, (number, at): Place) -> Option<Place> {
        if at > 0 {
            let before = (number, at - 1);
            debug_assert_ne!(self.line(before).kind, Kind::Removed);
            return Some(before);
        }
        // Only the lines before the first group can be an empty block among
        // those not removed.
        let mut earlier = self.block(number).previous;
        while let Some(number) = earlier {
            if let Some(last) = self.block(number).places().checked_sub(1) {
                return Some((number, last));
            }
            earlier = self.block(number).previous;
        }
        None
    }

    /// The file this was read from, if any.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// What is skipped or looks wrong in the text as it is now (after any
    /// edits), with line numbers counted from 1.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        self.diagnostics.get_or_init(|| {
            // The line each block starts on, counted from 0.
            let mut next = 0;
            self.blocks
                .iter()
                .flat_map(|block| {
                    let start = next;
                    next += block.len();
                    work::count(1 + block.problems.len() + block.tail_problems.len());
                    let tail_start = start + block.lines.len() - block.removed;
                    let problems = block
                        .problems
                        .iter()
                        .filter(|(at, _)| block.lines[*at].kind != Kind::Removed)
                        .map(move |(at, message)| (start + block.rank(*at), message));
                    let tail_problems = block.tail_problems.iter().map(move |(at, message)| {
                        (tail_start + at.wrapping_add(block.tail_front), message)
                    });
                    problems.chain(tail_problems).map(|(at, message)| {
                        work::count(
                            message.len()
                                + self.path.as_ref().map_or(0, |path| path.as_os_str().len()),
                        );
                        Diagnostic::new(self.path.clone(), Some(at + 1), message.clone())
                    })
                })
                .collect()
        })
    }

    /// The lines (counted from 1) of the comments that are not UTF-8, which
    /// reading keeps without a diagnostic and `validate` warns about; with a
    /// step per line and per byte of a comment checked.
    pub(crate) fn comments_not_utf8(&self) -> Vec<usize> {
        self.lines()
            .enumerate()
            .filter(|(_, line)| {
                line.kind == Kind::Comment && {
                    work::count(line.raw.len());
                    std::str::from_utf8(line.content()).is_err()
                }
            })
            .map(|(at, _)| at + 1)
            .collect()
    }

    /// The group names, each once, in the order they first appear.
    #[must_use]
    pub fn groups(&self) -> Vec<&str> {
        self.index.groups.iter().map(String::as_str).collect()
    }

    /// Whether the file has the group.
    #[must_use]
    pub fn has_group(&self, group: &str) -> bool {
        // Hashed and compared.
        work::count(2 * group.len());
        self.index.headers.contains_key(group)
    }

    /// The keys of `group` with their locales, each once, in the order they
    /// first appear.
    #[must_use]
    pub fn keys(&self, group: &str) -> Vec<(&str, Option<&str>)> {
        self.group(group).map(Group::keys).unwrap_or_default()
    }

    /// `group`, found once by its name, to look up its keys through.
    pub(crate) fn group(&self, group: &str) -> Option<Group<'_>> {
        self.group_number(group)
            .map(|number| Group { file: self, number })
    }

    /// The number of the first block of `group`, which names it in the
    /// index.
    fn group_number(&self, group: &str) -> Option<u64> {
        // Hashed and compared.
        work::count(2 * group.len());
        self.index.headers.get(group).map(|found| found[0])
    }

    /// The lines of `key` in `group`, by locale.
    fn key_lines(&self, group: &str, key: &str) -> Option<&KeyLines> {
        self.group(group)?.key_lines(key)
    }

    /// The place of the line holding the value of `key[locale]` in `group`:
    /// the last one, as a repeated key's last value counts.
    fn find(&self, group: &str, key: &str, locale: Option<&str>) -> Option<Place> {
        self.group(group)?.find(key, locale)
    }

    /// The value of `key[locale]` in `group` as written, escapes included.
    #[must_use]
    pub fn raw(&self, group: &str, key: &str, locale: Option<&str>) -> Option<&str> {
        self.group(group)?.raw(key, locale)
    }

    /// The line (counted from 1) that holds `key[locale]` in `group`.
    #[must_use]
    pub fn line_of(&self, group: &str, key: &str, locale: Option<&str>) -> Option<usize> {
        self.group(group)?.line_of(key, locale)
    }

    /// The line (counted from 1) of `group`'s first header.
    #[must_use]
    pub fn group_line(&self, group: &str) -> Option<usize> {
        self.group_number(group)
            .map(|number| self.line_number((number, 0)))
    }

    /// The value of the key with `lines` translated for `wanted` (a locale
    /// key such as `de_AT@euro`), comparing postfixes with their
    /// `.ENCODING` part stripped, as the spec says; the last such line
    /// counts.
    fn translated(&self, lines: &KeyLines, wanted: &str) -> Option<&str> {
        // A postfix without a `.` is its own stripped form. Looked through
        // for one.
        work::count(wanted.len());
        let plain = if wanted.contains('.') {
            None
        } else {
            lines.last(Some(wanted))
        };
        // A key's ids order as its lines do: the later one counts.
        plain
            .max(lines.last_encoded(wanted))
            .inspect(|_| work::count(1))
            .and_then(|id| self.line(self.index.place(id)).value())
    }

    /// The untranslated value of `key` in `group`, unescaped.
    #[must_use]
    pub fn string(&self, group: &str, key: &str) -> Option<String> {
        self.raw(group, key, None).map(unescape)
    }

    /// The value of `key` in `group` for the first of `locales` that has
    /// one (Desktop Entry spec "Localized values for keys": a postfix's
    /// `.ENCODING` part is ignored), else the untranslated value; unescaped.
    #[must_use]
    pub fn locale_string(&self, group: &str, key: &str, locales: &Locales) -> Option<String> {
        self.locale_raw(group, key, locales).map(unescape)
    }

    /// The value of `key` in `group` for the first of `locales` that has
    /// one, else the untranslated value, as written. The group and the key
    /// are found once, so that the work per locale tried does not grow
    /// with their names.
    pub(crate) fn locale_raw(&self, group: &str, key: &str, locales: &Locales) -> Option<&str> {
        let lines = self.key_lines(group, key)?;
        locales
            .select(|locale| self.translated(lines, locale))
            .or_else(|| {
                lines
                    .find(None)
                    .and_then(|id| self.line(self.index.place(id)).value())
            })
    }

    /// The untranslated value of `key` in `group` as a `;`-separated list.
    #[must_use]
    pub fn list(&self, group: &str, key: &str) -> Option<Vec<String>> {
        self.raw(group, key, None).map(split_list)
    }

    /// The value of `key` in `group` as a list, for the first of `locales`
    /// that has one, else untranslated.
    #[must_use]
    pub fn locale_list(&self, group: &str, key: &str, locales: &Locales) -> Option<Vec<String>> {
        self.locale_raw(group, key, locales).map(split_list)
    }

    /// Sets `key[locale]` in `group` to `raw`, written as given: no escapes
    /// are added (for text, [`KeyFile::set_string`],
    /// [`KeyFile::set_locale_string`] and [`KeyFile::set_list`] add them).
    /// An existing entry keeps everything before its value; a new
    /// one goes after the group's last entry; a missing group is added at
    /// the end.
    ///
    /// # Errors
    ///
    /// When the group, key or locale is not a valid name, or `raw` holds a
    /// line break or starts with a space or tab (which reading would drop);
    /// the file is then unchanged.
    pub fn set_raw(
        &mut self,
        group: &str,
        key: &str,
        locale: Option<&str>,
        raw: &str,
    ) -> Result<(), EditError> {
        if !valid_group_name(group) {
            return Err(EditError::GroupName(group.to_owned()));
        }
        if !self.key_names.allows(key) {
            return Err(EditError::KeyName(key.to_owned()));
        }
        if let Some(locale) = locale.filter(|locale| !valid_locale(locale)) {
            return Err(EditError::Locale(locale.to_owned()));
        }
        work::count(raw.len());
        if raw.contains(['\n', '\r']) || raw.starts_with([' ', '\t']) {
            return Err(EditError::Value);
        }
        if let Some((block, at)) = self.find(group, key, locale) {
            // Only the value changes: the line count and the line's problem
            // stay, so the line numbers and diagnostics worked out stay
            // right and `changed` is not needed.
            let line = &mut self.block_mut(block).lines[at];
            if let Kind::Entry { value_start, .. } = line.kind {
                let carriage_return = line.raw.ends_with(b"\r");
                line.raw.truncate(value_start);
                work::count(raw.len());
                line.raw.extend_from_slice(raw.as_bytes());
                if carriage_return {
                    line.raw.push(b'\r');
                }
            }
            return Ok(());
        }
        let text = match locale {
            Some(locale) => format!("{key}[{locale}]={raw}"),
            None => format!("{key}={raw}"),
        };
        work::count(text.len());
        if !self.has_group(group) {
            self.push_group(group);
        }
        let line = Line::entry(&text, key, locale, self.index.new_id());
        self.insert_entry(group, key, locale, line);
        Ok(())
    }

    /// Inserts `line` for a new `key[locale]` after the last entry in an
    /// existing `group` and updates the index. Insertion takes log n work,
    /// n the number of blocks (for the line counts), however long the file.
    ///
    /// The group's last entry (or header) ends the `lines` of its last
    /// block. Appending there puts the new entry before the tail without
    /// moving existing lines or changing positions in the index or problem
    /// lists. It also puts the entry after every other line for `key`, as
    /// [`KeyLines::add`] requires; its id, given last, is the largest.
    fn insert_entry(&mut self, group: &str, key: &str, locale: Option<&str>, mut line: Line) {
        // The group hashed and compared to find its blocks.
        work::count(2 * group.len());
        // The first block names the group; the entry goes in the last.
        let found = self
            .index
            .headers
            .get(group)
            .and_then(|found| Some((*found.first()?, *found.last()?)));
        debug_assert!(found.is_some(), "insert_entry into missing group [{group}]");
        let Some((first, number)) = found else {
            return;
        };
        // Always an entry: `set_raw` makes it.
        let Kind::Entry { id, .. } = line.kind else {
            return;
        };
        let slot = self.slot(number);
        let place = (number, self.blocks[slot].lines.len());
        self.match_line_end(place, &mut line);
        // A step for the line added, at a place known without looking.
        work::count(1);
        self.blocks[slot].push(line);
        self.sizes.add(slot, 1);
        self.index.set_place(id, place);
        // The key copied, hashed and compared for the entries (the key's
        // lines count the locale).
        work::count(3 * key.len());
        self.index
            .entries
            .entry((first, key.to_owned()))
            .or_default()
            .add(locale, id);
        // The key and locale copied for the key list (which counts the
        // rest).
        work::count(key.len() + locale.map_or(0, str::len));
        self.index
            .keys
            .entry(first)
            .or_default()
            .insert((key.to_owned(), locale.map(str::to_owned)));
        self.changed();
    }

    /// Sets `key` in `group` to the string `value`, escaped.
    ///
    /// # Errors
    ///
    /// When the group or key is not a valid name.
    pub fn set_string(&mut self, group: &str, key: &str, value: &str) -> Result<(), EditError> {
        self.set_raw(group, key, None, &escape(value, false))
    }

    /// Sets the translation of `key` in `group` for `locale`.
    ///
    /// # Errors
    ///
    /// When the group, key or locale is not a valid name.
    pub fn set_locale_string(
        &mut self,
        group: &str,
        key: &str,
        locale: &str,
        value: &str,
    ) -> Result<(), EditError> {
        self.set_raw(group, key, Some(locale), &escape(value, false))
    }

    /// Sets `key` in `group` to `true` or `false`.
    ///
    /// # Errors
    ///
    /// When the group or key is not a valid name.
    pub fn set_boolean(&mut self, group: &str, key: &str, value: bool) -> Result<(), EditError> {
        self.set_raw(group, key, None, if value { "true" } else { "false" })
    }

    /// Sets `key` in `group` to a list, each item escaped and followed by `;`.
    ///
    /// # Errors
    ///
    /// When the group or key is not a valid name.
    pub fn set_list<S: AsRef<str>>(
        &mut self,
        group: &str,
        key: &str,
        items: &[S],
    ) -> Result<(), EditError> {
        let mut raw = String::new();
        for item in items {
            raw.push_str(&escape(item.as_ref(), true));
            raw.push(';');
        }
        self.set_raw(group, key, None, &raw)
    }

    /// Removes every `key[locale]` entry from `group`; whether there was one.
    /// Takes a few steps per byte of the supplied names and amortized
    /// log n work per removed line, where n is the block's line count or
    /// the number of blocks. The work does not otherwise grow with the file
    /// or with the length of other lines' keys and locales.
    ///
    /// Removed lines stay in place until they form most of the block. The
    /// block is then compacted at one step per line, paid for by removals
    /// since the previous compaction. Moved lines are found by entry ID,
    /// and the key list removes gaps using key numbers. Neither operation
    /// hashes a key or locale again.
    pub fn remove(&mut self, group: &str, key: &str, locale: Option<&str>) -> bool {
        let Some(group) = self.group_number(group) else {
            return false;
        };
        // The key copied, hashed and compared to find its lines.
        work::count(3 * key.len());
        let name = (group, key.to_owned());
        let Some(lines) = self.index.entries.get_mut(&name) else {
            return false;
        };
        let ids = lines.remove(locale);
        if ids.is_empty() {
            return false;
        }
        if lines.lines.is_empty() {
            // Hashed and compared again to drop them.
            work::count(2 * key.len());
            self.index.entries.remove(&name);
        }
        if let Some(keys) = self.index.keys.get_mut(&group) {
            // The key and locale copied for the key list (which counts the
            // rest).
            work::count(key.len() + locale.map_or(0, str::len));
            keys.remove(&(key.to_owned(), locale.map(str::to_owned)));
            if keys.is_empty() {
                self.index.keys.remove(&group);
            }
        }
        // In file order, as the ids are.
        let places: Vec<Place> = ids
            .into_iter()
            .filter_map(|id| self.index.take_place(id))
            .collect();
        // The lines stay as removed ones, so no other place moves.
        for &(number, at) in &places {
            // A step for the line, and the line counts change.
            work::count(1);
            let slot = self.slot(number);
            let block = &mut self.blocks[slot];
            block.lines[at] = Line {
                raw: Vec::new(),
                kind: Kind::Removed,
                problem: None,
            };
            block.live.sub(at, 1);
            block.removed += 1;
            self.sizes.sub(slot, 1);
        }
        // Places are in file order, so each block comes once.
        let mut numbers: Vec<u64> = places.iter().map(|&(number, _)| number).collect();
        numbers.dedup();
        for number in numbers {
            self.tidy(number);
        }
        self.changed();
        true
    }

    /// Restores the layout of block `number` after lines of its `lines`
    /// were removed: they end with an entry or the header, as reading
    /// splits them, the lines after the last entry going to the front of
    /// the tail; the line before the last is not a removed one; and once
    /// most of them are removed ones, the block is compacted. Log n work
    /// per line removed, n the block's lines (for their counts), and
    /// otherwise constant amortized, whatever the keys and locales of the
    /// lines moved: their places are noted by their entries' ids.
    fn tidy(&mut self, number: u64) {
        let slot = self.slot(number);
        let Self { blocks, index, .. } = self;
        let block = &mut blocks[slot];
        debug_assert!(
            number > 0,
            "the lines before the first group hold no entries"
        );
        while let Some(last) = block.lines.last() {
            let removed = last.kind == Kind::Removed;
            if !removed && !matches!(last.kind, Kind::Blank | Kind::Comment | Kind::Invalid) {
                break;
            }
            // A step per line taken off the end, and per problem of it.
            work::count(1);
            let Some(line) = block.pop() else {
                break;
            };
            let at = block.lines.len();
            let mut problems = Vec::new();
            while block.problems.last().is_some_and(|(place, _)| *place == at) {
                work::count(1);
                problems.extend(block.problems.pop().map(|(_, problem)| problem));
            }
            if removed {
                block.removed -= 1;
                continue;
            }
            block.tail_front = block.tail_front.wrapping_add(1);
            block.tail.push_front(line);
            // Taken off last first, so put in front last first.
            for problem in problems {
                let stored = 0usize.wrapping_sub(block.tail_front);
                block.tail_problems.push_front((stored, problem));
            }
        }
        if 2 * block.removed > block.lines.len() {
            compact(block, number, index);
            return;
        }
        // The last line moves down into a removed one before it, with its
        // problems and its place in the index.
        while let [.., before, _] = block.lines.as_slice()
            && before.kind == Kind::Removed
        {
            work::count(1);
            let from = block.lines.len() - 1;
            let to = from - 1;
            block.live.pop();
            block.lines.swap_remove(to);
            block.live.add(to, 1);
            block.removed -= 1;
            // The removed line's problems go; the moved line's move with it.
            let mut moved = Vec::new();
            while block.problems.last().is_some_and(|(at, _)| *at >= to) {
                work::count(1);
                if let Some((at, problem)) = block.problems.pop()
                    && at == from
                {
                    moved.push(problem);
                }
            }
            block
                .problems
                .extend(moved.into_iter().rev().map(|problem| (to, problem)));
            if let Kind::Entry { id, .. } = block.lines[to].kind {
                index.set_place(id, (number, to));
            }
        }
    }

    /// Adds an empty group at the end, after a blank line, unless it exists.
    ///
    /// # Errors
    ///
    /// When `group` is not a valid group name.
    pub fn add_group(&mut self, group: &str) -> Result<(), EditError> {
        if !valid_group_name(group) {
            return Err(EditError::GroupName(group.to_owned()));
        }
        if !self.has_group(group) {
            self.push_group(group);
        }
        Ok(())
    }

    /// Appends a header for `group`, which the file does not have, after a
    /// blank line, and adds the group to the index.
    fn push_group(&mut self, group: &str) {
        if self
            .last_line()
            .is_some_and(|line| line.kind != Kind::Blank)
        {
            let mut blank = Line {
                raw: Vec::new(),
                kind: Kind::Blank,
                problem: None,
            };
            let end = self.end();
            self.match_line_end(end, &mut blank);
            let last = self.blocks.len() - 1;
            self.blocks[last].tail.push_back(blank);
            self.sizes.add(last, 1);
        }
        // Copied into the header twice, then copied, hashed and compared
        // for the index (the group list counts its own).
        work::count(5 * group.len());
        let mut header = Line {
            raw: format!("[{group}]").into_bytes(),
            kind: Kind::Group(group.to_owned()),
            problem: None,
        };
        self.match_line_end(self.end(), &mut header);
        // The last block is never a removed group's.
        let previous = self.blocks.len() - 1;
        let number = self.next_number;
        self.next_number = number
            .checked_add(1)
            .expect("the key file's block numbers are exhausted");
        let mut block = Block {
            number,
            previous: Some(self.blocks[previous].number),
            ..Block::default()
        };
        block.push(header);
        self.slots.insert(number, self.blocks.len());
        self.blocks.push(block);
        self.sizes.push(1);
        self.blocks[previous].next = Some(number);
        self.index.headers.insert(group.to_owned(), vec![number]);
        self.index.groups.insert(group.to_owned());
        self.changed();
    }

    /// Gives `line`, about to go in at `place`, the file's line end: a file
    /// with CRLF line ends (judged by the nearest line before `place` that
    /// has a line end) gets them on new lines too. A last line without a
    /// line end gets one when a line goes after it, and the new line is
    /// then the one without.
    fn match_line_end(&mut self, place: Place, line: &mut Line) {
        let at_end = place == self.end();
        let before = self.before(place);
        let judged = if self.final_newline || !at_end {
            before
        } else {
            // The line before has no line end.
            before.and_then(|before| self.before(before))
        };
        if !judged.is_some_and(|judged| self.line(judged).raw.ends_with(b"\r")) {
            return;
        }
        if !at_end || self.final_newline {
            line.raw.push(b'\r');
        } else if let Some((block, at)) = before {
            let last = &mut self.block_mut(block).line_mut(at).raw;
            if !last.ends_with(b"\r") {
                last.push(b'\r');
            }
        }
    }

    /// How many lines there are and whether the text ends in a line feed,
    /// in the one form each text has: a last empty line without a line
    /// feed is the line feed of the line before it, and no lines at all
    /// are the empty text.
    fn canonical(&self) -> (usize, bool) {
        let count = self.sizes.before(self.blocks.len());
        match self.last_line() {
            Some(last) if !self.final_newline && last.raw.is_empty() => (count - 1, true),
            _ => (count, self.final_newline || count == 0),
        }
    }

    /// Removes every occurrence of `group` with its lines, up to the next
    /// group; whether there was one. The work grows with the group's lines
    /// and keys, and log n per block, n the number of blocks; not with the
    /// file, nor with the names of other groups. Amortized: once the emptied
    /// blocks of removed groups outnumber the others, they are let go, at a
    /// step per block, which the removals since they were last let go pay
    /// for.
    pub fn remove_group(&mut self, group: &str) -> bool {
        // Hashed and compared.
        work::count(2 * group.len());
        let Some(numbers) = self.index.headers.remove(group) else {
            return false;
        };
        let first = numbers[0];
        // The blocks stay, empty, so that no other block moves.
        for number in numbers {
            let slot = self.slot(number);
            let block = std::mem::replace(
                &mut self.blocks[slot],
                Block {
                    number,
                    ..Block::default()
                },
            );
            // A step per line dropped.
            work::count(1 + block.places());
            self.dead += 1;
            self.sizes.sub(slot, block.len());
            if let Some(previous) = block.previous {
                self.block_mut(previous).next = block.next;
            }
            if let Some(next) = block.next {
                self.block_mut(next).previous = block.previous;
            }
        }
        // Except at the end, so that the last block is never a removed one.
        while self.blocks.len() > 1
            && self
                .blocks
                .last()
                .is_some_and(|block| block.lines.is_empty())
        {
            work::count(1);
            if let Some(block) = self.blocks.pop() {
                self.slots.remove(&block.number);
            }
            self.sizes.pop();
            self.dead -= 1;
        }
        if 2 * self.dead > self.blocks.len() {
            self.reclaim();
        }
        // Copied for the group list (which counts the rest).
        work::count(group.len());
        self.index.groups.remove(&group.to_owned());
        if let Some(keys) = self.index.keys.remove(&first) {
            for (key, _) in keys.iter() {
                // The key copied, hashed and compared; then its lines'
                // places dropped.
                work::count(3 * key.len());
                if let Some(lines) = self.index.entries.remove(&(first, key.clone())) {
                    for id in lines.ids() {
                        self.index.take_place(id);
                    }
                }
            }
        }
        self.changed();
        true
    }

    /// Releases the empty blocks of removed groups, moves the remaining
    /// blocks down, and rebuilds their slots and line counts in one step
    /// per block. Positions refer to stable block numbers, so they remain
    /// valid.
    fn reclaim(&mut self) {
        // A step per block looked at.
        work::count(self.blocks.len());
        self.blocks
            .retain(|block| block.number == 0 || !block.lines.is_empty());
        self.blocks.shrink_to_fit();
        self.dead = 0;
        self.slots = HashMap::with_capacity(self.blocks.len());
        self.sizes = Counts::default();
        for (slot, block) in self.blocks.iter().enumerate() {
            // A step per block kept (the line counts count their own).
            work::count(1);
            self.slots.insert(block.number, slot);
            self.sizes.push(block.len());
        }
    }

    /// The file's text: an unchanged file gives back exactly what was read.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        // A step per line (and block) is counted by `lines`.
        for (index, line) in self.lines().enumerate() {
            if index > 0 {
                out.push(b'\n');
            }
            work::count(line.raw.len());
            out.extend_from_slice(&line.raw);
        }
        if self.final_newline && self.last_line().is_some() {
            out.push(b'\n');
        }
        out
    }

    /// Writes the file atomically using a new file in the same folder,
    /// which is synced and then renamed over `path`. Preserves an existing
    /// file's permissions; a new file gets `0644`. The owner and group are
    /// the writer's. Follows symbolic links and replaces their targets,
    /// including targets that do not exist yet. As many links are followed
    /// as the system follows when reading: `MAXSYMLINKS` where the `libc`
    /// crate defines it (FreeBSD: 32, AIX: 20), 40 on Linux and Android (the
    /// kernel's `MAXSYMLINKS`), and 40 elsewhere (NetBSD, OpenBSD,
    /// `DragonFly`, illumos, Solaris, Apple systems, …), a fallback not
    /// verified there that may differ from the system's own limit. The
    /// system counts the links of the whole path; this counts only those of
    /// its last part.
    ///
    /// # Errors
    ///
    /// When one more link follows (`ELOOP`, as reading reports), the folder
    /// cannot be written or the rename fails, and `path` is then unchanged;
    /// or when syncing the folder after the rename fails, and `path`
    /// already holds the new contents.
    pub fn write(&self, path: impl AsRef<Path>) -> io::Result<()> {
        write_atomic(path.as_ref(), &self.to_bytes())
    }
}

/// A group of a key file, found once by its name: looking up its keys
/// through it does not hash the name again, so that the work per key does
/// not grow with the group's name.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Group<'a> {
    file: &'a KeyFile,
    /// The number of its first block, which names it in the index.
    number: u64,
}

impl<'a> Group<'a> {
    /// Its keys with their locales, as [`KeyFile::keys`] gives them.
    pub(crate) fn keys(self) -> Vec<(&'a str, Option<&'a str>)> {
        self.file
            .index
            .keys
            .get(&self.number)
            .map(|keys| {
                keys.iter()
                    .map(|(key, locale)| (key.as_str(), locale.as_deref()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The lines of `key`, by locale.
    fn key_lines(self, key: &str) -> Option<&'a KeyLines> {
        // The key copied, hashed and compared.
        work::count(3 * key.len());
        self.file.index.entries.get(&(self.number, key.to_owned()))
    }

    /// The place of the line holding the value of `key[locale]`: the last
    /// one, as a repeated key's last value counts.
    fn find(self, key: &str, locale: Option<&str>) -> Option<Place> {
        let id = self.key_lines(key)?.find(locale)?;
        Some(self.file.index.place(id))
    }

    /// Whether `key[locale]` exists, checked without reading its value.
    /// Entry lines must be UTF-8, so this is true exactly when
    /// [`Group::raw`] returns a value.
    pub(crate) fn has(self, key: &str, locale: Option<&str>) -> bool {
        self.key_lines(key)
            .and_then(|lines| lines.find(locale))
            .is_some()
    }

    /// The value of `key[locale]`, as [`KeyFile::raw`] gives it.
    pub(crate) fn raw(self, key: &str, locale: Option<&str>) -> Option<&'a str> {
        self.find(key, locale)
            .and_then(|place| self.file.line(place).value())
    }

    /// The line of `key[locale]`, as [`KeyFile::line_of`] gives it.
    pub(crate) fn line_of(self, key: &str, locale: Option<&str>) -> Option<usize> {
        self.find(key, locale)
            .map(|place| self.file.line_number(place))
    }
}

/// Drops the removed lines of block `number`, moving the others down and
/// noting the new places of its entries in `index`, by their ids.
fn compact(block: &mut Block, number: u64, index: &mut Index) {
    let lines = std::mem::take(&mut block.lines);
    block.live = Counts::default();
    // The new place of each line, `None` for a removed one.
    let mut places = Vec::with_capacity(lines.len());
    for (at, line) in lines.into_iter().enumerate() {
        work::count(1);
        if line.kind == Kind::Removed {
            places.push(None);
            continue;
        }
        let to = block.lines.len();
        places.push(Some(to));
        if let Kind::Entry { id, .. } = line.kind
            && to != at
        {
            index.set_place(id, (number, to));
        }
        block.push(line);
    }
    block.problems = std::mem::take(&mut block.problems)
        .into_iter()
        .filter_map(|(at, problem)| {
            work::count(1);
            places[at].map(|to| (to, problem))
        })
        .collect();
    block.removed = 0;
}

/// Reads one line (without its line feed): what it is, and what is wrong
/// with it.
fn classify(raw: &[u8], key_names: KeyNames) -> Line {
    let line = |kind, problem: Option<String>| {
        // The line copied, and its problem's message written.
        work::count(raw.len() + problem.as_ref().map_or(0, String::len));
        Line {
            raw: raw.to_vec(),
            kind,
            problem,
        }
    };
    let content = raw.strip_suffix(b"\r").unwrap_or(raw);
    // A comment is uninterpreted and may hold any bytes, as the Desktop
    // Entry spec says, so it is found before the line is checked for UTF-8,
    // with a step per byte looked at.
    let blanks = content
        .iter()
        .take_while(|&&byte| byte == b' ' || byte == b'\t')
        .count();
    work::count(blanks + 1);
    if content.get(blanks) == Some(&b'#') {
        return line(Kind::Comment, None);
    }
    // A step per byte checked for UTF-8, and per character looked at to
    // find the line's parts (the spaces trimmed, the `=`).
    work::count(2 * content.len());
    let Ok(text) = std::str::from_utf8(content) else {
        return line(Kind::Invalid, Some("line is not UTF-8; ignored".to_owned()));
    };
    let trimmed = &text[blanks..];
    if trimmed.is_empty() {
        return line(Kind::Blank, None);
    }
    if let Some(rest) = trimmed.strip_prefix('[') {
        let Some(name) = rest.trim_end_matches([' ', '\t']).strip_suffix(']') else {
            return line(
                Kind::Invalid,
                Some("group header without a closing ]; ignored".to_owned()),
            );
        };
        let problem = (!valid_group_name(name)).then(|| {
            format!(
                "group name [{}] has characters a group name may not have",
                quoted(name)
            )
        });
        work::count(name.len());
        return line(Kind::Group(name.to_owned()), problem);
    }
    let Some(eq) = trimmed.find('=') else {
        return line(
            Kind::Invalid,
            Some("line is not a comment, a group header or an entry; ignored".to_owned()),
        );
    };
    let key_text = trimmed[..eq].trim_end_matches([' ', '\t']);
    let (key, locale) = split_key(key_text);
    let problem = (!key_names.allows(key)).then(|| {
        format!(
            "key \"{}\" has characters a key name may not have",
            quoted(key_text)
        )
    });
    let value = trimmed[eq + 1..].trim_start_matches([' ', '\t']);
    work::count(key_text.len());
    line(
        Kind::Entry {
            key: key.to_owned(),
            locale: locale.map(str::to_owned),
            value_start: text.len() - value.len(),
            // Given when the file is indexed.
            id: 0,
        },
        problem,
    )
}

impl Line {
    /// The line `text` of a new entry `key[locale]` with the id `id`.
    fn entry(text: &str, key: &str, locale: Option<&str>, id: u64) -> Self {
        let value_start = text.find('=').map_or(text.len(), |eq| eq + 1);
        // Looked through for the `=`, then the line, key and locale copied.
        work::count(value_start + text.len() + key.len() + locale.map_or(0, str::len));
        Self {
            raw: text.as_bytes().to_vec(),
            kind: Kind::Entry {
                key: key.to_owned(),
                locale: locale.map(str::to_owned),
                value_start,
                id,
            },
            problem: None,
        }
    }
}

/// Most links followed when resolving the path to write: the system's
/// `MAXSYMLINKS`, which the `libc` crate defines for FreeBSD (32) and AIX
/// (20).
#[cfg(any(target_os = "freebsd", target_os = "aix"))]
const MAX_LINKS: usize = libc::MAXSYMLINKS.unsigned_abs() as usize;

/// Most links followed when resolving the path to write: the Linux kernel's
/// `MAXSYMLINKS` (`fs/namei.c`), which the `libc` crate does not define for
/// Linux or Android.
#[cfg(any(target_os = "linux", target_os = "android"))]
const MAX_LINKS: usize = 40;

/// Most links followed when resolving the path to write, where the `libc`
/// crate defines no `MAXSYMLINKS` (NetBSD, OpenBSD, `DragonFly`, illumos,
/// Solaris, Apple systems, …): 40, as on Linux, a fallback not verified on
/// those systems that may differ from their own limit.
#[cfg(not(any(
    target_os = "freebsd",
    target_os = "aix",
    target_os = "linux",
    target_os = "android"
)))]
const MAX_LINKS: usize = 40;

/// The file a write to `path` replaces: `path`, or the target its symbolic
/// links lead to, whether or not that exists yet. Up to [`MAX_LINKS`]
/// links are followed (the system's limit: `MAXSYMLINKS` where the `libc`
/// crate defines it, 40 on Linux and Android, else an unverified fallback
/// of 40); one more fails with `ELOOP`, as reading does. The system counts
/// the links of the whole path; these are only the links of its last part,
/// one after another (the folders on the way are left to the system).
fn write_target(path: &Path) -> io::Result<PathBuf> {
    let mut current = path.to_owned();
    let mut followed = 0;
    loop {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                if followed == MAX_LINKS {
                    return Err(rustix::io::Errno::LOOP.into());
                }
                followed += 1;
                let link = fs::read_link(&current)?;
                current = match current.parent() {
                    Some(dir) if link.is_relative() => dir.join(link),
                    _ => link,
                };
            }
            _ => return Ok(current),
        }
    }
}

/// Writes `bytes` to `path` through a temporary file in the same folder.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let target = write_target(path)?;
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?
        .to_string_lossy()
        .into_owned();
    // Short enough that the temporary name stays within NAME_MAX.
    let mut end = name.len().min(64);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    let stem = &name[..end];
    let dir = target
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mode = match fs::metadata(&target) {
        Ok(metadata) => metadata.permissions().mode() & 0o7777,
        Err(err) if err.kind() == io::ErrorKind::NotFound => 0o644,
        Err(err) => return Err(err),
    };
    let (temp, mut file) = loop {
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp = dir.join(format!(".{stem}.{}.{count}.tmp", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temp)
        {
            Ok(file) => break (temp, file),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    };
    let written = (|| {
        file.write_all(bytes)?;
        // The umask may have narrowed the mode at creation.
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.sync_all()?;
        fs::rename(&temp, &target)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp);
        return written;
    }
    // The rename itself is made durable by syncing the folder.
    fs::File::open(dir)?.sync_all()
}

/// Steps of work, counted in tests so that they can show the work grows
/// linearly with the input. A step is an item visited (a line, entry,
/// block, group, key, list item or argument) or a character looked at.
/// Outside tests counting does nothing, unless the hidden `__count-work`
/// feature is on: it is not public API, only for the workspace's own tests
/// (`xdg-kit-mime-apps` turns it on in its dev-dependencies, so that its
/// counted tests count the work done in here too, on the same counter; see
/// the hidden `__count_work` module of this crate).
///
/// The counts can only show what they are told: on a path a counted test
/// runs, every step that grows with the input counts, each item a loop
/// visits included (each repeat check, each line a lookup visits, each
/// line looked at to find where a new one goes), and each character a
/// scan of a value or line looks at, whether the loop is written here or
/// is a library call such as `find` or `from_utf8` (counted by the bytes
/// it may walk). So does each character of input text copied (`to_owned`,
/// `clone`, `format!`), hashed (a set or map key inserted or looked up:
/// hashed and compared, two steps per byte) or compared with other input,
/// and each byte of a message that input is written into. Comparing with a
/// fixed name (a literal or an entry of a fixed table) is bounded by that
/// name and not counted; nor is file I/O. `validate`'s final sort of its
/// issues is n log n in the number of issues and not counted. A new loop
/// over input on such a path must count its steps too.
///
/// A counted test runs at two sizes with `assert_linear`: the work at
/// twice the size may be at most 2.5 times the work at the first (linear
/// work about doubles, quadratic work about quadruples), and each count
/// stays within a bound of about twice the analysed cost.
pub(crate) mod work {
    #[cfg(any(test, feature = "__count-work"))]
    thread_local! {
        #[allow(
            clippy::missing_const_for_thread_local,
            reason = "already const; clippy flags it on targets without native thread locals (Android, OpenBSD, illumos)"
        )]
        static STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// Counts `steps` steps on this thread.
    #[inline]
    pub(crate) fn count(steps: usize) {
        #[cfg(any(test, feature = "__count-work"))]
        STEPS.with(|done| done.set(done.get() + steps));
        #[cfg(not(any(test, feature = "__count-work")))]
        let _ = steps;
    }

    /// Counts the steps `steps` works out, calling it only when counting,
    /// so that working them out costs nothing otherwise.
    #[inline]
    pub(crate) fn count_with(steps: impl FnOnce() -> usize) {
        #[cfg(any(test, feature = "__count-work"))]
        count(steps());
        #[cfg(not(any(test, feature = "__count-work")))]
        let _ = steps;
    }

    /// The steps counted on this thread so far.
    #[cfg(any(test, feature = "__count-work"))]
    pub(crate) fn total() -> usize {
        STEPS.with(std::cell::Cell::get)
    }

    /// What `f` returns, and the steps it counted on this thread.
    #[cfg(test)]
    pub(crate) fn steps<T>(f: impl FnOnce() -> T) -> (T, usize) {
        let before = total();
        let value = f();
        (value, total() - before)
    }

    /// Runs `run` at `n` items and at `2 * n`; it returns the steps
    /// counted and the bytes of input. Asserts that the work at `2 * n` is
    /// at most 2.5 times that at `n` (linear work about doubles, quadratic
    /// work about quadruples), then that each count is within `bound` of
    /// its items and bytes.
    #[cfg(test)]
    #[track_caller]
    pub(crate) fn assert_linear(
        n: usize,
        run: impl Fn(usize) -> (usize, usize),
        bound: impl Fn(usize, usize) -> usize,
    ) {
        let (single, single_bytes) = run(n);
        let (doubled, doubled_bytes) = run(2 * n);
        let (single_bound, doubled_bound) = (bound(n, single_bytes), bound(2 * n, doubled_bytes));
        assert!(
            2 * doubled <= 5 * single,
            "{doubled} steps for {} items against {single} for {n}: more than 2.5 times \
             (bounds {doubled_bound}, {single_bound})",
            2 * n
        );
        for (items, steps, bytes, bound) in [
            (n, single, single_bytes, single_bound),
            (2 * n, doubled, doubled_bytes, doubled_bound),
        ] {
            assert!(
                steps <= bound,
                "{steps} steps for {items} items, {bytes} bytes: more than the bound {bound} \
                 (n = {n})"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# A comment\n\
        [Desktop Entry]\n\
        Type=Application\n\
        Name = Foo Viewer\n\
        Name[de]=Foo-Betrachter\n\
        Keywords=view;\\;semi;back\\\\slash;\n\
        \n\
        [Desktop Action New]\n\
        Name=New Window\n";

    #[test]
    #[should_panic(expected = "the key file's entry ids are exhausted")]
    fn running_out_of_entry_ids_panics() {
        let mut file = KeyFile::parse(b"[G]\n");
        file.index.next_id = u64::MAX;
        let _ = file.set_string("G", "k", "v");
    }

    #[test]
    #[should_panic(expected = "the key file's block numbers are exhausted")]
    fn running_out_of_block_numbers_panics() {
        let mut file = KeyFile::parse(b"[G]\n");
        file.next_number = u64::MAX;
        let _ = file.add_group("H");
    }

    #[test]
    #[should_panic(expected = "the key file's list numbers are exhausted")]
    fn running_out_of_list_numbers_panics() {
        let mut listed = Listed::<String> {
            next: u64::MAX,
            ..Listed::default()
        };
        listed.insert("a".to_owned());
    }

    #[test]
    fn an_unchanged_file_is_written_back_byte_for_byte() {
        for text in [
            SAMPLE,
            "no final newline\n[G]\nk=v",
            "",
            "\n\n",
            "[G]\r\nk=v\r\n",
        ] {
            assert_eq!(
                KeyFile::parse(text.as_bytes()).to_bytes(),
                text.as_bytes(),
                "{text:?}"
            );
        }
        let crlf = KeyFile::parse(b"[G]\r\nk=v\r\n");
        assert_eq!(crlf.raw("G", "k", None), Some("v"));
        let mut bytes = b"[G]\nk=caf\xc3\xa9\n\xff\xfe\n".to_vec();
        bytes.extend_from_slice(b"x=1\n");
        let file = KeyFile::parse(&bytes);
        assert_eq!(file.to_bytes(), bytes);
        assert_eq!(file.diagnostics().len(), 1, "{:?}", file.diagnostics());
    }

    #[test]
    fn values_strings_locales_and_lists() {
        let file = KeyFile::parse(SAMPLE.as_bytes());
        assert!(file.diagnostics().is_empty(), "{:?}", file.diagnostics());
        assert_eq!(file.groups(), ["Desktop Entry", "Desktop Action New"]);
        assert_eq!(file.raw("Desktop Entry", "Name", None), Some("Foo Viewer"));
        let german = Locales::new(["de_AT"]);
        assert_eq!(
            file.locale_string("Desktop Entry", "Name", &german)
                .as_deref(),
            Some("Foo-Betrachter")
        );
        assert_eq!(
            file.locale_string("Desktop Entry", "Name", &Locales::new(["fr"]))
                .as_deref(),
            Some("Foo Viewer")
        );
        assert_eq!(
            file.list("Desktop Entry", "Keywords").unwrap(),
            ["view", ";semi", "back\\slash"]
        );
        assert_eq!(file.line_of("Desktop Action New", "Name", None), Some(9));
        let encoded = KeyFile::parse(
            "[G]\nName=Foo\nName[de_DE.UTF-8]=Deutsch\nName[sr.UTF-8@latin]=Srpski\n".as_bytes(),
        );
        assert_eq!(
            encoded
                .locale_string("G", "Name", &Locales::new(["de_DE.UTF-8"]))
                .as_deref(),
            Some("Deutsch"),
            "a postfix's encoding is ignored"
        );
        assert_eq!(
            encoded
                .locale_string("G", "Name", &Locales::new(["sr_RS@latin"]))
                .as_deref(),
            Some("Srpski")
        );
        assert_eq!(file.raw("Desktop Entry", "Missing", None), None);
    }

    #[test]
    fn escapes_round_trip() {
        assert_eq!(unescape(r"a\sb\nc\td\re\\f\x\"), "a b\nc\td\re\\f\\x\\");
        assert_eq!(escape(" a\\b\nc;d", false), "\\sa\\\\b\\nc;d");
        assert_eq!(escape("a;b", true), "a\\;b");
        assert_eq!(split_list(""), Vec::<String>::new());
        assert_eq!(split_list("a;;b"), ["a", "", "b"]);
        assert_eq!(split_list("a"), ["a"]);
        for value in [" lead", "x\\y;z", "tab\there"] {
            assert_eq!(split_list(&format!("{};", escape(value, true))), [value]);
            assert_eq!(unescape(&escape(value, false)), value);
        }
    }

    #[test]
    fn repeated_groups_join_and_the_last_repeated_key_counts() {
        let file = KeyFile::parse(b"[A]\nk=1\n[B]\nx=0\n[A]\nk=2\nj=3\n");
        assert_eq!(file.groups(), ["A", "B"]);
        assert_eq!(file.raw("A", "k", None), Some("2"));
        assert_eq!(file.raw("A", "j", None), Some("3"));
        assert_eq!(file.keys("A"), [("k", None), ("j", None)]);
        assert_eq!(file.diagnostics().len(), 2, "{:?}", file.diagnostics());
    }

    #[test]
    fn odd_lines_are_kept_and_reported() {
        let text = "k=before\n  # indented comment\n[G\n[Good]\njunk\nbad key=1\n  spaced = v \n";
        let file = KeyFile::parse(text.as_bytes());
        assert_eq!(file.to_bytes(), text.as_bytes());
        assert_eq!(file.raw("Good", "spaced", None), Some("v "));
        assert_eq!(file.raw("Good", "bad key", None), Some("1"));
        let messages: Vec<_> = file.diagnostics().iter().map(|d| d.line).collect();
        assert_eq!(messages, [Some(1), Some(3), Some(5), Some(6)]);
    }

    #[test]
    fn a_comment_that_is_not_utf8_is_a_comment() {
        // Latin-1 and stray bytes, indented, with a carriage return, and on
        // a last line without a line feed.
        let text: &[u8] = b"# caf\xe9\n[A]\n \t#\xff\xfe\r\nk=v\n#\n# end \xe9";
        let mut file = KeyFile::parse(text);
        assert_eq!(file.to_bytes(), text);
        assert!(file.diagnostics().is_empty(), "{:?}", file.diagnostics());
        assert_eq!(file.raw("A", "k", None), Some("v"));
        assert_eq!(file.comments_not_utf8(), [1, 3, 6]);
        // Edits keep their bytes, and their lines follow.
        file.set_string("A", "n", "1").unwrap();
        assert!(file.remove("A", "k", None));
        file.set_string("B", "k", "v").unwrap();
        assert_eq!(
            file.to_bytes(),
            b"# caf\xe9\n[A]\n \t#\xff\xfe\r\nn=1\n#\n# end \xe9\n\n[B]\nk=v"
        );
        assert_eq!(file.comments_not_utf8(), [1, 3, 6]);
        assert!(file.remove_group("A"));
        assert_eq!(file.comments_not_utf8(), [1]);
        assert_like_reparse(&file);
        // A line that is not UTF-8 and not a comment is still reported.
        let file = KeyFile::parse(b"[A]\nk=caf\xe9\n");
        assert_eq!(file.diagnostics().len(), 1, "{:?}", file.diagnostics());
        assert!(file.comments_not_utf8().is_empty());
    }

    #[test]
    fn edits_touch_only_their_lines() {
        let mut file = KeyFile::parse(SAMPLE.as_bytes());
        file.set_string("Desktop Entry", "Name", "Bar").unwrap();
        file.set_locale_string("Desktop Entry", "Name", "fr", "Barre")
            .unwrap();
        file.set_boolean("Desktop Entry", "Terminal", false)
            .unwrap();
        file.set_list("Desktop Entry", "Categories", &["Utility", "a;b"])
            .unwrap();
        file.set_string("X-New", "Key", " lead").unwrap();
        assert!(file.remove("Desktop Action New", "Name", None));
        assert!(!file.remove("Desktop Action New", "Name", None));
        let expected = "# A comment\n\
            [Desktop Entry]\n\
            Type=Application\n\
            Name = Bar\n\
            Name[de]=Foo-Betrachter\n\
            Keywords=view;\\;semi;back\\\\slash;\n\
            Name[fr]=Barre\n\
            Terminal=false\n\
            Categories=Utility;a\\;b;\n\
            \n\
            [Desktop Action New]\n\
            \n\
            [X-New]\n\
            Key=\\slead\n";
        assert_eq!(String::from_utf8(file.to_bytes()).unwrap(), expected);
        assert!(file.remove_group("Desktop Action New"));
        assert_eq!(file.groups(), ["Desktop Entry", "X-New"]);
        let mut empty = KeyFile::new();
        empty.set_string("Desktop Entry", "Type", "Link").unwrap();
        assert_eq!(empty.to_bytes(), b"[Desktop Entry]\nType=Link\n");
    }

    #[test]
    fn edits_that_would_change_the_structure_are_refused() {
        let mut file = KeyFile::parse(SAMPLE.as_bytes());
        let before = file.to_bytes();
        assert_eq!(
            file.set_raw("Desktop Entry", "Exec", None, "x\n[Evil]\nk=v"),
            Err(EditError::Value)
        );
        // Reading would drop these, so the value would change on the way.
        for raw in [" x", "\tx"] {
            assert_eq!(
                file.set_raw("Desktop Entry", "Name", Some("fr_CA"), raw),
                Err(EditError::Value)
            );
        }
        assert_eq!(
            file.add_group("A]\n[B"),
            Err(EditError::GroupName("A]\n[B".into()))
        );
        assert_eq!(
            file.set_locale_string("Desktop Entry", "Name", "de]=x", "y"),
            Err(EditError::Locale("de]=x".into()))
        );
        assert_eq!(
            file.set_string("Desktop Entry", "Bad Key", "v"),
            Err(EditError::KeyName("Bad Key".into()))
        );
        assert_eq!(file.to_bytes(), before, "nothing changed");
        file.set_string("Desktop Entry", "Exec", "x\n[Evil]")
            .unwrap();
        assert_eq!(
            KeyFile::parse(&file.to_bytes()).groups(),
            file.groups(),
            "escaped"
        );
    }

    #[test]
    fn diagnostics_follow_the_text_after_edits() {
        let mut file = KeyFile::parse(b"[A]\nk=1\nk=2\n");
        assert_eq!(file.diagnostics().len(), 1);
        assert!(file.remove("A", "k", None));
        file.set_string("A", "k", "fixed").unwrap();
        assert!(file.diagnostics().is_empty(), "{:?}", file.diagnostics());
        let text = KeyFile::parse(&file.to_bytes());
        assert_eq!(text, file, "equal when the text is");
    }

    /// The edited file answers as a fresh parse of its text does.
    fn assert_like_reparse(file: &KeyFile) {
        let fresh = KeyFile::parse_with(&file.to_bytes(), file.key_names());
        assert_eq!(file.groups(), fresh.groups());
        for group in fresh.groups() {
            assert_eq!(file.group_line(group), fresh.group_line(group), "[{group}]");
            assert_eq!(file.keys(group), fresh.keys(group), "[{group}]");
            for (key, locale) in fresh.keys(group) {
                assert_eq!(file.raw(group, key, locale), fresh.raw(group, key, locale));
                assert_eq!(
                    file.line_of(group, key, locale),
                    fresh.line_of(group, key, locale)
                );
            }
        }
        assert_eq!(file.diagnostics(), fresh.diagnostics());
    }

    #[test]
    fn the_index_follows_every_kind_of_edit() {
        let mut file =
            KeyFile::parse(b"k=early\n[A]\nx=1\nbad key=2\n[B]\ny=1\n[A]\nx=3\n\xff\n# end\n");
        assert_like_reparse(&file);
        file.set_string("A", "new", "v").unwrap();
        assert_like_reparse(&file);
        file.set_locale_string("A", "new", "de", "w").unwrap();
        assert_like_reparse(&file);
        file.set_string("B", "x", "z").unwrap();
        assert_like_reparse(&file);
        file.set_string("C", "c", "1").unwrap();
        assert_like_reparse(&file);
        assert!(file.remove("A", "x", None));
        assert_like_reparse(&file);
        file.add_group("D").unwrap();
        assert_like_reparse(&file);
        assert!(file.remove_group("B"));
        assert_like_reparse(&file);
        file.set_string("A", "x", "back").unwrap();
        assert_like_reparse(&file);
    }

    #[test]
    fn removing_keys_under_a_repeated_header_keeps_the_index() {
        // The block of the second `[A]` is not the group's first, which
        // names it in the index: compacting it must look its entries up
        // under the first.
        let mut file = KeyFile::parse(b"[A]\nx=1\n[B]\n[A]\na=1\nb=2\nc=3\nd=4\n");
        assert_like_reparse(&file);
        for key in ["a", "b", "c"] {
            assert!(file.remove("A", key, None));
            assert_like_reparse(&file);
        }
        assert_eq!(file.raw("A", "d", None), Some("4"));
        assert_eq!(file.line_of("A", "d", None), Some(5));
        assert!(file.remove("A", "d", None));
        assert_like_reparse(&file);
        // And so must moving its last entry down into a removed line.
        let mut file = KeyFile::parse(b"[A]\nx=1\n[B]\n[A]\na=1\nb=2\n# end\n");
        assert!(file.remove("A", "a", None));
        assert_eq!(file.raw("A", "b", None), Some("2"));
        assert_eq!(file.line_of("A", "b", None), Some(5));
        assert_like_reparse(&file);
    }

    #[test]
    fn removing_a_locale_that_is_not_there_changes_nothing() {
        // Under a repeated header, beside the key's other locales:
        // `de_DE.ISO` is neither `de_DE` nor `de_DE.UTF-8`.
        let mut file =
            KeyFile::parse(b"[A]\nx=1\n[B]\n[A]\nName=a\nName[de_DE.UTF-8]=b\nName[de_DE]=c\n");
        let before = file.clone();
        for locale in ["fr", "de_DE.ISO"] {
            assert!(!file.remove("A", "Name", Some(locale)), "{locale}");
            assert_eq!(file.to_bytes(), before.to_bytes(), "{locale}");
            assert_eq!(file.keys("A"), before.keys("A"), "{locale}");
        }
        assert_like_reparse(&file);
    }

    #[test]
    fn keys_of_an_added_group_can_be_removed() {
        let mut file = KeyFile::new();
        file.set_string("A", "k", "v").unwrap();
        assert!(file.remove("A", "k", None));
        assert_eq!(file, KeyFile::parse(&file.to_bytes()));
        assert_like_reparse(&file);
        // The added group's block names it in the index too: its last
        // entry moves down into a removed line, then the block is
        // compacted.
        for key in ["a", "b", "c", "d"] {
            file.set_string("A", key, "1").unwrap();
        }
        assert!(file.remove("A", "c", None));
        assert_eq!(file.line_of("A", "d", None), Some(4));
        assert_like_reparse(&file);
        assert!(file.remove("A", "a", None));
        assert!(file.remove("A", "b", None));
        assert_eq!(file.raw("A", "d", None), Some("1"));
        assert_eq!(file.line_of("A", "d", None), Some(2));
        assert_eq!(file, KeyFile::parse(&file.to_bytes()));
        assert_like_reparse(&file);
    }

    #[test]
    fn a_repeated_keys_problem_names_it_with_its_locale() {
        let file = KeyFile::parse(b"[A]\nName=a\nName[de]=b\nName[de]=c\nName=d\n");
        let found: Vec<_> = file
            .diagnostics()
            .iter()
            .map(|found| (found.line, found.message.as_str()))
            .collect();
        assert_eq!(
            found,
            [
                (
                    Some(4),
                    "key Name[de] appears again in this group; the last value counts"
                ),
                (
                    Some(5),
                    "key Name appears again in this group; the last value counts"
                ),
            ]
        );
    }

    #[test]
    fn names_in_problems_are_shown_escaped_and_cut() {
        let long = "a".repeat(5000);
        let text = format!(
            "[A\x1b]\nk\x1by=1\nk\x1by=2\nName[{long}]=1\nName[{long}]=2\n[A\x1b]\n\
             [{long}]\n[{long}]\n[{long}\x1b\n{long}=1\n"
        );
        let file = KeyFile::parse(text.as_bytes());
        let found: Vec<_> = file
            .diagnostics()
            .iter()
            .map(|found| found.message.as_str())
            .collect();
        assert_eq!(found.len(), 9, "{found:?}");
        for message in &found {
            crate::error::assert_shown_safely(message);
        }
        assert_eq!(
            found[0],
            "group name [A\\u{1b}] has characters a group name may not have"
        );
        assert_eq!(
            found[3],
            "key k\\u{1b}y appears again in this group; the last value counts"
        );
        let cut = format!("key Name[{}…] appears again", "a".repeat(32));
        assert!(found[4].starts_with(&cut), "{}", found[4]);
        let mut edited = KeyFile::new();
        let long = format!("{long}=]");
        for (group, key, locale) in [
            ("A]\n[\x1b", "k", None),
            (&*long, "k", None),
            ("A", "k\x1b", None),
            ("A", &*long, None),
            ("A", "k", Some("\x1b")),
            ("A", "k", Some(&*long)),
        ] {
            let refused = edited.set_raw(group, key, locale, "v").unwrap_err();
            crate::error::assert_shown_safely(&refused.to_string());
        }
    }

    #[test]
    fn other_key_files_take_any_key_names() {
        let mut file = KeyFile::parse_with(
            b"[Default Applications]\nimage/svg+xml=a.desktop;\n",
            KeyNames::Any,
        );
        assert!(file.diagnostics().is_empty(), "{:?}", file.diagnostics());
        assert_eq!(
            file.raw("Default Applications", "image/svg+xml", None),
            Some("a.desktop;")
        );
        file.set_raw(
            "Default Applications",
            "x-scheme-handler/https",
            None,
            "b.desktop;",
        )
        .unwrap();
        assert_like_reparse(&file);
        for bad in ["", "a=b", "a[b", " a", "a\t", "#a", "a\u{7}"] {
            assert_eq!(
                file.set_raw("G", bad, None, "x"),
                Err(EditError::KeyName(bad.into())),
                "{bad:?}"
            );
        }
        // Desktop entries keep their own rule.
        assert!(
            !KeyFile::parse(b"[A]\nimage/png=x\n")
                .diagnostics()
                .is_empty()
        );
        assert_eq!(
            KeyFile::new().set_raw("A", "image/png", None, "x"),
            Err(EditError::KeyName("image/png".into()))
        );
    }

    #[test]
    fn new_lines_in_a_crlf_file_end_in_crlf() {
        let mut file = KeyFile::parse(b"[A]\r\nk=v\r\n");
        file.set_string("A", "n", "1").unwrap();
        file.set_string("B", "m", "2").unwrap();
        assert_eq!(file.to_bytes(), b"[A]\r\nk=v\r\nn=1\r\n\r\n[B]\r\nm=2\r\n");

        // Without a final line feed: the last line gets its line end when a
        // line goes after it.
        let mut file = KeyFile::parse(b"[A]\r\nk=v");
        file.set_string("A", "n", "1").unwrap();
        assert_eq!(file.to_bytes(), b"[A]\r\nk=v\r\nn=1");
        assert_like_reparse(&file);
        file.set_string("B", "m", "2").unwrap();
        assert_eq!(file.to_bytes(), b"[A]\r\nk=v\r\nn=1\r\n\r\n[B]\r\nm=2");
        assert_like_reparse(&file);
    }

    #[test]
    fn files_with_the_same_text_are_equal_however_they_were_edited() {
        let mut file = KeyFile::parse(b"[A]\n\nk=v");
        file.remove("A", "k", None);
        assert_eq!(file.to_bytes(), b"[A]\n");
        assert_eq!(file, KeyFile::parse(b"[A]\n"));

        let mut file = KeyFile::parse(b"[A]\nk=v");
        file.remove_group("A");
        assert_eq!(file, KeyFile::new());
        assert_eq!(file, KeyFile::parse(b""));

        let mut file = KeyFile::parse(b"x=1");
        file.set_string("A", "k", "v").unwrap();
        file.remove_group("A");
        assert_eq!(file, KeyFile::parse(&file.to_bytes()));

        assert_ne!(KeyFile::parse(b"[A]"), KeyFile::parse(b"[A]\n"));
        assert_ne!(KeyFile::parse(b"[A]\n"), KeyFile::parse(b"[A]\n\n"));
    }

    #[test]
    fn a_file_built_key_by_key_stays_fast() {
        work::assert_linear(
            5_000,
            |n| {
                // The group is followed by another, so new lines go in the
                // middle.
                let mut file = KeyFile::parse(b"[Desktop Entry]\n\n[Other]\nk=v\n");
                // The bytes of the names and values given.
                let mut bytes = 0;
                let ((), steps) = work::steps(|| {
                    for at in 0..n {
                        let key = format!("X-K{at}");
                        bytes += "Desktop Entry".len() + key.len() + 1;
                        file.set_string("Desktop Entry", &key, "v").unwrap();
                    }
                });
                assert_eq!(file.keys("Desktop Entry").len(), n);
                assert_like_reparse(&file);
                (steps, bytes)
            },
            // A step per line added (the blank line after the group stays),
            // with the group hashed and compared and the key copied, hashed
            // and compared for each lookup and index entry, and its line's
            // place and its number in the key list noted: about 189 per key;
            // and per character of a name or value checked or escaped: about
            // 1.5 per byte. Twice that.
            |keys, bytes| 378 * keys + 3 * bytes,
        );
    }

    #[test]
    fn many_translations_of_a_key_stay_fast() {
        work::assert_linear(
            5_000,
            |n| {
                let mut file = KeyFile::parse(b"[Desktop Entry]\nName=n\n");
                // The bytes of the names, locales and values given.
                let mut bytes = 0;
                let ((), steps) = work::steps(|| {
                    for at in 0..n {
                        let locale = format!("l{at}.UTF-8");
                        bytes += "Desktop EntryName".len() + locale.len() + 1;
                        file.set_locale_string("Desktop Entry", "Name", &locale, "v")
                            .unwrap();
                    }
                    for at in 0..n {
                        let locale = format!("l{at}");
                        bytes += 2 * "Desktop EntryName".len() + 2 * locale.len() + ".UTF-8".len();
                        assert_eq!(
                            file.raw("Desktop Entry", "Name", Some(&format!("{locale}.UTF-8"))),
                            Some("v")
                        );
                        assert_eq!(
                            file.locale_string("Desktop Entry", "Name", &Locales::new([locale]))
                                .as_deref(),
                            Some("v")
                        );
                    }
                });
                assert_eq!(file.keys("Desktop Entry").len(), n + 1);
                assert_like_reparse(&file);
                (steps, bytes)
            },
            // A step per line added and per line a lookup visits, with the
            // group hashed and compared and the key and locale copied, hashed
            // and compared for each lookup and index entry, the locale
            // without its encoding hashed and compared, and the line's place
            // noted and looked up by its id: about 473 per translation; and
            // per character of a name, locale or value checked, looked
            // through or unescaped: about 1 per byte. Twice that.
            |translations, bytes| 946 * translations + 2 * bytes,
        );
    }

    #[test]
    fn adding_keys_never_moves_a_groups_trailing_lines() {
        use std::fmt::Write as _;
        work::assert_linear(
            5_000,
            |n| {
                // As many trailing lines as keys added, so that moving them
                // for each key would grow quadratically.
                let mut text = String::from("[Desktop Entry]\nk=v\n");
                for at in 0..n {
                    let _ = writeln!(text, "# comment {at}\n\njunk {at}");
                }
                text.push_str("[Other]\nk=v\n");
                let mut file = KeyFile::parse(text.as_bytes());
                // The bytes of the names and values given.
                let mut bytes = 0;
                let ((), steps) = work::steps(|| {
                    for at in 0..n {
                        let key = format!("X-K{at}");
                        bytes += "Desktop Entry".len() + key.len() + 1;
                        file.set_string("Desktop Entry", &key, "v").unwrap();
                    }
                });
                assert_eq!(file.keys("Desktop Entry").len(), n + 1);
                assert_like_reparse(&file);
                (steps, bytes)
            },
            // A step per line added, with the group hashed and compared and
            // the key copied, hashed and compared for each lookup and index
            // entry, and its line's place and its number in the key list
            // noted: about 189 per key; and per character of a name or value
            // checked or escaped: about 1.5 per byte. Twice that.
            |keys, bytes| 378 * keys + 3 * bytes,
        );
    }

    #[test]
    fn removing_keys_spread_over_groups_stays_fast() {
        use std::fmt::Write as _;
        work::assert_linear(
            1_000,
            |n| {
                // Each group's last key is removed in even groups (its
                // comment then ends the group) and its first key in odd ones.
                let mut text = String::new();
                for at in 0..n {
                    let _ = write!(text, "[G{at}]\na=1\n# c\nb=2\n");
                }
                let mut file = KeyFile::parse(text.as_bytes());
                // The bytes of the names given.
                let mut bytes = 0;
                let ((), steps) = work::steps(|| {
                    for at in 0..n {
                        let group = format!("G{at}");
                        let key = if at % 2 == 0 { "b" } else { "a" };
                        bytes += group.len() + key.len();
                        assert!(file.remove(&group, key, None));
                    }
                });
                assert_like_reparse(&file);
                (steps, bytes)
            },
            // Per character of the names given: hashed and compared to find
            // the group, the key copied, hashed and compared to find and drop
            // its lines and its place in the key list; with a step per line
            // removed, moved or taken off the end, per block tidied and per
            // bit of the number of blocks for its count: about 5.4 per byte,
            // items included. Twice that.
            |_, bytes| 11 * bytes,
        );
    }

    #[test]
    fn removing_keys_and_numbering_lines_in_turn_stays_fast() {
        use std::fmt::Write as _;
        work::assert_linear(
            2_000,
            |n| {
                let mut text = String::from("[Desktop Entry]\n");
                for at in 0..=2 * n {
                    let _ = writeln!(text, "K{at}=v");
                }
                let mut file = KeyFile::parse(text.as_bytes());
                let last = format!("K{}", 2 * n);
                // The bytes of the names given.
                let mut bytes = 0;
                let ((), steps) = work::steps(|| {
                    // Every other key from the front, each followed by the
                    // line of the last one.
                    for at in 0..n {
                        let key = format!("K{}", 2 * at);
                        bytes += 2 * "Desktop Entry".len() + key.len() + last.len();
                        assert!(file.remove("Desktop Entry", &key, None));
                        assert_eq!(
                            file.line_of("Desktop Entry", &last, None),
                            Some(2 * n + 1 - at)
                        );
                    }
                });
                assert_like_reparse(&file);
                (steps, bytes)
            },
            // Per character of the names given: the group hashed and
            // compared to find it, the key copied, hashed and compared to
            // find and drop its lines and its place in the key list, and to
            // look the last key up; with a step per line removed, and per bit
            // of the line count for each line's count changed and each line
            // number worked out: about 3.5 per byte, items included. Twice
            // that.
            |_, bytes| 7 * bytes,
        );
    }

    #[test]
    fn editing_and_numbering_lines_across_many_groups_stays_fast() {
        use std::fmt::Write as _;
        work::assert_linear(
            2_000,
            |n| {
                let mut text = String::new();
                for at in 0..n {
                    let _ = write!(text, "[G{at}]\nk=v\n");
                }
                let mut file = KeyFile::parse(text.as_bytes());
                let last = format!("G{}", n - 1);
                // The bytes of the names given.
                let mut bytes = 0;
                let ((), steps) = work::steps(|| {
                    // A key added to and one removed from each group but the
                    // last, each followed by the lines of the last group.
                    for at in 0..n - 1 {
                        let group = format!("G{at}");
                        bytes += 2 * group.len() + 4 * last.len() + "nk".len() + 2;
                        file.set_string(&group, "n", "v").unwrap();
                        assert_eq!(file.group_line(&last), Some(2 * n));
                        assert!(file.remove(&group, "k", None));
                        assert_eq!(file.line_of(&last, "k", None), Some(2 * n));
                    }
                });
                assert_like_reparse(&file);
                (steps, bytes)
            },
            // Per character of the names given: the groups hashed and
            // compared to find them, the keys copied, hashed and compared to
            // add and drop their lines and places in the key lists and to
            // look the last key up; with a step per line added or removed,
            // and per bit of the number of blocks for each block's count
            // changed and each block's start worked out: about 4 per byte,
            // items included. Twice that.
            |_, bytes| 8 * bytes,
        );
    }

    #[test]
    fn removing_many_translations_of_a_key_stays_fast() {
        use std::fmt::Write as _;
        work::assert_linear(
            2_000,
            |n| {
                let mut text = String::from("[Desktop Entry]\nName=n\n");
                for at in 0..n {
                    let _ = writeln!(text, "Name[l{at}.UTF-8]=v");
                }
                text.push_str("[Other]\nk=v\n");
                let mut file = KeyFile::parse(text.as_bytes());
                // The bytes of the names and locales given.
                let mut bytes = 0;
                let ((), steps) = work::steps(|| {
                    // Half from the front, half from the back.
                    for at in (0..n / 2).chain((n / 2..n).rev()) {
                        let locale = format!("l{at}.UTF-8");
                        bytes += 2 * "Desktop EntryName".len() + 2 * locale.len();
                        assert!(file.remove("Desktop Entry", "Name", Some(&locale)));
                        assert_eq!(
                            file.locale_string(
                                "Desktop Entry",
                                "Name",
                                &Locales::new([format!("l{at}")])
                            )
                            .as_deref(),
                            Some("n")
                        );
                    }
                });
                assert_eq!(file.keys("Desktop Entry"), [("Name", None)]);
                assert_like_reparse(&file);
                (steps, bytes)
            },
            // Per character of the names and locales given: copied, hashed
            // and compared (the group only hashed and compared) to find the
            // translation, drop its line, its set and its place in the key
            // list, and to look the next one up; with a step per line
            // removed, moved down or compacted and per list entry dropped:
            // about 4.5 per byte. Twice that.
            |_, bytes| 9 * bytes,
        );
    }

    /// Removes `n` short keys, last first, from a group whose last entry,
    /// `last` for `n`, follows them: each removal moves that entry down
    /// into the line removed before it. Returns the steps and the bytes of
    /// the names given.
    fn remove_backwards_before(n: usize, last: &str) -> (usize, usize) {
        use std::fmt::Write as _;
        let mut text = String::from("[Desktop Entry]\n");
        for at in 0..n {
            let _ = writeln!(text, "K{at}=v");
        }
        let _ = writeln!(text, "{last}");
        let mut file = KeyFile::parse(text.as_bytes());
        // The bytes of the names given.
        let mut bytes = 0;
        let ((), steps) = work::steps(|| {
            for at in (0..n).rev() {
                let key = format!("K{at}");
                bytes += "Desktop Entry".len() + key.len();
                assert!(file.remove("Desktop Entry", &key, None));
            }
        });
        assert_eq!(
            file.to_bytes(),
            format!("[Desktop Entry]\n{last}\n").into_bytes()
        );
        assert_like_reparse(&file);
        (steps, bytes)
    }

    #[test]
    fn removing_keys_backwards_before_a_long_last_key_stays_fast() {
        work::assert_linear(
            2_000,
            // A last key as long as there are keys before it, so that work
            // per move that grew with the key would grow quadratically.
            |n| remove_backwards_before(n, &format!("{}=v", "L".repeat(n))),
            // Per character of the names given: the group hashed and
            // compared to find it, the key copied, hashed and compared to
            // find and drop its lines and its place in the key list; with a
            // step per line removed and moved down, per place noted by its
            // id, and per bit of the line count for each line's count
            // changed: about 4.2 per byte, items included. Twice that.
            |_, bytes| 9 * bytes,
        );
    }

    #[test]
    fn removing_keys_backwards_before_a_long_last_locale_stays_fast() {
        work::assert_linear(
            2_000,
            // A last locale as long as there are keys before it, with an
            // encoding, so that work per move that grew with the locale or
            // its form without the encoding would grow quadratically.
            |n| remove_backwards_before(n, &format!("Name[{}.UTF-8]=v", "l".repeat(n))),
            // As for a long last key: about 4.2 per byte. Twice that.
            |_, bytes| 9 * bytes,
        );
    }

    #[test]
    fn compacting_a_block_moves_a_long_key_in_constant_time() {
        use std::fmt::Write as _;
        work::assert_linear(
            2_000,
            |n| {
                // A key 32 times as long as there are keys before it, then
                // a short one, so that it never moves down into a removed
                // line but moves each time the block is compacted, log n
                // times: work per move that grew with the key would
                // overshoot the bound many times over.
                let long = "L".repeat(32 * n);
                let mut text = String::from("[Desktop Entry]\n");
                for at in 0..n {
                    let _ = writeln!(text, "K{at}=v");
                }
                let _ = writeln!(text, "{long}=v\nk=v");
                let mut file = KeyFile::parse(text.as_bytes());
                // The bytes of the names given.
                let mut bytes = 0;
                let ((), steps) = work::steps(|| {
                    for at in 0..n {
                        let key = format!("K{at}");
                        bytes += "Desktop Entry".len() + key.len();
                        assert!(file.remove("Desktop Entry", &key, None));
                    }
                });
                assert_eq!(
                    file.to_bytes(),
                    format!("[Desktop Entry]\n{long}=v\nk=v\n").into_bytes()
                );
                assert_like_reparse(&file);
                (steps, bytes)
            },
            // As for removing keys backwards, with a step per line moved when
            // the block is compacted: about 4.6 per byte. Twice that.
            |_, bytes| 10 * bytes,
        );
    }

    #[test]
    fn removing_groups_one_by_one_stays_fast() {
        use std::fmt::Write as _;
        work::assert_linear(
            1_000,
            |n| {
                let mut text = String::from("# head\n");
                for at in 0..n {
                    let _ = write!(text, "[G{at}]\nk=v\n\n");
                }
                let mut file = KeyFile::parse(text.as_bytes());
                // The bytes of the names given.
                let mut bytes = 0;
                let ((), steps) = work::steps(|| {
                    // Every other group from the front, then the rest from the
                    // back.
                    let order = (0..n).step_by(2).chain((1..n).step_by(2).rev());
                    for at in order {
                        let group = format!("G{at}");
                        bytes += group.len();
                        assert!(file.remove_group(&group));
                    }
                });
                assert!(file.groups().is_empty());
                assert_eq!(file.to_bytes(), b"# head\n");
                (steps, bytes)
            },
            // Per character of the names given: hashed and compared to find
            // the group's blocks and its place in the group list, and each
            // key copied, hashed and compared to drop its lines; with a step
            // per line, block and key dropped, and per bit of the number of
            // blocks for each block's count: about 9.3 per byte, items
            // included. Twice that.
            |_, bytes| 19 * bytes,
        );
    }

    #[test]
    fn removing_a_long_named_group_with_many_keys_stays_fast() {
        use std::fmt::Write as _;
        work::assert_linear(
            2_000,
            |n| {
                // A group name as long as the group has keys, its header
                // repeated, between two other groups, so that work per key
                // that grew with the name would grow quadratically.
                let group = "G".repeat(n);
                let mut text = format!("[A]\nk=v\n[{group}]\n");
                for at in 0..n {
                    if at == n / 2 {
                        let _ = writeln!(text, "[B]\nk=v\n[{group}]");
                    }
                    let _ = writeln!(text, "K{at}=v");
                }
                text.push_str("# end\n");
                let mut file = KeyFile::parse(text.as_bytes());
                // The text, and the name given.
                let bytes = text.len() + group.len();
                let (removed, steps) = work::steps(|| file.remove_group(&group));
                assert!(removed);
                assert_eq!(file.groups(), ["A", "B"]);
                assert_eq!(file.to_bytes(), b"[A]\nk=v\n[B]\nk=v\n");
                assert_like_reparse(&file);
                (steps, bytes)
            },
            // Per character of the name given: hashed and compared to find
            // the group's blocks and its place in the group list, and copied
            // for that; each key copied, hashed and compared once to drop its
            // lines; with a step per line and block dropped: about 2 per byte
            // of the text and the name, items included. Twice that.
            |_, bytes| 4 * bytes,
        );
    }

    #[test]
    fn adding_and_removing_groups_in_turn_stays_fast() {
        // A small file whose groups come and go: what the file holds stays
        // the same size, so the work of each round must too, however many
        // groups came before.
        // The blocks held, with their slots and line counts.
        let blocks_held = |file: &KeyFile| {
            let held = file.blocks.len();
            assert_eq!((file.slots.len(), file.sizes.tree.len()), (held, held));
            held
        };
        work::assert_linear(
            2_000,
            |n| {
                let mut file = KeyFile::parse(b"[Keep]\nk=v\n");
                // The bytes of the names given.
                let mut bytes = 0;
                let ((), steps) = work::steps(|| {
                    for at in 0..n {
                        let group = format!("G{at}");
                        bytes += group.len();
                        file.add_group(&group).unwrap();
                        if at > 0 {
                            let previous = format!("G{}", at - 1);
                            bytes += previous.len();
                            assert!(file.remove_group(&previous));
                        }
                        assert!(file.diagnostics().is_empty());
                        assert_eq!(
                            file.to_bytes().len(),
                            "[Keep]\nk=v\n\n[]\n".len() + group.len()
                        );
                    }
                });
                assert_like_reparse(&file);
                // The lines before the first group, `[Keep]` and the last
                // group are left: the blocks held stay within twice that.
                assert!(
                    blocks_held(&file) <= 2 * 3,
                    "{} blocks held for 3 after {n} rounds",
                    blocks_held(&file)
                );
                (steps, bytes)
            },
            // Per character of the names given: copied into the header,
            // checked, hashed and compared to add and drop the group, its
            // place in the group list and its blocks; with a step per line
            // and block added, dropped, written out or looked at for its
            // problems, per block kept when the dropped ones are let go, and
            // per bit of the number of blocks for each block's count: about
            // 13.9 per byte, items included. Twice that.
            |_, bytes| 28 * bytes,
        );
    }

    #[test]
    fn adding_and_removing_keys_in_turn_beside_a_long_key_stays_fast() {
        work::assert_linear(
            2_000,
            |n| {
                // A key as long as there are rounds, listed before the keys
                // that come and go, so that work per round that grew with
                // it would grow quadratically.
                let long = "L".repeat(n);
                let mut file = KeyFile::parse(format!("[Desktop Entry]\n{long}=v\n").as_bytes());
                // The bytes of the names and values given.
                let mut bytes = 0;
                let ((), steps) = work::steps(|| {
                    for at in 0..n {
                        let key = format!("K{at}");
                        bytes += 3 * "Desktop Entry".len() + 2 * key.len() + 1;
                        file.set_string("Desktop Entry", &key, "v").unwrap();
                        assert!(file.remove("Desktop Entry", &key, None));
                        // Read in every round, so that a key list whose
                        // gaps were never dropped would show.
                        assert_eq!(file.keys("Desktop Entry"), [(long.as_str(), None)]);
                    }
                });
                assert_like_reparse(&file);
                (steps, bytes)
            },
            // Per character of the names and values given: checked, copied,
            // hashed and compared to add the key and its line, to find it
            // and drop them, and to find the group whose keys are read; with
            // a step per line added, removed or taken off the end, per item
            // or gap of the key list when its gaps are dropped or it is
            // read, and per bit of the number of blocks for each block's
            // count: about 5.7 per byte, items included. Twice that.
            |_, bytes| 12 * bytes,
        );
    }

    #[test]
    fn adding_and_removing_groups_in_turn_beside_a_long_named_group_stays_fast() {
        work::assert_linear(
            2_000,
            |n| {
                // A group name as long as there are rounds, listed before
                // the groups that come and go, so that work per round that
                // grew with it would grow quadratically.
                let long = "G".repeat(n);
                let mut file = KeyFile::parse(format!("[{long}]\nk=v\n").as_bytes());
                // The bytes of the names given.
                let mut bytes = 0;
                let ((), steps) = work::steps(|| {
                    for at in 0..n {
                        let group = format!("G{at}");
                        bytes += 2 * group.len();
                        file.add_group(&group).unwrap();
                        assert!(file.remove_group(&group));
                        // Read in every round, so that a group list whose
                        // gaps were never dropped would show.
                        assert_eq!(file.groups(), [long.as_str()]);
                    }
                });
                assert_like_reparse(&file);
                (steps, bytes)
            },
            // Per character of the names given: checked, copied into the
            // header, hashed and compared to add and drop the group, its
            // place in the group list and its block; with a step per line
            // and block added or dropped, per item or gap of the group list
            // when its gaps are dropped or it is read, and per bit of the
            // number of blocks for each block's count: about 10 per byte,
            // items included. Twice that.
            |_, bytes| 20 * bytes,
        );
    }

    #[test]
    fn removed_groups_blocks_outnumbering_the_rest_are_let_go() {
        use std::fmt::Write as _;
        work::assert_linear(
            1_000,
            |n| {
                let mut text = String::from("# head\n");
                for at in 0..n {
                    let _ = write!(text, "[G{at}]\nk=v\n");
                }
                let mut file = KeyFile::parse(text.as_bytes());
                // How many times the removed groups' blocks were let go,
                // while groups came and went and while they were removed.
                let mut reclaims = [0, 0];
                // After a removal: when blocks were let go, only the lines
                // before the first group and the groups left are held.
                let mut check = |file: &KeyFile, before: usize, phase: usize| {
                    if file.blocks.len() < before && !file.groups().is_empty() {
                        reclaims[phase] += 1;
                        assert_eq!(file.dead, 0, "dead blocks after they were let go");
                        assert_eq!(file.blocks.len(), 1 + file.groups().len());
                    }
                };
                // The bytes of the names given.
                let mut bytes = 0;
                let ((), steps) = work::steps(|| {
                    // The oldest group removed and a new one added, 2n
                    // times: the removed groups' blocks are always before
                    // the rest, and are let go once they are n + 2.
                    for at in 0..2 * n {
                        let (old, new) = (format!("G{at}"), format!("G{}", n + at));
                        bytes += old.len() + new.len();
                        let before = file.blocks.len();
                        assert!(file.remove_group(&old));
                        check(&file, before, 0);
                        file.add_group(&new).unwrap();
                    }
                    // Then every group, front to back.
                    for at in 2 * n..3 * n {
                        let group = format!("G{at}");
                        bytes += group.len();
                        let before = file.blocks.len();
                        assert!(file.remove_group(&group));
                        check(&file, before, 1);
                    }
                });
                assert!(
                    reclaims[0] >= 1 && reclaims[1] >= 1,
                    "{reclaims:?} reclaims"
                );
                assert_eq!((file.dead, file.blocks.len()), (0, 1));
                assert_eq!(file.to_bytes(), b"# head\n");
                (steps, bytes)
            },
            // Per character of the names given: checked, copied into the
            // header, hashed and compared to add and drop the group, its
            // place in the group list and its blocks; with a step per line
            // and block added or dropped, per block kept when the dropped
            // ones are let go, and per bit of the number of blocks for each
            // block's count: about 11.2 per byte, items included. Twice that.
            |_, bytes| 23 * bytes,
        );
    }

    #[test]
    fn a_file_built_group_by_group_stays_fast() {
        work::assert_linear(
            2_000,
            |n| {
                let mut file = KeyFile::new();
                // The bytes of the names and values given.
                let mut bytes = 0;
                let ((), steps) = work::steps(|| {
                    for at in 0..n {
                        let group = format!("G{at}");
                        bytes += 2 * group.len() + "k".len() + 1;
                        file.add_group(&group).unwrap();
                        file.set_string(&group, "k", "v").unwrap();
                    }
                });
                assert_eq!(file.groups().len(), n);
                assert_like_reparse(&file);
                (steps, bytes)
            },
            // Per character of the names and values given: checked, copied
            // into the header, hashed and compared to add the group and to
            // find it, and the key copied, hashed and compared to add it;
            // with a step per line added, per block looked at for the last
            // line, and per bit of the number of blocks for each block's
            // count: about 11.2 per byte, items included. Twice that.
            |_, bytes| 23 * bytes,
        );
    }

    #[test]
    fn many_encodings_of_one_locale_stay_fast() {
        work::assert_linear(
            2_000,
            |n| {
                let mut file = KeyFile::parse(b"[Desktop Entry]\nName=n\n[Other]\nk=v\n");
                let german = Locales::new(["de_DE"]);
                // The bytes of the names and locales given.
                let mut bytes = 0;
                let ((), steps) = work::steps(|| {
                    // Each `de_DE.E{at}` is `de_DE` once its encoding is
                    // stripped: added, then found as the last one.
                    for at in 0..n {
                        let locale = format!("de_DE.E{at}");
                        let value = format!("v{at}");
                        bytes += 2 * "Desktop EntryName".len() + locale.len() + value.len() + 5;
                        file.set_locale_string("Desktop Entry", "Name", &locale, &value)
                            .unwrap();
                        assert_eq!(
                            file.locale_string("Desktop Entry", "Name", &german),
                            Some(value)
                        );
                    }
                    // Removed from the last, the one before then found.
                    for at in (0..n).rev() {
                        let locale = format!("de_DE.E{at}");
                        bytes += 2 * "Desktop EntryName".len() + locale.len() + 5;
                        assert!(file.remove("Desktop Entry", "Name", Some(&locale)));
                        let before = at.checked_sub(1).map(|at| format!("v{at}"));
                        assert_eq!(
                            file.locale_string("Desktop Entry", "Name", &german),
                            Some(before.unwrap_or_else(|| "n".to_owned()))
                        );
                    }
                });
                assert_eq!(file.keys("Desktop Entry"), [("Name", None)]);
                assert_like_reparse(&file);
                (steps, bytes)
            },
            // Per character of the names and locales given: checked, copied,
            // hashed and compared to add, find and drop each line, its place
            // among the variants of `de_DE` and in the key list; with a step
            // per line added, removed or moved, and per level of the variants
            // of `de_DE` (log n) for each one added, found or removed: about
            // 7.6 per byte, items and levels included (a little more at twice
            // the size, a level more). Twice that.
            |_, bytes| 16 * bytes,
        );
    }

    #[test]
    fn large_files_are_read_in_linear_time() {
        use std::fmt::Write as _;
        work::assert_linear(
            20_000,
            |n| {
                let mut text = String::from("[Desktop Entry]\n");
                for at in 0..n {
                    let _ = writeln!(text, "X-Key{at}=v");
                }
                // The text, and the names looked up.
                let mut bytes = text.len();
                let ((keys, found), steps) = work::steps(|| {
                    let file = KeyFile::parse(text.as_bytes());
                    let keys = file.keys("Desktop Entry").len();
                    let found = (0..n).filter(|at| {
                        let key = format!("X-Key{at}");
                        bytes += "Desktop Entry".len() + key.len();
                        file.raw("Desktop Entry", &key, None).is_some()
                    });
                    (keys, found.count())
                });
                assert_eq!((keys, found), (n, n));
                (steps, bytes)
            },
            // A step per line split off, read and indexed, per key listed
            // and per line a lookup visits, with the line copied, its key
            // copied, hashed and compared to index it and to look it up, its
            // place noted and looked up by its id, and the group hashed and
            // compared to look it up: about 159 per key; and per character
            // split off, checked for UTF-8, looked through for the line's
            // parts, or of a name looked up: about 2 per byte. Twice that.
            |keys, bytes| 318 * keys + 4 * bytes,
        );
    }

    #[test]
    fn cloning_counts_what_it_copies() {
        use std::fmt::Write as _;
        // The steps of a clone, against a figure worked out by hand from
        // what each file is made of (see `Clone for KeyFile`): one per
        // block, line, problem, line count and slot, per group, key, locale
        // and id in the index, per entry's place, per diagnostic, and per
        // byte of each of these, the key and group lists' items twice, with
        // their numbers and where they are. Each part shows in
        // at least one file, so that leaving it out changes a count.
        let steps = |file: &KeyFile| {
            let (copy, steps) = work::steps(|| file.clone());
            assert_eq!(&copy, file);
            steps
        };
        let messages = |file: &KeyFile| -> (usize, usize) {
            let found = file.diagnostics();
            let bytes = found.iter().map(|found| found.message.len()).sum();
            (found.len(), bytes)
        };

        // One group, each translation twice: a problem for each repeat.
        let mut text = String::from("[Desktop Entry]\n");
        let mut locales = 0;
        for at in 0..100 {
            let _ = write!(text, "Name[l{at}]=v\nName[l{at}]=w\n");
            locales += format!("l{at}").len();
        }
        let file = KeyFile::parse(text.as_bytes());
        let (problems, problem_bytes) = messages(&file);
        assert_eq!(problems, 100);
        let blocks = 2 // the blocks
            + text.len() // each line and its bytes
            + "Desktop Entry".len() + 2 * (100 * "Name".len() + locales) // names on lines
            + problems + problem_bytes // the block's problems
            + 201 // its line counts
            + 2 * 2; // slots and line counts of the blocks
        let index = 1 + (2 + 2 * 13) // the group list
            + (1 + 13 + 1) // the group's headers
            + (1 + 100 + 100 * (2 + 2 * 4) + 2 * locales) // its key list
            + (1 + 4 + 100 * (1 + 2) + locales) // the key's lines by locale
            + 200; // the entries' places
        let diagnostics = problems + problem_bytes;
        assert_eq!(steps(&file), blocks + index + diagnostics);

        // Many groups, each with a key and a blank line after it.
        let mut text = String::new();
        let mut names = 0;
        for at in 0..100 {
            let _ = write!(text, "[G{at}]\nk=v\n\n");
            names += format!("G{at}").len();
        }
        let file = KeyFile::parse(text.as_bytes());
        assert_eq!(messages(&file), (0, 0));
        let blocks = 101 // the blocks
            + text.len() // each line and its bytes
            + names + 100 // names on lines
            + 100 * 2 // their line counts: the header and the key
            + 2 * 101; // slots and line counts of the blocks
        let index = 100 + (2 * 100 + 2 * names) // the group list
            + (100 * 2 + names) // the groups' headers
            + 100 * (1 + 1 + (2 + 2)) // their key lists
            + 100 * (1 + 1 + (1 + 1)) // their keys' lines
            + 100; // the entries' places
        assert_eq!(steps(&file), blocks + index);

        // A file read from a path: an entry before the first group, a key
        // name with a space, a locale with an encoding, a line that is not
        // UTF-8 after the last entry, a group without entries and a
        // repeated one.
        let text: &[u8] = b"# head\nk=early\n[A]\nx=1\nName[de_DE.UTF-8]=a\nbad key=2\n\
            # c\n\xff\n[B]\n[A]\ny=1\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.desktop");
        fs::write(&path, text).unwrap();
        let path_bytes = path.as_os_str().len();
        let (plain, asked) = (KeyFile::read(&path).unwrap(), KeyFile::read(&path).unwrap());
        let (problems, problem_bytes) = messages(&asked);
        assert_eq!(problems, 4);
        // The problems of lines themselves: the key name and the bytes.
        let line_problems: usize = asked.diagnostics()[1..3]
            .iter()
            .map(|found| found.message.len())
            .sum();
        assert_eq!(asked.diagnostics()[1].line, Some(6));
        assert_eq!(asked.diagnostics()[2].line, Some(8));
        let blocks = 4 // the blocks
            + text.len() // each line and its bytes
            + 3 + (1 + 1 + (4 + 11) + 7 + 1) // names on lines
            + line_problems
            + problems + problem_bytes // the blocks' problems
            + (2 + 4 + 1 + 2) // their line counts, up to the last entry
            + 2 * 4 // slots and line counts of the blocks
            + path_bytes;
        let index = 2 + 2 * (2 + 2) // the group list
            + (1 + 1 + 2) + (1 + 1 + 1) // the groups' headers
            + (1 + 4 + (2 + 2) + (2 + 2 * 15) + (2 + 2 * 7) + (2 + 2)) // A's key list
            + (1 + 1 + 2) // x's lines
            + (1 + 4 + (1 + 11 + 1) + (1 + 5 + 1)) // Name's, by locale and postfix
            + (1 + 7 + 2) // bad key's
            + (1 + 1 + 2) // y's
            + 4; // the places of the entries in groups
        let diagnostics = problems + problem_bytes + problems * path_bytes;
        assert_eq!(steps(&plain), blocks + index);
        assert_eq!(steps(&asked), blocks + index + diagnostics);
    }

    #[test]
    fn removals_leave_nothing_behind_in_the_index() {
        // The steps of a clone, against a figure worked out by hand as in
        // `cloning_counts_what_it_copies`: they count the index, so what a
        // removal left in it would show.
        let steps = |file: &KeyFile| {
            let (copy, steps) = work::steps(|| file.clone());
            assert_eq!(&copy, file);
            steps
        };

        // A removed group leaves nothing of it in the index but a gap in
        // the group list; its block, the last, is let go.
        let mut file = KeyFile::parse(b"[A]\nk=v\n[B]\nx=1\n");
        assert!(file.remove_group("B"));
        let blocks = 2 // the blocks
            + "[A]\nk=v\n".len() // each line and its bytes
            + 1 + 1 // names on lines
            + 2 // its line counts
            + 2 * 2; // slots and line counts of the blocks
        let index = 2 + (2 + 2) // the group list, with B's gap
            + (1 + 1 + 1) // A's headers
            + (1 + 1 + (2 + 2)) // its key list
            + (1 + 1 + (1 + 1)) // k's lines
            + 1; // its place
        assert_eq!(steps(&file), blocks + index);

        // A key whose last locale is removed leaves nothing of it in the
        // index, nor an empty key list for its group; its line is taken
        // off the end of its block.
        let mut file = KeyFile::parse(b"[A]\nk=v\n[B]\nName[de]=x\n");
        assert!(file.remove("B", "Name", Some("de")));
        let blocks = 3 // the blocks
            + "[A]\nk=v\n[B]\n".len() // each line and its bytes
            + 1 + 1 + 1 // names on lines
            + 2 + 1 // their line counts
            + 2 * 3; // slots and line counts of the blocks
        let index = 2 + 2 * (2 + 2) // the group list
            + 2 * (1 + 1 + 1) // the groups' headers
            + (1 + 1 + (2 + 2)) // A's key list
            + (1 + 1 + (1 + 1)) // k's lines
            + 1; // its place
        assert_eq!(steps(&file), blocks + index);
    }

    #[test]
    fn many_keys_under_a_long_group_name_are_read_in_linear_time() {
        use std::fmt::Write as _;
        work::assert_linear(
            2_000,
            |n| {
                // A group name as long as the group has entries, and each key
                // twice, so that work per entry or message that grew with the
                // name would grow quadratically.
                let group = "G".repeat(n);
                let mut text = format!("[{group}]\n");
                for at in 0..n {
                    let _ = writeln!(text, "K{}=v", at / 2);
                }
                // The text, and the name looked up.
                let bytes = text.len() + group.len();
                let ((keys, problems), steps) = work::steps(|| {
                    let file = KeyFile::parse(text.as_bytes());
                    (file.keys(&group).len(), file.diagnostics().len())
                });
                assert_eq!((keys, problems), (n / 2, n / 2));
                (steps, bytes)
            },
            // A step per line split off, read and indexed, with its key
            // copied, hashed and compared to index it, and each repeat's
            // message written and then copied into the diagnostics: about
            // 139 per entry; and per character split off, checked for UTF-8
            // or looked through for the line's parts, and of the group name
            // copied and hashed once: about 2 per byte. Twice that.
            |entries, bytes| 278 * entries + 4 * bytes,
        );
    }

    #[test]
    fn many_locales_under_a_long_group_name_and_key_are_tried_in_linear_time() {
        work::assert_linear(
            2_000,
            |n| {
                // A group name and a key as long as there are locales to
                // try, and only the last locale translated, so that work per
                // locale that grew with the name or the key would grow
                // quadratically.
                let group = "G".repeat(n);
                let key = "K".repeat(n);
                let text = format!("[{group}]\n{key}=u\n{key}[l{}]=t\n", n - 1);
                let file = KeyFile::parse(text.as_bytes());
                // Built outside the count: `Locales` is not this crate's,
                // and its work is not counted.
                let locales = Locales::new((0..n).map(|at| format!("l{at}")));
                let others = Locales::new((0..n).map(|at| format!("m{at}")));
                // The names, looked up four times, and the locales, each
                // set tried twice.
                let tried: usize = locales.keys().chain(others.keys()).map(str::len).sum();
                let bytes = 4 * (group.len() + key.len()) + 2 * tried;
                let (values, steps) = work::steps(|| {
                    (
                        file.locale_string(&group, &key, &locales),
                        file.locale_list(&group, &key, &locales),
                        file.locale_string(&group, &key, &others),
                        file.locale_list(&group, &key, &others),
                    )
                });
                assert_eq!(
                    values,
                    (
                        Some("t".to_owned()),
                        Some(vec!["t".to_owned()]),
                        Some("u".to_owned()),
                        Some(vec!["u".to_owned()])
                    )
                );
                (steps, bytes)
            },
            // Per character of a locale tried: looked through for a `.`,
            // copied, hashed and compared, and hashed and compared again
            // among the postfixes with an encoding; per character of the
            // group name and the key: hashed and compared, the key copied
            // too, once per lookup, not per locale: about 4.9 per byte.
            // Twice that.
            |_, bytes| 10 * bytes,
        );
    }

    #[test]
    fn one_long_list_item_is_read_in_linear_time() {
        work::assert_linear(
            512 * 1024,
            |n| {
                // One item of about n bytes with escapes, then a short one.
                let item = r"ab\;c\\d\s".repeat(n / 10);
                let text = format!("[Desktop Entry]\nKeywords={item};x;\n");
                let file = KeyFile::parse(text.as_bytes());
                let (items, steps) = work::steps(|| file.list("Desktop Entry", "Keywords"));
                assert_eq!(items, Some(vec!["ab;c\\d ".repeat(n / 10), "x".to_owned()]));
                (steps, item.len())
            },
            // A step per byte of the value checked for UTF-8, per character
            // split off and per character of the item unescaped: about 2.9
            // per byte. Twice that.
            |_, bytes| 6 * bytes,
        );
    }

    #[test]
    fn one_long_key_and_value_are_written_and_read_in_linear_time() {
        work::assert_linear(
            512 * 1024,
            |n| {
                let key = "K".repeat(n);
                // Characters that are escaped, and ones that are not.
                let value = "a\\b\nc\td;".repeat(n / 8);
                let mut file = KeyFile::parse(b"[Desktop Entry]\nType=Application\n");
                let (read, steps) = work::steps(|| {
                    file.set_string("Desktop Entry", &key, &value).unwrap();
                    KeyFile::parse(&file.to_bytes()).string("Desktop Entry", &key)
                });
                assert_eq!(read.as_deref(), Some(value.as_str()));
                (steps, key.len() + value.len())
            },
            // Per character of the key: checked, copied, hashed and compared
            // to look it up and index it, when written and again when read;
            // of the value: escaped, checked and copied into the line; of
            // both: written out, split off, checked for UTF-8 and looked
            // through for the line's parts when read, and the value
            // unescaped: about 25.9 per byte. Twice that.
            |_, bytes| 52 * bytes,
        );
    }

    #[test]
    fn long_names_and_dangling_links_can_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let long = dir.path().join(format!("{}.desktop", "n".repeat(240)));
        KeyFile::parse(SAMPLE.as_bytes()).write(&long).unwrap();
        assert_eq!(fs::read_to_string(&long).unwrap(), SAMPLE);
        let emoji = dir
            .path()
            .join(format!("{}.desktop", "\u{1f600}".repeat(61)));
        KeyFile::parse(SAMPLE.as_bytes()).write(&emoji).unwrap();
        assert_eq!(fs::read_to_string(&emoji).unwrap(), SAMPLE);
        let link = dir.path().join("link.desktop");
        std::os::unix::fs::symlink("not-yet.desktop", &link).unwrap();
        KeyFile::parse(SAMPLE.as_bytes()).write(&link).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("not-yet.desktop")).unwrap(),
            SAMPLE
        );
    }

    #[test]
    fn writing_is_atomic_and_keeps_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.desktop");
        fs::write(&path, SAMPLE).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.path().join("link.desktop");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let mut file = KeyFile::read(&link).unwrap();
        file.set_string("Desktop Entry", "Name", "Changed").unwrap();
        file.write(&link).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("Name = Changed")
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let leftovers: Vec<_> = fs::read_dir(dir.path()).unwrap().flatten().collect();
        assert_eq!(leftovers.len(), 2, "no temporary file left behind");
        let fresh = dir.path().join("new.desktop");
        KeyFile::new().write(&fresh).unwrap();
        assert_eq!(fs::read(&fresh).unwrap(), b"");
    }

    // Only where the limit is known: on other systems `MAX_LINKS` is an
    // unverified fallback.
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "aix"
    ))]
    /// Makes `links` symbolic links in `dir`, each to the next, the last
    /// to `real.desktop`, which holds `old`. Returns the first link.
    fn link_chain(dir: &Path, links: usize) -> PathBuf {
        fs::write(dir.join("real.desktop"), "old").unwrap();
        let mut next = PathBuf::from("real.desktop");
        for at in (0..links).rev() {
            let link = PathBuf::from(format!("l{at}"));
            std::os::unix::fs::symlink(&next, dir.join(&link)).unwrap();
            next = link;
        }
        dir.join(next)
    }

    // Only where the limit is known: on other systems `MAX_LINKS` is an
    // unverified fallback.
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "aix"
    ))]
    #[test]
    fn writing_follows_as_many_links_as_reading() {
        // The folder without links on the way, so that only the chain counts
        // toward the system's limit when reading.
        let folder = |dir: &tempfile::TempDir| dir.path().canonicalize().unwrap();
        for links in [MAX_LINKS - 1, MAX_LINKS] {
            let dir = tempfile::tempdir().unwrap();
            let first = link_chain(&folder(&dir), links);
            KeyFile::read(&first).unwrap();
            KeyFile::parse(SAMPLE.as_bytes()).write(&first).unwrap();
            assert!(
                fs::symlink_metadata(&first)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(
                fs::read_to_string(dir.path().join("real.desktop")).unwrap(),
                SAMPLE,
                "{links} links"
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let first = link_chain(&folder(&dir), MAX_LINKS + 1);
        let read = KeyFile::read(&first).unwrap_err();
        let write = KeyFile::parse(SAMPLE.as_bytes()).write(&first).unwrap_err();
        assert_eq!(
            read.raw_os_error(),
            Some(rustix::io::Errno::LOOP.raw_os_error())
        );
        assert_eq!(
            (write.kind(), write.raw_os_error()),
            (read.kind(), read.raw_os_error())
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("real.desktop")).unwrap(),
            "old"
        );
    }

    /// Makes a FIFO at `path`.
    #[cfg(not(target_vendor = "apple"))]
    fn fifo(path: &Path) {
        rustix::fs::mknodat(
            rustix::fs::CWD,
            path,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        )
        .unwrap();
    }

    /// Runs `f` in a thread and returns its result. If a FIFO blocks the
    /// read, the test fails after `timeout`.
    #[cfg(not(target_vendor = "apple"))]
    fn within<T: Send + 'static>(
        timeout: std::time::Duration,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || send.send(f()));
        receive.recv_timeout(timeout).expect("it returns in time")
    }

    // `mknodat` is not in rustix on Apple systems.
    #[cfg(not(target_vendor = "apple"))]
    #[test]
    fn a_fifo_is_refused_not_waited_on() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.desktop");
        fifo(&path);
        let read = within(std::time::Duration::from_secs(10), move || {
            KeyFile::read(&path).map(|_| ())
        });
        assert_eq!(read.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }
}
