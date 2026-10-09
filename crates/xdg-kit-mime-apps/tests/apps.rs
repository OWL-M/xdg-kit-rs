//! What `gio_conformance` cannot compare: the search order, diagnostics,
//! refresh, `TryExec`, how matches are labelled, editing the user's file,
//! and planning to open files and URIs.

mod common;

use std::fs;
use std::sync::Arc;

use common::Fixture;
use xdg_kit_desktop_entry::{AppIndex, Launch, LaunchOptions, Target};
use xdg_kit_mime::{MimeDatabase, MimeType};
use xdg_kit_mime_apps::{Matched, MediaType, MimeAppsList, OpenError, TypePattern, open_plan};

fn m(text: &str) -> MimeType {
    MimeType::new(text).unwrap()
}

fn ids(fixture: &Fixture, mime: &str) -> Vec<String> {
    fixture
        .mime_apps()
        .apps_for(&m(mime))
        .into_iter()
        .map(|found| found.app.id().to_owned())
        .collect()
}

#[test]
fn files_are_searched_in_the_spec_order() {
    let mut fixture = Fixture::new();
    fixture.desktops = vec!["XKT".into(), "Other".into()];
    let apps = fixture.mime_apps();
    let files: Vec<String> = apps
        .files()
        .iter()
        .map(|path| {
            path.strip_prefix(fixture.root.path())
                .unwrap()
                .display()
                .to_string()
        })
        .collect();
    let level = |dir: &str| {
        [
            format!("{dir}/xkt-mimeapps.list"),
            format!("{dir}/other-mimeapps.list"),
            format!("{dir}/mimeapps.list"),
        ]
    };
    let expected: Vec<String> = [
        "config",
        "etc",
        "etc2",
        "local/applications",
        "share1/applications",
        "share2/applications",
        "mime-data/applications",
    ]
    .iter()
    .flat_map(|dir| level(dir))
    .collect();
    assert_eq!(files, expected);
}

#[test]
fn desktop_names_cannot_lead_elsewhere() {
    let mut fixture = Fixture::new();
    fixture.desktops = vec![
        "../outside/x".into(),
        "/abs".into(),
        "a/b".into(),
        "..".into(),
        "Ok".into(),
    ];
    fixture.app(
        "share1/applications",
        "xkt-a.desktop",
        "text/x-xkt-plain;",
        "",
    );
    fixture.app(
        "share1/applications",
        "xkt-b.desktop",
        "text/x-xkt-plain;",
        "",
    );
    fixture.write(
        "outside/x-mimeapps.list",
        "[Default Applications]\ntext/x-xkt-plain=xkt-b.desktop;\n",
    );
    let apps = fixture.mime_apps();
    assert!(
        apps.files()
            .iter()
            .all(|file| file.starts_with(fixture.root.path())
                && !file.to_string_lossy().contains("..")),
        "{:?}",
        apps.files()
    );
    assert!(
        apps.files()
            .iter()
            .any(|file| file.ends_with("config/ok-mimeapps.list"))
    );
    assert_eq!(
        apps.default_for(&m("text/x-xkt-plain")).unwrap().id(),
        "xkt-a.desktop"
    );
    let skipped = apps
        .diagnostics()
        .iter()
        .filter(|found| found.message.contains("cannot name a file"))
        .count();
    assert_eq!(skipped, 4, "{:?}", apps.diagnostics());
}

