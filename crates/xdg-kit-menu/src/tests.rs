//! Loading, resolution and layout on fixtures in temporary directories:
//! nothing real is read, nothing is launched.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::{Limits, Menu, MenuContext, MenuError, MenuItem, MenuTree};

/// A configuration in a temporary directory: `config-home`, `config`,
/// `data-home` and `data` under one root.
struct Fixture {
    root: tempfile::TempDir,
    vars: HashMap<String, OsString>,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let at = |dir: &str| root.path().join(dir).into_os_string();
        let vars = HashMap::from([
            ("HOME".to_owned(), at("home")),
            ("XDG_CONFIG_HOME".to_owned(), at("config-home")),
            ("XDG_CONFIG_DIRS".to_owned(), at("config")),
            ("XDG_DATA_HOME".to_owned(), at("data-home")),
            ("XDG_DATA_DIRS".to_owned(), at("data")),
            ("XDG_CURRENT_DESKTOP".to_owned(), "GNOME".into()),
        ]);
        Self { root, vars }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.path().join(relative)
    }

    fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        path
    }

    fn app(&self, id_path: &str, extra: &str) -> PathBuf {
        self.write(
            &format!("data/applications/{id_path}"),
            &format!("[Desktop Entry]\nType=Application\nExec=true\n{extra}"),
        )
    }

    fn set(&mut self, name: &str, value: &str) {
        self.vars.insert(name.to_owned(), value.into());
    }

    fn context(&self) -> MenuContext {
        let mut context = MenuContext::from_vars(|name| self.vars.get(name).cloned()).unwrap();
        context.try_exec = None;
        context
    }

    fn load(&self, menu: &str) -> MenuTree {
        MenuTree::load_file(&self.context(), self.path(menu)).unwrap()
    }
}

/// The tree as `menu path: item…` lines, `>` before submenus.
fn lines(menu: &Menu) -> Vec<String> {
    let mut out = Vec::new();
    collect(menu, &mut out);
    out
}

fn collect(menu: &Menu, out: &mut Vec<String>) {
    for item in menu.items() {
        let line = match item {
            MenuItem::Entry(entry) => format!("{}: {}", menu.path(), entry.id()),
            MenuItem::Menu(child) => {
                out.push(format!("{}: >{}", menu.path(), child.name()));
                collect(child, out);
                continue;
            }
            MenuItem::Separator => format!("{}: ---", menu.path()),
            MenuItem::Header(header) => format!("{}: [{}]", menu.path(), header.label()),
            MenuItem::Alias(alias) => format!("{}: ~{}", menu.path(), alias.label()),
        };
        out.push(line);
    }
}

const DOCTYPE: &str = "<!DOCTYPE Menu PUBLIC \"-//freedesktop//DTD Menu 1.0//EN\"\n \
    \"http://www.freedesktop.org/standards/menu-spec/1.0/menu.dtd\">\n";

mod discovery {
    use super::*;

    #[test]
    fn the_users_menu_replaces_the_systems_and_the_prefix_selects_one() {
        let mut fixture = Fixture::new();
        fixture.app("a.desktop", "Name=A\n");
        let body = |name: &str| {
            format!(
                "{DOCTYPE}<Menu><Name>{name}</Name><DefaultAppDirs/><Include><All/></Include></Menu>"
            )
        };
        fixture.write("config/menus/applications.menu", &body("System"));
        let system = MenuTree::load(&fixture.context()).unwrap();
        assert_eq!(system.root().name(), "System");
        fixture.write("config-home/menus/applications.menu", &body("User"));
        assert_eq!(
            MenuTree::load(&fixture.context()).unwrap().root().name(),
            "User"
        );
        fixture.write("config/menus/gnome-applications.menu", &body("Gnome"));
        fixture.set("XDG_MENU_PREFIX", "gnome-");
        let prefixed = MenuTree::load(&fixture.context()).unwrap();
        assert_eq!(prefixed.root().name(), "Gnome");
        assert_eq!(
            prefixed.files(),
            [fixture.path("config/menus/gnome-applications.menu")]
        );
        // Another menu by name.
        fixture.write("config/menus/preferences.menu", &body("Prefs"));
        let named = MenuTree::load_named(&fixture.context(), "preferences.menu").unwrap();
        assert_eq!(named.root().name(), "Prefs");
        // None anywhere: every place looked is reported.
        let Err(MenuError::NotFound { searched }) =
            MenuTree::load_named(&fixture.context(), "absent.menu")
        else {
            panic!("not found");
        };
        assert_eq!(
            searched,
            [
                fixture.path("config-home/menus/absent.menu"),
                fixture.path("config/menus/absent.menu")
            ]
        );
    }

    #[test]
    fn default_merge_dirs_drop_the_prefix_and_follow_the_file_name() {
        let mut fixture = Fixture::new();
        fixture.app("a.desktop", "Categories=X-A;\n");
        fixture.app("b.desktop", "Categories=X-B;\n");
        fixture.write(
            "config/menus/gnome-applications.menu",
            "<Menu><Name>R</Name><DefaultAppDirs/><DefaultMergeDirs/></Menu>",
        );
        fixture.write(
            "config/menus/applications-merged/a.menu",
            "<Menu><Name>R</Name><Menu><Name>A</Name><Include><Category>X-A</Category></Include></Menu></Menu>",
        );
        fixture.write(
            "config/menus/gnome-applications-merged/b.menu",
            "<Menu><Name>R</Name><Menu><Name>B</Name><Include><Category>X-B</Category></Include></Menu></Menu>",
        );
        fixture.set("XDG_MENU_PREFIX", "gnome-");
        let tree = MenuTree::load(&fixture.context()).unwrap();
        // The prefix is dropped: `applications-merged`, not
        // `gnome-applications-merged`.
        assert_eq!(lines(tree.root()), [": >A", "A: a.desktop"]);
        fixture.write(
            "config/menus/preferences.menu",
            "<Menu><Name>P</Name><DefaultAppDirs/><DefaultMergeDirs/></Menu>",
        );
        fixture.write(
            "config/menus/preferences-merged/b.menu",
            "<Menu><Name>P</Name><Menu><Name>B</Name><Include><Category>X-B</Category></Include></Menu></Menu>",
        );
        let preferences = MenuTree::load_named(&fixture.context(), "preferences.menu").unwrap();
        assert_eq!(lines(preferences.root()), [": >B", "B: b.desktop"]);
    }
}

mod paths {
    use super::*;

