use std::fmt;
use std::path::{Path, PathBuf};

/// Something in a file that was skipped or read only in part, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Diagnostic {
    /// The file it was found in, when the text came from a file.
    pub path: Option<PathBuf>,
    /// The line, counted from 1, when the problem is on one line.
    pub line: Option<usize>,
    /// What was wrong.
    pub message: String,
}

impl Diagnostic {
    pub(crate) fn new(
        path: Option<PathBuf>,
        line: Option<usize>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            path,
            line,
            message: message.into(),
        }
    }
}

impl From<&xdg_kit_desktop_entry::Diagnostic> for Diagnostic {
    fn from(found: &xdg_kit_desktop_entry::Diagnostic) -> Self {
        Self::new(found.path.clone(), found.line, found.message.clone())
    }
}

/// Formats external `text` (from a file or the environment) for a message.
/// Uses [`escaped`] and truncates to 32 characters, followed by `…`.
pub(crate) fn quoted(text: &str) -> String {
    cut(text, 32)
}

/// Formats `path` for a message using [`escaped`]. Keeps the full path so
/// the reader can locate the file.
pub(crate) fn shown_path(path: &Path) -> String {
    escaped(&path.to_string_lossy())
}

/// Formats `text` using [`escaped`], truncating to `limit` characters
/// followed by `…`.
fn cut(text: &str, limit: usize) -> String {
    // The characters of `text` are counted, not those of its escaped form,
    // so an escape is never cut.
    match text.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", escaped(&text[..end])),
        None => escaped(text),
    }
}

/// Escapes `text` with [`char::escape_debug`], except for quotes and
/// combining marks. Control characters become `\n`, `\t`, `\0` or
/// `\u{…}`; invisible characters and those that affect surrounding text
/// (such as U+200B, U+2028 or U+202E) become `\u{…}`; `\` becomes `\\`.
/// Keeps `'` and `"` because messages do not use Rust's quoting syntax.
///
/// A combining mark is a character that [`str::escape_debug`] escapes
/// only at the start. Keeps it after an unescaped character so a
/// decomposed `é` stays readable. At the start of `text` or after an
/// escaped character, uses `\u{…}` to prevent it from attaching to text
/// outside the value.
fn escaped(text: &str) -> String {
    let mut shown = String::with_capacity(text.len());
    // Whether a combining mark here would join something outside the text.
    let mut after_escape = true;
    for c in text.chars() {
        let escape = c.escape_debug();
        let kept = matches!(c, '\'' | '"')
            || escape.len() == 1
            || !after_escape && format!("a{c}").escape_debug().count() == 2;
        if kept {
            shown.push(c);
        } else {
            shown.extend(escape);
        }
        after_escape = !kept;
    }
    shown
}

/// Formats external names `a` and `b` side by side using [`quoted`],
/// keeping their first difference visible. If they differ but share the
/// first 32 characters, shows 32 characters starting 8 characters before
/// the first difference, with `…` for omitted text. When one name is a
/// prefix of the other, the difference is where the shorter name ends.
///
/// At the first difference, escapes each non-ASCII character as `\u{…}`.
/// This distinguishes lookalikes: `Å` and the Angstrom sign U+212B, `K`
/// and the Kelvin sign U+212A, or a Hangul syllable and its jamo. Escapes
/// all combining marks from that point onward too, so differences in
/// accent representation stay visible: `é` as one character or as `e`
/// and a mark, or marks in a different order.
pub(crate) fn quoted_pair(a: &str, b: &str) -> (String, String) {
    // In characters, so an escape is never cut.
    let same = a.chars().zip(b.chars()).take_while(|(a, b)| a == b).count();
    if same < 32 || a == b {
        // When they are equal, all of each is before `same`: shown as
        // `quoted` shows it.
        return (quoted_from(a, same), quoted_from(b, same));
    }
    let from = |text: &str| {
        let start = text
            .char_indices()
            .nth(same - 8)
            .map_or(text.len(), |(at, _)| at);
        format!("…{}", quoted_from(&text[start..], 8))
    };
    (from(a), from(b))
}