#[test]
fn a_config_folder_that_is_an_applications_folder_keeps_its_apps() {
    let fixture = Fixture::new();
    fixture.app(
        "share1/applications",
        "xkt-a.desktop",
        "text/x-xkt-plain;",
        "",
    );
    let mut vars = fixture.vars();
    for (name, value) in &mut vars {
        if *name == "XDG_CONFIG_DIRS" {
            *value = fixture.path("share1/applications").into_os_string();
        }
    }
    let base_dirs = xdg_kit_basedir::BaseDirs::from_vars(|name| {
        vars.iter()
            .find(|(known, _)| *known == name)
            .map(|(_, value)| value.clone())
    })
    .unwrap();
    let apps = xdg_kit_mime_apps::MimeApps::load(
        &base_dirs,
        &xdg_kit_desktop_entry::Desktops::new(Vec::<String>::new()),
        Arc::new(AppIndex::load(&base_dirs)),
        Arc::new(MimeDatabase::from_dirs([fixture.path("mime-data/mime")])),
    );
    let found: Vec<String> = apps
        .apps_for(&m("text/x-xkt-plain"))
        .into_iter()
        .map(|found| found.app.id().to_owned())
        .collect();
    assert_eq!(found, ["xkt-a.desktop"]);
}

#[test]
fn a_config_home_that_is_an_applications_folder_lists_its_apps_later() {
    // Its desktop files count where the data levels are, so a removal in
    // $XDG_CONFIG_DIRS reaches them.
    let mut fixture = Fixture::new();
    fixture.folders = vec![("XDG_CONFIG_HOME", "local/applications")];
    fixture.app(
        "local/applications",
        "xkt-a.desktop",
        "text/x-xkt-plain;",
        "",
    );
    fixture.app(
        "share1/applications",
        "xkt-b.desktop",
        "text/x-xkt-plain;",
        "",
    );
    fixture.write(
        "etc/mimeapps.list",
        "[Removed Associations]\ntext/x-xkt-plain=xkt-a.desktop;\n",
    );
    assert_eq!(ids(&fixture, "text/x-xkt-plain"), ["xkt-b.desktop"]);
    assert_eq!(
        fixture
            .mime_apps()
            .default_for(&m("text/x-xkt-plain"))
            .unwrap()
            .id(),
        "xkt-b.desktop"
    );
}

#[test]
fn a_config_dir_that_is_an_applications_folder_lists_its_apps_later() {
    // The data home comes before the data dirs, wherever the config dirs
    // point.
    let mut fixture = Fixture::new();
    fixture.folders = vec![("XDG_CONFIG_DIRS", "share1/applications")];
    fixture.app(
        "local/applications",
        "xkt-local.desktop",
        "text/x-xkt-plain;",
        "",
    );
    fixture.app(
        "share1/applications",
        "xkt-share.desktop",
        "text/x-xkt-plain;",
        "",
    );
    assert_eq!(
        ids(&fixture, "text/x-xkt-plain"),
        ["xkt-local.desktop", "xkt-share.desktop"]
    );
    let apps = fixture.mime_apps();
    assert_eq!(
        apps.default_for(&m("text/x-xkt-plain")).unwrap().id(),
        "xkt-local.desktop"
    );
    let share1 = fixture.path("share1/applications/mimeapps.list");
    assert_eq!(
        apps.files().iter().filter(|file| **file == share1).count(),
        1,
        "the file is read once: {:?}",
        apps.files()
    );
}

#[test]
fn an_applications_folder_listed_again_counts_once() {
    // The config home is the data home's applications folder, and the data
    // dirs list the data home again: the folder is one data level, and an
    // index without it is reported once.
    let mut fixture = Fixture::new();
    fixture.folders = vec![("XDG_CONFIG_HOME", "local/applications")];
    fixture.prepended = vec![("XDG_DATA_DIRS", "local")];
    fixture.app(
        "local/applications",
        "xkt-local.desktop",
        "text/x-xkt-plain;",
        "",
    );
    fixture.app(
        "share1/applications",
        "xkt-share.desktop",
        "text/x-xkt-plain;",
        "",
    );
    let apps = fixture.mime_apps_with(AppIndex::from_dirs(
        fixture.applications().into_iter().skip(1),
    ));
    let missing = apps
        .diagnostics()
        .iter()
        .filter(|found| found.message.contains("not in the AppIndex"))
        .count();
    assert_eq!(missing, 1, "{:?}", apps.diagnostics());
    let found: Vec<String> = apps
        .apps_for(&m("text/x-xkt-plain"))
        .into_iter()
        .map(|found| found.app.id().to_owned())
        .collect();
    assert_eq!(found, ["xkt-share.desktop"]);
}

