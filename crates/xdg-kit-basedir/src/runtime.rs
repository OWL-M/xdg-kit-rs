use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::paths::normalize;

/// Why `$XDG_RUNTIME_DIR` cannot be used.
///
/// The spec asks applications to fall back to a replacement folder and print
/// a warning in these cases. This crate does neither: it reports the problem
/// so the caller can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuntimeDirProblem {
    /// The variable is unset or empty.
    Unset,
    /// The variable holds a relative path, which the spec says to ignore.
    Relative,
    /// The folder does not exist.
    Missing,
    /// The path exists but is not a folder.
    NotADirectory,
    /// The folder belongs to another user.
    NotOwned {
        /// The owner's user ID.
        owner: u32,
    },
    /// The folder's access permissions are not exactly `0700`.
    WrongMode {
        /// The access permission bits found (`rwx` for user, group, other).
        mode: u32,
    },
    /// The folder's metadata could not be read.
    Unreadable(io::ErrorKind),
}

impl fmt::Display for RuntimeDirProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unset => f.write_str("XDG_RUNTIME_DIR is not set"),
            Self::Relative => f.write_str("XDG_RUNTIME_DIR is a relative path"),
            Self::Missing => f.write_str("XDG_RUNTIME_DIR does not exist"),
            Self::NotADirectory => f.write_str("XDG_RUNTIME_DIR is not a folder"),
            Self::NotOwned { owner } => {
                write!(f, "XDG_RUNTIME_DIR belongs to user {owner}")
            }
            Self::WrongMode { mode } => {
                write!(f, "XDG_RUNTIME_DIR has mode {mode:04o}, not 0700")
            }
            Self::Unreadable(kind) => write!(f, "XDG_RUNTIME_DIR cannot be read: {kind}"),
        }
    }
}

impl std::error::Error for RuntimeDirProblem {}

/// Checks `$XDG_RUNTIME_DIR` against the spec's rules for the user `uid`.
pub(crate) fn check(value: Option<OsString>, uid: u32) -> Result<PathBuf, RuntimeDirProblem> {
    let value = value
        .filter(|value| !value.is_empty())
        .ok_or(RuntimeDirProblem::Unset)?;
    let path = Path::new(&value);
    if !path.is_absolute() {
        return Err(RuntimeDirProblem::Relative);
    }
    let metadata = fs::metadata(path).map_err(|err| match err.kind() {
        io::ErrorKind::NotFound => RuntimeDirProblem::Missing,
        kind => RuntimeDirProblem::Unreadable(kind),
    })?;
    if !metadata.is_dir() {
        return Err(RuntimeDirProblem::NotADirectory);
    }
    if metadata.uid() != uid {
        return Err(RuntimeDirProblem::NotOwned {
            owner: metadata.uid(),
        });
    }
    // The spec's "access mode": the rwx bits. Set-ID and sticky bits grant no
    // access, so they do not count.
    let mode = metadata.mode() & 0o777;
    if mode != 0o700 {
        return Err(RuntimeDirProblem::WrongMode { mode });
    }
    Ok(normalize(path))
}

/// The effective user ID of this process.
pub(crate) fn current_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn folder_with_mode(mode: u32) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(mode)).unwrap();
        dir
    }

    #[test]
    fn a_private_folder_of_ours_is_used() {
        let dir = folder_with_mode(0o700);
        let found = check(Some(dir.path().into()), current_uid()).unwrap();
        assert_eq!(found, dir.path());
    }

    #[test]
    fn each_problem_is_reported() {
        let uid = current_uid();
        assert_eq!(check(None, uid), Err(RuntimeDirProblem::Unset));
        assert_eq!(check(Some("".into()), uid), Err(RuntimeDirProblem::Unset));
        assert_eq!(
            check(Some("run/user".into()), uid),
            Err(RuntimeDirProblem::Relative)
        );

        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        assert_eq!(
            check(Some(missing.into()), uid),
            Err(RuntimeDirProblem::Missing)
        );

        let file = dir.path().join("file");
        fs::write(&file, b"").unwrap();
        assert_eq!(
            check(Some(file.into()), uid),
            Err(RuntimeDirProblem::NotADirectory)
        );

        for mode in [0o755, 0o600, 0o711] {
            let dir = folder_with_mode(mode);
            assert_eq!(
                check(Some(dir.path().into()), uid),
                Err(RuntimeDirProblem::WrongMode { mode })
            );
        }
    }

    #[test]
    fn the_sticky_bit_is_not_an_access_permission() {
        let dir = folder_with_mode(0o1700);
        assert!(check(Some(dir.path().into()), current_uid()).is_ok());
    }

    #[test]
    fn a_folder_of_another_user_is_refused() {
        let dir = folder_with_mode(0o700);
        let other = current_uid().wrapping_add(1);
        assert_eq!(
            check(Some(dir.path().into()), other),
            Err(RuntimeDirProblem::NotOwned {
                owner: current_uid()
            })
        );
    }
}
