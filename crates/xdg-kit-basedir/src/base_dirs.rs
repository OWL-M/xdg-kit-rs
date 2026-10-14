use std::ffi::{OsStr, OsString};
use std::fs::DirBuilder;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use crate::__hashed::{PathKey, Set};
use crate::error::quoted;
use crate::paths::{check_relative, normalize};
use crate::runtime::{self, RuntimeDirProblem};
use crate::work::{self, Counted};
use crate::{Diagnostic, Error, Limits};

/// What [`BaseDirs::read_config`] and [`BaseDirs::read_data`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReadFile {
    /// The most important file that could be read, and its contents;
    /// `None` when no folder has one.
    pub file: Option<(PathBuf, Vec<u8>)>,
    /// The files skipped because they may not be read, most important
    /// first: those before `file`, or every one when `file` is `None`.
    /// Missing files are not listed.
    pub skipped: Vec<Diagnostic>,
}

const DEFAULT_DATA_DIRS: &[&str] = &["/usr/local/share", "/usr/share"];
const DEFAULT_CONFIG_DIRS: &[&str] = &["/etc/xdg"];

/// The base directories of the XDG Base Directory spec (version 0.8), read
/// once from the environment.
///
/// Use [`BaseDirs::from_env`] to read the environment or
/// [`BaseDirs::from_vars`] to supply variables, then pass the result to callers.
#[derive(Debug, Clone)]
pub struct BaseDirs {
    home: PathBuf,
    data_home: PathBuf,
    config_home: PathBuf,
    state_home: PathBuf,
    cache_home: PathBuf,
    executable_home: PathBuf,
    data_dirs: Vec<PathBuf>,
    config_dirs: Vec<PathBuf>,
    runtime_dir: Result<PathBuf, RuntimeDirProblem>,
    diagnostics: Vec<Diagnostic>,
}

impl BaseDirs {
    /// Reads the base directories from this process's environment.
    ///
    /// Uses `$HOME` when it is set and absolute. If `$HOME` is unset or empty,
    /// uses the home folder from the user's password database entry.
    ///
    /// # Errors
    ///
    /// [`Error::NoHome`] when neither gives an absolute path.
    pub fn from_env() -> Result<Self, Error> {
        Self::build(
            &|name| std::env::var_os(name),
            std::env::home_dir(),
            runtime::current_uid(),
        )
    }

    /// Reads the base directories from `vars`, which returns the value of an
    /// environment variable by name.
    ///
    /// Unlike [`BaseDirs::from_env`], the home folder comes only from `HOME`.
    /// `XDG_RUNTIME_DIR` is still checked against this filesystem and this
    /// process's effective user ID.
    ///
    /// # Errors
    ///
    /// [`Error::NoHome`] when `HOME` is unset, empty or relative.
    pub fn from_vars(vars: impl Fn(&str) -> Option<OsString>) -> Result<Self, Error> {
        Self::build(&vars, None, runtime::current_uid())
    }

    fn build(
        vars: &dyn Fn(&str) -> Option<OsString>,
        home_fallback: Option<PathBuf>,
        uid: u32,
    ) -> Result<Self, Error> {
        let mut diagnostics = Vec::new();
        let home = home(vars, home_fallback, &mut diagnostics)?;
        // A folder under home: each byte of home copied (the fixed name
        // after it is bounded).
        let under_home = |default: &str| {
            work::count(home.as_os_str().len());
            home.join(default)
        };
        let single = |name: &str, default: &str, diagnostics: &mut Vec<Diagnostic>| {
            single(vars, name, || under_home(default), diagnostics)
        };
        Ok(Self {
            data_home: single("XDG_DATA_HOME", ".local/share", &mut diagnostics),
            config_home: single("XDG_CONFIG_HOME", ".config", &mut diagnostics),
            state_home: single("XDG_STATE_HOME", ".local/state", &mut diagnostics),
            cache_home: single("XDG_CACHE_HOME", ".cache", &mut diagnostics),
            executable_home: under_home(".local/bin"),
            data_dirs: list(vars, "XDG_DATA_DIRS", DEFAULT_DATA_DIRS, &mut diagnostics),
            config_dirs: list(
                vars,
                "XDG_CONFIG_DIRS",
                DEFAULT_CONFIG_DIRS,
                &mut diagnostics,
            ),
            runtime_dir: runtime::check(vars("XDG_RUNTIME_DIR"), uid),
            home,
            diagnostics,
        })
    }

    /// The user's home folder.
    #[must_use]
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// `$XDG_DATA_HOME`, by default `$HOME/.local/share`.
    #[must_use]
    pub fn data_home(&self) -> &Path {
        &self.data_home
    }

    /// `$XDG_CONFIG_HOME`, by default `$HOME/.config`.
    #[must_use]
    pub fn config_home(&self) -> &Path {
        &self.config_home
    }

    /// `$XDG_STATE_HOME`, by default `$HOME/.local/state`.
    #[must_use]
    pub fn state_home(&self) -> &Path {
        &self.state_home
    }

