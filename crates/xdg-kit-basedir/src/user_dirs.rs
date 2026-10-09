//! The XDG user folders (Desktop, Downloads, …) from `user-dirs.dirs`.
//!
//! `xdg-user-dirs-update` writes the file. This module only reads it, following
//! the reference parser that xdg-user-dirs ships for applications
//! (`src/xdg-user-dir-lookup.c`).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::paths::normalize;
use crate::work;
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
#[derive(Clone)]
pub struct UserDirs {
    home: PathBuf,
    dirs: Vec<(UserDirKind, PathBuf)>,
    // Where each name is in `dirs`. A name is its kind's name, so this
    // matches kinds as lookups do.
    index: HashMap<String, usize>,
    diagnostics: Vec<Diagnostic>,
}

impl std::fmt::Debug for UserDirs {
    /// The folders in file order; the index into them is left out.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserDirs")
            .field("home", &self.home)
            .field("dirs", &self.dirs)
            .field("diagnostics", &self.diagnostics)
            .finish_non_exhaustive()
    }
}

impl UserDirs {
    /// Reads `user-dirs.dirs` in `base_dirs`' config home. Returns no folders
    /// if the file is missing or unreadable, with a diagnostic if it exists
    /// but cannot be read.
    #[must_use]
    pub fn load(base_dirs: &BaseDirs) -> Self {
        let path = base_dirs.config_home().join("user-dirs.dirs");
        match crate::read_regular_file(&path) {
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
        let mut index: HashMap<String, usize> = HashMap::new();
        for line in contents.split(|&byte| byte == b'\n') {
            // The line, and each byte the split went over.
            work::count(1 + line.len());
            let Some((name, path)) = parse_line(home, line) else {
                continue;
            };
            // The lookup, and each byte of the name hashed and compared.
            work::count(1 + 2 * name.len());
            if let Some(&at) = index.get(name) {
                dirs[at].1 = path;
            } else {
                // Each byte of a new name copied into the key and the kind,
                // and hashed and compared again to insert it.
                work::count(4 * name.len());
                index.insert(name.to_owned(), dirs.len());
                dirs.push((UserDirKind::from_name(name), path));
            }
        }
        // Each byte of home copied.
        work::count(home.as_os_str().len());
        Self {
            home: home.to_owned(),
            dirs,
            index,
            diagnostics: Vec::new(),
        }
    }

    /// The folder for `kind`, if it is set and not disabled. Pointing a
    /// folder at the home folder itself disables it (xdg-user-dirs README:
    /// "To disable a directory, point it to the homedir").
    #[must_use]
    pub fn get(&self, kind: &UserDirKind) -> Option<&Path> {
        // Each byte of home compared.
        work::count(self.home.as_os_str().len());
        self.set(kind).filter(|path| *path != self.home)
    }

    /// Returns the same result as the reference lookup (`xdg_user_dir_lookup`):
    /// the configured folder, even if it is the home folder. If unset, returns
    /// `$HOME/Desktop` for `DESKTOP` and `$HOME` for any other name.
    #[must_use]
    pub fn get_or_fallback(&self, kind: &UserDirKind) -> PathBuf {
        // Each byte of the folder copied.
        let found = match self.set(kind) {
            Some(path) => path.to_owned(),
            None if kind.name() == "DESKTOP" => self.home.join("Desktop"),
            None => self.home.clone(),
        };
        work::count(found.as_os_str().len());
        found
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
        // The lookup, and each byte of the name hashed and compared.
        work::count(1 + 2 * kind.name().len());
        self.index
            .get(kind.name())
            .map(|&at| self.dirs[at].1.as_path())
    }
}

/// One line of `user-dirs.dirs`: the name and the folder, or `None` for a
/// line to skip.
fn parse_line<'a>(home: &Path, line: &'a [u8]) -> Option<(&'a str, PathBuf)> {
    // The reference reads C strings: a line ends at its first NUL.
    let line = line
        .split(|&byte| {
            work::count(1);
            byte == 0
        })
        .next()
        .unwrap_or_default();
    let line = skip_blanks(line);
    let rest = line.strip_prefix(b"XDG_")?;
    let equals = rest
        .iter()
        .inspect(|_| work::count(1))
        .position(|&byte| byte == b'=')?;
    let name = trim_blanks_end(&rest[..equals]).strip_suffix(b"_DIR")?;
    if name.is_empty()
        || name
            .iter()
            .inspect(|_| work::count(1))
            .any(|&byte| byte == b' ' || byte == b'\t')
    {
        return None;
    }
    // Each byte `from_utf8` checks.
    work::count(name.len());
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
        // Each byte of home copied.
        work::count(home.as_os_str().len());
        path.extend_from_slice(home.as_os_str().as_bytes());
        path.push(b'/');
    }
    let mut bytes = value.iter();
    loop {
        // Each byte of the value looked at and copied; a `\` and the byte
        // it takes count one each.
        work::count(1);
        match bytes.next()? {
            b'"' => break,
            b'\\' => {
                work::count(1);
                path.push(*bytes.next()?);
            }
            &byte => path.push(byte),
        }
    }
    Some((name, normalize(Path::new(OsStr::from_bytes(&path)))))
}

