//! What reading reports and editing or writing refuses.

use std::fmt;
use std::io;
use std::path::PathBuf;

/// Something in a file that was skipped or read only in part, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Diagnostic {
    /// The file it was found in, when the text came from a file.
    pub path: Option<PathBuf>,
    /// Where in the text, as a byte offset, when the problem has a place.
    /// The line and column can be computed from the document's bytes.
    pub offset: Option<usize>,
    /// What was wrong.
    pub message: String,
}

impl Diagnostic {
    pub(crate) fn new(offset: Option<usize>, message: impl Into<String>) -> Self {
        Self {
            path: None,
            offset,
            message: message.into(),
        }
    }
}

/// Why a mutation was refused. A refused mutation leaves the document
/// exactly as it was.
#[derive(Debug)]
#[non_exhaustive]
pub enum EditError {
    /// The document is not editable XML (see the crate documentation for
    /// what editing requires beyond tolerant reading); it can be read,
    /// with diagnostics, but not changed.
    NotEditable,
    /// No entry has this URI.
    NoSuchUri(String),
    /// Several entries have this URI, so a targeted update cannot know
    /// which one is meant. `remove` takes them all; updates refuse.
    AmbiguousUri(String),
    /// The named value contains a character that no escaping can make
    /// valid XML (a NUL, or another character outside the XML ranges).
    InvalidText(&'static str),
    /// The time cannot be written as an XBEL timestamp (the formatter
    /// covers the years 1583–9999).
    TimeOutOfRange,
    /// Adding the entry would exceed [`Limits::max_bookmarks`].
    ///
    /// [`Limits::max_bookmarks`]: crate::Limits::max_bookmarks
    TooManyBookmarks,
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotEditable => write!(f, "the document cannot be edited as read"),
            Self::NoSuchUri(uri) => write!(f, "no entry has the URI {}", quoted(uri)),
            Self::AmbiguousUri(uri) => {
                write!(f, "several entries have the URI {}", quoted(uri))
            }
            Self::InvalidText(what) => {
                write!(f, "the {what} contains characters XML cannot hold")
            }
            Self::TimeOutOfRange => write!(f, "the time cannot be written as a timestamp"),
            Self::TooManyBookmarks => write!(f, "the limit on entries is reached"),
        }
    }
}

impl std::error::Error for EditError {}

/// Why a write did not complete, or could not start.
#[derive(Debug)]
#[non_exhaustive]
pub enum WriteError {
    /// The document is not editable XML; writing it back would launder a
    /// read-only snapshot into a rewrite.
    NotEditable,
    /// The document was parsed from bytes and has no file.
    NoFile,
    /// The file changed since it was loaded: someone else wrote it. The
    /// pending edits are kept; reload, reapply and retry.
    Conflict,
    /// Writing the result would break the document's own limits: the
    /// serialized bytes are over [`Limits::max_file_len`], or the result
    /// would not load as an editable document under these limits (its
    /// nesting or entry count, say). Nothing was replaced; the pending
    /// edits stay.
    ///
    /// [`Limits::max_file_len`]: crate::Limits::max_file_len
    OverLimits,
    /// Reading back the destination for the conflict check, or writing,
    /// failed; nothing was replaced.
    Io(io::Error),
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotEditable => write!(f, "the document cannot be edited as read"),
            Self::NoFile => write!(f, "this document was not read from a file"),
            Self::Conflict => write!(f, "the file changed since it was loaded"),
            Self::OverLimits => {
                write!(f, "the result would break the document's own limits")
            }
            Self::Io(err) => write!(f, "cannot write the file: {err}"),
        }
    }
}

impl std::error::Error for WriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

/// Formats external text (from a file or a caller) for a message: control
/// characters and non-characters escaped, cut to 64 characters.
pub(crate) fn quoted(text: &str) -> String {
    let mut shown = String::new();
    for c in text.chars().take(64) {
        if c.is_control() {
            let _ = fmt::Write::write_fmt(&mut shown, format_args!("\\u{{{:x}}}", c as u32));
        } else {
            shown.push(c);
        }
    }
    if text.chars().count() > 64 {
        shown.push('…');
    }
    shown
}
