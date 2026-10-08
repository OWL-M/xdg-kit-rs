//! Key files (Desktop Entry spec "Basic format of the file"), read and
//! written without losing anything.
//!
//! Every line is kept as it was written: comments, blank lines, group
//! headers and entries, and the bytes of a line that is not UTF-8. A file
//! written back unchanged is byte-identical; changing a value rewrites only
//! the value part of its line.
//!
//! The spec forbids duplicate groups and keys. Reading follows `GLib`'s
//! `gkeyfile.c`: a repeated group continues the first one, and a repeated
//! key's last value counts. Both produce diagnostics.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use xdg_kit_locale::Locales;

use crate::Diagnostic;

/// What one line is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Blank,
    Comment,
    Group(String),
    /// `key[locale]=value`; the value starts at byte `value_start`.
    Entry {
        key: String,
        locale: Option<String>,
        value_start: usize,
    },
    /// Not UTF-8, or neither of the above.
    Invalid,
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

    fn value(&self) -> Option<&str> {
        match &self.kind {
            Kind::Entry { value_start, .. } => {
                std::str::from_utf8(self.content().get(*value_start..)?).ok()
            }
            _ => None,
        }
    }
}

/// A line's block and position within that block.
type Place = (usize, usize);

/// A group header and the lines up to the next header, or the lines
/// before the first header.
///
/// Only the first block, which holds lines before any header, may be empty.
/// Every other block starts with a header and contains no other headers.
/// Trailing comments, blank lines and invalid lines are stored separately
/// so an entry can be inserted before them without moving existing lines.
#[derive(Debug, Clone, Default)]
struct Block {
    /// The lines up to the last entry, or the header when there is none.
    lines: Vec<Line>,
    /// The lines after them.
    tail: Vec<Line>,
    /// Problems in `lines`, with their positions, in line order.
    problems: Vec<(usize, String)>,
    /// Problems in `tail`, with their positions within it, in line order.
    tail_problems: Vec<(usize, String)>,
}

impl Block {
    fn len(&self) -> usize {
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
        self.tail.last().or_else(|| self.lines.last())
    }
}

/// Where a group's lines are.
#[derive(Debug, Clone, Copy)]
struct GroupBlocks {
    /// The block of the group's first header.
    first: usize,
    /// The block of its last header, where a new entry goes: at the end of
    /// its `lines`.
    last: usize,
}

/// The lines of one key in a group, found by locale.
#[derive(Debug, Clone, Default)]
struct KeyLines {
    /// The last line of each locale (`None`: untranslated), as a repeated
    /// key's last value counts.
    last: HashMap<Option<String>, Place>,
    /// The last line of each locale postfix with an `.ENCODING` part, under
    /// the postfix without it.
    encoded: HashMap<String, Place>,
}

impl KeyLines {
    /// Adds the line at `place` for `locale`; it comes after the key's
    /// other lines.
    fn add(&mut self, locale: Option<&str>, place: Place) {
        if let Some(locale) = locale.filter(|locale| locale.contains('.')) {
            self.encoded
                .insert(strip_encoding(locale).into_owned(), place);
        }
        self.last.insert(locale.map(str::to_owned), place);
    }
}

/// An index of positions within the blocks, rebuilt after removals and
/// updated as entries are added.
#[derive(Debug, Clone, Default)]
struct Index {
    /// Group names, each once, in the order they first appear.
    groups: Vec<String>,
    /// Where each group's lines are.
    headers: HashMap<String, GroupBlocks>,
    /// Each group's keys with their locales, each once, in order.
    keys: HashMap<String, Vec<(String, Option<String>)>>,
    /// The lines of each (group, key), by locale.
    entries: HashMap<(String, String), KeyLines>,
}

/// A key file: groups of `key=value` entries, with comments.
///
/// Two key files are equal when their text is.
#[derive(Debug, Clone)]
pub struct KeyFile {
    /// The lines before the first group header, then one block per header,
    /// in file order. Positions are kept within a block, counted over its
    /// `lines` and then its `tail`, so that adding an entry moves no line.
    blocks: Vec<Block>,
    /// Whether the text ended with a line feed.
    final_newline: bool,
    path: Option<PathBuf>,
    key_names: KeyNames,
    index: Index,
    /// Each block's starting line, counted from 0 and cached on first
    /// access after a change.
    starts: OnceLock<Vec<usize>>,
    /// Problems with line numbers, cached on first access after a change.
    diagnostics: OnceLock<Vec<Diagnostic>>,
}

