//! The crate's [`Limits`], set through the `_with` entry points.

mod common;

use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;

use common::Fixture;
use xdg_kit_desktop_entry::{AppIndex, Desktops, LaunchOptions, Target};
use xdg_kit_mime::{MimeDatabase, MimeType};
use xdg_kit_mime_apps::{
    Limits, MimeApps, MimeAppsList, OpenError, mimeinfo_cache, mimeinfo_cache_with, open_plan,
    write_mimeinfo_cache_with,
};

const ENTRY: &str = "[Desktop Entry]\nType=Application\nName=A\nExec=a\nMimeType=text/plain;\n";

fn m(text: &str) -> MimeType {
    MimeType::new(text).unwrap()
}

#[test]
fn the_defaults_are_the_limits_without_with() {
    let limits = Limits::default();
    assert_eq!(
        (limits.max_file_len, limits.max_listed),
        (Some(xdg_kit_basedir::MAX_FILE_LEN), Some(100_000))
    );
    let unlimited = Limits::unlimited();
    assert_eq!((unlimited.max_file_len, unlimited.max_listed), (None, None));
    let fixture = Fixture::new();
    assert_eq!(fixture.mime_apps().limits(), &Limits::default());
}

/// A folder of four entries, `a` to `d`.
fn four_entries() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for name in ["a", "b", "c", "d"] {
        fs::write(dir.path().join(format!("{name}.desktop")), ENTRY).unwrap();
    }
    dir
}

#[test]
fn the_entries_a_cache_walk_lists_can_be_set_or_unlimited() {
    let dir = four_entries();
    let all = "[MIME Cache]\ntext/plain=a.desktop;b.desktop;c.desktop;d.desktop;\n";
    assert_eq!(mimeinfo_cache(dir.path()).unwrap(), all.as_bytes());
    let mut limits = Limits::default();
    limits.max_listed = Some(4);
    assert_eq!(
        mimeinfo_cache_with(dir.path(), &limits).unwrap(),
        all.as_bytes()
    );
    limits.max_listed = Some(3);
    let err = mimeinfo_cache_with(dir.path(), &limits).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Other);
    assert_eq!(
        err.to_string(),
        "mimeinfo.cache walk: more than 3 entries to list; no cache made"
    );
    assert!(write_mimeinfo_cache_with(dir.path(), &limits).is_err());
    assert!(!dir.path().join("mimeinfo.cache").exists());
    limits.max_listed = None;
    write_mimeinfo_cache_with(dir.path(), &limits).unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("mimeinfo.cache")).unwrap(),
        all
    );
}

/// Makes `levels` folders `d1`, `d2`, ... in `ext`, each but the last with
/// two links to the next, and two links to `d1` in `apps`: the paths to the
/// last folder double at each level.
fn fan_out(apps: &Path, ext: &Path, levels: usize) {
    for level in 1..=levels {
        let folder = ext.join(format!("d{level}"));
        fs::create_dir_all(&folder).unwrap();
        if level < levels {
            for name in ["l0", "l1"] {
                let next = ext.join(format!("d{}", level + 1));
                std::os::unix::fs::symlink(next, folder.join(name)).unwrap();
            }
        }
    }
    for name in ["l0", "l1"] {
        std::os::unix::fs::symlink(ext.join("d1"), apps.join(name)).unwrap();
    }
}

#[test]
fn a_cache_walk_past_the_default_cap_is_made_without_one() {
    let dir = four_entries();
    let ext = tempfile::tempdir().unwrap();
    // About 2^18 entries along every path, more than the default cap.
    fan_out(dir.path(), ext.path(), 17);
    assert!(mimeinfo_cache(dir.path()).is_err());
    let mut limits = Limits::default();
    limits.max_listed = None;
    assert_eq!(
        mimeinfo_cache_with(dir.path(), &limits).unwrap(),
        "[MIME Cache]\ntext/plain=a.desktop;b.desktop;c.desktop;d.desktop;\n".as_bytes()
    );
}

#[test]
fn a_cache_walk_skips_entries_larger_than_the_limit() {
    let dir = four_entries();
    fs::write(dir.path().join("b.desktop"), format!("{ENTRY}#\n")).unwrap();
    let mut limits = Limits::default();
    limits.max_file_len = Some(ENTRY.len() as u64);
    assert_eq!(
        mimeinfo_cache_with(dir.path(), &limits).unwrap(),
        "[MIME Cache]\ntext/plain=a.desktop;c.desktop;d.desktop;\n".as_bytes()
    );
    let all = "[MIME Cache]\ntext/plain=a.desktop;b.desktop;c.desktop;d.desktop;\n";
    assert_eq!(mimeinfo_cache(dir.path()).unwrap(), all.as_bytes());
    assert_eq!(
        mimeinfo_cache_with(dir.path(), &Limits::unlimited()).unwrap(),
        all.as_bytes()
    );
}

