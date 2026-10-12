//! Relative paths after the working directory changes. All tests in a
//! binary share the process's working directory, so this binary has only
//! one test.

use std::{env, fs, io};

use xdg_kit_desktop_entry::{AppIndex, DesktopEntry, Launch, LaunchOptions, prepare_launch};

#[test]
fn a_relative_path_keeps_its_files_after_the_folder_changes() {
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("A");
    let b = root.path().join("B");
    for (dir, id) in [(&a, "a"), (&b, "b")] {
        fs::create_dir_all(dir.join("apps")).unwrap();
        fs::write(
            dir.join("apps/x.desktop"),
            "[Desktop Entry]\nType=Application\nName=X\nExec=foo %k\n",
        )
        .unwrap();
        fs::create_dir_all(dir.join("applications")).unwrap();
        fs::write(
            dir.join(format!("applications/{id}.desktop")),
            "[Desktop Entry]\nType=Application\nName=X\nExec=foo\n",
        )
        .unwrap();
    }

    env::set_current_dir(&a).unwrap();
    let here = env::current_dir().unwrap();
    let entry = DesktopEntry::read("apps/x.desktop").unwrap();
    let index = AppIndex::from_dirs(["applications"]);
    // The absolute form, as written: `.` steps dropped, links not followed.
    assert_eq!(entry.path(), Some(here.join("apps/x.desktop").as_path()));
    assert_eq!(index.dirs(), [here.join("applications").as_path()]);
    assert_eq!(
        index.get("a.desktop").unwrap().path(),
        here.join("applications/a.desktop")
    );

    env::set_current_dir(&b).unwrap();
    // `%k` names the file read, not the one now at the same relative path.
    let plan = prepare_launch(&entry, &[], &LaunchOptions::default()).unwrap();
    let Launch::Commands(commands) = &plan.launch else {
        panic!("a command line");
    };
    assert_eq!(
        commands[0].args,
        [here.join("apps/x.desktop").into_os_string()]
    );
    // Nothing changed in A's folder.
    assert!(index.refresh().is_none());

    // With the current folder gone, a relative path is kept as given and
    // reads through it fail.
    let gone = root.path().join("gone");
    fs::create_dir(&gone).unwrap();
    env::set_current_dir(&gone).unwrap();
    fs::remove_dir(&gone).unwrap();
    let index = AppIndex::from_dirs(["applications"]);
    assert_eq!(index.dirs(), [std::path::Path::new("applications")]);
    assert_eq!(
        DesktopEntry::read("apps/x.desktop").unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    env::set_current_dir(root.path()).unwrap();
}