    /// `$XDG_CACHE_HOME`, by default `$HOME/.cache`.
    #[must_use]
    pub fn cache_home(&self) -> &Path {
        &self.cache_home
    }

    /// `$HOME/.local/bin`, where the spec says user-specific executables may
    /// be placed. No variable sets it.
    #[must_use]
    pub fn executable_home(&self) -> &Path {
        &self.executable_home
    }

    /// `$XDG_DATA_DIRS`, most important first, by default
    /// `/usr/local/share` and `/usr/share`. Duplicate entries keep their
    /// first position.
    #[must_use]
    pub fn data_dirs(&self) -> &[PathBuf] {
        &self.data_dirs
    }

    /// `$XDG_CONFIG_DIRS`, most important first, by default `/etc/xdg`.
    /// Duplicate entries keep their first position.
    #[must_use]
    pub fn config_dirs(&self) -> &[PathBuf] {
        &self.config_dirs
    }

    /// `$XDG_RUNTIME_DIR`, if it meets the spec's requirements: an absolute
    /// path to an existing folder, owned by this user, with access mode
    /// `0700`. Checked once, when this value was built.
    ///
    /// # Errors
    ///
    /// A [`RuntimeDirProblem`] describing why the directory cannot be used.
    pub fn runtime_dir(&self) -> Result<&Path, RuntimeDirProblem> {
        self.runtime_dir.as_deref().map_err(|problem| *problem)
    }

    /// Values that were ignored while reading the environment.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// The most important existing `rel` under the data directories:
    /// `$XDG_DATA_HOME`, then each of `$XDG_DATA_DIRS`. A folder is searched
    /// once, even when it is listed again, and the search stops at the
    /// first file found. Only checks that the file exists, not that it can
    /// be read: [`BaseDirs::read_data`] reads the first that can be.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRelativePath`] when `rel` is absolute, contains `..`,
    /// or names nothing (empty, or just `.`).
    pub fn find_data(&self, rel: impl AsRef<Path>) -> Result<Option<PathBuf>, Error> {
        Ok(existing(&self.data_home, &self.data_dirs, rel.as_ref())?.next())
    }

    /// Every existing `rel` under the data directories, most important first.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRelativePath`] when `rel` is absolute, contains `..`,
    /// or names nothing (empty, or just `.`).
    pub fn data_files(&self, rel: impl AsRef<Path>) -> Result<Vec<PathBuf>, Error> {
        Ok(existing(&self.data_home, &self.data_dirs, rel.as_ref())?.collect())
    }

    /// The most important existing `rel` under the config directories:
    /// `$XDG_CONFIG_HOME`, then each of `$XDG_CONFIG_DIRS`. A folder is
    /// searched once, even when it is listed again, and the search stops
    /// at the first file found. Only checks that the file exists, not that
    /// it can be read: [`BaseDirs::read_config`] reads the first that can
    /// be.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRelativePath`] when `rel` is absolute, contains `..`,
    /// or names nothing (empty, or just `.`).
    pub fn find_config(&self, rel: impl AsRef<Path>) -> Result<Option<PathBuf>, Error> {
        Ok(existing(&self.config_home, &self.config_dirs, rel.as_ref())?.next())
    }

    /// Every existing `rel` under the config directories, most important
    /// first.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRelativePath`] when `rel` is absolute, contains `..`,
    /// or names nothing (empty, or just `.`).
    pub fn config_files(&self, rel: impl AsRef<Path>) -> Result<Vec<PathBuf>, Error> {
        Ok(existing(&self.config_home, &self.config_dirs, rel.as_ref())?.collect())
    }

    /// Reads the highest-priority readable `rel` under the data directories,
    /// in [`BaseDirs::find_data`]'s order, within the default [`Limits`].
    ///
    /// As the spec requires, skips a folder whose file is missing or may not
    /// be read: missing files or folders (`NotFound`, `NotADirectory`)
    /// silently, permission failures
    /// ([`io::ErrorKind::PermissionDenied`](std::io::ErrorKind::PermissionDenied))
    /// with a diagnostic in [`ReadFile::skipped`]. Every other
    /// failure stops the search, including an overlong name or a link loop in
    /// any path component. A damaged file is never silently passed over for a
    /// lower-priority one.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRelativePath`] for a bad `rel`; [`Error::Io`] when a
    /// file cannot be read for another reason ([`crate::read_regular_file`]:
    /// not a regular file, too large, a name too long, a link loop, an I/O
    /// error).
    pub fn read_data(&self, rel: impl AsRef<Path>) -> Result<ReadFile, Error> {
        self.read_data_with(rel, &Limits::default())
    }

    /// As [`BaseDirs::read_data`], within `limits`.
    ///
    /// # Errors
    ///
    /// As [`BaseDirs::read_data`].
    pub fn read_data_with(
        &self,
        rel: impl AsRef<Path>,
        limits: &Limits,
    ) -> Result<ReadFile, Error> {
        first_readable(&self.data_home, &self.data_dirs, rel.as_ref(), limits)
    }