impl PartialEq for KeyFile {
    fn eq(&self, other: &Self) -> bool {
        let (count, final_newline) = self.canonical();
        let (other_count, other_final_newline) = other.canonical();
        final_newline == other_final_newline
            && count == other_count
            && self
                .lines()
                .zip(other.lines())
                .take(count)
                .all(|(a, b)| a.raw == b.raw)
    }
}

impl Eq for KeyFile {}

impl Default for KeyFile {
    fn default() -> Self {
        Self {
            blocks: vec![Block::default()],
            final_newline: true,
            path: None,
            key_names: KeyNames::default(),
            index: Index::default(),
            starts: OnceLock::new(),
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
            Self::GroupName(name) => write!(f, "{name:?} is not a group name"),
            Self::KeyName(name) => write!(f, "{name:?} is not a key name"),
            Self::Locale(locale) => write!(f, "{locale:?} is not a locale"),
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
            .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b'[' | b']' | b'='))
}

/// Whether `name` may name a group: printable ASCII except `[` and `]`.
pub(crate) fn valid_group_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| (0x20..0x7f).contains(&byte) && byte != b'[' && byte != b']')
}

/// Whether `name` may name a key: `A-Za-z0-9-`.
pub(crate) fn valid_key_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// Splits `Name[de_AT]` into `Name` and `de_AT`.
fn split_key(text: &str) -> (&str, Option<&str>) {
    match text.strip_suffix(']').and_then(|rest| rest.split_once('[')) {
        Some((key, locale)) => (key, Some(locale)),
        None => (text, None),
    }
}