/// `text` as [`quoted`] shows it, but with its character `from` (counted
/// from 0) escaped as `\u{…}` if it is not ASCII, and each combining mark
/// from there on escaped, even after a character kept as it is.
fn quoted_from(text: &str, from: usize) -> String {
    let (text, more) = match text.char_indices().nth(32) {
        Some((end, _)) => (&text[..end], "…"),
        None => (text, ""),
    };
    let at = text
        .char_indices()
        .nth(from)
        .map_or(text.len(), |(at, _)| at);
    // Alone, a mark is the first character, so it is escaped.
    let rest: String = text[at..]
        .chars()
        .enumerate()
        .map(|(i, c)| match c {
            c if i == 0 && !c.is_ascii() => c.escape_unicode().to_string(),
            c => escaped(c.encode_utf8(&mut [0; 4])),
        })
        .collect();
    format!("{}{rest}{more}", escaped(&text[..at]))
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(path) = &self.path {
            write!(f, "{}", shown_path(path))?;
            if let Some(line) = self.line {
                write!(f, ":{line}")?;
            }
            write!(f, ": ")?;
        } else if let Some(line) = self.line {
            write!(f, "line {line}: ")?;
        }
        write!(f, "{}", self.message)
    }
}

/// Asserts that `message` shows the text from a file it quotes escaped and
/// cut: no control character, and a length that does not grow with the
/// text.
#[cfg(test)]
#[track_caller]
pub(crate) fn assert_shown_safely(message: &str) {
    let shown: String = message.chars().take(100).collect();
    assert!(!message.chars().any(char::is_control), "{shown:?}…");
    assert!(message.chars().count() <= 250, "{shown:?}…");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_text_is_cut_by_its_own_characters() {
        // An escape is never split: the 32 characters are those of the text.
        let text = format!("{}\x1b{}", "a".repeat(31), "b".repeat(10));
        assert_eq!(quoted(&text), format!("{}\\u{{1b}}…", "a".repeat(31)));
        assert_eq!(quoted("a\nb\x1b"), "a\\nb\\u{1b}");
        let whole = "c".repeat(32);
        assert_eq!(quoted(&whole), whole);
        assert_eq!(quoted(&"c".repeat(33)), format!("{whole}…"));
    }

    #[test]
    fn a_path_is_shown_escaped_and_whole() {
        let long = "b".repeat(300);
        let path = PathBuf::from(format!("/a\x1b\n{long}"));
        let shown = Diagnostic::new(Some(path), Some(2), "m").to_string();
        assert_eq!(shown, format!("/a\\u{{1b}}\\n{long}:2: m"));
    }

    #[test]
    fn quotes_are_kept_and_invisible_characters_escaped() {
        assert_eq!(quoted(r#"a"b'c\d"#), r#"a"b'c\\d"#);
        // A direction override, a line separator and a zero-width space.
        assert_eq!(
            quoted("a\u{202e}b\u{2028}c\u{200b}d"),
            r"a\u{202e}b\u{2028}c\u{200b}d"
        );
        // A combining mark is kept after a character kept as it is, a quote
        // too, so a decomposed `é` stays readable.
        assert_eq!(quoted("ae\u{301}"), "ae\u{301}");
        assert_eq!(quoted("\"\u{301}'\u{301}"), "\"\u{301}'\u{301}");
        assert_eq!(
            shown_path(Path::new("/a\"b'\u{202e}c\u{301}")),
            "/a\"b'\\u{202e}c\u{301}"
        );
        // It is escaped where it would join something outside the text: at
        // the start, the start of a cut text too, or after an escape.
        assert_eq!(quoted("\u{301}a"), r"\u{301}a");
        assert_eq!(
            quoted(&format!("\u{301}{}", "a".repeat(39))),
            format!(r"\u{{301}}{}…", "a".repeat(31))
        );
        assert_eq!(quoted("\u{1b}\u{301}"), r"\u{1b}\u{301}");
        assert_eq!(quoted("\\\u{301}"), r"\\\u{301}");
        assert_eq!(quoted("\u{301}\u{302}"), r"\u{301}\u{302}");
    }

    #[test]
    fn two_names_are_cut_around_where_they_differ() {
        let pair = |a: &str, b: &str| {
            let (a, b) = quoted_pair(a, b);
            [a, b]
        };
        let a = |n: usize| "a".repeat(n);
        // Equal, and differing within the 32 characters `quoted` shows.
        assert_eq!(
            pair(&a(40), &a(40)),
            [format!("{}…", a(32)), format!("{}…", a(32))]
        );
        assert_eq!(pair("ab", "ab"), ["ab", "ab"]);
        assert_eq!(
            pair(&format!("x{}", a(40)), &format!("y{}", a(40))),
            [format!("x{}…", a(31)), format!("y{}…", a(31))]
        );
        assert_eq!(
            pair(&format!("{}x{}", a(31), a(9)), &format!("{}y", a(31))),
            [format!("{}x…", a(31)), format!("{}y", a(31))]
        );
        // Differing at the 33rd character or later: from 8 characters
        // before the first difference, 32 characters.
        assert_eq!(
            pair(&format!("{}x{}", a(32), a(40)), &format!("{}y", a(32))),
            [format!("…{}x{}…", a(8), a(23)), format!("…{}y", a(8))]
        );
        assert_eq!(
            pair(&format!("{}x", a(500)), &format!("{}yz", a(500))),
            [format!("…{}x", a(8)), format!("…{}yz", a(8))]
        );
        // One starting the other: they differ where the shorter ends.
        assert_eq!(
            pair(&a(40), &format!("{}b", a(40))),
            [format!("…{}", a(8)), format!("…{}b", a(8))]
        );
        // Characters are counted, not bytes, and an escape is never cut:
        // at the start of what is shown, nor at its end.
        let e = "é".repeat(40);
        assert_eq!(
            pair(&format!("{e}x"), &format!("{e}y")),
            [
                format!("…{}x", "é".repeat(8)),
                format!("…{}y", "é".repeat(8))
            ]
        );
        let at_start = format!("{}\u{1b}\u{301}{}", a(24), a(6));
        assert_eq!(
            pair(&format!("{at_start}x"), &format!("{at_start}y")),
            [
                format!(r"…\u{{1b}}\u{{301}}{}x", a(6)),
                format!(r"…\u{{1b}}\u{{301}}{}y", a(6))
            ]
        );
        // A combining mark at the start of what is shown is escaped.
        let mark = format!("{}\u{301}{}", a(24), a(7));
        assert_eq!(
            pair(&format!("{mark}x"), &format!("{mark}y")),
            [
                format!(r"…\u{{301}}{}x", a(7)),
                format!(r"…\u{{301}}{}y", a(7))
            ]
        );
        let at_end = format!("{}x{}\u{1b}\u{1b}", a(32), a(22));
        assert_eq!(
            pair(&at_end, &format!("{}y", a(32))),
            [
                format!(r"…{}x{}\u{{1b}}…", a(8), a(22)),
                format!("…{}y", a(8))
            ]
        );
    }

    #[test]
    fn two_names_differing_only_in_combining_marks_look_different() {
        let pair = |a: &str, b: &str| {
            let (a, b) = quoted_pair(a, b);
            [a, b]
        };
        // Precomposed and decomposed: they differ at `é` and `e`; `é`, not
        // ASCII, is escaped, and so is the mark after `e`.
        assert_eq!(
            pair("caf\u{e9}", "cafe\u{301}"),
            [r"caf\u{e9}", r"cafe\u{301}"]
        );
        // The same marks in another order.
        assert_eq!(
            pair("a\u{301}\u{323}", "a\u{323}\u{301}"),
            [r"a\u{301}\u{323}", r"a\u{323}\u{301}"]
        );
        // Marks before the first difference are kept, those after it
        // escaped, in both names.
        assert_eq!(
            pair("e\u{301}x\u{301}", "e\u{301}ye\u{302}"),
            ["e\u{301}x\\u{301}", "e\u{301}ye\\u{302}"]
        );
        assert_eq!(pair("xe\u{301}", "ye\u{301}"), [r"xe\u{301}", r"ye\u{301}"]);
        // Equal names, and a single name, show their marks as they are.
        assert_eq!(pair("e\u{301}", "e\u{301}"), ["e\u{301}", "e\u{301}"]);
        // With more than 32 characters in common, from 8 characters before
        // the difference.
        let marks = "e\u{301}".repeat(20);
        assert_eq!(pair(&marks, &marks), [quoted(&marks), quoted(&marks)]);
        assert_eq!(quoted(&marks), format!("{}…", "e\u{301}".repeat(16)));
        assert_eq!(
            pair(
                &format!("{marks}x\u{301}"),
                &format!("{marks}e\u{301}\u{302}")
            ),
            [
                format!("…{}x\\u{{301}}", "e\u{301}".repeat(4)),
                format!("…{}e\\u{{301}}\\u{{302}}", "e\u{301}".repeat(4))
            ]
        );
        assert_eq!(
            pair(&format!("{marks}af\u{e9}"), &format!("{marks}afe\u{301}")),
            [
                format!("…{}af\\u{{e9}}", "e\u{301}".repeat(3)),
                format!("…{}afe\\u{{301}}", "e\u{301}".repeat(3))
            ]
        );
    }

    #[test]
    fn two_names_differing_in_look_alike_characters_look_different() {
        let pair = |a: &str, b: &str| {
            let (a, b) = quoted_pair(a, b);
            [a, b]
        };
        // The first character where they differ is shown as `\u{…}` in
        // each name where it is not ASCII: `Å` and the Angstrom sign, `K`
        // and the Kelvin sign, a Hangul syllable and its jamo.
        assert_eq!(pair("\u{c5}", "\u{212b}"), [r"\u{c5}", r"\u{212b}"]);
        assert_eq!(pair("K", "\u{212a}"), ["K", r"\u{212a}"]);
        assert_eq!(
            pair("\u{ac00}", "\u{1100}\u{1161}"),
            [r"\u{ac00}", "\\u{1100}\u{1161}"]
        );
        // Characters before it are kept, those after it shown as before:
        // marks escaped, others kept.
        assert_eq!(
            pair("\u{e9}\u{c5}\u{e9}e\u{301}", "\u{e9}\u{212b}\u{e9}e\u{301}"),
            [
                "\u{e9}\\u{c5}\u{e9}e\\u{301}",
                "\u{e9}\\u{212b}\u{e9}e\\u{301}"
            ]
        );
        // Where the shorter one ends, the longer one's next character.
        assert_eq!(pair("ab", "ab\u{e9}"), ["ab", r"ab\u{e9}"]);
        // An ASCII character stays as it is.
        assert_eq!(pair("a", "b"), ["a", "b"]);
        assert_eq!(
            pair("\u{e9}a\u{e9}", "\u{e9}b\u{e9}"),
            ["\u{e9}a\u{e9}", "\u{e9}b\u{e9}"]
        );
        // Equal names are shown as `quoted` shows them.
        assert_eq!(pair("\u{c5}", "\u{c5}"), ["\u{c5}", "\u{c5}"]);
        // Past the first 32 characters, within what is shown.
        let long = "\u{e9}".repeat(40);
        assert_eq!(
            pair(&format!("{long}\u{c5}z"), &format!("{long}\u{212b}z")),
            [
                format!(r"…{}\u{{c5}}z", "\u{e9}".repeat(8)),
                format!(r"…{}\u{{212b}}z", "\u{e9}".repeat(8))
            ]
        );
        assert_eq!(
            pair(&format!("{long}K"), &format!("{long}\u{212a}")),
            [
                format!("…{}K", "\u{e9}".repeat(8)),
                format!(r"…{}\u{{212a}}", "\u{e9}".repeat(8))
            ]
        );
    }
}
