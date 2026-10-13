//! The text and binary files `update-mime-database` writes besides
//! `mime.cache` (spec "The glob files", "The magic files", "The
//! `XMLnamespaces` files", "The icon files", "The treemagic files").

use std::path::Path;

use super::Problems;
use crate::model::{
    Folder, Glob, MagicRule, Matchlet, NO_GLOBS, NO_MAGIC, TreeKind, TreeMatchlet, TreeRule,
};
use crate::{Diagnostic, Limits, Location, MimeType, work};

/// Which files to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Parts {
    /// Every file: the folder has no usable `mime.cache`.
    All,
    /// Only what `mime.cache` does not hold: `treemagic` and `types`.
    NotInCache,
}

/// Reads a folder's text files into `folder`, within `limits`. Missing
/// files are normal (a database may lack any of them); unreadable ones are
/// reported.
pub(crate) fn read(
    dir: &Path,
    parts: Parts,
    folder: &mut Folder,
    diagnostics: &mut Vec<Diagnostic>,
    limits: &Limits,
) {
    let mut file = |name: &str| -> Option<Vec<u8>> {
        let path = dir.join(name);
        match xdg_kit_basedir::read_regular_file_with(&path, limits.max_file_len) {
            Ok(bytes) => Some(bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                diagnostics.push(Diagnostic::new(
                    path,
                    None,
                    format!("cannot be read: {err}"),
                ));
                None
            }
        }
    };
    let globs2 = if parts == Parts::All {
        file("globs2")
    } else {
        None
    };
    let globs = if parts == Parts::All && globs2.is_none() {
        file("globs")
    } else {
        None
    };
    let other: Vec<(&str, Option<Vec<u8>>)> = if parts == Parts::All {
        [
            "magic",
            "aliases",
            "subclasses",
            "icons",
            "generic-icons",
            "XMLnamespaces",
        ]
        .into_iter()
        .map(|name| (name, file(name)))
        .collect()
    } else {
        Vec::new()
    };
    let treemagic = file("treemagic");
    let types = file("types");

    if parts == Parts::All {
        if let Some(bytes) = &globs2 {
            parse(
                "globs2",
                &dir.join("globs2"),
                bytes,
                folder,
                diagnostics,
                limits,
            );
        } else if let Some(bytes) = &globs {
            parse(
                "globs",
                &dir.join("globs"),
                bytes,
                folder,
                diagnostics,
                limits,
            );
        }
    }
    for (name, bytes) in &other {
        if let Some(bytes) = bytes {
            parse(name, &dir.join(name), bytes, folder, diagnostics, limits);
        }
    }
    if parts == Parts::All {
        folder.max_extent = folder.magic_extent();
    }
    for (name, bytes) in [("treemagic", &treemagic), ("types", &types)] {
        if let Some(bytes) = bytes {
            parse(name, &dir.join(name), bytes, folder, diagnostics, limits);
        }
    }
}

/// Parses the contents of the file called `name` (`globs2`, `globs`,
/// `magic`, `aliases`, `subclasses`, `icons`, `generic-icons`,
/// `XMLnamespaces`, `treemagic` or `types`) into `folder`, within
/// `limits`; other names are ignored. Lists at most
/// [`Limits::max_diagnostics`] problems, as the cache reader does.
pub(crate) fn parse(
    name: &str,
    path: &Path,
    bytes: &[u8],
    folder: &mut Folder,
    diagnostics: &mut Vec<Diagnostic>,
    limits: &Limits,
) {
    let problems = &mut Problems::new(path, diagnostics, limits.max_diagnostics);
    let depth = limits.rule_depth();
    match name {
        "globs2" => parse_globs(bytes, true, folder, problems),
        "globs" => parse_globs(bytes, false, folder, problems),
        "magic" => parse_magic(bytes, folder, problems, depth),
        "aliases" => folder.aliases = parse_pairs(bytes, ' ', problems),
        "subclasses" => folder.parents = parse_pairs(bytes, ' ', problems),
        "icons" => folder.icons = parse_icons(bytes, problems),
        "generic-icons" => folder.generic_icons = parse_icons(bytes, problems),
        "XMLnamespaces" => folder.namespaces = parse_namespaces(bytes, problems),
        "treemagic" => parse_treemagic(bytes, folder, problems, depth),
        "types" => folder.types = Some(parse_types(bytes, problems)),
        _ => {}
    }
}

/// The non-empty, non-comment lines of a text file, numbered from 1, one at
/// a time. Lines that are not UTF-8 are reported when reached, and skipped.
fn lines<'a>(
    bytes: &'a [u8],
    problems: &mut Problems<'_>,
) -> impl Iterator<Item = (usize, &'a str)> {
    bytes
        .split(|&byte| byte == b'\n')
        .enumerate()
        .filter_map(move |(index, line)| {
            // The line found by the split, then checked as UTF-8.
            work::count(2 * line.len() + 1);
            let number = index + 1;
            match std::str::from_utf8(line) {
                Ok(line) if line.is_empty() || line.starts_with('#') => None,
                Ok(line) => Some((number, line)),
                Err(_) => {
                    problems.report(Some(Location::Line(number)), "not UTF-8, skipped");
                    None
                }
            }
        })
}

/// Collects text file problems during parsing so they can be reported
/// after all invalid UTF-8 lines. Keeps at most one more problem than the
/// reporting limit so the summary line still appears.
struct Held {
    problems: Vec<(usize, String)>,
    /// The reporting limit (`None`: none).
    max: Option<usize>,
}

impl Held {
    fn new(max: Option<usize>) -> Self {
        Self {
            problems: Vec::new(),
            max,
        }
    }

    fn report(&mut self, line: usize, message: impl Into<String>) {
        // One past the limit brings the summary; more are never listed.
        if self.max.is_none_or(|max| self.problems.len() <= max) {
            let message = message.into();
            work::count(message.len());
            self.problems.push((line, message));
        }
    }

    /// Lists the held problems, in the order they were found.
    fn list(self, problems: &mut Problems<'_>) {
        for (line, message) in self.problems {
            problems.report(Some(Location::Line(line)), message);
        }
    }
}

