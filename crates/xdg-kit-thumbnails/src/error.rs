//! What reading reports and building or storing refuses.

use std::fmt;
use std::path::PathBuf;

/// Something that was skipped or read only in part, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Diagnostic {
    /// The file it concerns, when one is known.
    pub path: Option<PathBuf>,
    /// Where in the file, as a byte offset, when the problem has a place.
    pub offset: Option<usize>,
    /// What was wrong.
    pub message: String,
}

impl Diagnostic {
    pub(crate) fn new(
        path: Option<PathBuf>,
        offset: Option<usize>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            path,
            offset,
            message: message.into(),
        }
    }
}

/// Why a thumbnailer command cannot be built.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CommandError {
    /// The `Exec` line cannot be split: an unmatched quote or a trailing
    /// backslash.
    Unparsable,
    /// The line holds no command (empty, or only a comment), or the
    /// program expanded to nothing.
    Empty,
    /// A `%` is followed by a character that is no thumbnailer field
    /// code.
    UnknownFieldCode(char),
    /// A `%` ends an argument without its code.
    Incomplete,
    /// The line has no input placeholder (`%i` or `%u`) or no output
    /// (`%o`): it could never produce the requested thumbnail.
    MissingPlaceholder,
    /// `%i` needs a local path and the source is a remote URI.
    NoLocalPath,
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unparsable => write!(f, "the command line cannot be split"),
            Self::Empty => write!(f, "the command line holds no command"),
            Self::UnknownFieldCode(code) if code.is_ascii_graphic() => {
                write!(f, "%{code} is not a thumbnailer field code")
            }
            Self::UnknownFieldCode(code) => {
                write!(
                    f,
                    "%U+{:04X} is not a thumbnailer field code",
                    u32::from(*code)
                )
            }
            Self::Incomplete => write!(f, "a % ends an argument without its code"),
            Self::MissingPlaceholder => {
                write!(f, "the command names no input (%i or %u) or no output (%o)")
            }
            Self::NoLocalPath => {
                write!(f, "%i needs a local file and the source is a remote URI")
            }
        }
    }
}

impl std::error::Error for CommandError {}