fn skip_blanks(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .inspect(|_| work::count(1))
        .position(|&byte| byte != b' ' && byte != b'\t')
        .unwrap_or(bytes.len());
    &bytes[start..]
}

fn trim_blanks_end(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .inspect(|_| work::count(1))
        .rposition(|&byte| byte != b' ' && byte != b'\t')
        .map_or(0, |last| last + 1);
    &bytes[..end]
}

#[cfg(test)]
mod tests {
    use std::fs;

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
            "XDG_VIDEOS_DIR=\"$HOME/Videos\"\n",
            "XDG_MUSIC_DIR=\"$HOME/New\"\n",
            "XDG_MUSIC_DIR=\"$HOME/Broken\n",
        ));
        assert_eq!(
            dirs.get(&UserDirKind::Music),
            Some(Path::new("/home/u/New"))
        );
        // The replaced name keeps the place where it first appeared.
        let order: Vec<_> = dirs.iter().collect();
        assert_eq!(
            order,
            [
                (&UserDirKind::Music, Path::new("/home/u/New")),
                (&UserDirKind::Videos, Path::new("/home/u/Videos")),
            ]
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

    /// The bound for parsing `text`: twice the analysed cost of its lines,
    /// with `name` bytes in names, `value` bytes in values (closing quotes
    /// included) and `home` copied `homes` times. Per line ten steps: the
    /// line, the two blank skips and the trim, the lookup, and the five
    /// bytes `_DIR=` that the `=` search looks at after the name. Per byte:
    /// two for the line and NUL splits; nine more for a new name (the `=`
    /// search, the blank check, `from_utf8`, hashed and compared, copied
    /// twice, hashed and compared again); three more for a value (the value
    /// loop, normalized); three for each home copied (copied, normalized).
    fn bound(text: &str, name: usize, value: usize, homes: usize) -> usize {
        let lines = text.split('\n').count();
        2 * (10 * lines + 2 * text.len() + 9 * name + 3 * value + 3 * homes * HOME.len())
    }

    /// `n` lines, each with its own name, and the bytes of their names and
    /// of their values.
    fn many_names(n: usize) -> (String, usize, usize) {
        use std::fmt::Write as _;
        let mut text = String::new();
        let (mut name, mut value) = (0, 0);
        for at in 0..n {
            let _ = writeln!(text, "XDG_N{at}_DIR=\"$HOME/n{at}\"");
            name += format!("N{at}").len();
            value += format!("n{at}\"").len();
        }
        (text, name, value)
    }

    #[test]
    fn large_files_are_read_in_linear_time() {
        work::assert_linear(
            10_000,
            |n| {
                let (text, _, _) = many_names(n);
                let (dirs, steps) = work::steps(|| parse(&text));
                assert_eq!(dirs.iter().count(), n);
                (steps, text.len())
            },
            |n, _| {
                let (text, name, value) = many_names(n);
                bound(&text, name, value, n + 1)
            },
        );
    }

    #[test]
    fn lookups_take_constant_time() {
        work::assert_linear(
            10_000,
            |n| {
                let (text, _, _) = many_names(n);
                let dirs = parse(&text);
                let kinds: Vec<_> = (0..=n)
                    .map(|at| UserDirKind::from_name(&format!("N{at}")))
                    .collect();
                let (found, steps) = work::steps(|| {
                    kinds
                        .iter()
                        .filter(|kind| {
                            dirs.get(kind).is_some()
                                && dirs.get_or_fallback(kind) != Path::new(HOME)
                        })
                        .count()
                });
                assert_eq!(found, n);
                let names = kinds.iter().map(|kind| kind.name().len()).sum();
                (steps, names)
            },
            // Twice the analysed cost. Per kind (`n + 1` of them): two
            // lookups of one step each, home compared, and the folder (home,
            // a slash and about the name) copied. Per byte of a name: hashed
            // and compared by each lookup, and copied with the folder.
            |n, names| 2 * ((2 + HOME.len() + HOME.len() + 1) * (n + 1) + 5 * names),
        );
    }

    #[test]
    fn a_long_value_is_read_in_linear_time() {
        // Plain bytes, slashes and escaped quotes: each branch of the value
        // loop, and normalizing.
        let long = |n: usize| "a\\\"b/".repeat(n);
        let text = |n: usize| format!("XDG_DESKTOP_DIR=\"$HOME/{}\"\n", long(n));
        work::assert_linear(
            200_000,
            |n| {
                let (long, text) = (long(n), text(n));
                let (dirs, steps) = work::steps(|| parse(&text));
                let expected = long.replace('\\', "");
                assert_eq!(
                    dirs.get(&UserDirKind::Desktop),
                    Some(
                        Path::new(HOME)
                            .join(expected.trim_end_matches('/'))
                            .as_path()
                    )
                );
                (steps, long.len())
            },
            |n, long| bound(&text(n), "DESKTOP".len(), long + 1, 2),
        );
    }

    #[test]
    fn a_long_name_is_read_and_looked_up_in_linear_time() {
        // One name of about 1 MB, through each step on a name: the `=`
        // search, the blank check, `from_utf8`, hashing, the copies into
        // the index and the kind, and the lookups of `get` and
        // `get_or_fallback`, for that name and for one that is not set.
        let name = |n: usize| "Ab_".repeat(n);
        let text = |n: usize| format!("XDG_{}_DIR=\"$HOME/x\"\n", name(n));
        work::assert_linear(
            350_000,
            |n| {
                let (name, text) = (name(n), text(n));
                let set = UserDirKind::from_name(&name);
                let unset = UserDirKind::from_name(&format!("{name}Z"));
                let (dirs, parsing) = work::steps(|| parse(&text));
                let (found, lookups) = work::steps(|| {
                    (
                        dirs.get(&set),
                        dirs.get_or_fallback(&set),
                        dirs.get(&unset),
                        dirs.get_or_fallback(&unset),
                    )
                });
                let folder = Path::new("/home/u/x");
                assert_eq!(
                    found,
                    (Some(folder), folder.to_owned(), None, PathBuf::from(HOME))
                );
                assert_eq!(dirs.iter().count(), 1);
                (parsing + lookups, name.len())
            },
            // Parsing the line, with the value `x"` and home copied twice;
            // then twice the analysed cost of four lookups: one step each,
            // home compared or copied with at most two more bytes, and each
            // byte of the name (one more for the unset one) hashed and
            // compared.
            |n, name| {
                bound(&text(n), name, "x\"".len(), 2)
                    + 2 * (4 * (1 + HOME.len() + 2) + 4 * 2 * (name + 1))
            },
        );
    }

    #[cfg(not(target_vendor = "apple"))]
    #[test]
    fn a_fifo_is_reported_not_waited_on() {
        use crate::testing::{fifo, within};

        let root = tempfile::tempdir().unwrap();
        let config = root.path().to_owned();
        fifo(&config.join("user-dirs.dirs"));
        let base = BaseDirs::from_vars(move |name: &str| match name {
            "HOME" => Some(HOME.into()),
            "XDG_CONFIG_HOME" => Some(config.clone().into_os_string()),
            _ => None,
        })
        .unwrap();
        let dirs = within(std::time::Duration::from_secs(10), move || {
            UserDirs::load(&base)
        });
        assert_eq!(dirs.iter().count(), 0);
        assert_eq!(dirs.diagnostics().len(), 1);
    }
}
