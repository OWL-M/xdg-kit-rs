use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// Errors returned by this crate.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// No absolute home folder was found, so the default base directories
    /// cannot be determined.
    NoHome,
    /// A path given to a lookup or a placement was not a plain relative path:
    /// it was absolute, contained `..`, or named nothing (empty, or just
    /// `.`).
    InvalidRelativePath(PathBuf),
    /// An I/O operation failed.
    Io {
        /// The path of the failed operation.
        path: PathBuf,
        /// What went wrong.
        source: io::Error,
    },
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

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoHome => f.write_str("no absolute home folder"),
            Self::InvalidRelativePath(path) => {
                write!(f, "not a plain relative path: {}", shown_path(path))
            }
            Self::Io { path, source } => write!(f, "{}: {source}", shown_path(path)),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Something ignored while reading, along with its source.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Diagnostic {
    /// Where the ignored value came from.
    pub source: DiagnosticSource,
    /// What was wrong with it.
    pub message: String,
}

/// Where a [`Diagnostic`] came from.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiagnosticSource {
    /// An environment variable, by name.
    Variable(String),
    /// A file, and the line in it when the problem is on one line.
    File {
        /// The file.
        path: PathBuf,
        /// The line, counted from 1.
        line: Option<usize>,
    },
}

impl Diagnostic {
    pub(crate) fn variable(name: &str, message: String) -> Self {
        Self {
            source: DiagnosticSource::Variable(name.to_owned()),
            message,
        }
    }

    pub(crate) fn file(path: PathBuf, line: Option<usize>, message: String) -> Self {
        Self {
            source: DiagnosticSource::File { path, line },
            message,
        }
    }
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
    fn paths_are_shown_escaped_and_whole() {
        let long = "b".repeat(300);
        let path = PathBuf::from(format!("a\x1b\n{long}"));
        assert_eq!(
            Error::InvalidRelativePath(path.clone()).to_string(),
            format!("not a plain relative path: a\\u{{1b}}\\n{long}")
        );
        let failed = Error::Io {
            path,
            source: io::Error::other("gone"),
        };
        assert_eq!(failed.to_string(), format!("a\\u{{1b}}\\n{long}: gone"));
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