    /// As [`BaseDirs::read_data`], under the config directories, in
    /// [`BaseDirs::find_config`]'s order.
    ///
    /// ```no_run
    /// use xdg_kit_basedir::BaseDirs;
    ///
    /// let dirs = BaseDirs::from_env()?;
    /// let read = dirs.read_config("app/settings.ini")?;
    /// for skipped in &read.skipped {
    ///     eprintln!("{skipped:?}");
    /// }
    /// if let Some((path, bytes)) = read.file {
    ///     println!("{}: {} bytes", path.display(), bytes.len());
    /// }
    /// # Ok::<(), xdg_kit_basedir::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// As [`BaseDirs::read_data`].
    pub fn read_config(&self, rel: impl AsRef<Path>) -> Result<ReadFile, Error> {
        self.read_config_with(rel, &Limits::default())
    }

    /// As [`BaseDirs::read_config`], within `limits`.
    ///
    /// # Errors
    ///
    /// As [`BaseDirs::read_data`].
    pub fn read_config_with(
        &self,
        rel: impl AsRef<Path>,
        limits: &Limits,
    ) -> Result<ReadFile, Error> {
        first_readable(&self.config_home, &self.config_dirs, rel.as_ref(), limits)
    }

    /// The path to write `rel` under `$XDG_DATA_HOME`, creating missing
    /// parent folders with mode `0700`. Existing folders are left as they
    /// are; the file itself is not created.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRelativePath`] for a bad `rel`, [`Error::Io`] when a
    /// folder cannot be created.
    pub fn place_data(&self, rel: impl AsRef<Path>) -> Result<PathBuf, Error> {
        place(&self.data_home, rel.as_ref())
    }

    /// As [`BaseDirs::place_data`], under `$XDG_CONFIG_HOME`.
    ///
    /// # Errors
    ///
    /// As [`BaseDirs::place_data`].
    pub fn place_config(&self, rel: impl AsRef<Path>) -> Result<PathBuf, Error> {
        place(&self.config_home, rel.as_ref())
    }

    /// As [`BaseDirs::place_data`], under `$XDG_STATE_HOME`.
    ///
    /// # Errors
    ///
    /// As [`BaseDirs::place_data`].
    pub fn place_state(&self, rel: impl AsRef<Path>) -> Result<PathBuf, Error> {
        place(&self.state_home, rel.as_ref())
    }

    /// As [`BaseDirs::place_data`], under `$XDG_CACHE_HOME`.
    ///
    /// # Errors
    ///
    /// As [`BaseDirs::place_data`].
    pub fn place_cache(&self, rel: impl AsRef<Path>) -> Result<PathBuf, Error> {
        place(&self.cache_home, rel.as_ref())
    }
}

/// Formats a diagnostic for an ignored relative path `value`. Reads only
/// the prefix [`quoted`] can show: 32 characters and enough to check for
/// more. Each character, including invalid UTF-8 bytes displayed as one
/// character, takes at most four bytes.
fn ignored(value: &OsStr) -> String {
    let bytes = value.as_bytes();
    let start = String::from_utf8_lossy(&bytes[..bytes.len().min(33 * 4)]);
    format!("relative path {} ignored", quoted(&start))
}

fn home(
    vars: &dyn Fn(&str) -> Option<OsString>,
    fallback: Option<PathBuf>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<PathBuf, Error> {
    match vars("HOME").filter(|value| !value.is_empty()) {
        Some(value) if Path::new(&value).is_absolute() => return Ok(normalize(Path::new(&value))),
        Some(value) => {
            let message = ignored(&value);
            // Each byte of the message.
            work::count(message.len());
            diagnostics.push(Diagnostic::variable("HOME", message));
        }
        None => {}
    }
    fallback
        .filter(|path| path.is_absolute())
        .map(|path| normalize(&path))
        .ok_or(Error::NoHome)
}

/// Reads a single directory, using the default if unset, empty or relative.
fn single(
    vars: &dyn Fn(&str) -> Option<OsString>,
    name: &str,
    default: impl FnOnce() -> PathBuf,
    diagnostics: &mut Vec<Diagnostic>,
) -> PathBuf {
    match vars(name).filter(|value| !value.is_empty()) {
        Some(value) if Path::new(&value).is_absolute() => normalize(Path::new(&value)),
        Some(value) => {
            let message = ignored(&value);
            // Each byte of the message.
            work::count(message.len());
            diagnostics.push(Diagnostic::variable(name, message));
            default()
        }
        None => default(),
    }
}

/// Splits a directory list on `:`, skipping empty and relative entries.
/// Keeps the first occurrence of each normalized path without resolving
/// symbolic links. Uses the default if the variable is unset, empty, or has
/// no valid entries.
fn list(
    vars: &dyn Fn(&str) -> Option<OsString>,
    name: &str,
    default: &[&str],
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut seen = Set::<PathKey<Counted>>::new();
    if let Some(value) = vars(name) {
        for item in value.as_bytes().split(|&byte| byte == b':') {
            // The item, and each byte the split went over.
            work::count(1 + item.len());
            let item = Path::new(OsStr::from_bytes(item));
            if item.as_os_str().is_empty() {
                continue;
            }
            if item.is_absolute() {
                let item = normalize(item);
                // The check; the set counts each byte hashed, compared on a
                // match and copied if new.
                work::count(1);
                if seen.insert(&item) {
                    dirs.push(item);
                }
            } else {
                let message = ignored(item.as_os_str());
                // Each byte of the message.
                work::count(message.len());
                diagnostics.push(Diagnostic::variable(name, message));
            }
        }
    }
    if dirs.is_empty() {
        dirs = default.iter().map(PathBuf::from).collect();
    }
    dirs
}

/// Yields existing paths for `rel` under `home`, then `dirs`, in order of
/// importance. Checks one path at a time.
fn existing<'a>(
    home: &'a Path,
    dirs: &'a [PathBuf],
    rel: &'a Path,
) -> Result<impl Iterator<Item = PathBuf> + 'a, Error> {
    // Only a path whose stat succeeds: one whose stat fails, for whatever
    // reason (missing, a folder on the way that may not be searched, a
    // link loop), is skipped. Whether the file can be read is not checked;
    // `first_readable` does that.
    Ok(candidates(home, dirs, rel)?.filter(|path| matches!(path.try_exists(), Ok(true))))
}