    #[test]
    fn relative_references_are_relative_to_the_file_they_are_written_in() {
        let fixture = Fixture::new();
        fixture.write(
            "elsewhere/apps/a.desktop",
            "[Desktop Entry]\nType=Application\nExec=true\nCategories=X-A;\n",
        );
        fixture.write(
            "elsewhere/dirs/a.directory",
            "[Desktop Entry]\nType=Directory\nName=Merged A\n",
        );
        fixture.write(
            "elsewhere/part.menu",
            "<Menu><Name>x</Name><AppDir>apps</AppDir><DirectoryDir>dirs</DirectoryDir>\
             <Menu><Name>A</Name><Directory>a.directory</Directory>\
             <Include><Category>X-A</Category></Include></Menu></Menu>",
        );
        fixture.write(
            "menus/main.menu",
            "<Menu><Name>R</Name><MergeFile>../elsewhere/part.menu</MergeFile></Menu>",
        );
        let tree = fixture.load("menus/main.menu");
        assert_eq!(lines(tree.root()), [": >A", "A: a.desktop"]);
        let a = tree.root().submenu("A").unwrap();
        assert_eq!(a.label(), "Merged A");
        // Paths are joined as written (`..` is not collapsed, which would be
        // wrong across symbolic links): compare the files they name.
        let same = |found: &Path, expected: &str| {
            assert_eq!(
                fs::canonicalize(found).unwrap(),
                fs::canonicalize(fixture.path(expected)).unwrap()
            );
        };
        same(
            a.directory().unwrap().path().unwrap(),
            "elsewhere/dirs/a.directory",
        );
        // `A` is written only in the merged file; the root in both.
        let [part] = a.sources() else {
            panic!("one source: {:?}", a.sources());
        };
        same(part, "elsewhere/part.menu");
        let [main, part] = tree.root().sources() else {
            panic!("two sources: {:?}", tree.root().sources());
        };
        same(main, "menus/main.menu");
        same(part, "elsewhere/part.menu");
    }

    #[test]
    fn directory_names_cannot_climb_out_of_their_directory() {
        let fixture = Fixture::new();
        fixture.app("a.desktop", "");
        fixture.write(
            "outside.directory",
            "[Desktop Entry]\nType=Directory\nName=Escaped\n",
        );
        fixture.write(
            "menu.menu",
            "<Menu><Name>R</Name><DefaultAppDirs/><DirectoryDir>data</DirectoryDir>\
             <Menu><Name>M</Name><Directory>../outside.directory</Directory><Include><All/></Include></Menu></Menu>",
        );
        let tree = fixture.load("menu.menu");
        assert_eq!(tree.root().submenu("M").unwrap().label(), "M");
    }
}

mod semantics {
    use super::*;

    #[test]
    fn excluded_entries_stay_allocated_and_deleted_menus_still_allocate() {
        let fixture = Fixture::new();
        fixture.app("a.desktop", "Categories=Game;\n");
        fixture.app("b.desktop", "Categories=Game;Card;\n");
        fixture.app("c.desktop", "Categories=Office;\n");
        fixture.app("d.desktop", "");
        fixture.write(
            "menu.menu",
            "<Menu><Name>R</Name><DefaultAppDirs/>\
             <Menu><Name>Games</Name><Include><Category>Game</Category></Include><Exclude><Category>Card</Category></Exclude></Menu>\
             <Menu><Name>Gone</Name><Include><Category>Office</Category></Include><Deleted/><NotDeleted/><Deleted/></Menu>\
             <Menu><Name>Other</Name><OnlyUnallocated/><NotOnlyUnallocated/><OnlyUnallocated/><Include><All/></Include></Menu></Menu>",
        );
        let tree = fixture.load("menu.menu");
        assert_eq!(
            lines(tree.root()),
            [
                ": >Games",
                "Games: a.desktop",
                ": >Other",
                "Other: d.desktop"
            ]
        );
    }

    #[test]
    fn unknown_rules_never_widen_a_menu() {
        let fixture = Fixture::new();
        fixture.app("a.desktop", "Categories=X-A;\n");
        fixture.app("b.desktop", "Categories=X-B;\n");
        fixture.write(
            "menu.menu",
            "<Menu><Name>R</Name><DefaultAppDirs/>\
             <Menu><Name>In</Name><Include><Regex>.*</Regex><Category>X-A</Category></Include></Menu>\
             <Menu><Name>NotUnknown</Name><Include><Not><Regex>.*</Regex></Not></Include></Menu>\
             <Menu><Name>Ex</Name><Include><All/></Include><Exclude><Regex>b</Regex></Exclude></Menu></Menu>",
        );
        let tree = fixture.load("menu.menu");
        // The unknown rule adds nothing; its negation adds nothing; under
        // <Exclude> it may match, so it removes.
        assert_eq!(lines(tree.root()), [": >In", "In: a.desktop"]);
        assert!(
            tree.diagnostics()
                .iter()
                .any(|d| d.message.contains("<Regex>"))
        );
    }

    #[test]
    fn an_unnamed_menu_is_not_shown_but_still_allocates() {
        let fixture = Fixture::new();
        fixture.app("a.desktop", "Categories=X-A;\n");
        fixture.app("b.desktop", "");
        fixture.write(
            "menu.menu",
            "<Menu><Name>R</Name><DefaultAppDirs/>\
             <Menu><Name>bad/name</Name><Include><Category>X-A</Category></Include></Menu>\
             <Menu><Include><Category>X-A</Category></Include></Menu>\
             <Menu><Name>Rest</Name><OnlyUnallocated/><Include><All/></Include></Menu></Menu>",
        );
        let tree = fixture.load("menu.menu");
        assert_eq!(lines(tree.root()), [": >Rest", "Rest: b.desktop"]);
    }

    #[test]
    fn moves_into_themselves_are_refused_and_parents_are_made() {
        let fixture = Fixture::new();
        fixture.app("a.desktop", "Categories=X-A;\n");
        fixture.write(
            "menu.menu",
            "<Menu><Name>R</Name><DefaultAppDirs/>\
             <Menu><Name>A</Name><Include><Category>X-A</Category></Include></Menu>\
             <Move><Old>A</Old><New>A/Inner</New></Move>\
             <Move><Old>A</Old><New>X/Y/A2</New></Move></Menu>",
        );
        let tree = fixture.load("menu.menu");
        assert_eq!(
            lines(tree.root()),
            [": >X", "X: >Y", "X/Y: >A2", "X/Y/A2: a.desktop"]
        );
        assert!(
            tree.diagnostics()
                .iter()
                .any(|d| d.message.contains("into itself"))
        );
    }