#[test]
fn matches_say_how_they_matched() {
    let fixture = Fixture::new();
    fixture.app(
        "share1/applications",
        "xkt-src.desktop",
        "text/x-xkt-alias;",
        "",
    );
    fixture.app(
        "share1/applications",
        "xkt-plain.desktop",
        "text/x-xkt-plain;",
        "",
    );
    fixture.app("share1/applications", "xkt-text.desktop", "text/*;", "");
    fixture.app(
        "share1/applications",
        "xkt-data.desktop",
        "application/octet-stream;",
        "",
    );
    fixture.app("share1/applications", "xkt-all.desktop", "*/*;", "");
    let apps = fixture.mime_apps();
    let found: Vec<(String, Matched)> = apps
        .apps_for(&m("text/x-xkt-src"))
        .into_iter()
        .map(|found| (found.app.id().to_owned(), found.matched))
        .collect();
    assert_eq!(
        found,
        [
            ("xkt-src.desktop".into(), Matched::Exact),
            (
                "xkt-plain.desktop".into(),
                Matched::Parent(m("text/x-xkt-plain"))
            ),
            (
                "xkt-text.desktop".into(),
                Matched::Wildcard(TypePattern::Media(MediaType::new("text").unwrap()))
            ),
            (
                "xkt-data.desktop".into(),
                Matched::Parent(m("application/octet-stream"))
            ),
            (
                "xkt-all.desktop".into(),
                Matched::Wildcard(TypePattern::Any)
            ),
        ]
    );
    // Wildcards rank below the type's own parents, and `*/*` below
    // application/octet-stream (any data).
    assert_eq!(
        apps.default_for(&m("image/x-xkt-png")).unwrap().id(),
        "xkt-data.desktop"
    );
    assert_eq!(
        apps.default_for(&m("inode/directory")).unwrap().id(),
        "xkt-all.desktop",
        "inode types are not data"
    );
}

#[test]
fn any_data_stays_below_the_media_wildcard_when_aliased() {
    // A database may make application/octet-stream an alias; handlers for
    // any data still rank below the media wildcard.
    let fixture = Fixture::new();
    fixture.add_types(
        "aliased",
        r#"<?xml version="1.0"?>
<mime-info xmlns="http://www.freedesktop.org/standards/shared-mime-info">
  <mime-type type="application/x-xkt-data"><alias type="application/octet-stream"/></mime-type>
</mime-info>
"#,
    );
    fixture.app(
        "share1/applications",
        "xkt-data.desktop",
        "application/octet-stream;",
        "",
    );
    fixture.app("share1/applications", "xkt-image.desktop", "image/*;", "");
    let apps = fixture.mime_apps();
    let found: Vec<(String, Matched)> = apps
        .apps_for(&m("image/x-xkt-png"))
        .into_iter()
        .map(|found| (found.app.id().to_owned(), found.matched))
        .collect();
    assert_eq!(
        found,
        [
            (
                "xkt-image.desktop".into(),
                Matched::Wildcard(TypePattern::Media(MediaType::new("image").unwrap()))
            ),
            (
                "xkt-data.desktop".into(),
                Matched::Parent(m("application/x-xkt-data"))
            ),
        ]
    );
    assert_eq!(
        apps.default_for(&m("image/x-xkt-png")).unwrap().id(),
        "xkt-image.desktop"
    );
}