fn mime(text: &str, line: usize, held: &mut Held) -> Option<MimeType> {
    match MimeType::new(text) {
        Ok(mime) => Some(mime),
        Err(err) => {
            held.report(line, format!("{err}, line skipped"));
            None
        }
    }
}

/// `globs2` (`weight:type:pattern[:flags[:...]]`) or the older `globs`
/// (`type:pattern`, weight 50, the pattern running to the end of the line).
///
/// `update-mime-database` writes patterns as they are, so a `globs2`
/// pattern ends at the first `:` that `fnmatch` would not read as part of
/// it: one in a set (`*[[:digit:]]`) or escaped belongs to the pattern,
/// and a bracket that can never match (`[[.x`, no `.]`) takes the rest of
/// the line, flags included, so the pattern still never matches. A bare
/// `:` stays ambiguous; a flag other than `cs` is reported, so such a
/// misread line shows. So does a set that a trailing `\` keeps from
/// closing: the tool writes `[:\` with `cs` as `[:\:cs`, where the `[`
/// stands for itself and its `:` is bare, so the line reads as `[` with
/// the flag `\`.
///
/// `update-mime-database` writes every `globs2` line that has flags a second
/// time without them, "for older parsers". That copy is dropped: honouring it
/// would make a case-sensitive pattern such as `*.C` also match
/// case-insensitively, which the source XML and `mime.cache` do not say.
/// It is kept when the flagged line was read with another pattern: for a
/// never-matching bracket the pattern keeps the flags, so
/// [`MimeDatabase::patterns_for`](crate::MimeDatabase::patterns_for)
/// lists `*[a:b[.x:cs` and `*[a:b[.x` where `mime.cache` gives only
/// `*[a:b[.x`; none of them matches a name.
///
/// The problems with lines' fields are listed before those with their
/// types and patterns.
fn parse_globs(bytes: &[u8], version_two: bool, folder: &mut Folder, problems: &mut Problems<'_>) {
    let mut bad_fields = Held::new(problems.max());
    let mut bad_values = Held::new(problems.max());
    // The (weight, type, pattern) of the previous line, when it had flags.
    let mut flagged: Option<(&str, &str, &str)> = None;
    for (number, line) in lines(bytes, problems) {
        // Only the line right before counts.
        let previous = flagged.take();
        let (weight, mime_text, pattern, case_sensitive) = if version_two {
            let mut fields = line.splitn(3, ':');
            let (Some(weight), Some(mime), Some(rest)) =
                (fields.next(), fields.next(), fields.next())
            else {
                // The fields found scan the line.
                work::count(line.len());
                bad_fields.report(number, "fewer than three fields, line skipped");
                continue;
            };
            // The first two fields scanned for their `:`.
            work::count(weight.len() + mime.len() + 2);
            let end = crate::fnmatch::pattern_end(rest);
            let pattern = &rest[..end];
            // Further fields after the flags are for future use and ignored.
            let flags = rest
                .get(end + 1..)
                .map(|after| after.split(':').next().unwrap_or(""));
            // The flags scanned for their end, then for each word twice.
            work::count(3 * flags.map_or(0, str::len));
            let key = (weight, mime, pattern);
            // Compared with the previous line's.
            work::count(weight.len() + mime.len() + pattern.len());
            let copy_for_old_parsers = flags.is_none() && previous == Some(key);
            flagged = flags.is_some().then_some(key);
            if copy_for_old_parsers {
                continue;
            }
            work::count(weight.len());
            let Ok(weight) = weight.parse::<u32>() else {
                bad_fields.report(number, "weight is not a number, line skipped");
                continue;
            };
            let words = flags.into_iter().flat_map(|flags| flags.split(','));
            let case_sensitive = words.clone().any(|flag| flag == "cs");
            if let Some(other) = words.clone().find(|&flag| flag != "cs") {
                let other = crate::error::quoted(other);
                bad_fields.report(number, format!("unknown flag \"{other}\", ignored"));
            }
            (weight, mime, pattern, case_sensitive)
        } else {
            // Scanned for the `:`.
            work::count(line.len());
            let Some((mime, pattern)) = line.split_once(':') else {
                bad_fields.report(number, "no ':', line skipped");
                continue;
            };
            (50, mime, pattern, false)
        };
        let Some(mime) = mime(mime_text, number, &mut bad_values) else {
            continue;
        };
        if pattern == NO_GLOBS {
            folder.no_globs.push(mime);
        } else if pattern.is_empty() {
            bad_values.report(number, "empty pattern, line skipped");
        } else {
            work::count(pattern.len());
            folder.globs.push(Glob {
                weight: weight.min(100),
                pattern: pattern.to_owned(),
                mime,
                case_sensitive,
            });
        }
    }
    bad_fields.list(problems);
    bad_values.list(problems);
}

/// Lines of two fields separated by `separator`: `aliases` (`alias type`)
/// and `subclasses` (`type parent`).
fn parse_pairs(
    bytes: &[u8],
    separator: char,
    problems: &mut Problems<'_>,
) -> Vec<(MimeType, MimeType)> {
    let mut pairs = Vec::new();
    let mut held = Held::new(problems.max());
    for (number, line) in lines(bytes, problems) {
        let Some((first, second)) = line.split_once(separator) else {
            held.report(number, "one field only, line skipped");
            continue;
        };
        if let (Some(first), Some(second)) = (
            mime(first, number, &mut held),
            mime(second, number, &mut held),
        ) {
            pairs.push((first, second));
        }
    }
    held.list(problems);
    pairs
}

/// `icons` and `generic-icons`: `type:icon-name`.
fn parse_icons(bytes: &[u8], problems: &mut Problems<'_>) -> Vec<(MimeType, String)> {
    let mut icons = Vec::new();
    let mut held = Held::new(problems.max());
    for (number, line) in lines(bytes, problems) {
        match line.split_once(':') {
            Some((mime_text, icon)) if !icon.is_empty() => {
                if let Some(mime) = mime(mime_text, number, &mut held) {
                    icons.push((mime, icon.to_owned()));
                }
            }
            _ => held.report(number, "no icon name, line skipped"),
        }
    }
    held.list(problems);
    icons
}

