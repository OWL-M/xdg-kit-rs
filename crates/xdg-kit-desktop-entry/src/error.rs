use std::fmt;
use std::path::PathBuf;

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

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(path) = &self.path {
            write!(f, "{}", path.display())?;
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
