//! The text and binary files `update-mime-database` writes besides
//! `mime.cache` (spec "The glob files", "The magic files", "The
//! `XMLnamespaces` files", "The icon files", "The treemagic files").

use std::path::Path;

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
        match std::fs::read(&path) {
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
/// ignored.
pub(crate) fn parse(
    name: &str,
    path: &Path,
    bytes: &[u8],
    folder: &mut Folder,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match name {
        "globs2" => parse_globs(path, bytes, true, folder, diagnostics),
        "globs" => parse_globs(path, bytes, false, folder, diagnostics),
        "magic" => parse_magic(path, bytes, folder, diagnostics),
        "aliases" => folder.aliases = parse_pairs(path, bytes, ' ', diagnostics),
        "subclasses" => folder.parents = parse_pairs(path, bytes, ' ', diagnostics),
        "icons" => folder.icons = parse_icons(path, bytes, diagnostics),
        "generic-icons" => folder.generic_icons = parse_icons(path, bytes, diagnostics),
        "XMLnamespaces" => folder.namespaces = parse_namespaces(path, bytes, diagnostics),
        "treemagic" => parse_treemagic(path, bytes, folder, diagnostics),
        "types" => folder.types = Some(parse_types(path, bytes, diagnostics)),
        _ => {}
    }
}

/// The non-empty, non-comment lines of a text file, numbered from 1.
/// Lines that are not UTF-8 are reported and skipped.
fn lines<'a>(
    path: &Path,
    bytes: &'a [u8],
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<(usize, &'a str)> {
    let mut found = Vec::new();
    for (index, line) in bytes.split(|&byte| byte == b'\n').enumerate() {
        let number = index + 1;
        match std::str::from_utf8(line) {
            Ok(line) if line.is_empty() || line.starts_with('#') => {}
            Ok(line) => found.push((number, line)),
            Err(_) => diagnostics.push(Diagnostic::new(
                path,
                Some(Location::Line(number)),
                "not UTF-8, skipped",
            )),
        }
    }
    found
}

fn mime(
    text: &str,
    path: &Path,
    line: usize,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<MimeType> {
    match MimeType::new(text) {
        Ok(mime) => Some(mime),
        Err(err) => {
            diagnostics.push(Diagnostic::new(
                path,
                Some(Location::Line(line)),
                format!("{err}, line skipped"),
            ));
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
fn parse_globs(
    path: &Path,
    bytes: &[u8],
    version_two: bool,
    folder: &mut Folder,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut found = Vec::new();
    let mut bad = Vec::new();
    // The (weight, type, pattern) of the previous line, when it had flags.
    let mut flagged: Option<(&str, &str, &str)> = None;
    for (number, line) in lines(path, bytes, diagnostics) {
        // Only the line right before counts.
        let previous = flagged.take();
        let parsed = if version_two {
            let mut fields = line.splitn(4, ':');
            let (Some(weight), Some(mime), Some(pattern)) =
                (fields.next(), fields.next(), fields.next())
            else {
                bad.push((number, "fewer than three fields"));
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
                bad.push((number, "weight is not a number"));
                continue;
            };
            let case_sensitive = flags.split(',').any(|flag| flag == "cs");
            (weight, mime, pattern, case_sensitive)
        } else {
            let Some((mime, pattern)) = line.split_once(':') else {
                bad.push((number, "no ':'"));
                continue;
            };
            (50, mime, pattern, false)
        };
        found.push((number, parsed));
    }
    for (number, message) in bad {
        diagnostics.push(Diagnostic::new(
            path,
            Some(Location::Line(number)),
            format!("{message}, line skipped"),
        ));
    }
    for (number, (weight, mime_text, pattern, case_sensitive)) in found {
        let Some(mime) = mime(mime_text, path, number, diagnostics) else {
            continue;
        };
        if pattern == NO_GLOBS {
            folder.no_globs.push(mime);
        } else if pattern.is_empty() {
            diagnostics.push(Diagnostic::new(
                path,
                Some(Location::Line(number)),
                "empty pattern, line skipped",
            ));
        } else {
            folder.globs.push(Glob {
                weight: weight.min(100),
                pattern: pattern.to_owned(),
                mime,
                case_sensitive,
            });
        }
    }
}

/// Lines of two fields separated by `separator`: `aliases` (`alias type`)
/// and `subclasses` (`type parent`).
fn parse_pairs(
    path: &Path,
    bytes: &[u8],
    separator: char,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<(MimeType, MimeType)> {
    let mut pairs = Vec::new();
    let mut found = Vec::new();
    for (number, line) in lines(path, bytes, diagnostics) {
        found.push((number, line.split_once(separator)));
    }
    for (number, split) in found {
        let Some((first, second)) = split else {
            diagnostics.push(Diagnostic::new(
                path,
                Some(Location::Line(number)),
                "one field only, line skipped",
            ));
            continue;
        };
        if let (Some(first), Some(second)) = (
            mime(first, path, number, diagnostics),
            mime(second, path, number, diagnostics),
        ) {
            pairs.push((first, second));
        }
    }
    pairs
}

/// `icons` and `generic-icons`: `type:icon-name`.
fn parse_icons(
    path: &Path,
    bytes: &[u8],
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<(MimeType, String)> {
    let mut found = Vec::new();
    for (number, line) in lines(path, bytes, diagnostics) {
        found.push((number, line.split_once(':')));
    }
    let mut icons = Vec::new();
    for (number, split) in found {
        match split {
            Some((mime_text, icon)) if !icon.is_empty() => {
                if let Some(mime) = mime(mime_text, path, number, diagnostics) {
                    icons.push((mime, icon.to_owned()));
                }
            }
            _ => diagnostics.push(Diagnostic::new(
                path,
                Some(Location::Line(number)),
                "no icon name, line skipped",
            )),
        }
    }
    icons
}

/// `XMLnamespaces`: `namespaceURI localName type`, the local name possibly
/// empty (two spaces).
fn parse_namespaces(
    path: &Path,
    bytes: &[u8],
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<(String, String, MimeType)> {
    let mut found = Vec::new();
    for (number, line) in lines(path, bytes, diagnostics) {
        let mut fields = line.splitn(3, ' ');
        found.push((number, (fields.next(), fields.next(), fields.next())));
    }
    let mut namespaces = Vec::new();
    for (number, fields) in found {
        match fields {
            (Some(namespace), Some(local), Some(mime_text))
                if !(namespace.is_empty() && local.is_empty()) =>
            {
                if let Some(mime) = mime(mime_text, path, number, diagnostics) {
                    namespaces.push((namespace.to_owned(), local.to_owned(), mime));
                }
            }
            _ => diagnostics.push(Diagnostic::new(
                path,
                Some(Location::Line(number)),
                "not three fields, line skipped",
            )),
        }
    }
    namespaces
}

/// `types`: one type per line (written by `update-mime-database`, not
/// defined by the spec).
fn parse_types(path: &Path, bytes: &[u8], diagnostics: &mut Vec<Diagnostic>) -> Vec<MimeType> {
    let found: Vec<(usize, &str)> = lines(path, bytes, diagnostics);
    let mut types = Vec::new();
    for (number, line) in found {
        if let Some(mime) = mime(line, path, number, diagnostics) {
            types.push(mime);
        }
    }
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

/// Places `item` at nesting depth `indent` under the last item of each
/// shallower level. `false` if the depth skips a level.
fn place<T>(
    roots: &mut Vec<T>,
    indent: usize,
    item: T,
    children: fn(&mut T) -> &mut Vec<T>,
) -> bool {
    let mut level = roots;
    for _ in 0..indent {
        let Some(last) = level.last_mut() else {
            return false;
        };
        level = children(last);
    }
    level.push(item);
    true
}

/// The `magic` file.
fn parse_magic(path: &Path, bytes: &[u8], folder: &mut Folder, diagnostics: &mut Vec<Diagnostic>) {
    const HEADER: &[u8] = b"MIME-Magic\0\n";
    let report = |diagnostics: &mut Vec<Diagnostic>, at: usize, message: &str| {
        diagnostics.push(Diagnostic::new(path, Some(Location::Offset(at)), message));
    };
    let Some(body) = bytes.strip_prefix(HEADER) else {
        report(diagnostics, 0, "no MIME-Magic header, file skipped");
        return;
    };
    let mut cursor = Cursor { bytes: body, at: 0 };
    let mut rule: Option<MagicRule> = None;
    let finish = |rule: Option<MagicRule>, folder: &mut Folder| {
        let Some(mut rule) = rule else { return };
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
    };
    while cursor.peek().is_some() {
        let line_start = cursor.at + HEADER.len();
        if cursor.peek() == Some(b'[') {
            finish(rule.take(), folder);
            if let Some((priority, mime)) = cursor.section() {
                match MimeType::new(&mime) {
                    Ok(mime) => {
                        rule = Some(MagicRule {
                            priority: priority.min(100),
                            mime,
                            matchlets: Vec::new(),
                        });
                    }
                    Err(err) => {
                        report(diagnostics, line_start, &format!("{err}, section skipped"));
                        rule = None;
                    }
                }
            } else {
                report(
                    diagnostics,
                    line_start,
                    "bad section header, section skipped",
                );
                rule = None;
                // Skip to the next section header.
                while cursor.peek().is_some_and(|byte| byte != b'[') {
                    cursor.skip_line();
                }
            }
            continue;
        }
        match parse_matchlet(&mut cursor) {
            Ok((indent, matchlet)) => {
                if let Some(rule) = &mut rule
                    && (indent > MAX_DEPTH
                        || !place(&mut rule.matchlets, indent, matchlet, |m| &mut m.children))
                {
                    report(
                        diagnostics,
                        line_start,
                        "nesting skips a level or is too deep, line skipped",
                    );
                }
            }
            Err(Skip::Line) => {
                report(
                    diagnostics,
                    line_start,
                    "unknown or malformed field, line skipped",
                );
                cursor.skip_line();
            }
            Err(Skip::Rest) => {
                report(
                    diagnostics,
                    line_start,
                    "truncated value, rest of file skipped",
                );
                break;
            }
        }
    }
    finish(rule, folder);
}

enum Skip {
    /// Skip to the next newline and go on.
    Line,
    /// The binary value runs past the end: nothing after it can be trusted.
    Rest,
}

/// `[indent] ">" start "=" len value ["&" mask] ["~" word-size] ["+" range] "\n"`
fn parse_matchlet(cursor: &mut Cursor<'_>) -> Result<(usize, Matchlet), Skip> {
    let indent = if cursor.peek() == Some(b'>') {
        0
    } else {
        cursor.number().ok_or(Skip::Line)? as usize
    };
    if !cursor.eat(b'>') {
        return Err(Skip::Line);
    }
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
    Ok((
        indent,
        Matchlet {
            start,
            range,
            word_size,
            value,
            mask,
            children: Vec::new(),
        },
    ))
}

/// The `treemagic` file.
fn parse_treemagic(
    path: &Path,
    bytes: &[u8],
    folder: &mut Folder,
    diagnostics: &mut Vec<Diagnostic>,
) {
    const HEADER: &[u8] = b"MIME-TreeMagic\0\n";
    let report = |diagnostics: &mut Vec<Diagnostic>, at: usize, message: &str| {
        diagnostics.push(Diagnostic::new(path, Some(Location::Offset(at)), message));
    };
    let Some(body) = bytes.strip_prefix(HEADER) else {
        report(diagnostics, 0, "no MIME-TreeMagic header, file skipped");
        return;
    };
    let mut cursor = Cursor { bytes: body, at: 0 };
    let mut rule: Option<TreeRule> = None;
    let finish = |rule: Option<TreeRule>, folder: &mut Folder| {
        if let Some(rule) = rule.filter(|rule| !rule.matchlets.is_empty()) {
            folder.tree.push(rule);
        }
    };
    while cursor.peek().is_some() {
        let line_start = cursor.at + HEADER.len();
        if cursor.peek() == Some(b'[') {
            finish(rule.take(), folder);
            if let Some((priority, Ok(mime))) = cursor
                .section()
                .map(|(priority, mime)| (priority, MimeType::new(&mime)))
            {
                rule = Some(TreeRule {
                    priority: priority.min(100),
                    mime,
                    matchlets: Vec::new(),
                });
            } else {
                report(
                    diagnostics,
                    line_start,
                    "bad section header, section skipped",
                );
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
        match parse_tree_line(line) {
            Some((_, matchlet)) if leaves_root(&matchlet.path) => {
                report(
                    diagnostics,
                    line_start,
                    "rule path leaves the volume root (`..` or a leading `/`), line skipped",
                );
            }
            Some((indent, matchlet)) => {
                if let Some(rule) = &mut rule
                    && (indent > MAX_DEPTH
                        || !place(&mut rule.matchlets, indent, matchlet, |m| &mut m.children))
                {
                    report(
                        diagnostics,
                        line_start,
                        "nesting skips a level or is too deep, line skipped",
                    );
                }
            }
            None => report(diagnostics, line_start, "malformed line, skipped"),
        }
    }
    finish(rule, folder);
}

/// Whether a tree magic rule path could reach outside the volume root.
fn leaves_root(path: &str) -> bool {
    path.starts_with('/') || path.split('/').any(|component| component == "..")
}

/// `[indent] ">" "\"" path "\"" "=" type ["," option]*`
fn parse_tree_line(line: &[u8]) -> Option<(usize, TreeMatchlet)> {
    let line = std::str::from_utf8(line).ok()?;
    let (indent, rest) = line.split_once('>')?;
    let indent = if indent.is_empty() {
        0
    } else {
        indent.parse().ok()?
    };
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
    Some((indent, matchlet))
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
        // An unknown field, a level skipped, a truncated value.
        let bytes = b"MIME-Magic\0\n[50:a/b]\n>0=\x00\x01A?junk\n>0=\x00\x01B\n3>1=\x00\x01C\n>0=\x00\x09short";
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