#[test]
fn try_exec_counts_when_asked() {
    let fixture = Fixture::new();
    fixture.app(
        "share1/applications",
        "xkt-a.desktop",
        "text/x-xkt-plain;",
        "TryExec=xkt-no-such-program\n",
    );
    fixture.app(
        "share1/applications",
        "xkt-b.desktop",
        "text/x-xkt-plain;",
        "",
    );
    let apps = fixture.mime_apps();
    assert_eq!(
        apps.default_for(&m("text/x-xkt-plain")).unwrap().id(),
        "xkt-a.desktop"
    );
    let apps = apps.with_try_exec(fixture.path("bin").into_os_string());
    assert_eq!(
        apps.default_for(&m("text/x-xkt-plain")).unwrap().id(),
        "xkt-b.desktop"
    );
}

#[test]
fn problems_in_list_files_reach_the_snapshot() {
    let fixture = Fixture::new();
    fixture.write(
        "config/mimeapps.list",
        "[Added Associations]\nnot a type=xkt-a.desktop;\n",
    );
    let apps = fixture.mime_apps();
    let messages: Vec<String> = apps.diagnostics().iter().map(ToString::to_string).collect();
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(
        messages[0].contains("config/mimeapps.list:2: "),
        "{messages:?}"
    );
}

/// Makes a FIFO at `path`.
#[cfg(not(target_vendor = "apple"))]
fn fifo(path: &std::path::Path) {
    rustix::fs::mknodat(
        rustix::fs::CWD,
        path,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        0,
    )
    .unwrap();
}

/// Runs `f` in a thread and returns its result. If a FIFO blocks the
/// read, the test fails after `timeout`.
#[cfg(not(target_vendor = "apple"))]
fn within<T: Send + 'static>(
    timeout: std::time::Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || send.send(f()));
    receive.recv_timeout(timeout).expect("it returns in time")
}

#[cfg(not(target_vendor = "apple"))]
#[test]
fn a_fifo_list_file_is_reported_not_waited_on() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.path("config")).unwrap();
    fifo(&fixture.path("config/mimeapps.list"));
    let messages = within(std::time::Duration::from_secs(10), move || {
        let apps = fixture.mime_apps();
        apps.diagnostics()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<String>>()
    });
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(
        messages[0].contains("config/mimeapps.list: cannot be read"),
        "{messages:?}"
    );
}

#[test]
fn problems_in_list_files_see_aliases() {
    let fixture = Fixture::new();
    fixture.write(
        "config/mimeapps.list",
        "[Added Associations]\ntext/x-xkt-alias=xkt-a.desktop;\n[Removed Associations]\ntext/x-xkt-src=xkt-a.desktop;\n",
    );
    let apps = fixture.mime_apps();
    let messages: Vec<String> = apps.diagnostics().iter().map(ToString::to_string).collect();
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(
        messages[0].contains("both added and removed"),
        "{messages:?}"
    );
}

#[test]
fn edits_show_after_a_refresh() {
    let fixture = Fixture::new();
    fixture.app(
        "share1/applications",
        "xkt-a.desktop",
        "text/x-xkt-plain;",
        "",
    );
    fixture.app(
        "share1/applications",
        "xkt-b.desktop",
        "image/x-xkt-png;",
        "",
    );
    let apps = fixture.mime_apps();
    let (index, mime) = (
        Arc::clone(apps.app_index()),
        Arc::clone(apps.mime_database()),
    );
    assert!(apps.refresh(&index, &mime).is_none());
    assert_eq!(ids(&fixture, "text/x-xkt-plain"), ["xkt-a.desktop"]);

    let pattern = TypePattern::parse("text/x-xkt-plain").unwrap();
    let mut list = MimeAppsList::for_user(&fixture.base_dirs()).unwrap();
    list.set_default(&pattern, "xkt-b.desktop").unwrap();
    list.write().unwrap();
    let apps = apps
        .refresh(&index, &mime)
        .expect("the user's file appeared");
    assert_eq!(
        apps.default_for(&m("text/x-xkt-plain")).unwrap().id(),
        "xkt-b.desktop"
    );
    assert!(apps.refresh(&index, &mime).is_none());

    let mut list = MimeAppsList::for_user(&fixture.base_dirs()).unwrap();
    list.remove_association(&pattern, "xkt-a.desktop").unwrap();
    list.write().unwrap();
    let apps = apps
        .refresh(&index, &mime)
        .expect("the user's file changed");
    let found: Vec<String> = apps
        .apps_for(&m("text/x-xkt-plain"))
        .into_iter()
        .map(|found| found.app.id().to_owned())
        .collect();
    assert_eq!(found, ["xkt-b.desktop"]);

    // A new index (an application installed) gives a new snapshot too.
    fixture.app(
        "share2/applications",
        "xkt-c.desktop",
        "text/x-xkt-plain;",
        "",
    );
    let index = Arc::new(AppIndex::load(&fixture.base_dirs()));
    let apps = apps.refresh(&index, &mime).expect("a new index");
    assert_eq!(apps.apps_for(&m("text/x-xkt-plain")).len(), 2);
    let mime = Arc::new(MimeDatabase::from_dirs([fixture.path("mime-data/mime")]));
    assert!(apps.refresh(&index, &mime).is_some(), "a new database");
}

