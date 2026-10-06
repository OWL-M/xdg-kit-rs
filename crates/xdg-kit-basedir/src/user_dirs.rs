//! The XDG user folders (Desktop, Downloads, …) from `user-dirs.dirs`.
//!
//! `xdg-user-dirs-update` writes the file. This module only reads it, following
//! the reference parser that xdg-user-dirs ships for applications
//! (`src/xdg-user-dir-lookup.c`).

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::paths::normalize;
use crate::{BaseDirs, Diagnostic};

/// A user folder, by its name in `user-dirs.dirs` (`XDG_<NAME>_DIR`).
///
/// Lookups compare kinds by [`UserDirKind::name`], so `Other("MUSIC")` finds
/// the same folder as `Music`. New standard names may become variants in a
/// minor release, hence `#[non_exhaustive]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum UserDirKind {
    /// `DESKTOP`
    Desktop,
    /// `DOCUMENTS`
    Documents,
    /// `DOWNLOAD`
    Download,
    /// `MUSIC`
    Music,
    /// `PICTURES`
    Pictures,
    /// `PROJECTS`
    Projects,
    /// `PUBLICSHARE`
    PublicShare,
    /// `TEMPLATES`
    Templates,
    /// `VIDEOS`
    Videos,
    /// Any other name, as written between `XDG_` and `_DIR`.
    Other(String),
}

impl UserDirKind {
    /// The kind for a name as written between `XDG_` and `_DIR`.
    #[must_use]
    pub fn from_name(name: &str) -> Self {
        match name {
            "DESKTOP" => Self::Desktop,
            "DOCUMENTS" => Self::Documents,
            "DOWNLOAD" => Self::Download,
            "MUSIC" => Self::Music,
            "PICTURES" => Self::Pictures,
            "PROJECTS" => Self::Projects,
            "PUBLICSHARE" => Self::PublicShare,
            "TEMPLATES" => Self::Templates,
            "VIDEOS" => Self::Videos,
            other => Self::Other(other.to_owned()),
        }
    }

    /// The name as written between `XDG_` and `_DIR`.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Desktop => "DESKTOP",
            Self::Documents => "DOCUMENTS",
            Self::Download => "DOWNLOAD",
            Self::Music => "MUSIC",
            Self::Pictures => "PICTURES",
            Self::Projects => "PROJECTS",
            Self::PublicShare => "PUBLICSHARE",
            Self::Templates => "TEMPLATES",
            Self::Videos => "VIDEOS",
            Self::Other(name) => name,
        }
    }
}

/// The user folders read from `user-dirs.dirs`.
#[derive(Debug, Clone)]
pub struct UserDirs {
    home: PathBuf,
    dirs: Vec<(UserDirKind, PathBuf)>,
    diagnostics: Vec<Diagnostic>,
}

impl UserDirs {
    /// Reads `user-dirs.dirs` in `base_dirs`' config home. Returns no folders
    /// if the file is missing or unreadable, with a diagnostic if it exists
    /// but cannot be read.
    #[must_use]
    pub fn load(base_dirs: &BaseDirs) -> Self {
        let path = base_dirs.config_home().join("user-dirs.dirs");
        match fs::read(&path) {
            Ok(contents) => Self::from_bytes(base_dirs.home(), &contents),
            Err(err) => {
                let mut dirs = Self::from_bytes(base_dirs.home(), b"");
                if err.kind() != io::ErrorKind::NotFound {
                    dirs.diagnostics.push(Diagnostic::file(
                        path,
                        None,
                        format!("cannot be read: {err}"),
                    ));
                }
                dirs
            }
        }
    }