/// Reads the first of [`candidates`] that can be read, within `limits`
/// ([`BaseDirs::read_data`]).
fn first_readable(
    home: &Path,
    dirs: &[PathBuf],
    rel: &Path,
    limits: &Limits,
) -> Result<ReadFile, Error> {
    use std::io::ErrorKind;
    let mut skipped = Vec::new();
    for path in candidates(home, dirs, rel)? {
        match crate::read_regular_file_with(&path, limits.max_file_len) {
            Ok(bytes) => {
                return Ok(ReadFile {
                    file: Some((path, bytes)),
                    skipped,
                });
            }
            Err(err) if matches!(err.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {}
            Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                skipped.push(Diagnostic::file(
                    path,
                    None,
                    format!("cannot be read: {err}"),
                ));
            }
            Err(source) => return Err(Error::Io { path, source }),
        }
    }
    Ok(ReadFile {
        file: None,
        skipped,
    })
}

/// Yields the paths of `rel` under `home`, then `dirs`, in order of
/// importance, each folder once.
fn candidates<'a>(
    home: &'a Path,
    dirs: &'a [PathBuf],
    rel: &'a Path,
) -> Result<impl Iterator<Item = PathBuf> + 'a, Error> {
    check_relative(rel)?;
    // The list already has no duplicates. Search home first and skip any
    // matching list entry.
    Ok(std::iter::once(home)
        .chain(
            dirs.iter()
                .map(PathBuf::as_path)
                .filter(move |dir| *dir != home),
        )
        .map(move |base| {
            // The folder, compared with home, then joined with `rel`.
            work::count(1 + 2 * base.as_os_str().len() + rel.as_os_str().len());
            base.join(rel)
        }))
}

