//! The text and binary files `update-mime-database` writes besides
//! `mime.cache` (spec "The glob files", "The magic files", "The
//! `XMLnamespaces` files", "The icon files", "The treemagic files").

use std::path::Path;

use super::{MAX_DIAGNOSTICS, Problems};
use crate::model::{
    Folder, Glob, MAX_DEPTH, MagicRule, Matchlet, NO_GLOBS, NO_MAGIC, TreeKind, TreeMatchlet,
    TreeRule,
};
use crate::{Diagnostic, Location, MimeType};

/// Which files to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Parts {
    /// Every file: the folder has no usable `mime.cache`.
    All,
    /// Only what `mime.cache` does not hold: `treemagic` and `types`.
    NotInCache,
}

/// Reads a folder's text files into `folder`. Missing files are normal
/// (a database may lack any of them); unreadable ones are reported.
pub(crate) fn read(
    dir: &Path,
    parts: Parts,
    folder: &mut Folder,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut file = |name: &str| -> Option<Vec<u8>> {
        let path = dir.join(name);
        match xdg_kit_basedir::read_regular_file(&path) {
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
            parse("globs2", &dir.join("globs2"), bytes, folder, diagnostics);
        } else if let Some(bytes) = &globs {
            parse("globs", &dir.join("globs"), bytes, folder, diagnostics);
        }
    }
    for (name, bytes) in &other {
        if let Some(bytes) = bytes {
            parse(name, &dir.join(name), bytes, folder, diagnostics);
        }
    }
    if parts == Parts::All {
        folder.max_extent = folder.magic_extent();
    }
    for (name, bytes) in [("treemagic", &treemagic), ("types", &types)] {
        if let Some(bytes) = bytes {
            parse(name, &dir.join(name), bytes, folder, diagnostics);
        }
    }
}

/// Parses the contents of the file called `name` (`globs2`, `globs`,
/// `magic`, `aliases`, `subclasses`, `icons`, `generic-icons`,
/// `XMLnamespaces`, `treemagic` or `types`) into `folder`; other names are
/// ignored. Lists at most [`MAX_DIAGNOSTICS`](super::MAX_DIAGNOSTICS)
/// problems, as the cache reader does.
pub(crate) fn parse(
    name: &str,
    path: &Path,
    bytes: &[u8],
    folder: &mut Folder,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let problems = &mut Problems::new(path, diagnostics);
    match name {
        "globs2" => parse_globs(bytes, true, folder, problems),
        "globs" => parse_globs(bytes, false, folder, problems),
        "magic" => parse_magic(bytes, folder, problems),
        "aliases" => folder.aliases = parse_pairs(bytes, ' ', problems),
        "subclasses" => folder.parents = parse_pairs(bytes, ' ', problems),
        "icons" => folder.icons = parse_icons(bytes, problems),
        "generic-icons" => folder.generic_icons = parse_icons(bytes, problems),
        "XMLnamespaces" => folder.namespaces = parse_namespaces(bytes, problems),
        "treemagic" => parse_treemagic(bytes, folder, problems),
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
struct Held(Vec<(usize, String)>);

impl Held {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn report(&mut self, line: usize, message: impl Into<String>) {
        // One past the limit brings the summary; more are never listed.
        if self.0.len() <= MAX_DIAGNOSTICS {
            self.0.push((line, message.into()));
        }
    }

    /// Lists the held problems, in the order they were found.
    fn list(self, problems: &mut Problems<'_>) {
        for (line, message) in self.0 {
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
/// `update-mime-database` writes every `globs2` line that has flags a second
/// time without them, "for older parsers". That copy is dropped: honouring it
/// would make a case-sensitive pattern such as `*.C` also match
/// case-insensitively, which the source XML and `mime.cache` do not say.
///
/// The problems with lines' fields are listed before those with their
/// types and patterns.
fn parse_globs(bytes: &[u8], version_two: bool, folder: &mut Folder, problems: &mut Problems<'_>) {
    let mut bad_fields = Held::new();
    let mut bad_values = Held::new();
    // The (weight, type, pattern) of the previous line, when it had flags.
    let mut flagged: Option<(&str, &str, &str)> = None;
    for (number, line) in lines(bytes, problems) {
        // Only the line right before counts.
        let previous = flagged.take();
        let (weight, mime_text, pattern, case_sensitive) = if version_two {
            let mut fields = line.splitn(4, ':');
            let (Some(weight), Some(mime), Some(pattern)) =
                (fields.next(), fields.next(), fields.next())
            else {
                bad_fields.report(number, "fewer than three fields, line skipped");
                continue;
            };
            // Further fields after the flags are for future use and ignored.
            let flags = fields
                .next()
                .map(|rest| rest.split(':').next().unwrap_or(""));
            let key = (weight, mime, pattern);
            let copy_for_old_parsers = flags.is_none() && previous == Some(key);
            flagged = flags.is_some().then_some(key);
            if copy_for_old_parsers {
                continue;
            }
            let flags = flags.unwrap_or("");
            let Ok(weight) = weight.parse::<u32>() else {
                bad_fields.report(number, "weight is not a number, line skipped");
                continue;
            };
            let case_sensitive = flags.split(',').any(|flag| flag == "cs");
            (weight, mime, pattern, case_sensitive)
        } else {
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
    let mut held = Held::new();
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
    let mut held = Held::new();
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
    let mut held = Held::new();
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
    let mut held = Held::new();
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
    fn new(at: usize) -> Self {
        Self {
            at,
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
        let fits = depth == indent && indent <= MAX_DEPTH;
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

/// The `magic` file.
fn parse_magic(bytes: &[u8], folder: &mut Folder, problems: &mut Problems<'_>) {
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
                        rule = Some((found, Section::new(line_start)));
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

/// The `treemagic` file.
fn parse_treemagic(bytes: &[u8], folder: &mut Folder, problems: &mut Problems<'_>) {
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
            if let Some((priority, Ok(mime))) = cursor
                .section()
                .map(|(priority, mime)| (priority, MimeType::new(&mime)))
            {
                let found = TreeRule {
                    priority: priority.min(100),
                    mime,
                    matchlets: Vec::new(),
                };
                rule = Some((found, Section::new(line_start)));
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
        read(dir.path(), Parts::All, &mut folder, &mut diagnostics);
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
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
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
        assert!(folder.magic.is_empty());
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
        read(dir.path(), Parts::All, &mut folder, &mut diagnostics);
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
        read(dir.path(), Parts::NotInCache, &mut folder, &mut Vec::new());
        assert!(folder.globs.is_empty());
        assert!(folder.types.is_some());
    }
}