    /// Parses a `user-dirs.dirs` file, using `home` for `$HOME`.
    ///
    /// Follows the reference parser's line format: optional leading spaces
    /// and tabs, `XDG_<NAME>_DIR`, `=` with optional spaces and tabs around it,
    /// then a double-quoted value starting with `$HOME/` or `/`. A `\` takes
    /// the next character literally. Skips other lines, including `#`
    /// comments and values without a closing quote. A later line for a name
    /// replaces an earlier one.
    ///
    /// Differences from the reference concern truncated values. A NUL byte
    /// ends the line, as it does in the reference's C strings, leaving a value
    /// without a closing quote. Lines longer than the reference's 511-byte
    /// buffer are read whole rather than split. xdg-user-dirs 0.20 accepts
    /// truncated values; later versions skip them. This parser skips any
    /// value without a closing quote.
    #[must_use]
    pub fn from_bytes(home: &Path, contents: &[u8]) -> Self {
        let mut dirs: Vec<(UserDirKind, PathBuf)> = Vec::new();
        for line in contents.split(|&byte| byte == b'\n') {
            let Some((name, path)) = parse_line(home, line) else {
                continue;
            };
            let kind = UserDirKind::from_name(name);
            match dirs
                .iter_mut()
                .find(|(known, _)| known.name() == kind.name())
            {
                Some((_, known)) => *known = path,
                None => dirs.push((kind, path)),
            }
        }
        Self {
            home: home.to_owned(),
            dirs,
            diagnostics: Vec::new(),
        }
    }

    /// The folder for `kind`, if it is set and not disabled. Pointing a
    /// folder at the home folder itself disables it (xdg-user-dirs README:
    /// "To disable a directory, point it to the homedir").
    #[must_use]
    pub fn get(&self, kind: &UserDirKind) -> Option<&Path> {
        self.set(kind).filter(|path| *path != self.home)
    }

    /// Returns the same result as the reference lookup (`xdg_user_dir_lookup`):
    /// the configured folder, even if it is the home folder. If unset, returns
    /// `$HOME/Desktop` for `DESKTOP` and `$HOME` for any other name.
    #[must_use]
    pub fn get_or_fallback(&self, kind: &UserDirKind) -> PathBuf {
        match self.set(kind) {
            Some(path) => path.to_owned(),
            None if kind.name() == "DESKTOP" => self.home.join("Desktop"),
            None => self.home.clone(),
        }
    }

    /// Every folder that is set and not disabled, in the order in which each
    /// name first appears in the file.
    pub fn iter(&self) -> impl Iterator<Item = (&UserDirKind, &Path)> {
        self.dirs
            .iter()
            .map(|(kind, path)| (kind, path.as_path()))
            .filter(|(_, path)| *path != self.home)
    }

    /// Problems encountered while reading the file.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    fn set(&self, kind: &UserDirKind) -> Option<&Path> {
        self.dirs
            .iter()
            .find(|(known, _)| known.name() == kind.name())
            .map(|(_, path)| path.as_path())
    }
}

/// One line of `user-dirs.dirs`: the name and the folder, or `None` for a
/// line to skip.
fn parse_line<'a>(home: &Path, line: &'a [u8]) -> Option<(&'a str, PathBuf)> {
    // The reference reads C strings: a line ends at its first NUL.
    let line = line.split(|&byte| byte == 0).next().unwrap_or_default();
    let line = skip_blanks(line);
    let rest = line.strip_prefix(b"XDG_")?;
    let equals = rest.iter().position(|&byte| byte == b'=')?;
    let name = trim_blanks_end(&rest[..equals]).strip_suffix(b"_DIR")?;
    if name.is_empty() || name.iter().any(|&byte| byte == b' ' || byte == b'\t') {
        return None;
    }
    let name = std::str::from_utf8(name).ok()?;
    let value = skip_blanks(&rest[equals + 1..]).strip_prefix(b"\"")?;
    let (relative, value) = match value.strip_prefix(b"$HOME/") {
        Some(rest) => (true, rest),
        None if value.first() == Some(&b'/') => (false, value),
        None => return None,
    };
    // Concatenate home, a slash, and the value as the reference does.
    // Path joining would discard home for values such as `$HOME//Music`.
    let mut path = Vec::with_capacity(home.as_os_str().len() + 1 + value.len());
    if relative {
        path.extend_from_slice(home.as_os_str().as_bytes());
        path.push(b'/');
    }
    let mut bytes = value.iter();
    loop {
        match bytes.next()? {
            b'"' => break,
            b'\\' => path.push(*bytes.next()?),
            &byte => path.push(byte),
        }
    }
    Some((name, normalize(Path::new(OsStr::from_bytes(&path)))))
}