    #[test]
    fn visibility_follows_desktops_and_no_display_after_allocation() {
        let mut fixture = Fixture::new();
        fixture.app("quiet.desktop", "Categories=X-A;\nNoDisplay=true\n");
        fixture.app("kde.desktop", "Categories=X-A;\nOnlyShowIn=KDE;\n");
        fixture.app("plain.desktop", "");
        fixture.write(
            "menu.menu",
            "<Menu><Name>R</Name><DefaultAppDirs/>\
             <Menu><Name>A</Name><Include><Category>X-A</Category></Include></Menu>\
             <Menu><Name>Rest</Name><OnlyUnallocated/><Include><All/></Include></Menu></Menu>",
        );
        let tree = fixture.load("menu.menu");
        // Both were allocated by A, then hidden there: Rest does not get them.
        assert_eq!(lines(tree.root()), [": >Rest", "Rest: plain.desktop"]);
        fixture.set("XDG_CURRENT_DESKTOP", "KDE");
        let mut context = fixture.context();
        context.show.no_display = true;
        let shown = MenuTree::load_file(&context, fixture.path("menu.menu")).unwrap();
        assert_eq!(
            lines(shown.root()),
            [
                ": >A",
                "A: kde.desktop",
                "A: quiet.desktop",
                ": >Rest",
                "Rest: plain.desktop"
            ]
        );
    }

    #[test]
    fn try_exec_ignores_an_entry_as_if_absent() {
        let fixture = Fixture::new();
        fixture.write(
            "data/applications/a.desktop",
            "[Desktop Entry]\nType=Application\nExec=true\nName=System\n",
        );
        fixture.write(
            "data-home/applications/a.desktop",
            "[Desktop Entry]\nType=Application\nExec=true\nName=User\nTryExec=no-such-program-xkt\n",
        );
        fixture.write(
            "menu.menu",
            "<Menu><Name>R</Name><DefaultAppDirs/><Include><All/></Include></Menu>",
        );
        let mut context = fixture.context();
        let unchecked = MenuTree::load_file(&context, fixture.path("menu.menu")).unwrap();
        assert_eq!(unchecked.root().entries().next().unwrap().label(), "User");
        context.try_exec = Some(OsString::from("/nonexistent"));
        let checked = MenuTree::load_file(&context, fixture.path("menu.menu")).unwrap();
        assert_eq!(
            checked.root().entries().next().unwrap().label(),
            "System",
            "the less important copy stands in"
        );
    }

    #[test]
    fn entries_keep_the_path_launching_needs() {
        let fixture = Fixture::new();
        let path = fixture.app(
            "org.example.App.desktop",
            "Name=App\nExec=tool %k\nIcon=app-icon\n",
        );
        fixture.write(
            "menu.menu",
            "<Menu><Name>R</Name><DefaultAppDirs/><Include><All/></Include></Menu>",
        );
        let tree = fixture.load("menu.menu");
        let entry = tree.root().entries().next().unwrap();
        assert_eq!(entry.id(), "org.example.App.desktop");
        assert_eq!(entry.path(), path);
        assert_eq!(entry.entry().path(), Some(path.as_path()));
        assert_eq!(entry.icon(), Some("app-icon"));
        assert_eq!(entry.app_dir(), fixture.path("data/applications"));
        let plan = xdg_kit_desktop_entry::prepare_launch(
            entry.entry(),
            &[],
            &xdg_kit_desktop_entry::LaunchOptions::default(),
        )
        .unwrap();
        let xdg_kit_desktop_entry::Launch::Commands(commands) = plan.launch else {
            panic!("a command");
        };
        assert_eq!(commands[0].args, [path.as_os_str()], "%k is the file");
    }

    #[test]
    fn directory_captions_are_localized_and_fall_back() {
        let mut fixture = Fixture::new();
        fixture.app("a.desktop", "");
        fixture.write(
            "data/desktop-directories/d.directory",
            "[Desktop Entry]\nType=Directory\nName=Tools\nName[de]=Werkzeuge\nIcon=tools\n",
        );
        fixture.write(
            "data/desktop-directories/hidden.directory",
            "[Desktop Entry]\nType=Directory\nName=Hidden one\nHidden=true\n",
        );
        fixture.write(
            "menu.menu",
            "<Menu><Name>R</Name><DefaultAppDirs/><DefaultDirectoryDirs/>\
             <Menu><Name>M</Name><Directory>d.directory</Directory><Directory>hidden.directory</Directory>\
             <Include><All/></Include></Menu></Menu>",
        );
        fixture.set("LANG", "de_DE.UTF-8");
        let tree = fixture.load("menu.menu");
        let menu = tree.root().submenu("M").unwrap();
        assert_eq!(
            menu.label(),
            "Werkzeuge",
            "a Hidden .directory counts as missing"
        );
        assert_eq!(menu.icon(), Some("tools"));
        assert_eq!(menu.directory().unwrap().id(), "d.directory");
    }
}

mod layout {
    use super::*;

    #[test]
    fn layouts_order_separate_and_inline_at_the_point_of_reference() {
        let fixture = Fixture::new();
        for (id, name, category) in [
            ("a", "Alpha", "X-In"),
            ("z", "Zulu", "X-In"),
            ("m", "Mike", ""),
            ("n", "November", ""),
        ] {
            fixture.app(
                &format!("{id}.desktop"),
                &format!("Name={name}\nCategories={category};\n"),
            );
        }
        fixture.write(
            "menu.menu",
            "<Menu><Name>R</Name><DefaultAppDirs/>\
             <Include><Filename>m.desktop</Filename><Filename>n.desktop</Filename></Include>\
             <Menu><Name>In</Name><Include><Category>X-In</Category></Include></Menu>\
             <Layout><Separator/><Filename>n.desktop</Filename><Separator/><Separator/>\
             <Menuname inline=\"true\" inline_header=\"false\">In</Menuname><Merge type=\"files\"/>\
             <Separator/></Layout></Menu>",
        );
        let tree = fixture.load("menu.menu");
        assert_eq!(
            lines(tree.root()),
            [
                ": n.desktop",
                ": ---",
                ": a.desktop",
                ": z.desktop",
                ": m.desktop"
            ]
        );
    }

    #[test]
    fn a_layout_without_merges_shows_only_what_it_names() {
        let fixture = Fixture::new();
        fixture.app("a.desktop", "");
        fixture.app("b.desktop", "");
        fixture.write(
            "menu.menu",
            "<Menu><Name>R</Name><DefaultAppDirs/><Include><All/></Include>\
             <Layout><Filename>b.desktop</Filename></Layout></Menu>",
        );
        let tree = fixture.load("menu.menu");
        assert_eq!(lines(tree.root()), [": b.desktop"]);
        assert!(
            tree.diagnostics()
                .iter()
                .any(|d| d.message.contains("no <Merge>"))
        );
    }

