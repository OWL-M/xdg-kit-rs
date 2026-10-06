//! Compares `UserDirs` with xdg-user-dirs' own `xdg-user-dir` tool, which
//! prints what the reference lookup answers.
//!
//! The tool must be on `PATH` (the dev shell and `nix flake check` provide
//! it). These fixtures omit unterminated quotes: xdg-user-dirs 0.20 accepts
//! them, while later versions skip them. Unit tests cover those cases.
//! Fixtures also omit lines longer than the reference's 511-byte buffer and
//! values containing a NUL byte, which the reference truncates before the
//! closing quote.
//!
//! Each fixture is chosen so that a wrong parse gives a different answer from
//! the fallback the reference prints when it skips a line.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use xdg_kit_basedir::{BaseDirs, UserDirKind, UserDirs};

const FIXTURES: &[&str] = &[
    "",
    "XDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_DOWNLOAD_DIR=\"/data/downloads\"\n",
    " \tXDG_MUSIC_DIR \t= \t\"$HOME/Music\"\n",
    "XDG_VIDEOS_DIR=\"$HOME/My \\\"Clips\\\" \\\\ more\"\n",
    "XDG_PICTURES_DIR=\"Pictures\"\nXDG_MUSIC_DIR=$HOME/Music\nXDG_TEMPLATES=\"$HOME/T\"\n",
    "#XDG_VIDEOS_DIR=\"$HOME/Videos\"\nXDG_DOCUMENTS_DIR\"$HOME/Documents\"\n",
    "XDG_MUSIC_DIR=\"$HOME/Old\"\nXDG_MUSIC_DIR=\"$HOME/New\"\n",
    "XDG_DESKTOP_DIR=\"$HOME/\"\nXDG_PUBLICSHARE_DIR=\"/data/public\"\n",
    "XDG_GAMES_DIR=\"$HOME/Games\"\nXDG_PROJECTS_DIR=\"$HOME/src\"\n",
    "XDG_MUSIC_DIR=\"$HOME//Music\"\nXDG_VIDEOS_DIR=\"$HOME/\\/Videos\"\n",
    "XDG_MUSIC_DIR=\"$HOME/a//b/\"\nXDG_VIDEOS_DIR=\"/x/./y/\"\nXDG_DESKTOP_DIR=\"$HOME\"\n",
];

const NAMES: &[&str] = &[
    "DESKTOP",
    "DOCUMENTS",
    "DOWNLOAD",
    "MUSIC",
    "PICTURES",
    "PROJECTS",
    "PUBLICSHARE",
    "TEMPLATES",
    "VIDEOS",
    "GAMES",
];

fn reference(home: &Path, name: &str) -> PathBuf {
    let output = Command::new("xdg-user-dir")
        .arg(name)
        // Only HOME (and PATH, to find the tool) reach it, so the tool reads
        // the test's file, never the user's.
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .output()
        .expect("xdg-user-dir is not on PATH: run the tests in the dev shell");
    assert!(
        output.status.success(),
        "xdg-user-dir {name} failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let printed = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
    PathBuf::from(OsStr::from_bytes(printed))
}

#[test]
fn every_answer_matches_the_reference_tool() {
    for contents in FIXTURES {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir(home.path().join(".config")).unwrap();
        fs::write(home.path().join(".config/user-dirs.dirs"), contents).unwrap();
        let base = BaseDirs::from_vars(|name| {
            (name == "HOME").then(|| home.path().as_os_str().to_owned())
        })
        .unwrap();
        let dirs = UserDirs::load(&base);
        for name in NAMES {
            // Paths compare by component, so the reference's trailing slash
            // for "$HOME/" does not count as a difference.
            assert_eq!(
                dirs.get_or_fallback(&UserDirKind::from_name(name)),
                reference(home.path(), name),
                "{name} for {contents:?} (left: UserDirs, right: xdg-user-dir)"
            );
        }
    }
}