/// A locale postfix without its `.ENCODING` part: `de_DE.UTF-8@euro` →
/// `de_DE@euro`.
fn strip_encoding(locale: &str) -> std::borrow::Cow<'_, str> {
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
    let mut chars = raw.chars();
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
    let mut chars = raw.chars();
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
            for raw in body.split(|&byte| byte == b'\n') {
                work::count(1);
                let line = classify(raw, key_names);
                if matches!(line.kind, Kind::Group(_)) {
                    file.blocks.push(Block::default());
                }
                let (block, _) = file.end();
                file.blocks[block].lines.push(line);
            }
        }
        file.reindex();
        file
    }

    /// Rebuilds the blocks' tails, their problems and the index from the
    /// lines.
    fn reindex(&mut self) {
        let mut index = Index::default();
        let mut seen_keys: HashSet<(String, String, Option<String>)> = HashSet::new();
        for (number, block) in self.blocks.iter_mut().enumerate() {
            let Block {
                lines,
                tail,
                problems,
                tail_problems,
            } = block;
            // The tail starts after the last entry, or after the header.
            let split = lines
                .iter()
                .inspect(|_| work::count(1))
                .rposition(|line| matches!(line.kind, Kind::Entry { .. }))
                .map_or(usize::from(number > 0), |last| last + 1);
            if split < lines.len() {
                tail.splice(0..0, lines.drain(split..));
            }
            problems.clear();
            tail_problems.clear();
            for (at, line) in tail.iter().enumerate() {
                work::count(1);
                if let Some(problem) = &line.problem {
                    tail_problems.push((at, problem.clone()));
                }
            }
            let mut group: Option<&str> = None;
            for (at, line) in lines.iter().enumerate() {
                work::count(1);
                if let Some(problem) = &line.problem {
                    problems.push((at, problem.clone()));
                }
                match &line.kind {
                    Kind::Group(name) => {
                        if let Some(found) = index.headers.get_mut(name) {
                            found.last = number;
                            problems.push((
                                at,
                                format!(
                                    "group [{name}] appears again; its entries join the first one"
                                ),
                            ));
                        } else {
                            index.headers.insert(
                                name.clone(),
                                GroupBlocks {
                                    first: number,
                                    last: number,
                                },
                            );
                            index.groups.push(name.clone());
                        }
                        group = Some(name);
                    }
                    Kind::Entry { key, locale, .. } => {
                        let Some(group) = group else {
                            problems.push((at, "entry before the first group; ignored".to_owned()));
                            continue;
                        };
                        let id = (group.to_owned(), key.clone(), locale.clone());
                        if seen_keys.insert(id) {
                            index
                                .keys
                                .entry(group.to_owned())
                                .or_default()
                                .push((key.clone(), locale.clone()));
                        } else {
                            let shown = locale
                                .as_ref()
                                .map_or_else(|| key.clone(), |locale| format!("{key}[{locale}]"));
                            problems.push((
                                at,
                                format!(
                                    "key {shown} appears again in [{group}]; the last value counts"
                                ),
                            ));
                        }
                        index
                            .entries
                            .entry((group.to_owned(), key.clone()))
                            .or_default()
                            .add(locale.as_deref(), (number, at));
                    }
                    Kind::Blank | Kind::Comment | Kind::Invalid => {}
                }
            }
        }
        self.index = index;
        self.changed();
    }

    /// Clears cached line numbers and diagnostics after a change.
    fn changed(&mut self) {
        self.starts.take();
        self.diagnostics.take();
    }

    /// The line each block starts on, counted from 0.
    fn starts(&self) -> &[usize] {
        self.starts.get_or_init(|| {
            work::count(self.blocks.len());
            let mut next = 0;
            self.blocks
                .iter()
                .map(|block| {
                    let start = next;
                    next += block.len();
                    start
                })
                .collect()
        })
    }

    /// The line number of `place`, counted from 1.
    fn line_number(&self, (block, at): Place) -> usize {
        self.starts()[block] + at + 1
    }

    fn line(&self, (block, at): Place) -> &Line {
        let block = &self.blocks[block];
        match at.checked_sub(block.lines.len()) {
            Some(in_tail) => &block.tail[in_tail],
            None => &block.lines[at],
        }
    }

    /// Every line, in file order.
    fn lines(&self) -> impl Iterator<Item = &Line> {
        self.blocks
            .iter()
            .flat_map(|block| block.lines.iter().chain(&block.tail))
    }

    fn last_line(&self) -> Option<&Line> {
        self.blocks.iter().rev().find_map(Block::last_line)
    }

    /// The place after the last line.
    fn end(&self) -> Place {
        let last = self.blocks.len() - 1;
        (last, self.blocks[last].len())
    }

    /// The place of the line before `place`, if there is one.
    fn before(&self, (block, at): Place) -> Option<Place> {
        if at > 0 {
            return Some((block, at - 1));
        }
        // Only the lines before the first group can be an empty block.
        (0..block).rev().find_map(|earlier| {
            let len = self.blocks[earlier].len();
            len.checked_sub(1).map(|last| (earlier, last))
        })
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
            let starts = self.starts();
            self.blocks
                .iter()
                .zip(starts)
                .flat_map(|(block, start)| {
                    work::count(1 + block.problems.len() + block.tail_problems.len());
                    let tail_start = start + block.lines.len();
                    let problems = block
                        .problems
                        .iter()
                        .map(move |(at, message)| (start + at, message));
                    let tail_problems = block
                        .tail_problems
                        .iter()
                        .map(move |(at, message)| (tail_start + at, message));
                    problems.chain(tail_problems).map(|(at, message)| {
                        Diagnostic::new(self.path.clone(), Some(at + 1), message.clone())
                    })
                })
                .collect()
        })
    }

    /// The group names, each once, in the order they first appear.
    #[must_use]
    pub fn groups(&self) -> Vec<&str> {
        work::count(self.index.groups.len());
        self.index.groups.iter().map(String::as_str).collect()
    }

    /// Whether the file has the group.
    #[must_use]
    pub fn has_group(&self, group: &str) -> bool {
        self.index.headers.contains_key(group)
    }

    /// The keys of `group` with their locales, each once, in the order they
    /// first appear.
    #[must_use]
    pub fn keys(&self, group: &str) -> Vec<(&str, Option<&str>)> {
        self.index
            .keys
            .get(group)
            .map(|keys| {
                work::count(keys.len());
                keys.iter()
                    .map(|(key, locale)| (key.as_str(), locale.as_deref()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The lines of `key` in `group`, by locale.
    fn key_lines(&self, group: &str, key: &str) -> Option<&KeyLines> {
        self.index.entries.get(&(group.to_owned(), key.to_owned()))
    }

    /// The place of the line holding the value of `key[locale]` in `group`:
    /// the last one, as a repeated key's last value counts.
    fn find(&self, group: &str, key: &str, locale: Option<&str>) -> Option<Place> {
        self.key_lines(group, key)?
            .last
            .get(&locale.map(str::to_owned))
            .inspect(|_| work::count(1))
            .copied()
    }

    /// The value of `key[locale]` in `group` as written, escapes included.
    #[must_use]
    pub fn raw(&self, group: &str, key: &str, locale: Option<&str>) -> Option<&str> {
        self.find(group, key, locale)
            .and_then(|place| self.line(place).value())
    }

    /// The line (counted from 1) that holds `key[locale]` in `group`.
    #[must_use]
    pub fn line_of(&self, group: &str, key: &str, locale: Option<&str>) -> Option<usize> {
        self.find(group, key, locale)
            .map(|place| self.line_number(place))
    }

    /// The line (counted from 1) of `group`'s first header.
    #[must_use]
    pub fn group_line(&self, group: &str) -> Option<usize> {
        self.index
            .headers
            .get(group)
            .map(|found| self.line_number((found.first, 0)))
    }

    /// The value of `key` in `group` translated for `wanted` (a locale key
    /// such as `de_AT@euro`), comparing postfixes with their `.ENCODING`
    /// part stripped, as the spec says; the last such line counts.
    fn translated(&self, group: &str, key: &str, wanted: &str) -> Option<&str> {
        let lines = self.key_lines(group, key)?;
        // A postfix without a `.` is its own stripped form.
        let plain = if wanted.contains('.') {
            None
        } else {
            lines.last.get(&Some(wanted.to_owned()))
        };
        // Places order as lines do: the later one counts.
        plain
            .max(lines.encoded.get(wanted))
            .inspect(|_| work::count(1))
            .and_then(|&place| self.line(place).value())
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
        locales
            .select(|locale| self.translated(group, key, locale))
            .or_else(|| self.raw(group, key, None))
            .map(unescape)
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
        locales
            .select(|locale| self.translated(group, key, locale))
            .or_else(|| self.raw(group, key, None))
            .map(split_list)
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
        if raw.contains(['\n', '\r']) || raw.starts_with([' ', '\t']) {
            return Err(EditError::Value);
        }
        if let Some((block, at)) = self.find(group, key, locale) {
            // Only the value changes: the line count and the line's problem
            // stay, so the line numbers and diagnostics worked out stay
            // right and `changed` is not needed.
            let line = &mut self.blocks[block].lines[at];
            if let Kind::Entry { value_start, .. } = line.kind {
                let carriage_return = line.raw.ends_with(b"\r");
                line.raw.truncate(value_start);
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
        if !self.has_group(group) {
            self.push_group(group);
        }
        self.insert_entry(group, key, locale, Line::entry(&text, key, locale));
        Ok(())
    }

    /// Inserts `line` for a new `key[locale]` after the last entry in an
    /// existing `group` and updates the index. Insertion takes constant
    /// time regardless of file length.
    ///
    /// The group's last entry (or header) ends the `lines` of its last
    /// block. Appending there puts the new entry before the tail without
    /// moving existing lines or changing positions in the index or problem
    /// lists. It also puts the entry after every other line for `key`, as
    /// [`KeyLines::add`] requires.
    fn insert_entry(&mut self, group: &str, key: &str, locale: Option<&str>, mut line: Line) {
        let found = self.index.headers.get(group);
        debug_assert!(found.is_some(), "insert_entry into missing group [{group}]");
        let Some(found) = found else {
            return;
        };
        let number = found.last;
        let place = (number, self.blocks[number].lines.len());
        self.match_line_end(place, &mut line);
        // A step for the line added, at a place known without looking.
        work::count(1);
        self.blocks[number].lines.push(line);
        self.index
            .entries
            .entry((group.to_owned(), key.to_owned()))
            .or_default()
            .add(locale, place);
        self.index
            .keys
            .entry(group.to_owned())
            .or_default()
            .push((key.to_owned(), locale.map(str::to_owned)));
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
    pub fn remove(&mut self, group: &str, key: &str, locale: Option<&str>) -> bool {
        if self.find(group, key, locale).is_none() {
            return false;
        }
        // The lines before the first group, the first block, are in none;
        // a block's entries are all in its `lines`.
        for block in self.blocks.iter_mut().skip(1) {
            work::count(1);
            if !matches!(&block.lines[0].kind, Kind::Group(name) if name == group) {
                continue;
            }
            block.lines.retain(|line| {
                work::count(1);
                !matches!(
                    &line.kind,
                    Kind::Entry { key: k, locale: l, .. } if k == key && l.as_deref() == locale
                )
            });
        }
        self.reindex();
        true
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
            let (block, at) = self.end();
            self.match_line_end((block, at), &mut blank);
            self.blocks[block].tail.push(blank);
        }
        let mut header = Line {
            raw: format!("[{group}]").into_bytes(),
            kind: Kind::Group(group.to_owned()),
            problem: None,
        };
        self.match_line_end(self.end(), &mut header);
        self.blocks.push(Block {
            lines: vec![header],
            ..Block::default()
        });
        let number = self.blocks.len() - 1;
        self.index.headers.insert(
            group.to_owned(),
            GroupBlocks {
                first: number,
                last: number,
            },
        );
        self.index.groups.push(group.to_owned());
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
            let last = &mut self.blocks[block].line_mut(at).raw;
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
        let count = self.blocks.iter().map(Block::len).sum();
        match self.last_line() {
            Some(last) if !self.final_newline && last.raw.is_empty() => (count - 1, true),
            _ => (count, self.final_newline || count == 0),
        }
    }

    /// Removes every occurrence of `group` with its lines, up to the next
    /// group; whether there was one.
    pub fn remove_group(&mut self, group: &str) -> bool {
        let before = self.blocks.len();
        // The lines before the first group, the first block, have no header.
        self.blocks.retain(|block| {
            work::count(1);
            !matches!(
                block.lines.first().map(|line| &line.kind),
                Some(Kind::Group(name)) if name == group
            )
        });
        let removed = self.blocks.len() < before;
        if removed {
            self.reindex();
        }
        removed
    }

    /// The file's text: an unchanged file gives back exactly what was read.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (index, line) in self.lines().enumerate() {
            work::count(1);
            if index > 0 {
                out.push(b'\n');
            }
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
    /// including targets that do not exist yet.
    ///
    /// # Errors
    ///
    /// When the folder cannot be written or the rename fails, and `path` is
    /// then unchanged; or when syncing the folder after the rename fails, and
    /// `path` already holds the new contents.
    pub fn write(&self, path: impl AsRef<Path>) -> io::Result<()> {
        write_atomic(path.as_ref(), &self.to_bytes())
    }
}

/// Reads one line (without its line feed): what it is, and what is wrong
/// with it.
fn classify(raw: &[u8], key_names: KeyNames) -> Line {
    let line = |kind, problem: Option<String>| Line {
        raw: raw.to_vec(),
        kind,
        problem,
    };
    let content = raw.strip_suffix(b"\r").unwrap_or(raw);
    let Ok(text) = std::str::from_utf8(content) else {
        return line(Kind::Invalid, Some("line is not UTF-8; ignored".to_owned()));
    };
    let trimmed = text.trim_start_matches([' ', '\t']);
    if trimmed.is_empty() {
        return line(Kind::Blank, None);
    }
    if trimmed.starts_with('#') {
        return line(Kind::Comment, None);
    }
    if let Some(rest) = trimmed.strip_prefix('[') {
        let Some(name) = rest.trim_end_matches([' ', '\t']).strip_suffix(']') else {
            return line(
                Kind::Invalid,
                Some("group header without a closing ]; ignored".to_owned()),
            );
        };
        let problem = (!valid_group_name(name))
            .then(|| format!("group name [{name}] has characters a group name may not have"));
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
    let problem = (!key_names.allows(key))
        .then(|| format!("key {key_text:?} has characters a key name may not have"));
    let value = trimmed[eq + 1..].trim_start_matches([' ', '\t']);
    line(
        Kind::Entry {
            key: key.to_owned(),
            locale: locale.map(str::to_owned),
            value_start: text.len() - value.len(),
        },
        problem,
    )
}

impl Line {
    fn entry(text: &str, key: &str, locale: Option<&str>) -> Self {
        let value_start = text.find('=').map_or(text.len(), |eq| eq + 1);
        Self {
            raw: text.as_bytes().to_vec(),
            kind: Kind::Entry {
                key: key.to_owned(),
                locale: locale.map(str::to_owned),
                value_start,
            },
            problem: None,
        }
    }
}

/// Most links followed when resolving the path to write.
const MAX_LINKS: usize = 40;

/// The file a write to `path` replaces: `path`, or the target its symbolic
/// links lead to, whether or not that exists yet.
fn write_target(path: &Path) -> io::Result<PathBuf> {
    let mut current = path.to_owned();
    for _ in 0..MAX_LINKS {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let link = fs::read_link(&current)?;
                current = match current.parent() {
                    Some(dir) if link.is_relative() => dir.join(link),
                    _ => link,
                };
            }
            _ => return Ok(current),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "too many levels of symbolic links",
    ))
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

/// Steps of work (lines, entries or blocks visited), counted in tests so
/// that they can show the work grows linearly with the input. Outside tests
/// counting does nothing.
///
/// The counts can only show what they are told: on a path a counted test
/// runs, every step that grows with the input counts, each item a loop
/// visits included (each repeat check, each line a lookup visits, each
/// line looked at to find where a new one goes). A new loop over input on
/// such a path must count its steps too.
pub(crate) mod work {
    #[cfg(test)]
    thread_local! {
        static STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// Counts `steps` steps on this thread.
    #[inline]
    pub(crate) fn count(steps: usize) {
        #[cfg(test)]
        STEPS.with(|done| done.set(done.get() + steps));
        #[cfg(not(test))]
        let _ = steps;
    }

    /// What `f` returns, and the steps it counted on this thread.
    #[cfg(test)]
    pub(crate) fn steps<T>(f: impl FnOnce() -> T) -> (T, usize) {
        let before = STEPS.with(std::cell::Cell::get);
        let value = f();
        (value, STEPS.with(std::cell::Cell::get) - before)
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
        // The group is followed by another, so new lines go in the middle.
        let mut file = KeyFile::parse(b"[Desktop Entry]\n\n[Other]\nk=v\n");
        let ((), steps) = work::steps(|| {
            for at in 0..10_000 {
                file.set_string("Desktop Entry", &format!("X-K{at}"), "v")
                    .unwrap();
            }
        });
        assert_eq!(file.keys("Desktop Entry").len(), 10_000);
        // A step per line added; the blank line after the group stays.
        assert!(steps <= 2 * 10_000, "{steps} steps for 10,000 keys");
        assert_like_reparse(&file);
    }

    #[test]
    fn many_translations_of_a_key_stay_fast() {
        let mut file = KeyFile::parse(b"[Desktop Entry]\nName=n\n");
        let ((), steps) = work::steps(|| {
            for at in 0..10_000 {
                file.set_locale_string("Desktop Entry", "Name", &format!("l{at}.UTF-8"), "v")
                    .unwrap();
            }
            for at in 0..10_000 {
                let locale = format!("l{at}");
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
        assert_eq!(file.keys("Desktop Entry").len(), 10_001);
        // A step per line added and per line a lookup visits.
        assert!(steps <= 4 * 10_000, "{steps} steps for 10,000 translations");
        assert_like_reparse(&file);
    }

    #[test]
    fn adding_keys_never_moves_a_groups_trailing_lines() {
        use std::fmt::Write as _;
        let mut text = String::from("[Desktop Entry]\nk=v\n");
        for at in 0..10_000 {
            let _ = writeln!(text, "# comment {at}\n\njunk {at}");
        }
        text.push_str("[Other]\nk=v\n");
        let mut file = KeyFile::parse(text.as_bytes());
        let ((), steps) = work::steps(|| {
            for at in 0..10_000 {
                file.set_string("Desktop Entry", &format!("X-K{at}"), "v")
                    .unwrap();
            }
        });
        assert_eq!(file.keys("Desktop Entry").len(), 10_001);
        // A step per line added.
        assert!(steps <= 2 * 10_000, "{steps} steps for 10,000 keys");
        assert_like_reparse(&file);
    }

    #[test]
    fn large_files_are_read_in_linear_time() {
        use std::fmt::Write as _;
        let mut text = String::from("[Desktop Entry]\n");
        for at in 0..40_000 {
            let _ = writeln!(text, "X-Key{at}=v");
        }
        let ((keys, found), steps) = work::steps(|| {
            let file = KeyFile::parse(text.as_bytes());
            let keys = file.keys("Desktop Entry").len();
            let found = (0..40_000).filter(|at| {
                file.raw("Desktop Entry", &format!("X-Key{at}"), None)
                    .is_some()
            });
            (keys, found.count())
        });
        assert_eq!((keys, found), (40_000, 40_000));
        // A step per line split off, read and indexed, per key listed and
        // per line a lookup visits.
        assert!(steps <= 5 * 40_000, "{steps} steps for 40,000 keys");
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