    #[test]
    fn default_layouts_are_inherited_and_replaced_whole() {
        let fixture = Fixture::new();
        fixture.write(
            "menu.menu",
            "<Menu><Name>R</Name>\
             <DefaultLayout show_empty=\"true\"><Merge type=\"menus\"/><Merge type=\"files\"/></DefaultLayout>\
             <Menu><Name>Keeps</Name><Menu><Name>Empty</Name></Menu></Menu>\
             <Menu><Name>Resets</Name><DefaultLayout><Merge type=\"all\"/></DefaultLayout>\
             <Menu><Name>Empty</Name></Menu></Menu></Menu>",
        );
        let tree = fixture.load("menu.menu");
        // `Resets`'s own default layout drops show_empty for its children,
        // but `Resets` itself is placed by its parent's (show_empty=true).
        assert_eq!(
            lines(tree.root()),
            [": >Keeps", "Keeps: >Empty", ": >Resets"]
        );
    }
}

mod malformed {
    use super::*;

    #[test]
    fn a_broken_root_is_an_error_and_a_broken_merge_is_skipped() {
        let fixture = Fixture::new();
        fixture.app("a.desktop", "Categories=X-A;\n");
        for (name, text) in [
            ("not-xml.menu", "<Menu><Name>R</Name>"),
            ("not-menu.menu", "<Other/>"),
            ("nameless.menu", "<Menu><Include><All/></Include></Menu>"),
            (
                "entity.menu",
                "<!DOCTYPE Menu [<!ENTITY x \"y\">]><Menu><Name>&x;</Name></Menu>",
            ),
        ] {
            fixture.write(name, text);
            let err = MenuTree::load_file(&fixture.context(), fixture.path(name)).unwrap_err();
            assert!(matches!(err, MenuError::Malformed { .. }), "{name}: {err}");
        }
        fixture.write("broken.menu", "<Menu><Name>x</Name><Menu>");
        fixture.write(
            "menu.menu",
            "<Menu><Name>R</Name><DefaultAppDirs/><MergeFile>broken.menu</MergeFile>\
             <MergeFile>missing.menu</MergeFile><Include><All/></Include></Menu>",
        );
        let tree = fixture.load("menu.menu");
        assert!(tree.is_degraded());
        assert_eq!(lines(tree.root()), [": a.desktop"]);
        // A missing merge file is no damage.
        fixture.write(
            "menu2.menu",
            "<Menu><Name>R</Name><MergeFile>missing.menu</MergeFile></Menu>",
        );
        assert!(!fixture.load("menu2.menu").is_degraded());
    }

    #[test]
    fn foreign_namespaces_are_ignored_and_the_menu_namespace_is_read() {
        let fixture = Fixture::new();
        fixture.app("a.desktop", "Categories=X-A;\n");
        fixture.write(
            "menu.menu",
            "<Menu xmlns=\"http://www.freedesktop.org/standards/menu\" xmlns:x=\"urn:other\">\
             <Name>R</Name><DefaultAppDirs/>\
             <x:Include><All/></x:Include>\
             <Include xmlns=\"urn:elsewhere\"><All/></Include></Menu>",
        );
        let tree = fixture.load("menu.menu");
        assert_eq!(lines(tree.root()), Vec::<String>::new());
    }

    #[test]
    fn cycles_are_cut_but_repeated_merges_are_kept() {
        let fixture = Fixture::new();
        fixture.app("a.desktop", "Categories=X-A;\n");
        fixture.write(
            "shared.menu",
            "<Menu><Name>s</Name><Include><Category>X-A</Category></Include><MergeFile>menu.menu</MergeFile></Menu>",
        );
        fixture.write(
            "menu.menu",
            "<Menu><Name>R</Name><DefaultAppDirs/>\
             <Menu><Name>One</Name><MergeFile>shared.menu</MergeFile></Menu>\
             <Menu><Name>Two</Name><MergeFile>shared.menu</MergeFile></Menu></Menu>",
        );
        // An alias of the same file is the same file.
        std::os::unix::fs::symlink(fixture.path("menu.menu"), fixture.path("alias.menu")).unwrap();
        fixture.write(
            "loop.menu",
            "<Menu><Name>L</Name><MergeFile>alias.menu</MergeFile></Menu>",
        );
        let tree = fixture.load("menu.menu");
        assert_eq!(
            lines(tree.root()),
            [": >One", "One: a.desktop", ": >Two", "Two: a.desktop"]
        );
        let cycles = tree
            .diagnostics()
            .iter()
            .filter(|d| d.message.contains("cycle"))
            .count();
        assert_eq!(
            cycles,
            2,
            "each copy's merge back to the root: {:?}",
            tree.diagnostics()
        );
    }
}

mod limits {
    use std::fmt::Write as _;

    use super::*;

    fn load_with(fixture: &Fixture, menu: &str, limits: Limits) -> Result<MenuTree, MenuError> {
        let mut context = fixture.context();
        context.limits = limits;
        MenuTree::load_file(&context, fixture.path(menu))
    }

    #[test]
    fn every_configuration_limit_fails_the_load() {
        let fixture = Fixture::new();
        for at in 0..5 {
            fixture.app(&format!("a{at}.desktop"), "");
        }
        fixture.write(
            "part.menu",
            "<Menu><Name>p</Name><Menu><Name>P</Name></Menu></Menu>",
        );
        fixture.write(
            "menu.menu",
            "<Menu><Name>R</Name><DefaultAppDirs/><MergeFile>part.menu</MergeFile>\
             <Menu><Name>A</Name><Menu><Name>B</Name><Include><All/></Include></Menu></Menu></Menu>",
        );
        assert!(load_with(&fixture, "menu.menu", Limits::default()).is_ok());
        let cases: Vec<(&str, Limits)> = vec![
            (
                "file length",
                Limits {
                    max_file_len: Some(16),
                    ..Limits::default()
                },
            ),
            (
                "files",
                Limits {
                    max_files: 1,
                    ..Limits::default()
                },
            ),
            (
                "merge depth",
                Limits {
                    max_merge_depth: 0,
                    ..Limits::default()
                },
            ),
            (
                "depth",
                Limits {
                    max_depth: 2,
                    ..Limits::default()
                },
            ),
            (
                "elements",
                Limits {
                    max_elements: 5,
                    ..Limits::default()
                },
            ),
            (
                "menus",
                Limits {
                    max_menus: 2,
                    ..Limits::default()
                },
            ),
            (
                "listed",
                Limits {
                    max_listed: 2,
                    ..Limits::default()
                },
            ),
            (
                "matches",
                Limits {
                    max_matches: 3,
                    ..Limits::default()
                },
            ),
        ];
        for (what, limits) in cases {
            let err = load_with(&fixture, "menu.menu", limits).unwrap_err();
            assert!(matches!(err, MenuError::Limit { .. }), "{what}: {err}");
        }
    }

