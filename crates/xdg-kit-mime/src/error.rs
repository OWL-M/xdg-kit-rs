use std::fmt;
use std::path::PathBuf;

/// Something in the database that was skipped or read only in part, and
/// where.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Diagnostic {
    /// The file it was found in.
    pub path: PathBuf,
    /// Where in the file, when the problem is in one place.
    pub location: Option<Location>,
    /// What was wrong.
    pub message: String,
}

/// A place in a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Location {
    /// A line of a text file, counted from 1.
    Line(usize),
    /// A byte offset into a binary file.
    Offset(usize),
}

impl Diagnostic {
    pub(crate) fn new(
        path: impl Into<PathBuf>,
        location: Option<Location>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            location,
            message: message.into(),
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.path.display())?;
        match self.location {
            Some(Location::Line(line)) => write!(f, ":{line}")?,
            Some(Location::Offset(offset)) => write!(f, "@{offset}")?,
            None => {}
        }
        write!(f, ": {}", self.message)
    }
}