fn skip_blanks(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|&byte| byte != b' ' && byte != b'\t')
        .unwrap_or(bytes.len());
    &bytes[start..]
}

fn trim_blanks_end(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .rposition(|&byte| byte != b' ' && byte != b'\t')
        .map_or(0, |last| last + 1);
    &bytes[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/home/u";

    fn parse(contents: &str) -> UserDirs {
        UserDirs::from_bytes(Path::new(HOME), contents.as_bytes())
    }

    #[test]
    fn values_relative_to_home_and_absolute_ones_are_read() {
        let dirs = parse(concat!(
            "# This file is written by xdg-user-dirs-update\n",
            "XDG_DESKTOP_DIR=\"$HOME/Desktop\"\n",
            "XDG_DOWNLOAD_DIR=\"/data/downloads\"\n",
        ));
        assert_eq!(
            dirs.get(&UserDirKind::Desktop),
            Some(Path::new("/home/u/Desktop"))
        );
        assert_eq!(
            dirs.get(&UserDirKind::Download),
            Some(Path::new("/data/downloads"))
        );
        assert_eq!(dirs.get(&UserDirKind::Music), None);
    }

    #[test]
    fn blanks_around_the_parts_are_allowed() {
        let dirs = parse(" \tXDG_MUSIC_DIR \t= \t\"$HOME/Music\"\n");
        assert_eq!(
            dirs.get(&UserDirKind::Music),
            Some(Path::new("/home/u/Music"))
        );
    }

    #[test]
    fn a_backslash_takes_the_next_character() {
        let dirs = parse(r#"XDG_VIDEOS_DIR="$HOME/My \"Clips\" \\ more""#);
        assert_eq!(
            dirs.get(&UserDirKind::Videos),
            Some(Path::new(r#"/home/u/My "Clips" \ more"#))
        );
    }

    #[test]
    fn lines_the_reference_parser_skips_are_skipped() {
        let dirs = parse(concat!(
            "XDG_DESKTOP_DIR=\"Desktop\"\n",          // neither $HOME/ nor /
            "XDG_MUSIC_DIR=\"$HOME\"\n",              // $HOME without a slash
            "XDG_MUSIC_DIR=\"$HOME/Music\\\n",        // backslash, then the end
            "XDG_MUSIC_DIR=\"$HOME/a\0b\"\n",         // NUL ends the line first
            "XDG_MUSIC_DIR=$HOME/Music\n",            // no quotes
            "XDG_PICTURES_DIR=\"$HOME/Pictures\n",    // no closing quote
            "XDG_TEMPLATES=\"$HOME/Templates\"\n",    // no _DIR
            "XDG__DIR=\"$HOME/Nothing\"\n",           // empty name
            "#XDG_VIDEOS_DIR=\"$HOME/Videos\"\n",     // comment
            "XDG_DOCUMENTS_DIR\"$HOME/Documents\"\n", // no =
        ));
        assert_eq!(dirs.iter().count(), 0);
    }

    #[test]
    fn a_later_line_replaces_an_earlier_one_but_a_broken_one_does_not() {
        let dirs = parse(concat!(
            "XDG_MUSIC_DIR=\"$HOME/Old\"\n",
            "XDG_MUSIC_DIR=\"$HOME/New\"\n",
            "XDG_MUSIC_DIR=\"$HOME/Broken\n",
        ));
        assert_eq!(
            dirs.get(&UserDirKind::Music),
            Some(Path::new("/home/u/New"))
        );
    }

    #[test]
    fn a_line_longer_than_the_reference_buffer_is_read_whole() {
        // The reference reads lines into 512 bytes and would split this one.
        let name = "x".repeat(600);
        let dirs = parse(&format!("XDG_MUSIC_DIR=\"$HOME/{name}\"\n"));
        assert_eq!(
            dirs.get(&UserDirKind::Music),
            Some(Path::new("/home/u").join(&name).as_path())
        );
    }

    #[test]
    fn a_folder_set_to_home_is_disabled() {
        let dirs = parse(concat!(
            "XDG_TEMPLATES_DIR=\"$HOME/\"\n",
            "XDG_PUBLICSHARE_DIR=\"/home/u\"\n",
        ));
        assert_eq!(dirs.get(&UserDirKind::Templates), None);
        assert_eq!(dirs.get(&UserDirKind::PublicShare), None);
        assert_eq!(dirs.iter().count(), 0);
        assert_eq!(
            dirs.get_or_fallback(&UserDirKind::Templates),
            Path::new("/home/u")
        );
    }

    #[test]
    fn a_known_name_found_through_other_is_the_same_folder() {
        let dirs = parse("XDG_MUSIC_DIR=\"$HOME/Music\"\n");
        assert_eq!(
            dirs.get(&UserDirKind::Other("MUSIC".into())),
            Some(Path::new("/home/u/Music"))
        );
    }

    #[test]
    fn unknown_names_are_kept() {
        let dirs = parse("XDG_GAMES_DIR=\"$HOME/Games\"\n");
        let games = UserDirKind::from_name("GAMES");
        assert_eq!(games, UserDirKind::Other("GAMES".into()));
        assert_eq!(dirs.get(&games), Some(Path::new("/home/u/Games")));
        assert_eq!(games.name(), "GAMES");
    }

    #[test]
    fn a_slash_after_home_keeps_the_home_folder() {
        let dirs = parse(concat!(
            "XDG_MUSIC_DIR=\"$HOME//Music\"\n",
            "XDG_VIDEOS_DIR=\"$HOME/\\/Videos\"\n",
        ));
        assert_eq!(
            dirs.get(&UserDirKind::Music),
            Some(Path::new("/home/u/Music"))
        );
        assert_eq!(
            dirs.get(&UserDirKind::Videos),
            Some(Path::new("/home/u/Videos"))
        );
    }

    #[test]
    fn the_desktop_fallback_follows_the_name() {
        let dirs = parse("");
        assert_eq!(
            dirs.get_or_fallback(&UserDirKind::Other("DESKTOP".into())),
            Path::new("/home/u/Desktop")
        );
    }

    #[test]
    fn the_fallback_is_the_reference_one() {
        let dirs = parse("");
        assert_eq!(
            dirs.get_or_fallback(&UserDirKind::Desktop),
            Path::new("/home/u/Desktop")
        );
        assert_eq!(
            dirs.get_or_fallback(&UserDirKind::Music),
            Path::new("/home/u")
        );
    }

    #[test]
    fn known_names_round_trip() {
        for kind in [
            UserDirKind::Desktop,
            UserDirKind::Documents,
            UserDirKind::Download,
            UserDirKind::Music,
            UserDirKind::Pictures,
            UserDirKind::Projects,
            UserDirKind::PublicShare,
            UserDirKind::Templates,
            UserDirKind::Videos,
        ] {
            assert_eq!(UserDirKind::from_name(kind.name()), kind);
        }
    }

    #[test]
    fn loading_reads_the_config_home_and_tolerates_a_missing_file() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config");
        let vars = {
            let config = config.clone();
            move |name: &str| match name {
                "HOME" => Some(HOME.into()),
                "XDG_CONFIG_HOME" => Some(config.clone().into_os_string()),
                _ => None,
            }
        };
        let base = BaseDirs::from_vars(vars).unwrap();
        let missing = UserDirs::load(&base);
        assert_eq!(missing.iter().count(), 0);
        assert!(missing.diagnostics().is_empty());

        fs::create_dir(&config).unwrap();
        fs::write(
            config.join("user-dirs.dirs"),
            "XDG_MUSIC_DIR=\"$HOME/Music\"\n",
        )
        .unwrap();
        let loaded = UserDirs::load(&base);
        assert_eq!(
            loaded.get(&UserDirKind::Music),
            Some(Path::new("/home/u/Music"))
        );

        // A file that exists but cannot be read is reported
        fs::remove_file(config.join("user-dirs.dirs")).unwrap();
        fs::create_dir(config.join("user-dirs.dirs")).unwrap();
        let unreadable = UserDirs::load(&base);
        assert_eq!(unreadable.iter().count(), 0);
        assert_eq!(unreadable.diagnostics().len(), 1);
    }
}