#[test]
fn only_applications_are_associated() {
    let fixture = Fixture::new();
    let dir = "share1/applications";
    fixture.write(
        &format!("{dir}/xkt-a-link.desktop"),
        "[Desktop Entry]\nType=Link\nName=L\nURL=https://example.org/\nMimeType=text/x-xkt-plain;\n",
    );
    fixture.write(
        &format!("{dir}/xkt-b-folder.desktop"),
        "[Desktop Entry]\nType=Directory\nName=D\nMimeType=text/x-xkt-plain;\n",
    );
    fixture.app(dir, "xkt-c.desktop", "text/x-xkt-plain;", "");
    fixture.write(
        "config/mimeapps.list",
        "[Default Applications]\ntext/x-xkt-plain=xkt-a-link.desktop;\n",
    );
    assert_eq!(ids(&fixture, "text/x-xkt-plain"), ["xkt-c.desktop"]);
    assert_eq!(
        fixture
            .mime_apps()
            .default_for(&m("text/x-xkt-plain"))
            .unwrap()
            .id(),
        "xkt-c.desktop"
    );
}

#[test]
fn a_file_of_unknown_type_does_not_hold_its_id() {
    // The index ignores the higher file, so the lower one is the ID's file,
    // with its `MimeType=` and the lower level's added association. GLib
    // counts every file name in a folder, so the higher file would hide
    // both there; ignoring it is deliberate (decision 21).
    let fixture = Fixture::new();
    fixture.write(
        "share1/applications/xkt-foo.desktop",
        "[Desktop Entry]\nType=Service\nName=S\nMimeType=text/x-xkt-plain;\n",
    );
    fixture.app(
        "share2/applications",
        "xkt-foo.desktop",
        "text/x-xkt-plain;",
        "",
    );
    fixture.write(
        "share2/applications/mimeapps.list",
        "[Added Associations]\ntext/x-xkt-foo=xkt-foo.desktop;\n",
    );
    let apps = fixture.mime_apps();
    let lower = fixture.path("share2/applications/xkt-foo.desktop");
    for mime in ["text/x-xkt-plain", "text/x-xkt-foo"] {
        let found = apps.apps_for(&m(mime));
        let paths: Vec<_> = found.iter().map(|found| found.app.path()).collect();
        assert_eq!(paths, [lower.as_path()], "{mime}");
    }
}

