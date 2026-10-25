//! `AppEntry::try_exec` against installed `GLib` (the dev shell's `gio`):
//! `gio launch` refuses a desktop entry whose `TryExec` check fails
//! (`g_desktop_app_info_new_from_filename` returns nothing), so its exit
//! status is the reference's answer for the same entry under the same
//! `PATH`.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use xdg_kit_desktop_entry::AppIndex;

/// Whether `gio launch` accepts the entry under `path_var`. The child's
/// working directory is a neutral empty folder, so only the entry's own
/// `Path=` and the given `PATH` decide.
fn gio_accepts(gio: &Path, entry: &Path, path_var: &OsStr, neutral: &Path) -> bool {
    Command::new(gio)
        .arg("launch")
        .arg(entry)
        .env("PATH", path_var)
        .current_dir(neutral)
        .status()
        .expect("gio runs in the dev shell")
        .success()
}

#[test]
fn try_exec_answers_as_gio_does() {
    let gio = which("gio");
    let run = which("true");
    let root = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let neutral = tempfile::tempdir().unwrap();
    let tool = bin.path().join("tool");
    fs::write(&tool, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    fs::create_dir(bin.path().join("sub")).unwrap();
    fs::copy(&tool, bin.path().join("sub/nested")).unwrap();
    for path in [root.path(), bin.path(), home.path()] {
        assert!(
            !path.as_os_str().as_encoded_bytes().contains(&b':'),
            "TMPDIR holds a ':', which PATH cannot carry"
        );
    }
    // (TryExec, Path=) cases covering the direct candidate and the
    // search fallback; `home` is a Path= folder without the programs.
    let cases: &[(&str, Option<&Path>)] = &[
        ("tool", None),
        ("absent", None),
        ("sub/nested", None),
        ("sub/absent", None),
        ("tool", Some(bin.path())),
        ("tool", Some(home.path())),
        ("sub/nested", Some(bin.path())),
        ("sub/nested", Some(home.path())),
        (tool.to_str().unwrap(), None),
        ("/nonexistent/tool", None),
        ("/nonexistent/tool", Some(bin.path())),
    ];
    for (searched, label) in [
        (bin.path().as_os_str().to_owned(), "bin on PATH"),
        ("/nonexistent".into(), "bin off PATH"),
    ] {
        for (at, (try_exec, working_dir)) in cases.iter().enumerate() {
            let id = format!("case-{at}.desktop");
            let path_line = working_dir.map_or(String::new(), |dir| {
                format!("Path={}\n", dir.to_str().unwrap())
            });
            fs::write(
                root.path().join(&id),
                format!(
                    "[Desktop Entry]\nType=Application\nName=x\n\
                     Exec={run}\nTryExec={try_exec}\n{path_line}",
                    run = run.display()
                ),
            )
            .unwrap();
            let index = AppIndex::from_dirs([root.path()]);
            let ours = index.get(&id).unwrap().try_exec(&searched);
            let reference = gio_accepts(&gio, &root.path().join(&id), &searched, neutral.path());
            assert_eq!(
                ours, reference,
                "TryExec={try_exec} Path={working_dir:?} with {label}"
            );
        }
    }
}

/// The absolute path of `name` on the test's own `PATH`.
fn which(name: &str) -> std::path::PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").expect("a PATH"))
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("{name} runs in the dev shell"))
}