/// `XMLnamespaces`: `namespaceURI localName type`, the local name possibly
/// empty (two spaces).
fn parse_namespaces(bytes: &[u8], problems: &mut Problems<'_>) -> Vec<(String, String, MimeType)> {
    let mut namespaces = Vec::new();
    let mut held = Held::new(problems.max());
    for (number, line) in lines(bytes, problems) {
        let mut fields = line.splitn(3, ' ');
        match (fields.next(), fields.next(), fields.next()) {
            (Some(namespace), Some(local), Some(mime_text))
                if !(namespace.is_empty() && local.is_empty()) =>
            {
                if let Some(mime) = mime(mime_text, number, &mut held) {
                    namespaces.push((namespace.to_owned(), local.to_owned(), mime));
                }
            }
            _ => held.report(number, "not three fields, line skipped"),
        }
    }
    held.list(problems);
    namespaces
}

/// `types`: one type per line (written by `update-mime-database`, not
/// defined by the spec).
fn parse_types(bytes: &[u8], problems: &mut Problems<'_>) -> Vec<MimeType> {
    let mut types = Vec::new();
    let mut held = Held::new(problems.max());
    for (number, line) in lines(bytes, problems) {
        if let Some(mime) = mime(line, number, &mut held) {
            types.push(mime);
        }
    }
    held.list(problems);
    types
}

/// A cursor over a binary-with-text file (`magic`, `treemagic`).
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    /// A decimal number; `None` if there is none or it overflows.
    fn number(&mut self) -> Option<u32> {
        let start = self.at;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.at += 1;
        }
        std::str::from_utf8(&self.bytes[start..self.at])
            .ok()?
            .parse()
            .ok()
    }

    fn take(&mut self, len: usize) -> Option<&[u8]> {
        let end = self.at.checked_add(len)?;
        let taken = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(taken)
    }

    /// Moves past the next newline, or to the end.
    fn skip_line(&mut self) {
        match self.bytes[self.at..].iter().position(|&byte| byte == b'\n') {
            Some(newline) => self.at += newline + 1,
            None => self.at = self.bytes.len(),
        }
    }

    /// A magic line's start, `[indent] ">"`: the nesting depth, or `None`
    /// if the line does not start that way.
    fn indent(&mut self) -> Option<usize> {
        let indent = if self.peek() == Some(b'>') {
            0
        } else {
            self.number()? as usize
        };
        self.eat(b'>').then_some(indent)
    }

    /// A section header `[priority:type]\n`.
    fn section(&mut self) -> Option<(u32, String)> {
        if !self.eat(b'[') {
            return None;
        }
        let priority = self.number()?;
        if !self.eat(b':') {
            return None;
        }
        let rest = &self.bytes[self.at..];
        let close = rest
            .iter()
            .position(|&byte| byte == b']' || byte == b'\n')?;
        if rest[close] != b']' {
            return None;
        }
        let mime = std::str::from_utf8(&rest[..close]).ok()?.to_owned();
        self.at += close + 1;
        self.eat(b'\n').then_some((priority, mime))
    }
}

/// A magic or tree magic test that holds nested tests.
trait Nested: Sized {
    fn children(&mut self) -> &mut Vec<Self>;
}

impl Nested for Matchlet {
    fn children(&mut self) -> &mut Vec<Self> {
        &mut self.children
    }
}

impl Nested for TreeMatchlet {
    fn children(&mut self) -> &mut Vec<Self> {
        &mut self.children
    }
}

/// A whole line that later lines may still go under.
struct Open<T> {
    item: T,
    /// Whether a line directly under it was skipped, or dropped.
    lost: bool,
}

/// The lines of one `magic` or `treemagic` section, nested.
///
/// A damaged test fails closed. If all its nested lines were skipped,
/// the test is dropped: keeping it alone would match more than the rule
/// allows. This applies recursively to its parents. A test with some
/// surviving nested lines is kept.
///
/// Only the last line at each level can accept more nested lines, so only
/// those lines are kept open. A skipped line leaves only a flag behind.
struct Section<T> {
    /// Where the section header starts.
    at: usize,
    /// The deepest nesting allowed.
    max_depth: usize,
    /// The finished top-level tests.
    kept: Vec<T>,
    /// The last whole line of each level, from the top.
    open: Vec<Open<T>>,
    /// Whether the last line of known depth was skipped, under the
    /// deepest open line: deeper lines go with it.
    lost_last: bool,
    /// Whether a line was placed whole.
    placed: bool,
    /// Whether a skipped line's depth is unknown, and so where any line
    /// belongs.
    unknown_depth: bool,
}

impl<T: Nested> Section<T> {
    fn new(at: usize, max_depth: usize) -> Self {
        Self {
            at,
            max_depth,
            kept: Vec::new(),
            open: Vec::new(),
            lost_last: false,
            placed: false,
            unknown_depth: false,
        }
    }

    /// Places `item` (`None`: a skipped line, already reported) at nesting
    /// depth `indent` (`None`: unknown) under the last line of each
    /// shallower level. A line below a skipped line goes with it, without
    /// a diagnostic of its own.
    ///
    /// Returns the problem to report when `item` skips a level or is too
    /// deep: it is then skipped, under the deepest line it reaches.
    fn place(&mut self, indent: Option<usize>, item: Option<T>) -> Option<&'static str> {
        let Some(indent) = indent else {
            self.unknown_depth = true;
            return None;
        };
        let depth = indent.min(self.open.len());
        if depth < indent && self.lost_last {
            return None;
        }
        self.close(depth);
        let fits = depth == indent && indent <= self.max_depth;
        let whole = item.is_some();
        if let Some(item) = item.filter(|_| fits) {
            self.open.push(Open { item, lost: false });
            self.lost_last = false;
            self.placed = true;
        } else {
            if let Some(parent) = self.open.last_mut() {
                parent.lost = true;
            }
            self.lost_last = true;
        }
        (whole && !fits).then_some("nesting skips a level or is too deep, line skipped")
    }

    /// Finishes the open lines at `depth` and below, deepest first: each
    /// goes under the line above it, or is dropped if it lost all its
    /// nested lines.
    fn close(&mut self, depth: usize) {
        while self.open.len() > depth
            && let Some(Open { mut item, lost }) = self.open.pop()
        {
            let keep = !(lost && item.children().is_empty());
            match (self.open.last_mut(), keep) {
                (Some(parent), true) => parent.item.children().push(item),
                (Some(parent), false) => parent.lost = true,
                (None, true) => self.kept.push(item),
                (None, false) => {}
            }
        }
    }

    /// The tests left whole. A section that had some and is left with none
    /// is reported.
    fn finish(mut self, problems: &mut Problems<'_>) -> Vec<T> {
        self.close(0);
        let (kept, message) = if self.unknown_depth {
            (
                Vec::new(),
                "a skipped line of unknown depth, section skipped",
            )
        } else {
            (
                self.kept,
                "no test left whole (one lost all its nested lines), section skipped",
            )
        };
        if self.placed && kept.is_empty() {
            problems.report(Some(Location::Offset(self.at)), message);
        }
        kept
    }
}