#[test]
fn a_url_scheme_has_only_its_own_handlers() {
    let fixture = Fixture::new();
    let dir = "share1/applications";
    fixture.app(dir, "xkt-data.desktop", "application/octet-stream;", "");
    fixture.app(dir, "xkt-all.desktop", "*/*;", "");
    fixture.app(dir, "xkt-urls.desktop", "x-scheme-handler/*;", "");
    let apps = fixture.mime_apps();
    let found: Vec<(String, Matched)> = apps
        .apps_for(&m("x-scheme-handler/xkt-none"))
        .into_iter()
        .map(|found| (found.app.id().to_owned(), found.matched))
        .collect();
    assert_eq!(
        found,
        [(
            "xkt-urls.desktop".into(),
            Matched::Wildcard(TypePattern::Media(
                MediaType::new("x-scheme-handler").unwrap()
            ))
        )]
    );
    fs::remove_file(fixture.path("share1/applications/xkt-urls.desktop")).unwrap();
    let apps = fixture.mime_apps();
    assert!(apps.default_for(&m("x-scheme-handler/xkt-none")).is_none());
    assert!(matches!(
        open_plan(
            &Target::Uri("xkt-none:x".into()),
            &apps,
            &LaunchOptions::default()
        ),
        Err(OpenError::NoApplication(_))
    ));
}

#[test]
fn a_link_is_followed_when_desktop_entries_are_an_alias() {
    // Detection gives the canonical type, which the database may name
    // differently.
    let fixture = Fixture::new();
    fixture.add_types(
        "aliased",
        r#"<?xml version="1.0"?>
<mime-info xmlns="http://www.freedesktop.org/standards/shared-mime-info">
  <mime-type type="application/x-xkt-desktop">
    <alias type="application/x-desktop"/>
    <glob pattern="*.desktop"/>
  </mime-type>
</mime-info>
"#,
    );
    fixture.write(
        "share1/applications/xkt-web.desktop",
        "[Desktop Entry]\nType=Application\nName=Web\nExec=true %u\nMimeType=x-scheme-handler/xkt-web;\n",
    );
    fixture.write(
        "files/site.desktop",
        "[Desktop Entry]\nType=Link\nName=Site\nURL=xkt-web://example.org/\n",
    );
    let apps = fixture.mime_apps();
    let path = fixture.path("files/site.desktop");
    assert_eq!(
        apps.mime_database()
            .type_for_path(&path, &xdg_kit_mime::PathOptions::default())
            .unwrap()
            .mime,
        m("application/x-xkt-desktop")
    );
    let plan = open_plan(&Target::Path(path), &apps, &LaunchOptions::default()).unwrap();
    let Launch::Commands(commands) = plan.launch else {
        panic!("{plan:?}")
    };
    assert_eq!(commands[0].args, ["xkt-web://example.org/"]);
}

#[test]
fn a_url_scheme_aliased_to_data_has_only_its_own_handlers() {
    // A database may make a scheme's type an alias of a data type; the URL
    // still gets no data application (decision 8).
    let fixture = Fixture::new();
    fixture.add_types(
        "aliased",
        r#"<?xml version="1.0"?>
<mime-info xmlns="http://www.freedesktop.org/standards/shared-mime-info">
  <mime-type type="application/x-xkt-web"><alias type="x-scheme-handler/xkt-web"/></mime-type>
</mime-info>
"#,
    );
    let dir = "share1/applications";
    fixture.app(dir, "xkt-web.desktop", "x-scheme-handler/xkt-web;", "");
    fixture.app(dir, "xkt-data.desktop", "application/octet-stream;", "");
    fixture.app(dir, "xkt-all.desktop", "*/*;", "");
    fixture.app(dir, "xkt-apps.desktop", "application/*;", "");
    fixture.app(dir, "xkt-urls.desktop", "x-scheme-handler/*;", "");
    let apps = fixture.mime_apps();
    assert_eq!(
        apps.mime_database()
            .canonical(&m("x-scheme-handler/xkt-web")),
        m("application/x-xkt-web")
    );
    let found: Vec<(String, Matched)> = apps
        .apps_for(&m("x-scheme-handler/xkt-web"))
        .into_iter()
        .map(|found| (found.app.id().to_owned(), found.matched))
        .collect();
    assert_eq!(
        found,
        [
            ("xkt-web.desktop".into(), Matched::Exact),
            (
                "xkt-urls.desktop".into(),
                Matched::Wildcard(TypePattern::Media(
                    MediaType::new("x-scheme-handler").unwrap()
                ))
            ),
        ]
    );
}

