//! A prepared write must keep addressing the file it selected when the
//! working directory changes before the commit. The working directory is
//! process-global, so this binary holds exactly one test and nothing
//! else runs beside it.

use std::fs;

use xdg_kit_basedir::{CreateMode, Durability, PreparedWrite, SymlinkPolicy, WriteOptions};

const OPTIONS: WriteOptions = WriteOptions {
    create: CreateMode::Exact(0o644),
    preserve_mode: true,
    symlinks: SymlinkPolicy::FollowTarget,
};

fn temp_files(dir: &std::path::Path) -> Vec<std::ffi::OsString> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().ends_with(".tmp"))
        .collect()
}

#[test]
fn a_prepared_write_survives_a_working_directory_change() {
    let here = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    std::env::set_current_dir(here.path()).unwrap();

    // Committed after the move: the file lands where it was prepared.
    let prepared = PreparedWrite::new("relative.txt".as_ref(), b"anchored", &OPTIONS).unwrap();
    std::env::set_current_dir(elsewhere.path()).unwrap();
    let committed = prepared.commit().unwrap();
    assert!(matches!(committed.durability, Durability::Synced));
    assert_eq!(
        fs::read(here.path().join("relative.txt")).unwrap(),
        b"anchored"
    );
    assert!(!elsewhere.path().join("relative.txt").exists());
    assert_eq!(temp_files(here.path()), Vec::<std::ffi::OsString>::new());
    assert_eq!(
        temp_files(elsewhere.path()),
        Vec::<std::ffi::OsString>::new()
    );

    // Dropped after the move: the temporary is removed where it was made.
    std::env::set_current_dir(here.path()).unwrap();
    let dropped = PreparedWrite::new("dropped.txt".as_ref(), b"gone", &OPTIONS).unwrap();
    std::env::set_current_dir(elsewhere.path()).unwrap();
    drop(dropped);
    assert_eq!(temp_files(here.path()), Vec::<std::ffi::OsString>::new());
    assert!(!here.path().join("dropped.txt").exists());
}
