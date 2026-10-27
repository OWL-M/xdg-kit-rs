//! What loading reports, and why it can fail.

use std::fmt;
use std::io;
use std::path::PathBuf;

/// Something that was skipped, ignored or read only in part, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Diagnostic {
    /// The file or directory it concerns, when one is known.
    pub path: Option<PathBuf>,
    /// The line in that file, counted from 1, when the problem has one.
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

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(path) = &self.path {
            write!(f, "{}", shown(&path.to_string_lossy()))?;
            if let Some(line) = self.line {
                write!(f, ":{line}")?;
            }
            write!(f, ": ")?;
        }
        write!(f, "{}", self.message)
    }
}

/// Why no menu could be built.
#[derive(Debug)]
#[non_exhaustive]
pub enum MenuError {
    /// No menu file of that name in any configuration directory.
    NotFound {
        /// Every path looked at, most important first.
        searched: Vec<PathBuf>,
    },
    /// The root menu file could not be read.
    Unreadable(PathBuf, io::Error),
    /// The root menu file is not a menu: not well-formed XML, or its root is
    /// no `<Menu>` with a usable `<Name>`.
    Malformed {
        /// The file.
        path: PathBuf,
        /// The line of the problem, when known.
        line: Option<usize>,
        /// What is wrong.
        message: String,
    },
    /// A resource limit was hit while loading the configuration or scanning
    /// for desktop entries. Nothing is returned: a menu built without the
    /// omitted part could show what it was there to hide.
    Limit {
        /// Which limit, and where.
        message: String,
    },
    /// A path given could not be made absolute (it is empty, or the
    /// working directory is gone).
    Path(io::Error),
}

impl fmt::Display for MenuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { searched } => {
                write!(f, "no menu file found")?;
                if let Some(first) = searched.first() {
                    write!(
                        f,
                        " (first place looked: {})",
                        shown(&first.to_string_lossy())
                    )?;
                }
                Ok(())
            }
            Self::Unreadable(path, err) => {
                write!(
                    f,
                    "{} cannot be read: {err}",
                    shown(&path.to_string_lossy())
                )
            }
            Self::Malformed {
                path,
                line,
                message,
            } => {
                write!(f, "{}", shown(&path.to_string_lossy()))?;
                if let Some(line) = line {
                    write!(f, ":{line}")?;
                }
                write!(f, ": {message}")
            }
            Self::Limit { message } => write!(f, "a resource limit was hit: {message}"),
            Self::Path(err) => write!(f, "the path cannot be made absolute: {err}"),
        }
    }
}

impl std::error::Error for MenuError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unreadable(_, err) | Self::Path(err) => Some(err),
            _ => None,
        }
    }
}

/// Shows external text in a message: control characters escaped, cut.
pub(crate) fn shown(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars().take(96) {
        if c.is_control() {
            use std::fmt::Write as _;
            let _ = write!(out, "\\u{{{:x}}}", c as u32);
        } else {
            out.push(c);
        }
    }
    if text.chars().count() > 96 {
        out.push('…');
    }
    out
}