#[test]
fn edits_reach_keys_spelled_as_aliases() {
    let fixture = Fixture::new();
    fixture.write(
        "config/mimeapps.list",
        "[Default Applications]\ntext/x-xkt-alias=xkt-a.desktop;\n",
    );
    fixture.app(
        "share1/applications",
        "xkt-a.desktop",
        "text/x-xkt-src;",
        "",
    );
    fixture.app(
        "share1/applications",
        "xkt-b.desktop",
        "text/x-xkt-src;",
        "",
    );
    let database = Arc::new(MimeDatabase::from_dirs([fixture.path("mime-data/mime")]));
    let mut list = MimeAppsList::for_user(&fixture.base_dirs())
        .unwrap()
        .resolving_aliases(Arc::clone(&database));
    let src = TypePattern::parse("text/x-xkt-src").unwrap();
    list.set_default(&src, "xkt-b.desktop").unwrap();
    list.write().unwrap();
    let text = fs::read_to_string(fixture.path("config/mimeapps.list")).unwrap();
    assert_eq!(
        text,
        "[Default Applications]\ntext/x-xkt-alias=xkt-b.desktop;xkt-a.desktop;\n\n[Added Associations]\ntext/x-xkt-src=xkt-b.desktop;\n"
    );
    assert_eq!(
        fixture
            .mime_apps()
            .default_for(&m("text/x-xkt-src"))
            .unwrap()
            .id(),
        "xkt-b.desktop"
    );
}

/// The `file:` URI of `path`: bytes other than RFC 3986 unreserved ones
/// and `/` are percent-encoded, as the temporary folder may hold any.
fn file_uri(path: &std::path::Path) -> String {
    use std::fmt::Write;
    use std::os::unix::ffi::OsStrExt;
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            uri.push(char::from(byte));
        } else {
            let _ = write!(uri, "%{byte:02X}");
        }
    }
    uri
}

#[test]
fn opening_files_and_uris() {
    let fixture = Fixture::new();
    fixture.app(
        "share1/applications",
        "xkt-text.desktop",
        "text/x-xkt-plain;",
        "",
    );
    fixture.write(
        "share1/applications/xkt-web.desktop",
        "[Desktop Entry]\nType=Application\nName=Web\nExec=true %u\nMimeType=x-scheme-handler/xkt-web;\n",
    );
    fixture.write("files/notes.xkt-plain", "hello\n");
    fixture.write(
        "files/site.desktop",
        "[Desktop Entry]\nType=Link\nName=Site\nURL=xkt-web://example.org/\n",
    );
    fixture.write(
        "files/broken.desktop",
        "[Desktop Entry]\nType=Link\nName=Broken\n",
    );
    let apps = fixture.mime_apps();
    let options = LaunchOptions::default();
    let program = |target: Target| -> String {
        let plan = open_plan(&target, &apps, &options).unwrap();
        let Launch::Commands(commands) = plan.launch else {
            panic!("{plan:?}")
        };
        format!(
            "{} {:?}",
            commands[0].program.to_string_lossy(),
            commands[0].args
        )
    };
    let notes = fixture.path("files/notes.xkt-plain");
    assert_eq!(
        program(Target::Path(notes.clone())),
        format!("true [{:?}]", notes.as_os_str())
    );
    let uri = file_uri(&notes);
    assert_eq!(
        program(Target::Uri(uri)),
        format!("true [{:?}]", notes.as_os_str()),
        "a file: URI opens as the file"
    );
    assert_eq!(
        program(Target::Uri("xkt-web://example.org/a".into())),
        "true [\"xkt-web://example.org/a\"]"
    );
    assert_eq!(
        program(Target::Path(fixture.path("files/site.desktop"))),
        "true [\"xkt-web://example.org/\"]",
        "a link opens its URL"
    );
    let link_uri = file_uri(&fixture.path("files/site.desktop"));
    assert_eq!(
        program(Target::Uri(link_uri)),
        "true [\"xkt-web://example.org/\"]",
        "as a file: URI too"
    );
    assert!(matches!(
        open_plan(
            &Target::Path(fixture.path("files/broken.desktop")),
            &apps,
            &options
        ),
        Err(OpenError::LinkWithoutUrl(_))
    ));
    assert!(matches!(
        open_plan(&Target::Uri("xkt-none:x".into()), &apps, &options),
        Err(OpenError::NoApplication(mime)) if mime.as_str() == "x-scheme-handler/xkt-none"
    ));
    assert!(matches!(
        open_plan(&Target::Uri("no scheme".into()), &apps, &options),
        Err(OpenError::BadUri(_))
    ));
    assert!(matches!(
        open_plan(
            &Target::Path(fixture.path("files/missing")),
            &apps,
            &options
        ),
        Err(OpenError::Type(..))
    ));
    fs::remove_file(&notes).unwrap();
}

