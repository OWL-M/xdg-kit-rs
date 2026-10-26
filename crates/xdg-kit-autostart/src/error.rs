//! What discovery reports.

use std::path::PathBuf;

/// Something that was skipped or read only in part, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Diagnostic {
    /// The file or directory it concerns, when one is known.
    pub path: Option<PathBuf>,
    /// What was wrong.
    pub message: String,
}

impl Diagnostic {
    pub(crate) fn new(path: Option<PathBuf>, message: impl Into<String>) -> Self {
        Self {
            path,
            message: message.into(),
        }
    }
}

/// Shows external text in a message: control characters escaped, cut.
pub(crate) fn shown(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars().take(64) {
        if c.is_control() {
            use std::fmt::Write as _;
            let _ = write!(out, "\\u{{{:x}}}", c as u32);
        } else {
            out.push(c);
        }
    }
    if text.chars().count() > 64 {
        out.push('…');
    }
    out
}