fn place(base: &Path, rel: &Path) -> Result<PathBuf, Error> {
    check_relative(rel)?;
    let path = base.join(rel);
    if let Some(parent) = path.parent() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|source| Error::Io {
                path: parent.to_owned(),
                source,
            })?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::io;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::DiagnosticSource;

    fn dirs(vars: &[(&str, &[u8])]) -> Result<BaseDirs, Error> {
        let vars: HashMap<String, OsString> = vars
            .iter()
            .map(|(name, value)| ((*name).to_owned(), OsStr::from_bytes(value).to_owned()))
            .collect();
        BaseDirs::from_vars(move |name| vars.get(name).cloned())
    }

    fn paths(list: &[&str]) -> Vec<PathBuf> {
        list.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn unset_variables_take_the_defaults() {
        let dirs = dirs(&[("HOME", b"/home/u")]).unwrap();
        assert_eq!(dirs.home(), Path::new("/home/u"));
        assert_eq!(dirs.data_home(), Path::new("/home/u/.local/share"));
        assert_eq!(dirs.config_home(), Path::new("/home/u/.config"));
        assert_eq!(dirs.state_home(), Path::new("/home/u/.local/state"));
        assert_eq!(dirs.cache_home(), Path::new("/home/u/.cache"));
        assert_eq!(dirs.executable_home(), Path::new("/home/u/.local/bin"));
        assert_eq!(dirs.data_dirs(), paths(&["/usr/local/share", "/usr/share"]));
        assert_eq!(dirs.config_dirs(), paths(&["/etc/xdg"]));
        assert_eq!(dirs.runtime_dir(), Err(RuntimeDirProblem::Unset));
        assert_eq!(dirs.diagnostics(), []);
    }

    #[test]
    fn empty_variables_take_the_defaults() {
        let dirs = dirs(&[
            ("HOME", b"/home/u"),
            ("XDG_DATA_HOME", b""),
            ("XDG_DATA_DIRS", b""),
            ("XDG_CONFIG_DIRS", b""),
        ])
        .unwrap();
        assert_eq!(dirs.data_home(), Path::new("/home/u/.local/share"));
        assert_eq!(dirs.data_dirs(), paths(&["/usr/local/share", "/usr/share"]));
        assert_eq!(dirs.config_dirs(), paths(&["/etc/xdg"]));
        assert_eq!(dirs.diagnostics(), []);
    }

    #[test]
    fn absolute_values_are_used_without_trailing_slashes() {
        let dirs = dirs(&[
            ("HOME", b"/home/u/"),
            ("XDG_CONFIG_HOME", b"/cfg/"),
            ("XDG_DATA_DIRS", b"/a/::/b//c/"),
        ])
        .unwrap();
        assert_eq!(dirs.home().as_os_str(), "/home/u");
        assert_eq!(dirs.config_home().as_os_str(), "/cfg");
        let data: Vec<_> = dirs.data_dirs().iter().map(|p| p.as_os_str()).collect();
        assert_eq!(data, ["/a", "/b/c"]);
    }

    #[test]
    fn relative_values_are_ignored_and_reported() {
        let dirs = dirs(&[
            ("HOME", b"/home/u"),
            ("XDG_CACHE_HOME", b"cache"),
            ("XDG_DATA_DIRS", b"share:/opt/share"),
        ])
        .unwrap();
        assert_eq!(dirs.cache_home(), Path::new("/home/u/.cache"));
        assert_eq!(dirs.data_dirs(), paths(&["/opt/share"]));
        let sources: Vec<_> = dirs
            .diagnostics()
            .iter()
            .map(|d| d.source.clone())
            .collect();
        assert_eq!(
            sources,
            [
                DiagnosticSource::Variable("XDG_CACHE_HOME".into()),
                DiagnosticSource::Variable("XDG_DATA_DIRS".into()),
            ]
        );
    }

    #[test]
    fn relative_values_are_shown_escaped_and_cut() {
        let bad = format!("a\x1b\n{}", "b".repeat(5000));
        let vars: HashMap<&str, OsString> = [
            ("HOME", bad.clone()),
            ("XDG_CACHE_HOME", bad.clone()),
            ("XDG_DATA_DIRS", format!("{bad}:/opt/share")),
        ]
        .into_iter()
        .map(|(name, value)| (name, value.into()))
        .collect();
        let dirs = BaseDirs::build(
            &|name| vars.get(name).cloned(),
            Some(PathBuf::from("/home/u")),
            runtime::current_uid(),
        )
        .unwrap();
        let shown = format!("relative path a\\u{{1b}}\\n{}… ignored", "b".repeat(29));
        let messages: Vec<_> = dirs
            .diagnostics()
            .iter()
            .map(|d| d.message.as_str())
            .collect();
        assert_eq!(messages, [shown.as_str(); 3]);
    }

    #[test]
    fn a_list_left_without_items_takes_the_default() {
        let dirs = dirs(&[("HOME", b"/home/u"), ("XDG_CONFIG_DIRS", b"xdg:etc")]).unwrap();
        assert_eq!(dirs.config_dirs(), paths(&["/etc/xdg"]));
        assert_eq!(dirs.diagnostics().len(), 2);
    }

    #[test]
    fn a_repeated_list_item_keeps_its_first_place() {
        let dirs = dirs(&[
            ("HOME", b"/home/u"),
            ("XDG_DATA_DIRS", b"/b:/a:/b/:/usr/share:/a"),
        ])
        .unwrap();
        assert_eq!(dirs.data_dirs(), paths(&["/b", "/a", "/usr/share"]));
        assert_eq!(dirs.diagnostics(), []);
    }

    /// The bound for reading `HOME` and an `XDG_DATA_DIRS` of `items`
    /// items in `list` bytes: twice the analysed cost. Per item two steps
    /// (split off, the repeat check); per byte one to split it off, two
    /// to normalize it, one to copy it and two to hash and compare it.
    /// Home is normalized (two per byte) and copied under five folders.
    fn list_bound(home: &[u8], items: usize, list: usize) -> usize {
        2 * (2 * items + 6 * list + 7 * home.len())
    }

    #[test]
    fn long_lists_are_read_in_linear_time() {
        let home = b"/home/u";
        work::assert_linear(
            10_000,
            |n| {
                let list = (0..n)
                    .map(|at| format!("/d{at}"))
                    .collect::<Vec<_>>()
                    .join(":");
                let (dirs, steps) = work::steps(|| {
                    dirs(&[("HOME", home), ("XDG_DATA_DIRS", list.as_bytes())]).unwrap()
                });
                assert_eq!(dirs.data_dirs().len(), n);
                (steps, list.len())
            },
            |items, list| list_bound(home, items, list),
        );
    }

    #[test]
    fn a_long_list_item_is_read_in_linear_time() {
        let home = b"/home/u";
        work::assert_linear(
            350_000,
            |n| {
                let long = format!("/{}", "ab/".repeat(n));
                let (dirs, steps) = work::steps(|| {
                    dirs(&[("HOME", home), ("XDG_DATA_DIRS", long.as_bytes())]).unwrap()
                });
                assert_eq!(dirs.data_dirs(), [Path::new(long.trim_end_matches('/'))]);
                (steps, long.len())
            },
            // One item, whatever its parts.
            |_, long| list_bound(home, 1, long),
        );
    }

    #[test]
    fn long_single_values_are_read_in_linear_time() {
        // A relative home (reported) and a long home from the password
        // database, copied under the folders left unset; an absolute config
        // home; a relative cache home (reported); a relative list item
        // (reported).
        work::assert_linear(
            200_000,
            |n| {
                let long = "ab/".repeat(n);
                let fallback = PathBuf::from(format!("/{long}"));
                let vars: HashMap<&str, OsString> = HashMap::from([
                    ("HOME", OsString::from(&long)),
                    ("XDG_CONFIG_HOME", OsString::from(format!("/c/{long}"))),
                    ("XDG_CACHE_HOME", OsString::from(&long)),
                    ("XDG_DATA_DIRS", OsString::from(format!("{long}:/a"))),
                ]);
                let get = |name: &str| vars.get(name).cloned();
                let (dirs, steps) =
                    work::steps(|| BaseDirs::build(&get, Some(fallback.clone()), 0).unwrap());
                let home = normalize(&fallback);
                assert_eq!(dirs.home(), home);
                assert_eq!(dirs.data_home(), home.join(".local/share"));
                assert_eq!(dirs.cache_home(), home.join(".cache"));
                assert_eq!(
                    dirs.config_home(),
                    normalize(Path::new(&format!("/c/{long}")))
                );
                assert_eq!(dirs.data_dirs(), paths(&["/a"]));
                assert_eq!(dirs.diagnostics().len(), 3);
                (steps, long.len())
            },
            // Twice the analysed cost: a few steps per value, and the three
            // messages, which read at most 132 bytes of the value and show
            // at most 32 characters of it (about 60 bytes each); per byte of
            // the long part 9: one to split the list, two to normalize the
            // config home, and for the home two to normalize it and one for
            // each of four copies.
            |_, long| 2 * (200 + 9 * long),
        );
    }

    #[test]
    fn a_folder_listed_again_is_searched_once() {
        let root = tempfile::tempdir().unwrap();
        let data_home = root.path().join("data");
        fs::create_dir_all(data_home.join("mime")).unwrap();
        fs::write(data_home.join("mime/types"), b"").unwrap();
        let dirs = dirs(&[
            ("HOME", b"/home/u"),
            ("XDG_DATA_HOME", data_home.as_os_str().as_bytes()),
            ("XDG_DATA_DIRS", data_home.as_os_str().as_bytes()),
        ])
        .unwrap();
        assert_eq!(
            dirs.data_files("mime/types").unwrap(),
            [data_home.join("mime/types")]
        );
    }

    #[test]
    fn a_long_relative_value_is_shown_from_its_start() {
        let message = |value: &[u8]| {
            let dirs = dirs(&[("HOME", b"/home/u"), ("XDG_DATA_HOME", value)]).unwrap();
            dirs.diagnostics()[0].message.clone()
        };
        // Bytes that are not UTF-8, and a character cut where reading
        // stops, are shown as when the whole value is read.
        for value in [
            [b"a\xff\xfe".as_slice(), &"é".repeat(200).into_bytes()].concat(),
            "\u{1f600}".repeat(33).into_bytes(),
            format!("a{}", "\u{1f600}".repeat(40)).into_bytes(),
            b"\xe2\x82".repeat(70),
            "\u{1b}".repeat(33).into_bytes(),
        ] {
            let whole = format!(
                "relative path {} ignored",
                quoted(&String::from_utf8_lossy(&value))
            );
            assert_eq!(message(&value), whole, "{value:?}");
        }
    }

    #[test]
    fn non_utf8_paths_survive() {
        let dirs = dirs(&[
            ("HOME", b"/home/u"),
            ("XDG_DATA_HOME", b"/d\xffata"),
            ("XDG_CONFIG_DIRS", b"/e\xfftc:/etc/xdg"),
        ])
        .unwrap();
        assert_eq!(dirs.data_home().as_os_str().as_bytes(), b"/d\xffata");
        assert_eq!(dirs.config_dirs()[0].as_os_str().as_bytes(), b"/e\xfftc");
    }

    #[test]
    fn a_missing_or_relative_home_is_an_error() {
        assert!(matches!(dirs(&[]), Err(Error::NoHome)));
        assert!(matches!(dirs(&[("HOME", b"")]), Err(Error::NoHome)));
        assert!(matches!(dirs(&[("HOME", b"home/u")]), Err(Error::NoHome)));
    }

    #[test]
    fn the_home_fallback_is_used_only_without_a_usable_home() {
        let no_vars = |_: &str| None;
        let dirs = BaseDirs::build(&no_vars, Some("/from/passwd".into()), 0).unwrap();
        assert_eq!(dirs.home(), Path::new("/from/passwd"));
        let relative = BaseDirs::build(&no_vars, Some("passwd".into()), 0);
        assert!(matches!(relative, Err(Error::NoHome)));
    }

    /// Fails at once when `path`, under the temporary folder, holds a `:`,
    /// which would split it in an XDG path list.
    fn assert_listable(path: &Path) {
        assert!(
            !path.as_os_str().as_bytes().contains(&b':'),
            "TMPDIR contains ':', which an XDG path list cannot hold; set TMPDIR to a path without ':'"
        );
    }

    #[test]
    fn lookups_go_in_order_of_importance_and_skip_what_is_missing() {
        let root = tempfile::tempdir().unwrap();
        assert_listable(root.path());
        let base = |name: &str| root.path().join(name);
        for name in ["home-data", "sys-a", "sys-c"] {
            fs::create_dir_all(base(name).join("mime")).unwrap();
            fs::write(base(name).join("mime/types"), b"").unwrap();
        }
        let data_dirs = [base("sys-a"), base("sys-b"), base("sys-c")]
            .map(|path| path.into_os_string().into_vec())
            .join(&b':');
        let dirs = dirs(&[
            ("HOME", b"/home/u"),
            ("XDG_DATA_HOME", base("home-data").as_os_str().as_bytes()),
            ("XDG_DATA_DIRS", &data_dirs),
        ])
        .unwrap();
        assert_eq!(
            dirs.data_files("mime/types").unwrap(),
            [
                base("home-data/mime/types"),
                base("sys-a/mime/types"),
                base("sys-c/mime/types"),
            ]
        );
        assert_eq!(
            dirs.find_data("mime/types").unwrap(),
            Some(base("home-data/mime/types"))
        );
        assert_eq!(dirs.find_data("mime/none").unwrap(), None);
        assert_eq!(dirs.find_config("mime/types").unwrap(), None);
        assert!(matches!(
            dirs.find_config("/etc/passwd"),
            Err(Error::InvalidRelativePath(_))
        ));
    }

    #[test]
    fn the_first_existing_file_ends_the_search() {
        // A file in each home: however many folders follow, finding it
        // looks at none of them. Listing every file still looks at each.
        let root = tempfile::tempdir().unwrap();
        let data_home = root.path().join("data");
        let config_home = root.path().join("config");
        for home in [&data_home, &config_home] {
            fs::create_dir_all(home.join("mime")).unwrap();
            fs::write(home.join("mime/types"), b"").unwrap();
        }
        let steps = |n: usize| {
            let list = (0..n)
                .map(|at| format!("/nonexistent/d{at}"))
                .collect::<Vec<_>>()
                .join(":");
            let dirs = dirs(&[
                ("HOME", b"/home/u"),
                ("XDG_DATA_HOME", data_home.as_os_str().as_bytes()),
                ("XDG_CONFIG_HOME", config_home.as_os_str().as_bytes()),
                ("XDG_DATA_DIRS", list.as_bytes()),
                ("XDG_CONFIG_DIRS", list.as_bytes()),
            ])
            .unwrap();
            let (data, finding_data) = work::steps(|| dirs.find_data("mime/types").unwrap());
            assert_eq!(data, Some(data_home.join("mime/types")));
            let (config, finding_config) = work::steps(|| dirs.find_config("mime/types").unwrap());
            assert_eq!(config, Some(config_home.join("mime/types")));
            let (all, listing) = work::steps(|| dirs.data_files("mime/types").unwrap());
            assert_eq!(all, [data_home.join("mime/types")]);
            (finding_data, finding_config, listing)
        };
        let (few, many) = (steps(1), steps(1000));
        assert_eq!((few.0, few.1), (many.0, many.1));
        assert!(many.2 > few.2 + 1000, "{few:?} {many:?}");
    }

    /// Base directories under `root`: homes `config` and `data`, then
    /// `sys-config` and `sys-data`.
    fn under(root: &Path) -> BaseDirs {
        let p = |name: &str| root.join(name).into_os_string().into_vec();
        dirs(&[
            ("HOME", b"/home/u"),
            ("XDG_CONFIG_HOME", &p("config")),
            ("XDG_CONFIG_DIRS", &p("sys-config")),
            ("XDG_DATA_HOME", &p("data")),
            ("XDG_DATA_DIRS", &p("sys-data")),
        ])
        .unwrap()
    }

    /// Both readers: of config files and of data files, within `limits`.
    type Reader = fn(&BaseDirs, &str, &Limits) -> Result<ReadFile, Error>;
    const READERS: [(&str, Reader); 2] = [
        ("config", |dirs, rel, limits| {
            dirs.read_config_with(rel, limits)
        }),
        ("data", |dirs, rel, limits| dirs.read_data_with(rel, limits)),
    ];

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn reading_skips_a_file_that_may_not_be_read_and_reports_it() {
        for (kind, read) in READERS {
            let root = tempfile::tempdir().unwrap();
            let dirs = under(root.path());
            let user = root.path().join(format!("{kind}/app/a.conf"));
            let system = root.path().join(format!("sys-{kind}/app/a.conf"));
            write(&user, b"user");
            write(&system, b"system");
            fs::set_permissions(&user, fs::Permissions::from_mode(0o000)).unwrap();
            if fs::read(&user).is_ok() {
                eprintln!("an unreadable file can be read (root?): not checked");
                return;
            }
            let found = read(&dirs, "app/a.conf", &Limits::default()).unwrap();
            let refused = fs::read(&user).unwrap_err().to_string();
            assert_eq!(found.file, Some((system, b"system".to_vec())), "{kind}");
            assert_eq!(
                found.skipped,
                [Diagnostic::file(
                    user,
                    None,
                    format!("cannot be read: {refused}")
                )],
                "{kind}"
            );
        }
    }

    #[test]
    fn reading_skips_a_missing_file_or_folder_without_a_report() {
        for (kind, read) in READERS {
            let root = tempfile::tempdir().unwrap();
            let dirs = under(root.path());
            let system = root.path().join(format!("sys-{kind}/app/a.conf"));
            write(&system, b"system");
            let found = read(&dirs, "app/a.conf", &Limits::default()).unwrap();
            assert_eq!(found.file, Some((system.clone(), b"system".to_vec())));
            assert!(found.skipped.is_empty(), "{:?}", found.skipped);
            // A step on the way that is a file, not a folder.
            write(&root.path().join(format!("{kind}/app")), b"");
            let found = read(&dirs, "app/a.conf", &Limits::default()).unwrap();
            assert_eq!(found.file, Some((system, b"system".to_vec())));
            assert!(found.skipped.is_empty(), "{:?}", found.skipped);
            // Nothing anywhere.
            let found = read(&dirs, "app/none.conf", &Limits::default()).unwrap();
            assert_eq!(found.file, None);
            assert!(found.skipped.is_empty(), "{:?}", found.skipped);
        }
    }

    #[test]
    fn reading_stops_at_a_file_that_cannot_be_read_for_another_reason() {
        for (kind, read) in READERS {
            let root = tempfile::tempdir().unwrap();
            let dirs = under(root.path());
            let user = root.path().join(format!("{kind}/app/a.conf"));
            write(
                &root.path().join(format!("sys-{kind}/app/a.conf")),
                b"system",
            );
            // A folder: not a regular file.
            fs::create_dir_all(&user).unwrap();
            let Err(Error::Io { path, source }) = read(&dirs, "app/a.conf", &Limits::default())
            else {
                panic!("{kind}: a folder gave no error");
            };
            assert_eq!(
                (path, source.kind()),
                (user.clone(), io::ErrorKind::InvalidInput)
            );
            // Too large.
            fs::remove_dir(&user).unwrap();
            write(&user, b"too long");
            let limits = Limits {
                max_file_len: Some(4),
            };
            let Err(Error::Io { path, source }) = read(&dirs, "app/a.conf", &limits) else {
                panic!("{kind}: a file too large gave no error");
            };
            assert_eq!((path, source.kind()), (user, io::ErrorKind::FileTooLarge));
            assert!(matches!(
                read(&dirs, "../a.conf", &Limits::default()),
                Err(Error::InvalidRelativePath(_))
            ));
        }
    }

    #[test]
    fn reading_without_limits_reads_as_with_the_default_ones() {
        let root = tempfile::tempdir().unwrap();
        let dirs = under(root.path());
        write(&root.path().join("sys-config/a.conf"), b"c");
        write(&root.path().join("sys-data/a.conf"), b"d");
        let config = dirs.read_config("a.conf").unwrap().file.unwrap();
        assert_eq!(
            config,
            (root.path().join("sys-config/a.conf"), b"c".to_vec())
        );
        let data = dirs.read_data("a.conf").unwrap().file.unwrap();
        assert_eq!(data, (root.path().join("sys-data/a.conf"), b"d".to_vec()));
    }

    #[test]
    fn placing_creates_private_parents_and_leaves_existing_ones() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
        let dirs = dirs(&[
            ("HOME", b"/home/u"),
            ("XDG_STATE_HOME", state.as_os_str().as_bytes()),
        ])
        .unwrap();

        let path = dirs.place_state("app/logs/today.log").unwrap();
        assert_eq!(path, state.join("app/logs/today.log"));
        assert!(!path.exists(), "the file itself was created");
        for created in [state.join("app"), state.join("app/logs")] {
            let mode = fs::metadata(&created).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", created.display());
        }
        let mode = fs::metadata(&state).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "an existing folder was changed");

        for bad in ["", ".", "./", "/abs", "../up"] {
            assert!(matches!(
                dirs.place_state(bad),
                Err(Error::InvalidRelativePath(_))
            ));
        }
    }
}