/// The `magic` file, tests nested at most `max_depth` deep.
fn parse_magic(bytes: &[u8], folder: &mut Folder, problems: &mut Problems<'_>, max_depth: usize) {
    const HEADER: &[u8] = b"MIME-Magic\0\n";
    let report = |problems: &mut Problems<'_>, at: usize, message: &str| {
        problems.report(Some(Location::Offset(at)), message);
    };
    let Some(body) = bytes.strip_prefix(HEADER) else {
        report(problems, 0, "no MIME-Magic header, file skipped");
        return;
    };
    let mut cursor = Cursor { bytes: body, at: 0 };
    let mut rule: Option<(MagicRule, Section<Matchlet>)> = None;
    let finish = |rule: Option<(MagicRule, Section<Matchlet>)>,
                  folder: &mut Folder,
                  problems: &mut Problems<'_>| {
        if let Some((mut rule, section)) = rule {
            rule.matchlets = section.finish(problems);
            add_magic_rule(rule, folder);
        }
    };
    while cursor.peek().is_some() {
        let line_start = cursor.at + HEADER.len();
        if cursor.peek() == Some(b'[') {
            finish(rule.take(), folder, problems);
            if let Some((priority, mime)) = cursor.section() {
                match MimeType::new(&mime) {
                    Ok(mime) => {
                        let found = MagicRule {
                            priority: priority.min(100),
                            mime,
                            matchlets: Vec::new(),
                        };
                        rule = Some((found, Section::new(line_start, max_depth)));
                    }
                    Err(err) => {
                        report(problems, line_start, &format!("{err}, section skipped"));
                        rule = None;
                    }
                }
            } else {
                report(problems, line_start, "bad section header, section skipped");
                rule = None;
                // Skip to the next section header.
                while cursor.peek().is_some_and(|byte| byte != b'[') {
                    cursor.skip_line();
                }
            }
            continue;
        }
        let indent = cursor.indent();
        let parsed = if indent.is_some() {
            parse_matchlet(&mut cursor)
        } else {
            Err(Skip::Line)
        };
        let section = rule.as_mut().map(|(_, section)| section);
        match parsed {
            Ok(matchlet) => {
                if let Some(problem) =
                    section.and_then(|section| section.place(indent, Some(matchlet)))
                {
                    report(problems, line_start, problem);
                }
            }
            Err(skip) => {
                if let Some(section) = section {
                    section.place(indent, None);
                }
                if let Skip::Rest = skip {
                    report(
                        problems,
                        line_start,
                        "truncated value, rest of file skipped",
                    );
                    break;
                }
                report(
                    problems,
                    line_start,
                    "unknown or malformed field, line skipped",
                );
                cursor.skip_line();
            }
        }
    }
    finish(rule, folder, problems);
}

/// Adds `rule` to `folder`, its `__NOMAGIC__` marker as a deletion.
fn add_magic_rule(mut rule: MagicRule, folder: &mut Folder) {
    let marker = |matchlet: &Matchlet| {
        matchlet.start == 0 && matchlet.value == NO_MAGIC && matchlet.children.is_empty()
    };
    if rule.matchlets.iter().any(marker) {
        folder.no_magic.push(rule.mime.clone());
        rule.matchlets.retain(|matchlet| !marker(matchlet));
    }
    if !rule.matchlets.is_empty() {
        folder.magic.push(rule);
    }
}

enum Skip {
    /// Skip to the next newline and go on.
    Line,
    /// The binary value runs past the end: nothing after it can be trusted.
    Rest,
}

/// After the indent (see [`Cursor::indent`]): `start "=" len value ["&"
/// mask] ["~" word-size] ["+" range] "\n"`
fn parse_matchlet(cursor: &mut Cursor<'_>) -> Result<Matchlet, Skip> {
    let start = cursor.number().ok_or(Skip::Line)?;
    if !cursor.eat(b'=') {
        return Err(Skip::Line);
    }
    let len = cursor.take(2).ok_or(Skip::Rest)?;
    let len = usize::from(u16::from_be_bytes([len[0], len[1]]));
    let value = cursor.take(len).ok_or(Skip::Rest)?.to_vec();
    let mut mask = None;
    if cursor.eat(b'&') {
        mask = Some(cursor.take(len).ok_or(Skip::Rest)?.to_vec());
    }
    let mut word_size = 1;
    if cursor.eat(b'~') {
        word_size = cursor.number().ok_or(Skip::Line)?;
    }
    let mut range = 1;
    if cursor.eat(b'+') {
        range = cursor.number().ok_or(Skip::Line)?;
    }
    if !cursor.eat(b'\n') {
        // An unknown field: the spec says to ignore the whole line.
        return Err(Skip::Line);
    }
    Ok(Matchlet {
        start,
        range,
        word_size,
        value,
        mask,
        children: Vec::new(),
    })
}