#[test]
fn a_list_larger_than_the_limit_cannot_be_opened() {
    let fixture = Fixture::new();
    let text = "[Default Applications]\ntext/x-xkt-plain=a.desktop;\n";
    fixture.write("config/mimeapps.list", text);
    let path = fixture.path("config/mimeapps.list");
    let mut limits = Limits::default();
    limits.max_file_len = Some(text.len() as u64 - 1);
    let err = MimeAppsList::open_with(&path, &limits).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);
    let err = MimeAppsList::for_user_with(&fixture.base_dirs(), &limits).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);
    assert!(MimeAppsList::open(&path).is_ok());
    for limits in [Limits::unlimited(), {
        limits.max_file_len = Some(text.len() as u64);
        limits
    }] {
        let list = MimeAppsList::for_user_with(&fixture.base_dirs(), &limits).unwrap();
        assert_eq!(list.path(), Some(path.as_path()));
        assert!(MimeAppsList::open_with(&path, &limits).is_ok());
    }
}

/// [`MimeApps::load_with`] of `fixture`'s folders.
fn load_with(fixture: &Fixture, limits: &Limits) -> MimeApps {
    MimeApps::load_with(
        &fixture.base_dirs(),
        &Desktops::new(Vec::<String>::new()),
        Arc::new(AppIndex::load(&fixture.base_dirs())),
        Arc::new(MimeDatabase::from_dirs([fixture.path("mime-data/mime")])),
        limits,
    )
}

#[test]
fn lists_larger_than_the_limit_are_reported_also_after_refresh() {
    let fixture = Fixture::new();
    let plain = m("text/x-xkt-plain");
    fixture.app("share1/applications", "a.desktop", "text/x-xkt-plain;", "");
    fixture.app("share1/applications", "b.desktop", "text/x-xkt-plain;", "");
    let text = "[Default Applications]\ntext/x-xkt-plain=b.desktop;\n";
    fixture.write("config/mimeapps.list", text);
    let default = |apps: &MimeApps| apps.default_for(&plain).map(|app| app.id().to_owned());
    assert_eq!(default(&fixture.mime_apps()).as_deref(), Some("b.desktop"));
    let mut limits = Limits::default();
    limits.max_file_len = Some(text.len() as u64 - 1);
    let apps = load_with(&fixture, &limits);
    let unreadable = |apps: &MimeApps| {
        apps.diagnostics()
            .iter()
            .filter(|d| d.message.starts_with("cannot be read"))
            .count()
    };
    assert_eq!(default(&apps).as_deref(), Some("a.desktop"));
    assert_eq!(unreadable(&apps), 1, "{:?}", apps.diagnostics());
    fixture.write("config/mimeapps.list", &format!("{text}#\n"));
    let apps = apps
        .refresh(apps.app_index(), apps.mime_database())
        .expect("the list changed");
    assert_eq!(apps.limits(), &limits);
    assert_eq!(default(&apps).as_deref(), Some("a.desktop"));
    assert_eq!(unreadable(&apps), 1);
    let unlimited = load_with(&fixture, &Limits::unlimited());
    assert_eq!(default(&unlimited).as_deref(), Some("b.desktop"));
    assert_eq!(unreadable(&unlimited), 0);
}

#[test]
fn a_link_larger_than_the_limit_is_not_followed() {
    let fixture = Fixture::new();
    fixture.write(
        "share1/applications/xkt-web.desktop",
        "[Desktop Entry]\nType=Application\nName=Web\nExec=true %u\nMimeType=x-scheme-handler/xkt-web;\n",
    );
    let link = "[Desktop Entry]\nType=Link\nName=Site\nURL=xkt-web://example.org/\n";
    fixture.write("files/site.desktop", link);
    let target = Target::Path(fixture.path("files/site.desktop"));
    let options = LaunchOptions::default();
    assert!(open_plan(&target, &fixture.mime_apps(), &options).is_ok());
    let mut limits = Limits::default();
    limits.max_file_len = Some(link.len() as u64 - 1);
    let err = open_plan(&target, &load_with(&fixture, &limits), &options).unwrap_err();
    assert!(
        matches!(&err, OpenError::NoApplication(mime) if *mime == m("application/x-desktop")),
        "{err:?}"
    );
    assert!(
        open_plan(
            &target,
            &load_with(&fixture, &Limits::unlimited()),
            &options
        )
        .is_ok()
    );
}

#[test]
fn an_entry_too_large_for_memory_fails_the_walk_without_an_abort() {
    // `tmpfs` in `/dev/shm` takes a sparse file of any `i64` length;
    // 2^60 bytes are more than any address space can hold.
    let Ok(dir) = tempfile::tempdir_in("/dev/shm") else {
        eprintln!("skipped: no /dev/shm here for a sparse file of 2^60 bytes");
        return;
    };
    fs::write(dir.path().join("a.desktop"), ENTRY).unwrap();
    let large = fs::File::create(dir.path().join("large.desktop")).unwrap();
    if large.set_len(1 << 60).is_err() {
        eprintln!("skipped: /dev/shm takes no sparse file of 2^60 bytes");
        return;
    }
    let err = mimeinfo_cache_with(dir.path(), &Limits::unlimited()).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::OutOfMemory, "{err}");
    // The default limit skips it.
    assert_eq!(
        mimeinfo_cache(dir.path()).unwrap(),
        "[MIME Cache]\ntext/plain=a.desktop;\n".as_bytes()
    );
}