    #[test]
    fn copying_pools_spends_the_matching_budget() {
        let fixture = Fixture::new();
        for at in 0..10 {
            fixture.app(&format!("a{at}.desktop"), "");
        }
        // No rule at all: only each menu's own pool is work.
        let mut menu = String::from("<Menu><Name>R</Name>");
        for at in 0..10 {
            write!(menu, "<Menu><Name>M{at}</Name><DefaultAppDirs/></Menu>").unwrap();
        }
        menu.push_str("</Menu>");
        fixture.write("menu.menu", &menu);
        // 10 menus copy the 10 entries: 100 steps, and laying the 11 menus
        // out a few more.
        let enough = Limits {
            max_matches: 200,
            ..Limits::default()
        };
        assert!(load_with(&fixture, "menu.menu", enough).is_ok());
        let short = Limits {
            max_matches: 99,
            ..Limits::default()
        };
        let err = load_with(&fixture, "menu.menu", short).unwrap_err();
        assert!(matches!(err, MenuError::Limit { .. }), "{err}");
    }

    #[test]
    fn loading_is_linear_in_menus_and_entries() {
        crate::work::assert_linear(
            100,
            |n| {
                let fixture = Fixture::new();
                let mut menu = String::from("<Menu><Name>R</Name><DefaultAppDirs/>");
                for at in 0..n {
                    fixture.app(
                        &format!("app{at:04}.desktop"),
                        &format!("Name=App {at:04}\nCategories=X-{at};\n"),
                    );
                    write!(
                        menu,
                        "<Menu><Name>M{at:04}</Name><Include><Category>X-{at}</Category></Include></Menu>"
                    )
                    .unwrap();
                }
                menu.push_str("</Menu>");
                let bytes = menu.len();
                fixture.write("menu.menu", &menu);
                let context = fixture.context();
                let (tree, steps) = crate::work::steps(|| {
                    MenuTree::load_file(&context, fixture.path("menu.menu")).unwrap()
                });
                assert_eq!(tree.root().items().len(), n);
                (steps, bytes)
            },
            // Per menu its pass over the pool is the quadratic part matching
            // has by nature (menus × entries); bounded generously here, the
            // test is that nothing *else* grows faster.
            |n, bytes| 2 * (80 * n * n + 400 * n + 40 * bytes),
        );
    }
}

mod refresh {
    use super::*;

    type Step = Box<dyn Fn(&Fixture)>;

    #[test]
    fn refresh_notices_what_appears_changes_or_goes() {
        let fixture = Fixture::new();
        fixture.app("a.desktop", "Name=A\n");
        fixture.write(
            "config/menus/applications.menu",
            "<Menu><Name>R</Name><DefaultAppDirs/><DefaultDirectoryDirs/><DefaultMergeDirs/>\
             <MergeFile>optional.menu</MergeFile>\
             <Menu><Name>M</Name><Directory>m.directory</Directory><Include><All/></Include></Menu></Menu>",
        );
        let first = MenuTree::load(&fixture.context()).unwrap();
        assert!(first.refresh().is_none(), "nothing changed");
        let steps: Vec<Step> = vec![
            // A missing merge file appears.
            Box::new(|f: &Fixture| {
                f.write("config/menus/optional.menu", "<Menu><Name>o</Name></Menu>");
            }),
            // A merge directory gains a file.
            Box::new(|f: &Fixture| {
                f.write(
                    "config/menus/applications-merged/x.menu",
                    "<Menu><Name>x</Name></Menu>",
                );
            }),
            // A missing .directory appears.
            Box::new(|f: &Fixture| {
                f.write(
                    "data/desktop-directories/m.directory",
                    "[Desktop Entry]\nType=Directory\nName=Em\n",
                );
            }),
            // A desktop entry changes.
            Box::new(|f: &Fixture| {
                f.app("a.desktop", "Name=Changed A\n");
            }),
            // A new entry in a user directory that did not exist.
            Box::new(|f: &Fixture| {
                f.write(
                    "data-home/applications/b.desktop",
                    "[Desktop Entry]\nType=Application\nExec=true\n",
                );
            }),
            // A user menu appears, replacing the system one.
            Box::new(|f: &Fixture| {
                f.write(
                    "config-home/menus/applications.menu",
                    "<Menu><Name>U</Name></Menu>",
                );
            }),
        ];
        let mut current = first;
        for (at, step) in steps.iter().enumerate() {
            step(&fixture);
            current = current
                .refresh()
                .unwrap_or_else(|| panic!("step {at} unnoticed"))
                .unwrap();
            assert!(current.refresh().is_none(), "step {at} settles");
        }
        assert_eq!(current.root().name(), "U");
    }
}

#[test]
fn a_simple_menu_resolves() {
    let fixture = Fixture::new();
    fixture.app("a.desktop", "Name=A\n");
    let path = fixture.write(
        "menu.menu",
        &format!("{DOCTYPE}<Menu><Name>R</Name><DefaultAppDirs/><Include><All/></Include></Menu>"),
    );
    let tree = MenuTree::load_file(&fixture.context(), &path).unwrap();
    assert_eq!(lines(tree.root()), [": a.desktop"]);
    assert_eq!(tree.files(), [path]);
    let _: &Path = tree.files()[0].as_path();
}

mod review {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn load_with(fixture: &Fixture, limits: Limits) -> Result<MenuTree, MenuError> {
        let mut context = fixture.context();
        context.limits = limits;
        MenuTree::load_file(&context, fixture.path("menu.menu"))
    }

