//! What `gio_conformance` cannot compare: the search order, diagnostics,
//! refresh, `TryExec`, how matches are labelled, editing the user's file,
//! and planning to open files and URIs.

mod common;

use std::fs;
use std::sync::Arc;

use common::Fixture;
use xdg_kit_desktop_entry::{AppIndex, Launch, LaunchOptions, Target};
use xdg_kit_mime::{MimeDatabase, MimeType};
use xdg_kit_mime_apps::{Matched, MimeAppsList, OpenError, TypePattern, open_plan};

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
                Matched::Wildcard(TypePattern::Media("text".into()))
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
            Matched::Wildcard(TypePattern::Media("x-scheme-handler".into()))
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
    let uri = format!("file://{}", notes.display());
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
    let link_uri = format!("file://{}", fixture.path("files/site.desktop").display());
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
