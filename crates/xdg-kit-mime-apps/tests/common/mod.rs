//! Shared by the integration tests: a set of XDG folders in a temporary
//! folder, with a small MIME database built by `update-mime-database`.

#![allow(dead_code)]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use xdg_kit_basedir::BaseDirs;
use xdg_kit_desktop_entry::{AppIndex, Desktops};
use xdg_kit_mime::MimeDatabase;
use xdg_kit_mime_apps::MimeApps;

/// The types the tests use: made up, so that no application installed on
/// the machine running the tests claims them (`gio` also reads the Nix
/// profiles' applications). `text/x-xkt-foo` has no explicit parent on
/// purpose.
const TYPES: &str = r#"<?xml version="1.0"?>
<mime-info xmlns="http://www.freedesktop.org/standards/shared-mime-info">
  <mime-type type="text/x-xkt-plain"><glob pattern="*.xkt-plain"/></mime-type>
  <mime-type type="text/x-xkt-src">
    <sub-class-of type="text/x-xkt-plain"/>
    <alias type="text/x-xkt-alias"/>
    <glob pattern="*.xkt-src"/>
  </mime-type>
  <mime-type type="text/x-xkt-foo"><glob pattern="*.xkt-foo"/></mime-type>
  <mime-type type="application/x-xkt-xml"><sub-class-of type="text/x-xkt-plain"/><glob pattern="*.xkt-xml"/></mime-type>
  <mime-type type="image/x-xkt-svg"><sub-class-of type="application/x-xkt-xml"/><glob pattern="*.xkt-svg"/></mime-type>
  <mime-type type="image/x-xkt-png"><glob pattern="*.xkt-png"/></mime-type>
  <mime-type type="application/x-desktop"><glob pattern="*.desktop"/></mime-type>
</mime-info>
"#;

/// XDG folders under one temporary folder:
/// - `config` (`XDG_CONFIG_HOME`), `etc` and `etc2` (`XDG_CONFIG_DIRS`);
/// - `local` (`XDG_DATA_HOME`), `share1`, `share2` and `mime-data`
///   (`XDG_DATA_DIRS`, the last holding only the MIME database).
pub struct Fixture {
    pub root: tempfile::TempDir,
    pub desktops: Vec<String>,
}

impl Fixture {
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let mime = root.path().join("mime-data/mime");
        fs::create_dir_all(mime.join("packages")).unwrap();
        fs::write(mime.join("packages/test.xml"), TYPES).unwrap();
        let data = root.path().join("mime-data");
        let status = Command::new("update-mime-database")
            .arg(&mime)
            .env("XDG_DATA_DIRS", &data)
            .env("XDG_DATA_HOME", &data)
            .status()
            .expect(
                "update-mime-database is not on PATH: run the tests in the dev shell (nix develop)",
            );
        assert!(status.success(), "update-mime-database failed");
        Self {
            root,
            desktops: Vec::new(),
        }
    }

    pub fn path(&self, relative: &str) -> PathBuf {
        self.root.path().join(relative)
    }

    pub fn write(&self, relative: &str, text: &str) {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// Writes an application `id` (in folder `dir`, such as
    /// `share1/applications`) that lists `types`, plus `extra` lines.
    pub fn app(&self, dir: &str, id: &str, types: &str, extra: &str) {
        self.write(
            &format!("{dir}/{id}"),
            &format!(
                "[Desktop Entry]\nType=Application\nName={id}\nExec=true %f\nMimeType={types}\n{extra}"
            ),
        );
    }

    /// The `XDG_*` variables of these folders.
    pub fn vars(&self) -> Vec<(&'static str, OsString)> {
        let p = |name: &str| self.path(name).into_os_string();
        let mut data_dirs = p("share1");
        for name in ["share2", "mime-data"] {
            data_dirs.push(":");
            data_dirs.push(p(name));
        }
        vec![
            ("HOME", self.root.path().as_os_str().to_owned()),
            ("XDG_CONFIG_HOME", p("config")),
            ("XDG_CONFIG_DIRS", {
                let mut dirs = p("etc");
                dirs.push(":");
                dirs.push(p("etc2"));
                dirs
            }),
            ("XDG_DATA_HOME", p("local")),
            ("XDG_DATA_DIRS", data_dirs),
            ("XDG_CURRENT_DESKTOP", self.desktops.join(":").into()),
        ]
    }

    pub fn base_dirs(&self) -> BaseDirs {
        let vars = self.vars();
        BaseDirs::from_vars(|name| {
            vars.iter()
                .find(|(known, _)| *known == name)
                .map(|(_, value)| value.clone())
        })
        .unwrap()
    }

    pub fn mime_apps(&self) -> MimeApps {
        let base_dirs = self.base_dirs();
        MimeApps::load(
            &base_dirs,
            &Desktops::new(self.desktops.clone()),
            Arc::new(AppIndex::load(&base_dirs)),
            Arc::new(MimeDatabase::from_dirs([self.path("mime-data/mime")])),
        )
    }

    /// The `applications` folders of the data levels.
    pub fn applications(&self) -> Vec<PathBuf> {
        ["local", "share1", "share2", "mime-data"]
            .iter()
            .map(|name| self.path(name).join("applications"))
            .collect()
    }
}

/// Runs `update-desktop-database` on `folder`, writing its `mimeinfo.cache`.
pub fn update_desktop_database(folder: &Path) {
    let output = Command::new("update-desktop-database")
        .arg("--quiet")
        .arg(folder)
        .output()
        .expect(
            "update-desktop-database is not on PATH: run the tests in the dev shell (nix develop)",
        );
    assert!(
        output.status.success(),
        "update-desktop-database failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The lines of `tests/<name>`, a list of reviewed differences: `fields`
/// tab-separated fields each, none empty or with spaces at either end, and
/// the first `fields - 1` fields (the key) not repeated. `#` lines and blank
/// lines are skipped.
pub fn read_differences(name: &str, fields: usize) -> Vec<Vec<String>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(name);
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("{}: cannot be read: {err}", path.display()));
    let mut keys = std::collections::HashSet::new();
    let mut lines = Vec::new();
    for line in text
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
    {
        let found: Vec<String> = line.split('\t').map(str::to_owned).collect();
        assert!(
            found.len() == fields
                && found
                    .iter()
                    .all(|field| !field.is_empty() && field == field.trim()),
            "{}: want {fields} tab-separated fields: {line:?}",
            path.display()
        );
        assert!(
            keys.insert(found[..fields - 1].to_vec()),
            "{}: listed twice: {line:?}",
            path.display()
        );
        lines.push(found);
    }
    lines
}