    #[test]
    fn moves_stay_within_the_depth_and_menu_limits() {
        let fixture = Fixture::new();
        fixture.app("a.desktop", "");
        let path: Vec<String> = (0..80).map(|at| format!("P{at}")).collect();
        fixture.write(
            "menu.menu",
            &format!(
                "<Menu><Name>R</Name><DefaultAppDirs/><Menu><Name>A</Name><Include><All/></Include></Menu>\
                 <Move><Old>A</Old><New>{}</New></Move></Menu>",
                path.join("/")
            ),
        );
        let err = load_with(&fixture, Limits::default()).unwrap_err();
        assert!(matches!(err, MenuError::Limit { .. }), "{err}");
        // The moved menu's own depth counts too.
        fixture.write(
            "menu.menu",
            "<Menu><Name>R</Name><Menu><Name>A</Name><Menu><Name>B</Name><Menu><Name>C</Name>\
             </Menu></Menu></Menu><Move><Old>A</Old><New>X/Y/A</New></Move></Menu>",
        );
        let shallow = Limits {
            max_depth: 4,
            ..Limits::default()
        };
        assert!(matches!(
            load_with(&fixture, shallow),
            Err(MenuError::Limit { .. })
        ));
        let enough = Limits {
            max_depth: 5,
            ..Limits::default()
        };
        assert!(load_with(&fixture, enough).is_ok());
        // Menus made by a move count.
        fixture.write(
            "menu.menu",
            "<Menu><Name>R</Name><Menu><Name>A</Name></Menu><Move><Old>A</Old><New>X/Y/Z/A</New></Move></Menu>",
        );
        let few = Limits {
            max_menus: 3,
            ..Limits::default()
        };
        assert!(matches!(
            load_with(&fixture, few),
            Err(MenuError::Limit { .. })
        ));
        let enough = Limits {
            max_menus: 4,
            ..Limits::default()
        };
        assert!(load_with(&fixture, enough).is_ok());
    }

    #[test]
    fn a_file_over_the_size_limit_fails_the_load() {
        let small = || Limits {
            max_file_len: Some(512),
            ..Limits::default()
        };
        let padding = format!("# {}\n", "x".repeat(600));
        // A more important entry hiding a less important one.
        let fixture = Fixture::new();
        fixture.app("a.desktop", "Name=Exposed\n");
        fixture.write(
            "data-home/applications/a.desktop",
            &format!("[Desktop Entry]\nType=Application\nHidden=true\n{padding}"),
        );
        fixture.write(
            "menu.menu",
            "<Menu><Name>R</Name><DefaultAppDirs/><Include><All/></Include></Menu>",
        );
        assert!(matches!(
            load_with(&fixture, small()),
            Err(MenuError::Limit { .. })
        ));
        // A .directory hiding its menu.
        let fixture = Fixture::new();
        fixture.app("a.desktop", "");
        fixture.write(
            "data/desktop-directories/m.directory",
            &format!("[Desktop Entry]\nType=Directory\nNoDisplay=true\n{padding}"),
        );
        fixture.write(
            "menu.menu",
            "<Menu><Name>R</Name><DefaultAppDirs/><DefaultDirectoryDirs/><Menu><Name>M</Name>\
             <Directory>m.directory</Directory><Include><All/></Include></Menu></Menu>",
        );
        assert!(matches!(
            load_with(&fixture, small()),
            Err(MenuError::Limit { .. })
        ));
        let shown = load_with(&fixture, Limits::default()).unwrap();
        assert_eq!(
            lines(shown.root()),
            Vec::<String>::new(),
            "NoDisplay hides it"
        );
        // A legacy entry.
        let fixture = Fixture::new();
        fixture.write(
            "legacy/l.desktop",
            &format!("[Desktop Entry]\nType=Application\nExec=true\nHidden=true\n{padding}"),
        );
        fixture.write(
            "menu.menu",
            "<Menu><Name>R</Name><LegacyDir>legacy</LegacyDir></Menu>",
        );
        assert!(matches!(
            load_with(&fixture, small()),
            Err(MenuError::Limit { .. })
        ));
    }

    #[test]
    fn malformed_leaf_rules_are_unknown() {
        let fixture = Fixture::new();
        fixture.app("a.desktop", "Categories=X-A;\n");
        for rule in [
            "<Not><Filename><Unknown/></Filename></Not>",
            "<Not><Category>X-A<Unknown/></Category></Not>",
            "<Not><Filename>  </Filename></Not>",
            "<All>broken</All>",
            "<All><Unknown/></All>",
        ] {
            fixture.write(
                "menu.menu",
                &format!("<Menu><Name>R</Name><DefaultAppDirs/><Include>{rule}</Include></Menu>"),
            );
            let tree = fixture.load("menu.menu");
            assert_eq!(lines(tree.root()), Vec::<String>::new(), "{rule}");
            assert!(
                tree.diagnostics()
                    .iter()
                    .any(|d| d.message.contains("malformed rule")),
                "{rule}: {:?}",
                tree.diagnostics()
            );
        }
    }

    #[test]
    fn candidates_passed_over_spend_the_budget() {
        let fixture = Fixture::new();
        for at in 0..50 {
            fixture.app(&format!("a{at:02}.desktop"), "");
        }
        let rules = "<Include><Filename>none.desktop</Filename></Include>".repeat(200);
        fixture.write(
            "menu.menu",
            &format!(
                "<Menu><Name>R</Name><DefaultAppDirs/><Menu><Name>A</Name><Include><All/></Include></Menu>\
                 <Menu><Name>B</Name><OnlyUnallocated/>{rules}</Menu></Menu>"
            ),
        );
        // Every entry is allocated by A: B evaluates no rule, but visits 50
        // candidates for each of its 200 includes.
        let short = Limits {
            max_matches: 5_000,
            ..Limits::default()
        };
        assert!(matches!(
            load_with(&fixture, short),
            Err(MenuError::Limit { .. })
        ));
        assert!(load_with(&fixture, Limits::default()).is_ok());
    }

    #[test]
    fn refresh_notices_a_try_exec_program() {
        let fixture = Fixture::new();
        let bin = fixture.path("bin");
        fs::create_dir_all(&bin).unwrap();
        fixture.app("a.desktop", "TryExec=xkt-tool\n");
        fixture.write(
            "menu.menu",
            "<Menu><Name>R</Name><DefaultAppDirs/><Include><All/></Include></Menu>",
        );
        let mut context = fixture.context();
        context.try_exec = Some(bin.clone().into_os_string());
        let tree = MenuTree::load_file(&context, fixture.path("menu.menu")).unwrap();
        assert_eq!(lines(tree.root()), Vec::<String>::new());
        assert!(tree.refresh().is_none());
        let tool = bin.join("xkt-tool");
        fs::write(&tool, "#!/bin/sh\n").unwrap();
        let mode = |mode| fs::set_permissions(&tool, fs::Permissions::from_mode(mode)).unwrap();
        mode(0o755);
        let tree = tree.refresh().expect("the program appeared").unwrap();
        assert_eq!(lines(tree.root()), [": a.desktop"]);
        mode(0o644);
        let tree = tree
            .refresh()
            .expect("the program can no longer run")
            .unwrap();
        assert_eq!(lines(tree.root()), Vec::<String>::new());
        mode(0o755);
        let tree = tree.refresh().expect("it can again").unwrap();
        fs::remove_file(&tool).unwrap();
        let tree = tree.refresh().expect("the program went").unwrap();
        assert_eq!(lines(tree.root()), Vec::<String>::new());
        assert!(tree.refresh().is_none());
    }

