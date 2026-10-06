use std::path::{Component, Path, PathBuf};

use crate::Error;

/// `path` with repeated and trailing slashes and `.` components removed.
/// `..` is kept: resolving it needs the filesystem.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    path.components().collect()
}

/// Checks that `rel` is a plain relative path: not absolute, no `..`, and at
/// least one name in it (so not empty, and not just `.`).
pub(crate) fn check_relative(rel: &Path) -> Result<(), Error> {
    let plain = rel
        .components()
        .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
        && rel
            .components()
            .any(|component| matches!(component, Component::Normal(_)));
    if plain {
        Ok(())
    } else {
        Err(Error::InvalidRelativePath(rel.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizing_drops_trailing_and_repeated_slashes() {
        assert_eq!(
            normalize(Path::new("/usr//local/share/")).as_os_str(),
            "/usr/local/share"
        );
        assert_eq!(normalize(Path::new("/")).as_os_str(), "/");
    }

    #[test]
    fn only_plain_relative_paths_pass() {
        assert!(check_relative(Path::new("mime/packages")).is_ok());
        assert!(check_relative(Path::new("./applications")).is_ok());
        for bad in ["", ".", "./", "/etc/xdg", "../outside", "a/../../b"] {
            assert!(
                matches!(
                    check_relative(Path::new(bad)),
                    Err(Error::InvalidRelativePath(_))
                ),
                "{bad:?} passed"
            );
        }
    }
}