/// The `treemagic` file, tests nested at most `max_depth` deep.
fn parse_treemagic(
    bytes: &[u8],
    folder: &mut Folder,
    problems: &mut Problems<'_>,
    max_depth: usize,
) {
    const HEADER: &[u8] = b"MIME-TreeMagic\0\n";
    let report = |problems: &mut Problems<'_>, at: usize, message: &str| {
        problems.report(Some(Location::Offset(at)), message);
    };
    let Some(body) = bytes.strip_prefix(HEADER) else {
        report(problems, 0, "no MIME-TreeMagic header, file skipped");
        return;
    };
    let mut cursor = Cursor { bytes: body, at: 0 };
    let mut rule: Option<(TreeRule, Section<TreeMatchlet>)> = None;
    let finish = |rule: Option<(TreeRule, Section<TreeMatchlet>)>,
                  folder: &mut Folder,
                  problems: &mut Problems<'_>| {
        if let Some((mut rule, section)) = rule {
            rule.matchlets = section.finish(problems);
            if !rule.matchlets.is_empty() {
                folder.tree.push(rule);
            }
        }
    };
    while cursor.peek().is_some() {
        let line_start = cursor.at + HEADER.len();
        if cursor.peek() == Some(b'[') {
            finish(rule.take(), folder, problems);
            if let Some((priority, mime)) = cursor.section() {
                match MimeType::new(&mime) {
                    Ok(mime) => {
                        let found = TreeRule {
                            priority: priority.min(100),
                            mime,
                            matchlets: Vec::new(),
                        };
                        rule = Some((found, Section::new(line_start, max_depth)));
                    }
                    Err(err) => {
                        report(problems, line_start, &format!("{err}, section skipped"));
                        rule = None;
                    }
                }
            } else {
                report(problems, line_start, "bad section header, section skipped");
                rule = None;
                cursor.skip_line();
                while cursor.peek().is_some_and(|byte| byte != b'[') {
                    cursor.skip_line();
                }
            }
            continue;
        }
        let line_end = body[cursor.at..]
            .iter()
            .position(|&byte| byte == b'\n')
            .map_or(body.len(), |newline| cursor.at + newline);
        let line = &body[cursor.at..line_end];
        cursor.at = (line_end + 1).min(body.len());
        let (indent, matchlet) = parse_tree_line(line);
        let problem = match &matchlet {
            None => Some("malformed line, skipped"),
            Some(matchlet) if leaves_root(&matchlet.path) => {
                Some("rule path leaves the volume root (`..` or a leading `/`), line skipped")
            }
            Some(_) => None,
        };
        if let Some(problem) = problem {
            report(problems, line_start, problem);
        }
        if let Some((_, section)) = &mut rule
            && let Some(problem) = section.place(indent, matchlet.filter(|_| problem.is_none()))
        {
            report(problems, line_start, problem);
        }
    }
    finish(rule, folder, problems);
}

/// Whether a tree magic rule path could reach outside the volume root.
fn leaves_root(path: &str) -> bool {
    path.starts_with('/') || path.split('/').any(|component| component == "..")
}

/// `[indent] ">" "\"" path "\"" "=" type ["," option]*`: the nesting
/// depth, if the line starts with one, and the matchlet, if the line is
/// well-formed.
fn parse_tree_line(line: &[u8]) -> (Option<usize>, Option<TreeMatchlet>) {
    let Some(close) = line.iter().position(|&byte| byte == b'>') else {
        return (None, None);
    };
    let indent = match &line[..close] {
        [] => Some(0),
        digits => std::str::from_utf8(digits)
            .ok()
            .and_then(|digits| digits.parse().ok()),
    };
    let matchlet = indent.and_then(|_| parse_tree_matchlet(&line[close + 1..]));
    (indent, matchlet)
}

