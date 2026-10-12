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

/// Formats external `text` (from a file or the environment) for a message.
/// Uses [`escaped`] and truncates to 32 characters, followed by `…`.
pub(crate) fn quoted(text: &str) -> String {
    cut(text, 32)
}

/// Formats `path` for a message using [`escaped`], keeping the full path
/// so the reader can locate the file. Encodes each non-UTF-8 byte as
/// `\xHH`, with two uppercase hex digits (such as `\xFF`), and `\` as
/// `\\`, so different paths remain distinguishable. Escapes a combining
/// mark after either sequence, as after any escape.
pub(crate) fn shown_path(path: &Path) -> String {
    let bytes = std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str());
    let mut shown = String::with_capacity(bytes.len());
    // Each part in UTF-8 starts the path or follows an escaped byte, as
    // `escaped` takes the start of its text.
    for chunk in bytes.utf8_chunks() {
        shown.push_str(&escaped(chunk.valid()));
        for byte in chunk.invalid() {
            // Writing to a `String` cannot fail.
            let _ = std::fmt::Write::write_fmt(&mut shown, format_args!("\\x{byte:02X}"));
        }
    }
    shown
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

/// A whole message from another program (such as a D-Bus error) as shown
/// in a message: [`escaped`] and cut to 256 characters, then `…`.
pub(crate) fn quoted_message(text: &str) -> String {
    cut(text, 256)
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
    fn names_differing_only_in_bytes_that_are_not_utf8_show_apart() {
        use std::os::unix::ffi::OsStrExt;
        let shown = |bytes: &[u8]| shown_path(Path::new(std::ffi::OsStr::from_bytes(bytes)));
        // Read lossily, both would show as `/a\u{fffd}b`.
        assert_ne!(shown(b"/a\xffb"), shown(b"/a\xfeb"));
        assert_eq!(shown(b"/a\xffb"), r"/a\xFFb");
        // Neither the text `\xFF` nor the replacement character.
        assert_eq!(shown(br"/a\xFFb"), r"/a\\xFFb");
        assert_eq!(shown("/a\u{fffd}b".as_bytes()), "/a\u{fffd}b");
        // A mark after an escaped byte is escaped, as after any escape.
        assert_eq!(
            shown(b"/\xff\xcc\x81e\xcc\x81\n"),
            "/\\xFF\\u{301}e\u{301}\\n"
        );
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
}