#[test]
fn a_links_url_is_shown_escaped_and_cut() {
    let fixture = Fixture::new();
    // An escape character, a line feed once unescaped, and length.
    let bad = format!("\x1b\\n{}", "a".repeat(5000));
    fixture.write(
        "files/no-scheme.desktop",
        &format!("[Desktop Entry]\nType=Link\nName=a\nURL={bad}\n"),
    );
    fixture.write(
        "files/missing.desktop",
        &format!("[Desktop Entry]\nType=Link\nName=a\nURL=file:///missing/{bad}\n"),
    );
    fixture.write(
        &format!("files/no-url\x1b{}.desktop", "c".repeat(100)),
        "[Desktop Entry]\nType=Link\nName=a\n",
    );
    let apps = fixture.mime_apps();
    let options = LaunchOptions::default();
    let message = |target: Target| open_plan(&target, &apps, &options).unwrap_err().to_string();
    let no_scheme = message(Target::Path(fixture.path("files/no-scheme.desktop")));
    assert!(!no_scheme.chars().any(char::is_control), "{no_scheme:?}");
    assert!(no_scheme.chars().count() <= 250, "{no_scheme:?}");
    // Paths are escaped but shown whole.
    let missing = message(Target::Path(fixture.path("files/missing.desktop")));
    assert!(!missing.chars().any(char::is_control), "{missing:?}");
    let path = format!("/missing/\\u{{1b}}\\n{}", "a".repeat(5000));
    assert!(
        missing.starts_with(&format!("{path}: type not found: ")),
        "{missing:?}"
    );
    let no_url = message(Target::Path(
        fixture.path(&format!("files/no-url\x1b{}.desktop", "c".repeat(100))),
    ));
    assert!(!no_url.chars().any(char::is_control), "{no_url:?}");
    assert!(no_url.contains(&"c".repeat(100)), "{no_url:?}");
    // A type, which has only safe characters and is short, is shown whole.
    let scheme = format!("xkt-{}", "s".repeat(100));
    let no_app = message(Target::Uri(format!("{scheme}:x")));
    assert_eq!(
        no_app,
        format!("no application opens x-scheme-handler/{scheme}")
    );
}

#[test]
fn desktop_names_from_the_environment_are_shown_escaped_and_cut() {
    let mut fixture = Fixture::new();
    fixture.desktops = vec![format!("a/\x1b{}", "d".repeat(5000))];
    let apps = fixture.mime_apps();
    let found: Vec<String> = apps
        .diagnostics()
        .iter()
        .map(|found| found.message.clone())
        .filter(|message| message.starts_with("desktop name"))
        .collect();
    assert_eq!(
        found,
        [format!(
            "desktop name \"a/\\u{{1b}}{}…\" cannot name a file; skipped",
            "d".repeat(29)
        )]
    );
}