    #[test]
    fn an_empty_namespace_declaration_is_no_namespace() {
        let fixture = Fixture::new();
        fixture.app("a.desktop", "");
        fixture.write(
            "menu.menu",
            "<Menu xmlns=\"\"><Name>R</Name><DefaultAppDirs/>\
             <Include xmlns=\"urn:other\"><Filename xmlns=\"\">a.desktop</Filename></Include>\
             <Include><All/></Include></Menu>",
        );
        let tree = fixture.load("menu.menu");
        assert_eq!(lines(tree.root()), [": a.desktop"]);
    }

    #[test]
    fn directory_files_need_their_suffix() {
        let fixture = Fixture::new();
        fixture.app("a.desktop", "");
        fixture.write(
            "data/desktop-directories/wrong.txt",
            "[Desktop Entry]\nType=Directory\nName=Wrong\n",
        );
        fixture.write(
            "menu.menu",
            "<Menu><Name>R</Name><DefaultAppDirs/><DefaultDirectoryDirs/><Menu><Name>M</Name>\
             <Directory>wrong.txt</Directory><Include><All/></Include></Menu></Menu>",
        );
        let tree = fixture.load("menu.menu");
        assert_eq!(tree.root().submenu("M").unwrap().label(), "M");
        assert!(
            tree.diagnostics()
                .iter()
                .any(|d| d.message.contains("does not name a .directory"))
        );
    }
}

mod review_budgets {
    use std::fmt::Write as _;

    use super::*;

    /// `count` empty menus, named `M0` on.
    fn empty_menus(count: usize) -> String {
        let mut menus = String::new();
        for at in 0..count {
            write!(menus, "<Menu><Name>M{at}</Name></Menu>").unwrap();
        }
        menus
    }

    fn load_with(fixture: &Fixture, limits: Limits) -> Result<MenuTree, MenuError> {
        let mut context = fixture.context();
        context.limits = limits;
        MenuTree::load_file(&context, fixture.path("menu.menu"))
    }

    #[test]
    fn an_inherited_layout_spends_the_budget_in_every_menu() {
        let fixture = Fixture::new();
        let names = "<Filename>x.desktop</Filename>".repeat(2_000);
        let children = empty_menus(50);
        fixture.write(
            "menu.menu",
            &format!(
                "<Menu><Name>R</Name><DefaultLayout show_empty=\"true\">{names}<Merge type=\"all\"/>\
                 </DefaultLayout>{children}</Menu>"
            ),
        );
        // 51 menus lay out the 2,001 nodes each: no rule is evaluated.
        let short = Limits {
            max_matches: 50_000,
            ..Limits::default()
        };
        assert!(matches!(
            load_with(&fixture, short),
            Err(MenuError::Limit { .. })
        ));
        assert_eq!(
            load_with(&fixture, Limits::default())
                .unwrap()
                .root()
                .items()
                .len(),
            50
        );
    }

    #[test]
    fn move_searches_spend_the_budget_found_or_not() {
        let fixture = Fixture::new();
        let children = empty_menus(100);
        let moves = "<Move><Old>none</Old><New>x</New></Move>".repeat(100);
        fixture.write(
            "menu.menu",
            &format!("<Menu><Name>R</Name>{children}{moves}</Menu>"),
        );
        // Each search passes the 100 menus and finds nothing.
        let short = Limits {
            max_matches: 5_000,
            ..Limits::default()
        };
        assert!(matches!(
            load_with(&fixture, short),
            Err(MenuError::Limit { .. })
        ));
        assert!(load_with(&fixture, Limits::default()).is_ok());
    }

    #[test]
    fn declarations_must_be_where_xml_puts_them() {
        let fixture = Fixture::new();
        let menu = "<Menu><Name>R</Name></Menu>";
        let doctype = "<!DOCTYPE Menu PUBLIC \"-//freedesktop//DTD Menu 1.0//EN\" \"menu.dtd\">";
        for (text, fine) in [
            (format!("<?xml version=\"1.0\"?>\n{doctype}\n{menu}"), true),
            (format!("\u{FEFF}<?xml version=\"1.0\"?>{menu}"), true),
            (format!("<!-- c --><?pi x?>{doctype}{menu}"), true),
            (
                "<Menu><Name>R</Name><?xml version=\"1.0\"?></Menu>".to_owned(),
                false,
            ),
            (format!("{menu}<?xml version=\"1.0\"?>"), false),
            (format!(" <?xml version=\"1.0\"?>{menu}"), false),
            (format!("<!-- c --><?xml version=\"1.0\"?>{menu}"), false),
            (
                format!("<?xml version=\"1.0\"?><?xml version=\"1.0\"?>{menu}"),
                false,
            ),
            (format!("{doctype}{doctype}{menu}"), false),
            (format!("{doctype}<?xml version=\"1.0\"?>{menu}"), false),
        ] {
            fixture.write("menu.menu", &text);
            let loaded = load_with(&fixture, Limits::default());
            if fine {
                assert!(loaded.is_ok(), "{text:?}: {:?}", loaded.err());
            } else {
                assert!(
                    matches!(loaded, Err(MenuError::Malformed { .. })),
                    "{text:?}: {:?}",
                    loaded.map(|_| ())
                );
            }
        }
    }
}

mod review_strict {
    use std::fmt::Write as _;

    use super::*;

    fn load_with(fixture: &Fixture, limits: Limits) -> Result<MenuTree, MenuError> {
        let mut context = fixture.context();
        context.limits = limits;
        MenuTree::load_file(&context, fixture.path("menu.menu"))
    }

    #[test]
    fn repeated_merges_spend_the_budget_for_what_they_pass_over() {
        let fixture = Fixture::new();
        let mut menu = String::from("<Menu><Name>R</Name>");
        for at in 0..200 {
            write!(menu, "<Menu><Name>M{at}</Name></Menu>").unwrap();
        }
        // Merges before the names: each passes over the 200 named menus.
        menu.push_str("<Layout>");
        menu.push_str(&"<Merge type=\"menus\"/>".repeat(200));
        for at in 0..200 {
            write!(menu, "<Menuname show_empty=\"true\">M{at}</Menuname>").unwrap();
        }
        menu.push_str("</Layout></Menu>");
        fixture.write("menu.menu", &menu);
        let short = Limits {
            max_matches: 20_000,
            ..Limits::default()
        };
        assert!(matches!(
            load_with(&fixture, short),
            Err(MenuError::Limit { .. })
        ));
        assert_eq!(
            load_with(&fixture, Limits::default())
                .unwrap()
                .root()
                .items()
                .len(),
            200
        );
    }

