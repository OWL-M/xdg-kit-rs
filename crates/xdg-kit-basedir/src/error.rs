use std::fmt;
use std::io;
use std::path::PathBuf;

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

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoHome => f.write_str("no absolute home folder"),
            Self::InvalidRelativePath(path) => {
                write!(f, "not a plain relative path: {}", path.display())
            }
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
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