/// After the indent: `"\"" path "\"" "=" type ["," option]*`
fn parse_tree_matchlet(rest: &[u8]) -> Option<TreeMatchlet> {
    let rest = std::str::from_utf8(rest).ok()?;
    let rest = rest.strip_prefix('"')?;
    let (path, rest) = rest.split_once('"')?;
    let rest = rest.strip_prefix('=')?;
    let mut fields = rest.split(',');
    let kind = match fields.next()? {
        "file" => TreeKind::File,
        "directory" => TreeKind::Directory,
        "link" => TreeKind::Link,
        "any" => TreeKind::Any,
        _ => return None,
    };
    let mut matchlet = TreeMatchlet {
        path: path.to_owned(),
        kind,
        match_case: false,
        executable: false,
        non_empty: false,
        mimetype: None,
        children: Vec::new(),
    };
    for option in fields {
        match option {
            "executable" => matchlet.executable = true,
            "match-case" => matchlet.match_case = true,
            "non-empty" => matchlet.non_empty = true,
            mime => matchlet.mimetype = Some(MimeType::new(mime).ok()?),
        }
    }
    Some(matchlet)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder_from(name: &str, bytes: &[u8]) -> (Folder, Vec<Diagnostic>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(name), bytes).unwrap();
        let mut folder = Folder::default();
        let mut diagnostics = Vec::new();
        read(
            dir.path(),
            Parts::All,
            &mut folder,
            &mut diagnostics,
            &Limits::default(),
        );
        (folder, diagnostics)
    }

    fn m(text: &str) -> MimeType {
        MimeType::new(text).unwrap()
    }

    #[test]
    fn globs2_lines_flags_markers_and_comments() {
        let (folder, diagnostics) = folder_from(
            "globs2",
            b"# comment\n\
              55:text/x-diff:*.patch\n\
              50:text/x-c++src:*.C:cs,newflag:newfeature:x\n\
              0:text/x-diff:__NOGLOBS__\n\
              50:text/plain:*.txt:notcs\n",
        );
        // Flags other than `cs` are reported; the lines are kept.
        let lines: Vec<_> = diagnostics.iter().map(|d| d.location).collect();
        assert_eq!(
            lines,
            [Some(Location::Line(3)), Some(Location::Line(5))],
            "{diagnostics:?}"
        );
        assert_eq!(folder.no_globs, [m("text/x-diff")]);
        assert_eq!(
            folder.globs,
            [
                Glob {
                    weight: 55,
                    pattern: "*.patch".into(),
                    mime: m("text/x-diff"),
                    case_sensitive: false
                },
                Glob {
                    weight: 50,
                    pattern: "*.C".into(),
                    mime: m("text/x-c++src"),
                    case_sensitive: true
                },
                Glob {
                    weight: 50,
                    pattern: "*.txt".into(),
                    mime: m("text/plain"),
                    case_sensitive: false
                },
            ]
        );
    }

    #[test]
    fn the_flagless_copy_of_a_flagged_line_is_dropped() {
        let (folder, _) = folder_from(
            "globs2",
            b"50:text/x-c++src:*.C:cs\n50:text/x-c++src:*.C\n50:text/x-c++src:*.cc\n50:text/x-c++src:*.cc\n",
        );
        let patterns: Vec<_> = folder
            .globs
            .iter()
            .map(|g| (g.pattern.as_str(), g.case_sensitive))
            .collect();
        // Only a copy right after a flagged line is dropped; plain repeats stay.
        assert_eq!(patterns, [("*.C", true), ("*.cc", false), ("*.cc", false)]);
        let (folder, _) = folder_from(
            "globs2",
            b"50:text/x-c++src:*.C:cs\nbroken line\n50:text/x-c++src:*.C\n",
        );
        assert_eq!(folder.globs.len(), 2, "a line in between ends the pairing");
    }

    #[test]
    fn a_colon_in_a_set_belongs_to_the_pattern() {
        // Sets as `fnmatch` reads them: classes, a leading `]`, negation,
        // escapes. A `[` that never closes is itself, so the next `:` ends
        // the pattern; an escaped `:` is not one that ends it. A bracket
        // that can never match (`[.` with no `.]`) takes the rest.
        let (folder, diagnostics) = folder_from(
            "globs2",
            b"50:application/x-digit:*[[:digit:]].foo\n\
              50:text/x-b:*[[:upper:]]:cs\n\
              50:text/x-c:a[:]b\n\
              50:text/x-d:[]:]x:cs\n\
              50:text/x-e:[!:]x:cs:future\n\
              50:text/x-f:[^\\]:]x\n\
              50:text/x-g:*[x:cs\n\
              50:text/x-h:a\\:b\n\
              50:text/x-i:a[[.x:cs\n\
              50:text/x-j:x:y:cs\n\
              50:text/x-k:[[.:\n",
        );
        let globs: Vec<_> = folder
            .globs
            .iter()
            .map(|g| (g.pattern.as_str(), g.case_sensitive))
            .collect();
        assert_eq!(
            globs,
            [
                ("*[[:digit:]].foo", false),
                ("*[[:upper:]]", true),
                ("a[:]b", false),
                ("[]:]x", true),
                ("[!:]x", true),
                ("[^\\]:]x", false),
                ("*[x", true),
                ("a\\:b", false),
                ("a[[.x:cs", false),
                ("x", false),
                ("[[.:", false),
            ]
        );
        // A bare `:` stays ambiguous: `x:y:cs` reads as `x` with the flag
        // `y`, which is reported.
        let lines: Vec<_> = diagnostics.iter().map(|d| d.location).collect();
        assert_eq!(lines, [Some(Location::Line(10))], "{diagnostics:?}");
    }

    #[test]
    fn a_set_an_escaped_colon_keeps_open_leaves_its_colon_bare() {
        // `[:\` and `x[:a\` with `cs`, as `update-mime-database` writes
        // them, each with its flagless copy. The `[` is itself, so the
        // pattern ends at the `:` after it; the flag `\` is reported.
        let (folder, diagnostics) = folder_from(
            "globs2",
            b"50:text/x-a:[:\\:cs\n50:text/x-a:[:\\\n\
              50:text/x-b:x[:a\\:cs\n50:text/x-b:x[:a\\\n",
        );
        let globs: Vec<_> = folder
            .globs
            .iter()
            .map(|g| (g.pattern.as_str(), g.case_sensitive))
            .collect();
        assert_eq!(
            globs,
            [
                ("[", false),
                ("[:\\", false),
                ("x[", false),
                ("x[:a\\", false)
            ]
        );
        let found: Vec<_> = diagnostics
            .iter()
            .map(|d| (d.location, d.message.as_str()))
            .collect();
        assert_eq!(
            found,
            [
                (Some(Location::Line(1)), "unknown flag \"\\\\\", ignored"),
                (Some(Location::Line(3)), "unknown flag \"a\\\\\", ignored"),
            ]
        );
    }

    #[test]
    fn globs2_lines_are_split_in_linear_time() {
        // Long patterns of brackets: unclosed (the `:` after them ends the
        // pattern), never matching (they take the flags), sets holding a
        // `:`, a long class name; and many short lines. Each with the
        // first pattern the file should give, its case flag, and its
        // steps per byte rounded up, the bound being twice that.
        //
        // A long pattern's byte costs eight: two to find and check the
        // line, two to decode and copy it in the split and two to mark
        // it and look at it for a `.]`, one to compare it with the
        // previous line's and one to copy it. On top: `[` × n, read
        // (one) and a pass and a stop (two): eleven; `[[.` × n, none
        // (the bracket jumps to the end): eight; `*[[:digit:]]`, 16 a
        // unit of 12 bytes (`*` and `[` read, a step for the member
        // and for `]`, `digit:` read and copied): 9.33; `[` × n then
        // `[:`, `a` × n and `:]]`, a member step per `[` and two per
        // `a` read and copied: 9.5. Short lines of 20 bytes, 132 each
        // (the line 39, its first fields 12, the split 35, the flags 6,
        // the comparison 14, the weight 2, the type 20, the copy 4):
        // 6.6.
        type Shape = fn(usize) -> (String, String, bool);
        let shapes: [(Shape, usize); 5] = [
            (
                |n| {
                    let pattern = "[".repeat(n);
                    (format!("50:text/x-a:{pattern}:cs\n"), pattern, true)
                },
                11,
            ),
            (
                |n| {
                    let pattern = format!("{}:cs", "[[.".repeat(n));
                    (format!("50:text/x-a:{pattern}\n"), pattern, false)
                },
                8,
            ),
            (
                |n| {
                    let pattern = "*[[:digit:]]".repeat(n);
                    (format!("50:text/x-a:{pattern}\n"), pattern, false)
                },
                10,
            ),
            (
                |n| {
                    let pattern = format!("{}[:{}:]]", "[".repeat(n), "a".repeat(n));
                    (format!("50:text/x-a:{pattern}:cs\n"), pattern, true)
                },
                10,
            ),
            (
                |n| ("50:text/x-a:*[:]:cs\n".repeat(n), "*[:]".to_owned(), true),
                7,
            ),
        ];
        for (shape, per) in shapes {
            work::assert_linear(
                5000,
                |n| {
                    let (text, pattern, case_sensitive) = shape(n);
                    let mut folder = Folder::default();
                    let mut diagnostics = Vec::new();
                    let ((), steps) = work::steps(|| {
                        parse(
                            "globs2",
                            Path::new("globs2"),
                            text.as_bytes(),
                            &mut folder,
                            &mut diagnostics,
                            &Limits::default(),
                        );
                    });
                    assert!(diagnostics.is_empty(), "{diagnostics:?}");
                    let first = &folder.globs[0];
                    assert!(
                        first.pattern == pattern && first.case_sensitive == case_sensitive,
                        "{} bytes, case-sensitive {}",
                        first.pattern.len(),
                        first.case_sensitive
                    );
                    (steps, text.len())
                },
                |_, bytes| 2 * (100 + per * bytes),
            );
        }
    }

    #[test]
    fn an_unknown_flag_is_quoted_and_cut() {
        let long = "f".repeat(40);
        let (_, diagnostics) = folder_from(
            "globs2",
            format!("50:text/x-a:*.a:{long}\n50:text/x-b:*.b:cs,\"\u{202e}x\t\n").as_bytes(),
        );
        let messages: Vec<_> = diagnostics.iter().map(|d| d.message.as_str()).collect();
        assert_eq!(
            messages,
            [
                format!("unknown flag \"{}…\", ignored", "f".repeat(32)),
                "unknown flag \"\"\\u{202e}x\\t\", ignored".to_owned(),
            ]
        );
    }

    #[test]
    fn old_globs_are_read_only_without_globs2() {
        let (folder, _) = folder_from("globs", b"text/x-diff:*.patch: odd\n");
        assert_eq!(folder.globs[0].pattern, "*.patch: odd");
        assert_eq!(folder.globs[0].weight, 50);
    }

    #[test]
    fn bad_glob_lines_are_reported_and_skipped() {
        let (folder, diagnostics) = folder_from(
            "globs2",
            b"x:text/plain:*.a\n50:notatype:*.b\n50:text/plain\n50:text/plain:*.c\n",
        );
        assert_eq!(folder.globs.len(), 1);
        assert_eq!(diagnostics.len(), 3);
        assert!(
            diagnostics
                .iter()
                .all(|d| matches!(d.location, Some(Location::Line(1..=3))))
        );
    }

    #[test]
    fn glob_problems_are_listed_by_kind_then_by_line() {
        // Lines that are not UTF-8 first, then bad fields, then bad types
        // and patterns.
        let (_, diagnostics) = folder_from(
            "globs2",
            b"50:text/plain:\nx\n\xff\n50:notatype:*.c\ny:text/plain:*.d\n",
        );
        let lines: Vec<_> = diagnostics
            .iter()
            .map(|d| match d.location {
                Some(Location::Line(line)) => line,
                _ => 0,
            })
            .collect();
        assert_eq!(lines, [3, 2, 5, 1, 4], "{diagnostics:?}");
    }

    #[test]
    fn the_spec_magic_example_is_read() {
        let bytes = b"MIME-Magic\0\n[50:text/x-diff]\n>0=\x00\x05diff\t\n>0=\x00\x04***\t\n>0=\x00\x17Common subdirectories: \n";
        let (folder, diagnostics) = folder_from("magic", bytes);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(folder.magic.len(), 1);
        let rule = &folder.magic[0];
        assert_eq!((rule.priority, rule.mime.as_str()), (50, "text/x-diff"));
        assert_eq!(rule.matchlets.len(), 3);
        assert_eq!(rule.matchlets[0].value, b"diff\t");
        assert_eq!(folder.max_extent, 1 + 23);
    }

    #[test]
    fn magic_fields_nesting_and_markers() {
        let bytes = b"MIME-Magic\0\n\
            [80:image/x-test]\n\
            >4=\x00\x02\xab\xcd&\xff\xf0~2+8\n\
            1>10=\x00\x01Z\n\
            1>20=\x00\x01Y\n\
            2>30=\x00\x01X\n\
            [0:image/x-gone]\n\
            >0=\x00\x0b__NOMAGIC__\n";
        let (folder, diagnostics) = folder_from("magic", bytes);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(folder.no_magic, [m("image/x-gone")]);
        assert_eq!(folder.magic.len(), 1);
        let top = &folder.magic[0].matchlets[0];
        assert_eq!((top.start, top.range, top.word_size), (4, 8, 2));
        assert_eq!(top.mask.as_deref(), Some(&b"\xff\xf0"[..]));
        assert_eq!(top.children.len(), 2);
        assert_eq!(top.children[1].children[0].value, b"X");
        // The deepest matchlet: offset 30, range 1, one byte.
        assert_eq!(folder.max_extent, 32);
    }

    #[test]
    fn broken_magic_is_skipped_in_parts() {
        // A level skipped, an unknown field, a truncated value. (A line that
        // skips a level below `B` would drop `B`, its only nested line lost.)
        let bytes = b"MIME-Magic\0\n[50:a/b]\n3>1=\x00\x01C\n>0=\x00\x01A?junk\n>0=\x00\x01B\n>0=\x00\x09short";
        let (folder, diagnostics) = folder_from("magic", bytes);
        assert_eq!(folder.magic[0].matchlets.len(), 1);
        assert_eq!(folder.magic[0].matchlets[0].value, b"B");
        assert_eq!(diagnostics.len(), 3, "{diagnostics:?}");
        let (folder, diagnostics) = folder_from("magic", b"not magic");
        assert_eq!(folder.magic, []);
        assert_eq!(diagnostics.len(), 1);
    }

    #[test]
    fn pairs_icons_namespaces_and_types() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, text: &str| std::fs::write(dir.path().join(name), text).unwrap();
        write("aliases", "audio/x-midi audio/midi\n");
        write("subclasses", "image/svg+xml application/xml\n");
        write("icons", "application/msword:x-office-document\n");
        write("generic-icons", "application/zip:package-x-generic\n");
        write(
            "XMLnamespaces",
            "http://www.w3.org/1999/xhtml html application/xhtml+xml\nurn:x  application/x-any\n",
        );
        write("types", "text/plain\nimage/png\n");
        let mut folder = Folder::default();
        let mut diagnostics = Vec::new();
        read(
            dir.path(),
            Parts::All,
            &mut folder,
            &mut diagnostics,
            &Limits::default(),
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(folder.aliases, [(m("audio/x-midi"), m("audio/midi"))]);
        assert_eq!(folder.parents, [(m("image/svg+xml"), m("application/xml"))]);
        assert_eq!(
            folder.icons,
            [(m("application/msword"), "x-office-document".to_owned())]
        );
        assert_eq!(folder.generic_icons[0].1, "package-x-generic");
        assert_eq!(
            folder.namespaces[1],
            ("urn:x".to_owned(), String::new(), m("application/x-any"))
        );
        assert_eq!(folder.types, Some(vec![m("text/plain"), m("image/png")]));
    }

    #[test]
    fn treemagic_rules_options_and_nesting() {
        let bytes = b"MIME-TreeMagic\0\n\
            [50:x-content/image-dcf]\n\
            >\"dcim\"=directory,non-empty\n\
            1>\"dcim/x\"=file,executable,match-case,image/jpeg\n\
            >\"../etc\"=directory\n\
            >\"/etc\"=directory\n\
            [20:x-content/bad]\n\
            >\"a\"=socket\n";
        let (folder, diagnostics) = folder_from("treemagic", bytes);
        assert_eq!(diagnostics.len(), 3, "{diagnostics:?}");
        assert_eq!(
            folder.tree[0].matchlets.len(),
            1,
            "paths leaving the root are skipped"
        );
        assert_eq!(folder.tree.len(), 1);
        let top = &folder.tree[0].matchlets[0];
        assert_eq!(
            (top.path.as_str(), top.kind, top.non_empty),
            ("dcim", TreeKind::Directory, true)
        );
        let child = &top.children[0];
        assert!(child.executable && child.match_case);
        assert_eq!(child.mimetype, Some(m("image/jpeg")));
    }

    #[test]
    fn a_refused_type_skips_only_its_own_section() {
        // Right after the refused header, or after its own lines.
        for bytes in [
            b"MIME-TreeMagic\0\n[50:bad]\n[60:x-content/good]\n>\"dcim\"=directory\n".as_slice(),
            b"MIME-TreeMagic\0\n[50:bad]\n>\"x\"=directory\n[60:x-content/good]\n>\"dcim\"=directory\n",
        ] {
            let (folder, diagnostics) = folder_from("treemagic", bytes);
            let rules: Vec<_> = folder
                .tree
                .iter()
                .map(|rule| (rule.mime.as_str(), rule.matchlets[0].path.as_str()))
                .collect();
            assert_eq!(rules, [("x-content/good", "dcim")]);
            assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
            assert!(
                diagnostics[0].message.ends_with(", section skipped"),
                "{diagnostics:?}"
            );
        }
        // The `magic` reader does the same.
        for bytes in [
            b"MIME-Magic\0\n[50:bad]\n[60:a/good]\n>0=\x00\x01G\n".as_slice(),
            b"MIME-Magic\0\n[50:bad]\n>0=\x00\x01X\n[60:a/good]\n>0=\x00\x01G\n",
        ] {
            let (folder, diagnostics) = folder_from("magic", bytes);
            let rules: Vec<_> = folder
                .magic
                .iter()
                .map(|rule| (rule.mime.as_str(), rule.matchlets[0].value.as_slice()))
                .collect();
            assert_eq!(rules, [("a/good", b"G".as_slice())]);
            assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        }
    }

    #[test]
    fn a_file_lists_a_limited_number_of_problems() {
        let limit = crate::read::MAX_DIAGNOSTICS;
        for (name, header, line) in [
            ("globs2", &b""[..], &b"x\n"[..]),
            ("globs", b"", b"x\n"),
            ("magic", b"MIME-Magic\0\n", b">\n"),
            ("treemagic", b"MIME-TreeMagic\0\n", b"x\n"),
            ("aliases", b"", b"x\n"),
            ("subclasses", b"", b"x\n"),
            ("icons", b"", b"x\n"),
            ("generic-icons", b"", b"x\n"),
            ("XMLnamespaces", b"", b"x\n"),
            ("types", b"", b"x\n"),
        ] {
            let mut bytes = header.to_vec();
            for _ in 0..3 * limit {
                bytes.extend_from_slice(line);
            }
            let (_, diagnostics) = folder_from(name, &bytes);
            assert_eq!(diagnostics.len(), limit + 1, "{name}");
            assert!(
                diagnostics[..limit]
                    .iter()
                    .all(|diagnostic| diagnostic.location.is_some()),
                "{name}: {diagnostics:?}"
            );
            let last = &diagnostics[limit];
            assert_eq!(last.location, None, "{name}");
            assert_eq!(
                last.message,
                format!("more than {limit} problems; the rest are not listed"),
                "{name}"
            );
        }
    }

    #[test]
    fn only_tree_magic_and_types_with_a_cache() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("globs2"), "50:text/plain:*.txt\n").unwrap();
        std::fs::write(dir.path().join("types"), "text/plain\n").unwrap();
        let mut folder = Folder::default();
        read(
            dir.path(),
            Parts::NotInCache,
            &mut folder,
            &mut Vec::new(),
            &Limits::default(),
        );
        assert_eq!(folder.globs, []);
        assert!(folder.types.is_some());
    }
}