    #[test]
    fn depth_checks_spend_the_budget_for_every_item() {
        let fixture = Fixture::new();
        let items = "<NotDeleted/>".repeat(1_000);
        let moves =
            "<Move><Old>A</Old><New>B</New></Move><Move><Old>B</Old><New>A</New></Move>".repeat(10);
        fixture.write(
            "menu.menu",
            &format!("<Menu><Name>R</Name><Menu><Name>A</Name>{items}</Menu>{moves}</Menu>"),
        );
        // 20 moves look at the moved menu's 1,000 items each.
        let short = Limits {
            max_matches: 10_000,
            ..Limits::default()
        };
        assert!(matches!(
            load_with(&fixture, short),
            Err(MenuError::Limit { .. })
        ));
        assert!(load_with(&fixture, Limits::default()).is_ok());
    }

    #[test]
    fn xml_grammar_is_checked() {
        let fixture = Fixture::new();
        let body = "<Name>R</Name>";
        let wrap = |inner: &str| format!("<Menu>{body}{inner}</Menu>");
        let fine = [
            format!("<?xml version=\"1.0\"?>{}", wrap("")),
            format!(
                "<?xml version='1.1' encoding='utf-8' standalone='yes' ?>{}",
                wrap("")
            ),
            format!("<?xml version = \"1.0\" encoding=\"UTF-8\"?>{}", wrap("")),
            wrap("<Layout><Merge type='all'/><Merge\ttype = \"menus\" /></Layout>"),
            wrap("<Directory>a&amp;b&#65;&#x42;.directory</Directory>"),
            format!("<Menu a=\"&amp;&lt;&#60;&#x3C;\">{body}</Menu>"),
            format!("<?pi-target data?><?other?>{}", wrap("")),
            wrap("<Directory>]]&gt;.directory</Directory>"),
        ];
        let malformed = [
            format!("<?xml?>{}", wrap("")),
            format!("<?xml version=\"garbage\"?>{}", wrap("")),
            format!("<?xml version=\"1.\"?>{}", wrap("")),
            format!("<?xml encoding=\"UTF-8\" version=\"1.0\"?>{}", wrap("")),
            format!("<?xml version=\"1.0\" standalone=\"maybe\"?>{}", wrap("")),
            format!(
                "<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?>{}",
                wrap("")
            ),
            format!("<?xml version=\"1.0\" junk=\"1\"?>{}", wrap("")),
            wrap("<123/>"),
            wrap("<-a/>"),
            format!("<Menu a=\"<\">{body}</Menu>"),
            format!("<Menu a=\"&\">{body}</Menu>"),
            format!("<Menu a=\"&#xZZ;\">{body}</Menu>"),
            format!("<Menu a=\"1\"b=\"2\">{body}</Menu>"),
            format!("<Menu a=1>{body}</Menu>"),
            format!("<Menu 1a=\"1\">{body}</Menu>"),
            format!("<?XmL x?>{}", wrap("")),
            format!("<?1pi?>{}", wrap("")),
            wrap("<Directory>]]>.directory</Directory>"),
        ];
        for text in &fine {
            fixture.write("menu.menu", text);
            let loaded = load_with(&fixture, Limits::default());
            assert!(loaded.is_ok(), "{text}: {:?}", loaded.err());
        }
        for text in &malformed {
            fixture.write("menu.menu", text);
            let loaded = load_with(&fixture, Limits::default());
            assert!(
                matches!(loaded, Err(MenuError::Malformed { .. })),
                "{text}: {:?}",
                loaded.map(|_| ())
            );
        }
    }
}

mod review_prolog {
    use super::*;

    #[test]
    fn the_prolog_and_what_is_ignored_are_checked() {
        let fixture = Fixture::new();
        let menu = "<Menu><Name>R</Name></Menu>";
        let fine = [
            format!("{DOCTYPE}{menu}"),
            format!("<!DOCTYPE Menu SYSTEM 'menu.dtd'>{menu}"),
            // After a byte order mark, and after the declaration that may
            // follow one.
            format!("\u{FEFF}<!DOCTYPE Menu>{menu}"),
            format!("\u{FEFF}{menu}"),
            format!("\u{FEFF}<?xml version=\"1.0\"?><!DOCTYPE Menu>{menu}"),
            format!("<!DOCTYPE Menu>{menu}"),
            format!("<!DOCTYPE Menu [ <!-- c --> <?pi x?> ]>{menu}"),
            format!("<!DOCTYPE Menu PUBLIC \"-//freedesktop//DTD Menu 1.0//EN\" 'x'  [ ] >{menu}"),
            format!(" \t\r\n<!-- é -->\n{menu}\n<?pi ü?>\n"),
        ];
        let malformed = [
            format!("<!DOCTYPE nonsense junk>{menu}"),
            format!("<!doctype Menu>{menu}"),
            format!("\u{FEFF}\u{FEFF}<!DOCTYPE Menu>{menu}"),
            format!("\u{FEFF}\u{FEFF}{menu}"),
            format!("\u{FEFF}\u{FEFF}<?xml version=\"1.0\"?>{menu}"),
            format!("\u{FEFF}{menu}\u{FEFF}"),
            format!("<!DOCTYPE>{menu}"),
            format!("<!DOCTYPE Menu SYSTEM>{menu}"),
            format!("<!DOCTYPE Menu PUBLIC \"bad{{id}}\" \"x\">{menu}"),
            format!("<!DOCTYPE Menu PUBLIC \"id\">{menu}"),
            format!("<!DOCTYPE Menu [<!ATTLIST Menu xmlns CDATA \"urn:x\">]>{menu}"),
            format!("<!DOCTYPE Menu [<!ENTITY x \"y\">]>{menu}"),
            format!("<!DOCTYPE Menu [<!-- a -- b -->]>{menu}"),
            format!("<!-- \u{1} -->{menu}"),
            format!("<?pi \u{1}?>{menu}"),
            format!("{menu}<!-- \u{1} -->"),
            format!("\u{A0}{menu}"),
            format!("&#32;{menu}"),
            format!("{menu}&#32;"),
            format!("{menu}\u{2003}"),
        ];
        for text in &fine {
            fixture.write("menu.menu", text);
            let loaded = MenuTree::load_file(&fixture.context(), fixture.path("menu.menu"));
            assert!(loaded.is_ok(), "{text:?}: {:?}", loaded.err());
        }
        for text in &malformed {
            fixture.write("menu.menu", text);
            let loaded = MenuTree::load_file(&fixture.context(), fixture.path("menu.menu"));
            assert!(
                matches!(loaded, Err(MenuError::Malformed { .. })),
                "{text:?}: {:?}",
                loaded.map(|_| ())
            );
        }
    }
}
