use std::ffi::{OsStr, OsString};
use std::fs::DirBuilder;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use crate::paths::{check_relative, normalize};
use crate::runtime::{self, RuntimeDirProblem};
use crate::{Diagnostic, Error};

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
        let single = |name: &str, default: &str, diagnostics: &mut Vec<Diagnostic>| {
            single(vars, name, || home.join(default), diagnostics)
        };
        Ok(Self {
            data_home: single("XDG_DATA_HOME", ".local/share", &mut diagnostics),
            config_home: single("XDG_CONFIG_HOME", ".config", &mut diagnostics),
            state_home: single("XDG_STATE_HOME", ".local/state", &mut diagnostics),
            cache_home: single("XDG_CACHE_HOME", ".cache", &mut diagnostics),
            executable_home: home.join(".local/bin"),
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
    /// once, even when it is listed again.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRelativePath`] when `rel` is absolute, contains `..`,
    /// or names nothing (empty, or just `.`).
    pub fn find_data(&self, rel: impl AsRef<Path>) -> Result<Option<PathBuf>, Error> {
        Ok(self.data_files(rel)?.into_iter().next())
    }

    /// Every existing `rel` under the data directories, most important first.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRelativePath`] when `rel` is absolute, contains `..`,
    /// or names nothing (empty, or just `.`).
    pub fn data_files(&self, rel: impl AsRef<Path>) -> Result<Vec<PathBuf>, Error> {
        existing(&self.data_home, &self.data_dirs, rel.as_ref())
    }

    /// The most important existing `rel` under the config directories:
    /// `$XDG_CONFIG_HOME`, then each of `$XDG_CONFIG_DIRS`. A folder is
    /// searched once, even when it is listed again.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRelativePath`] when `rel` is absolute, contains `..`,
    /// or names nothing (empty, or just `.`).
    pub fn find_config(&self, rel: impl AsRef<Path>) -> Result<Option<PathBuf>, Error> {
        Ok(self.config_files(rel)?.into_iter().next())
    }

    /// Every existing `rel` under the config directories, most important
    /// first.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRelativePath`] when `rel` is absolute, contains `..`,
    /// or names nothing (empty, or just `.`).
    pub fn config_files(&self, rel: impl AsRef<Path>) -> Result<Vec<PathBuf>, Error> {
        existing(&self.config_home, &self.config_dirs, rel.as_ref())
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

fn home(
    vars: &dyn Fn(&str) -> Option<OsString>,
    fallback: Option<PathBuf>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<PathBuf, Error> {
    match vars("HOME").filter(|value| !value.is_empty()) {
        Some(value) if Path::new(&value).is_absolute() => return Ok(normalize(Path::new(&value))),
        Some(value) => diagnostics.push(Diagnostic::variable(
            "HOME",
            format!("relative path {} ignored", Path::new(&value).display()),
        )),
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
            diagnostics.push(Diagnostic::variable(
                name,
                format!("relative path {} ignored", Path::new(&value).display()),
            ));
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
    if let Some(value) = vars(name) {
        for item in value.as_bytes().split(|&byte| byte == b':') {
            let item = Path::new(OsStr::from_bytes(item));
            if item.as_os_str().is_empty() {
                continue;
            }
            if item.is_absolute() {
                let item = normalize(item);
                if !dirs.contains(&item) {
                    dirs.push(item);
                }
            } else {
                diagnostics.push(Diagnostic::variable(
                    name,
                    format!("relative path {} ignored", item.display()),
                ));
            }
        }
    }
    if dirs.is_empty() {
        dirs = default.iter().map(PathBuf::from).collect();
    }
    dirs
}

fn existing(home: &Path, dirs: &[PathBuf], rel: &Path) -> Result<Vec<PathBuf>, Error> {
    check_relative(rel)?;
    // The list already has no duplicates. Search home first and skip any
    // matching list entry.
    Ok(std::iter::once(home)
        .chain(dirs.iter().map(PathBuf::as_path).filter(|dir| *dir != home))
        .map(|base| base.join(rel))
        // A path that cannot be checked is skipped, as the spec says.
        .filter(|path| matches!(path.try_exists(), Ok(true)))
        .collect())
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
        assert!(dirs.diagnostics().is_empty());
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
        assert!(dirs.diagnostics().is_empty());
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
        assert!(dirs.diagnostics().is_empty());
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

    #[test]
    fn lookups_go_in_order_of_importance_and_skip_what_is_missing() {
        let root = tempfile::tempdir().unwrap();
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
