//! A snapshot made from relative directories keeps addressing the
//! directories it read when the working directory changes: its refresh,
//! and the writes and removals of its overrides. The working directory is
//! process-global, so this binary holds exactly one test and nothing else
//! runs beside it.

use std::fs;

use xdg_kit_autostart::{AutostartEntries, Limits};

const APP: &str = "[Desktop Entry]\nType=Application\nName=A\nExec=a\n";

#[test]
fn relative_directories_are_anchored_at_discovery() {
    let root = tempfile::tempdir().unwrap();
    let (here, elsewhere) = (root.path().join("here"), root.path().join("elsewhere"));
    for base in [&here, &elsewhere] {
        fs::create_dir_all(base.join("user")).unwrap();
        fs::create_dir_all(base.join("system")).unwrap();
        fs::write(base.join("system/a.desktop"), APP).unwrap();
    }
    std::env::set_current_dir(&here).unwrap();
    let entries = AutostartEntries::from_dirs("user", ["system"], &Limits::default()).unwrap();
    assert_eq!(entries.user_dir(), here.join("user"));
    let mut edit = entries.edit("a.desktop").unwrap();
    edit.disable().unwrap();
    std::env::set_current_dir(&elsewhere).unwrap();

    // The write lands where the snapshot was taken.
    let _ = edit.write().unwrap();
    assert!(here.join("user/a.desktop").exists());
    assert!(
        !elsewhere.join("user/a.desktop").exists(),
        "never redirected"
    );

    // Refresh watches the same directories.
    let after = entries.refresh().expect("the override appeared");
    assert!(after.get("a.desktop").unwrap().is_user_override());

    // A removal too.
    let mut reset = after.edit("a.desktop").unwrap();
    reset.reset().unwrap();
    fs::write(elsewhere.join("user/a.desktop"), APP).unwrap();
    let _ = reset.write().unwrap();
    assert!(!here.join("user/a.desktop").exists());
    assert!(elsewhere.join("user/a.desktop").exists(), "the decoy stays");

    // A path that cannot be anchored is refused, not guessed.
    assert!(AutostartEntries::from_dirs("", ["system"], &Limits::default()).is_err());
}
