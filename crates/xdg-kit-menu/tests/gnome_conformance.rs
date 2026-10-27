//! Whole resolved trees against GNOME's libgnome-menu (gnome-menus from
//! the locked nixpkgs, through `tests/gmenu-dump.py` and Python's `GObject`
//! introspection; `XDG_KIT_GI_TYPELIB_PATH` from the dev shell).
//!
//! Each scenario builds a menu configuration in a temporary directory —
//! nothing real is read — and both sides print the tree as lines of
//! `menu`, `entry`, `separator`, `header` and `alias`, in display order.
//! They must agree, except for the scenarios in `DIFFERENCES`: there the
//! two must still differ, this crate must give exactly the lines listed,
//! and the reason says why the spec asks for them.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::Command;

use xdg_kit_menu::{Aliased, Menu, MenuContext, MenuItem, MenuTree};

struct Scenario {
    name: &'static str,
    /// (path below the root, contents); `@ROOT@` is the root's path.
    files: &'static [(&'static str, &'static str)],
    /// The menu file, below the root.
    menu: &'static str,
    /// Variables beyond the common ones.
    vars: &'static [(&'static str, &'static str)],
}

const APP: &str = "[Desktop Entry]\nType=Application\nExec=true\n";

macro_rules! app {
    ($extra:literal) => {
        concat!("[Desktop Entry]\nType=Application\nExec=true\n", $extra)
    };
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "categories-directories-localized",
        files: &[
            (
                "data/applications/editor.desktop",
                app!("Name=Editor\nName[de]=Bearbeiter\nCategories=Utility;TextEditor;\n"),
            ),
            (
                "data/applications/viewer.desktop",
                app!("Name=Viewer\nCategories=Graphics;Viewer;\n"),
            ),
            (
                "data/applications/vendor/tool.desktop",
                app!("Name=Tool\nCategories=Utility;\n"),
            ),
            (
                "data/desktop-directories/utility.directory",
                "[Desktop Entry]\nType=Directory\nName=Accessories\nName[de]=Zubehör\nIcon=util\n",
            ),
            (
                "menu.menu",
                "<Menu><Name>Applications</Name><DefaultAppDirs/><DefaultDirectoryDirs/>\
                 <Menu><Name>Utility</Name><Directory>utility.directory</Directory>\
                 <Include><Category>Utility</Category></Include></Menu>\
                 <Menu><Name>Graphics</Name><Directory>missing.directory</Directory>\
                 <Include><Category>Graphics</Category></Include></Menu></Menu>",
            ),
        ],
        menu: "menu.menu",
        vars: &[("LANG", "de_DE.UTF-8")],
    },
    Scenario {
        name: "layout-order-and-separators",
        files: &[
            (
                "data/applications/a.desktop",
                app!("Name=Alpha\nCategories=X-A;\n"),
            ),
            (
                "data/applications/b.desktop",
                app!("Name=Beta\nCategories=X-A;\n"),
            ),
            (
                "data/applications/c.desktop",
                app!("Name=Gamma\nCategories=X-A;\n"),
            ),
            (
                "data/applications/d.desktop",
                app!("Name=Delta\nCategories=X-B;\n"),
            ),
            (
                "menu.menu",
                "<Menu><Name>Root</Name><DefaultAppDirs/>\
                 <Include><Category>X-A</Category></Include>\
                 <Menu><Name>Sub</Name><Include><Category>X-B</Category></Include></Menu>\
                 <Menu><Name>Other</Name><Include><Category>X-B</Category></Include></Menu>\
                 <Layout><Separator/><Filename>c.desktop</Filename><Separator/><Separator/>\
                 <Menuname>Sub</Menuname><Merge type=\"files\"/><Separator/>\
                 <Merge type=\"menus\"/><Separator/><Filename>gone.desktop</Filename></Layout></Menu>",
            ),
        ],
        menu: "menu.menu",
        vars: &[],
    },
    Scenario {
        name: "merge-all-mixes",
        files: &[
            (
                "data/applications/b.desktop",
                app!("Name=Bravo\nCategories=X-A;\n"),
            ),
            (
                "data/applications/d.desktop",
                app!("Name=Delta\nCategories=X-A;\n"),
            ),
            (
                "data/applications/x.desktop",
                app!("Name=Xray\nCategories=X-B;\n"),
            ),
            (
                "data/applications/y.desktop",
                app!("Name=Yankee\nCategories=X-C;\n"),
            ),
            (
                "menu.menu",
                "<Menu><Name>Root</Name><DefaultAppDirs/>\
                 <Include><Category>X-A</Category></Include>\
                 <Menu><Name>Charlie</Name><Include><Category>X-B</Category></Include></Menu>\
                 <Menu><Name>Echo</Name><Include><Category>X-C</Category></Include></Menu>\
                 <Layout><Merge type=\"all\"/></Layout></Menu>",
            ),
        ],
        menu: "menu.menu",
        vars: &[],
    },
    Scenario {
        name: "inline-header-alias-limit",
        files: &[
            (
                "data/applications/a.desktop",
                app!("Name=Alpha\nCategories=X-One;\n"),
            ),
            (
                "data/applications/b.desktop",
                app!("Name=Beta\nCategories=X-Two;\n"),
            ),
            (
                "data/applications/c.desktop",
                app!("Name=Gamma\nCategories=X-Two;\n"),
            ),
            (
                "data/applications/d.desktop",
                app!("Name=Delta\nCategories=X-Many;\n"),
            ),
            (
                "data/applications/e.desktop",
                app!("Name=Epsilon\nCategories=X-Many;\n"),
            ),
            (
                "data/applications/f.desktop",
                app!("Name=Phi\nCategories=X-Many;\n"),
            ),
            ("data/applications/top.desktop", app!("Name=Top\n")),
            (
                "menu.menu",
                "<Menu><Name>Root</Name><DefaultAppDirs/>\
                 <Include><Filename>top.desktop</Filename></Include>\
                 <Menu><Name>One</Name><Include><Category>X-One</Category></Include></Menu>\
                 <Menu><Name>Two</Name><Include><Category>X-Two</Category></Include></Menu>\
                 <Menu><Name>Many</Name><Include><Category>X-Many</Category></Include></Menu>\
                 <Menu><Name>Empty</Name></Menu>\
                 <Layout><Menuname inline=\"true\" inline_alias=\"true\">One</Menuname>\
                 <Menuname inline=\"true\" inline_header=\"true\">Two</Menuname>\
                 <Menuname inline=\"true\" inline_limit=\"2\">Many</Menuname>\
                 <Menuname show_empty=\"true\">Empty</Menuname><Merge type=\"files\"/></Layout></Menu>",
            ),
        ],
        menu: "menu.menu",
        vars: &[],
    },
    Scenario {
        name: "inline-alias-not-first",
        files: &[
            (
                "data/applications/a.desktop",
                app!("Name=Alpha\nCategories=X-One;\n"),
            ),
            (
                "data/applications/b.desktop",
                app!("Name=Beta\nCategories=X-Two;\n"),
            ),
            (
                "data/applications/c.desktop",
                app!("Name=Gamma\nCategories=X-Two;\n"),
            ),
            (
                "menu.menu",
                "<Menu><Name>Root</Name><DefaultAppDirs/>\
                 <Menu><Name>First</Name><Include><Category>X-Two</Category></Include></Menu>\
                 <Menu><Name>One</Name><Include><Category>X-One</Category></Include></Menu>\
                 <Layout><Merge type=\"menus\"/>\
                 <Menuname inline=\"true\" inline_alias=\"true\">One</Menuname>\
                 <Merge type=\"files\"/></Layout></Menu>",
            ),
        ],
        menu: "menu.menu",
        vars: &[],
    },
    Scenario {
        name: "exec-program-missing",
        files: &[
            ("data/applications/real.desktop", app!("Name=Real\n")),
            (
                "data/applications/ghost.desktop",
                "[Desktop Entry]\nType=Application\nName=Ghost\nExec=no-such-program-xkt\n",
            ),
            (
                "menu.menu",
                "<Menu><Name>Root</Name><DefaultAppDirs/><Include><All/></Include></Menu>",
            ),
        ],
        menu: "menu.menu",
        vars: &[],
    },
    Scenario {
        name: "directory-fallback-and-priority",
        files: &[
            (
                "data/applications/a.desktop",
                app!("Name=Alpha\nCategories=X-A;\n"),
            ),
            (
                "data/desktop-directories/first.directory",
                "[Desktop Entry]\nType=Directory\nName=First choice\n",
            ),
            (
                "data/desktop-directories/shared.directory",
                "[Desktop Entry]\nType=Directory\nName=From the parent\n",
            ),
            (
                "own/shared.directory",
                "[Desktop Entry]\nType=Directory\nName=From the child\n",
            ),
            (
                "menu.menu",
                "<Menu><Name>Root</Name><DefaultAppDirs/><DefaultDirectoryDirs/>\
                 <Menu><Name>Fallback</Name><Directory>first.directory</Directory>\
                 <Directory>missing.directory</Directory><Include><All/></Include></Menu>\
                 <Menu><Name>Child</Name><DirectoryDir>own</DirectoryDir>\
                 <Directory>shared.directory</Directory><Include><All/></Include></Menu></Menu>",
            ),
        ],
        menu: "menu.menu",
        vars: &[],
    },
    Scenario {
        name: "inline-without-header",
        files: &[
            (
                "data/applications/a.desktop",
                app!("Name=Alpha\nCategories=X-In;\n"),
            ),
            (
                "data/applications/z.desktop",
                app!("Name=Zulu\nCategories=X-In;\n"),
            ),
            ("data/applications/m.desktop", app!("Name=Mike\n")),
            (
                "menu.menu",
                "<Menu><Name>Root</Name><DefaultAppDirs/>\
                 <Include><Filename>m.desktop</Filename></Include>\
                 <Menu><Name>In</Name><Include><Category>X-In</Category></Include></Menu>\
                 <Layout><Menuname inline=\"true\" inline_header=\"false\">In</Menuname>\
                 <Merge type=\"files\"/></Layout></Menu>",
            ),
        ],
        menu: "menu.menu",
        vars: &[],
    },
    Scenario {
        name: "default-layout-inheritance",
        files: &[
            (
                "data/applications/a.desktop",
                app!("Name=Alpha\nCategories=X-Deep;\n"),
            ),
            (
                "data/applications/b.desktop",
                app!("Name=Beta\nCategories=X-Mid;\n"),
            ),
            (
                "menu.menu",
                "<Menu><Name>Root</Name><DefaultAppDirs/>\
                 <DefaultLayout show_empty=\"true\"><Merge type=\"files\"/><Separator/><Merge type=\"menus\"/></DefaultLayout>\
                 <Menu><Name>Mid</Name><Include><Category>X-Mid</Category></Include>\
                 <Menu><Name>Deep</Name><Include><Category>X-Deep</Category></Include></Menu>\
                 <Menu><Name>Hollow</Name></Menu></Menu>\
                 <Menu><Name>Plain</Name><DefaultLayout><Merge type=\"menus\"/><Merge type=\"files\"/></DefaultLayout>\
                 <Menu><Name>Void</Name></Menu></Menu></Menu>",
            ),
        ],
        menu: "menu.menu",
        vars: &[],
    },
    Scenario {
        name: "allocation-and-exclusion",
        files: &[
            (
                "data/applications/a.desktop",
                app!("Name=Alpha\nCategories=Game;\n"),
            ),
            (
                "data/applications/b.desktop",
                app!("Name=Beta\nCategories=Game;CardGame;\n"),
            ),
            (
                "data/applications/c.desktop",
                app!("Name=Gamma\nCategories=Office;\n"),
            ),
            ("data/applications/d.desktop", app!("Name=Delta\n")),
            (
                "menu.menu",
                "<Menu><Name>Root</Name><DefaultAppDirs/>\
                 <Menu><Name>Games</Name><Include><Category>Game</Category></Include>\
                 <Exclude><Category>CardGame</Category></Exclude></Menu>\
                 <Menu><Name>Gone</Name><Include><Category>Office</Category></Include><Deleted/></Menu>\
                 <Menu><Name>Other</Name><OnlyUnallocated/><Include><All/></Include></Menu></Menu>",
            ),
        ],
        menu: "menu.menu",
        vars: &[],
    },
    Scenario {
        name: "visibility",
        files: &[
            ("data/applications/shown.desktop", app!("Name=Shown\n")),
            (
                "data/applications/nodisplay.desktop",
                app!("Name=Quiet\nNoDisplay=true\n"),
            ),
            (
                "data/applications/kde.desktop",
                app!("Name=Kay\nOnlyShowIn=KDE;\n"),
            ),
            (
                "data/applications/notgnome.desktop",
                app!("Name=Not\nNotShowIn=GNOME;\n"),
            ),
            (
                "data/applications/gnome.desktop",
                app!("Name=Gee\nOnlyShowIn=GNOME;\n"),
            ),
            (
                "data/applications/missing.desktop",
                app!("Name=Missing\nTryExec=no-such-program-xkt\n"),
            ),
            (
                "data/applications/x.desktop",
                app!("Name=Xenon\nCategories=X-Hid;\n"),
            ),
            (
                "data/desktop-directories/hid.directory",
                "[Desktop Entry]\nType=Directory\nName=Hid\nNoDisplay=true\n",
            ),
            (
                "data/desktop-directories/kde.directory",
                "[Desktop Entry]\nType=Directory\nName=KDE only\nOnlyShowIn=KDE;\n",
            ),
            (
                "menu.menu",
                "<Menu><Name>Root</Name><DefaultAppDirs/><DefaultDirectoryDirs/>\
                 <Include><All/></Include>\
                 <Menu><Name>Hid</Name><Directory>hid.directory</Directory><Include><All/></Include></Menu>\
                 <Menu><Name>Kde</Name><Directory>kde.directory</Directory><Include><All/></Include></Menu></Menu>",
            ),
        ],
        menu: "menu.menu",
        vars: &[],
    },
    Scenario {
        name: "pool-precedence",
        files: &[
            ("data/applications/dup.desktop", app!("Name=System copy\n")),
            (
                "data-home/applications/dup.desktop",
                app!("Name=User copy\n"),
            ),
            ("data/applications/gone.desktop", app!("Name=Gone\n")),
            (
                "data-home/applications/gone.desktop",
                "[Desktop Entry]\nType=Application\nName=Gone\nExec=true\nHidden=true\n",
            ),
            ("extra/dup.desktop", app!("Name=Extra copy\n")),
            ("extra/sub/deep.desktop", app!("Name=Deep\n")),
            (
                "menu.menu",
                "<Menu><Name>Root</Name><DefaultAppDirs/><Include><All/></Include>\
                 <Menu><Name>Child</Name><AppDir>extra</AppDir><Include><All/></Include></Menu></Menu>",
            ),
        ],
        menu: "menu.menu",
        vars: &[],
    },
    Scenario {
        name: "moves",
        files: &[
            (
                "data/applications/a.desktop",
                app!("Name=Alpha\nCategories=X-A;\n"),
            ),
            (
                "data/applications/b.desktop",
                app!("Name=Beta\nCategories=X-B;\n"),
            ),
            (
                "data/applications/c.desktop",
                app!("Name=Gamma\nCategories=X-C;\n"),
            ),
            (
                "menu.menu",
                "<Menu><Name>Root</Name><DefaultAppDirs/>\
                 <Menu><Name>Old</Name><Include><Category>X-A</Category></Include></Menu>\
                 <Menu><Name>Src</Name><Include><Category>X-B</Category></Include></Menu>\
                 <Menu><Name>Dest</Name><Include><Category>X-C</Category></Include></Menu>\
                 <Move><Old>Old</Old><New>Deep/Renamed</New></Move>\
                 <Move><Old>Src</Old><New>Dest</New></Move>\
                 <Move><Old>Absent</Old><New>Nowhere</New></Move></Menu>",
            ),
        ],
        menu: "menu.menu",
        vars: &[],
    },
    Scenario {
        name: "merges",
        files: &[
            (
                "data/applications/a.desktop",
                app!("Name=Alpha\nCategories=X-A;\n"),
            ),
            (
                "data/applications/b.desktop",
                app!("Name=Beta\nCategories=X-B;\n"),
            ),
            (
                "data/applications/c.desktop",
                app!("Name=Gamma\nCategories=X-C;\n"),
            ),
            (
                "data/applications/d.desktop",
                app!("Name=Delta\nCategories=X-D;\n"),
            ),
            (
                "parts/one.menu",
                "<Menu><Name>Ignored</Name><Menu><Name>A</Name><Include><Category>X-A</Category></Include></Menu><MergeFile>two.menu</MergeFile></Menu>",
            ),
            (
                "parts/two.menu",
                "<Menu><Name>X</Name><Menu><Name>A</Name><Include><Category>X-B</Category></Include></Menu><MergeFile>one.menu</MergeFile></Menu>",
            ),
            (
                "config/menus/applications-merged/c.menu",
                "<Menu><Name>Applications</Name><Menu><Name>C</Name><Include><Category>X-C</Category></Include></Menu></Menu>",
            ),
            (
                "config-home/menus/applications-merged/d.menu",
                "<Menu><Name>Applications</Name><Menu><Name>C</Name><Include><Category>X-D</Category></Include><Exclude><Category>X-C</Category></Exclude></Menu></Menu>",
            ),
            (
                "config-home/menus/applications.menu",
                "<Menu><Name>Applications</Name><DefaultAppDirs/><MergeFile>@ROOT@/parts/one.menu</MergeFile>\
                 <DefaultMergeDirs/></Menu>",
            ),
        ],
        menu: "config-home/menus/applications.menu",
        vars: &[],
    },
    Scenario {
        name: "merge-parent",
        files: &[
            (
                "data/applications/a.desktop",
                app!("Name=Alpha\nCategories=X-A;\n"),
            ),
            (
                "data/applications/b.desktop",
                app!("Name=Beta\nCategories=X-B;\n"),
            ),
            (
                "config/menus/applications.menu",
                "<Menu><Name>Applications</Name><DefaultAppDirs/><Menu><Name>Sys</Name><Include><Category>X-A</Category></Include></Menu></Menu>",
            ),
            (
                "config-home/menus/applications.menu",
                "<Menu><Name>Applications</Name><MergeFile type=\"parent\">/ignored.menu</MergeFile>\
                 <Menu><Name>Mine</Name><Include><Category>X-B</Category></Include></Menu></Menu>",
            ),
        ],
        menu: "config-home/menus/applications.menu",
        vars: &[],
    },
    Scenario {
        name: "legacy",
        files: &[
            ("legacy/Top.desktop", app!("Name=Top\n")),
            ("legacy/Games/game.desktop", app!("Name=Game\n")),
            (
                "legacy/Games/.directory",
                "[Desktop Entry]\nType=Directory\nName=Legacy games\n",
            ),
            (
                "legacy/Games/categorized.desktop",
                app!("Name=Categorized\nCategories=Office;\n"),
            ),
            (
                "menu.menu",
                "<Menu><Name>Root</Name><LegacyDir prefix=\"old-\">legacy</LegacyDir>\
                 <Menu><Name>Office</Name><Include><Category>Office</Category></Include></Menu>\
                 <Menu><Name>All legacy</Name><Include><And><Category>Legacy</Category><Not><Category>Office</Category></Not></And></Include></Menu></Menu>",
            ),
        ],
        menu: "menu.menu",
        vars: &[],
    },
    Scenario {
        name: "rule-logic",
        files: &[
            (
                "data/applications/a.desktop",
                app!("Name=Alpha\nCategories=X-1;X-2;\n"),
            ),
            (
                "data/applications/b.desktop",
                app!("Name=Beta\nCategories=X-1;\n"),
            ),
            (
                "data/applications/c.desktop",
                app!("Name=Gamma\nCategories=X-3;\n"),
            ),
            (
                "menu.menu",
                "<Menu><Name>Root</Name><DefaultAppDirs/>\
                 <Menu><Name>Both</Name><Include><And><Category>X-1</Category><Category>X-2</Category></And></Include></Menu>\
                 <Menu><Name>Either</Name><Include><Or><Category>X-2</Category><Category>X-3</Category></Or></Include></Menu>\
                 <Menu><Name>Neither</Name><Include><Not><Category>X-1</Category><Category>X-3</Category></Not></Include></Menu>\
                 <Menu><Name>Named</Name><Include><Filename>c.desktop</Filename><Filename>b.desktop</Filename></Include>\
                 <Exclude><Filename>b.desktop</Filename></Exclude><Include><Filename>b.desktop</Filename></Include></Menu></Menu>",
            ),
        ],
        menu: "menu.menu",
        vars: &[],
    },
];

/// (scenario, this crate's lines, reason).
const DIFFERENCES: &[(&str, &str, &str)] = &[
    (
        "inline-without-header",
        "entry  a.desktop label=Alpha\nentry  z.desktop label=Zulu\nentry  m.desktop label=Mike",
        "the spec inlines a submenu \"at the current point\" of the layout; libgnome-menu pools \
         header-less inlined items with the menu's own and sorts them together",
    ),
    (
        "inline-header-alias-limit",
        "alias  label=One -> entry a.desktop\nheader  label=Two\nentry  b.desktop label=Beta\n\
         entry  c.desktop label=Gamma\nmenu Many label=Many\nentry Many d.desktop label=Delta\n\
         entry Many e.desktop label=Epsilon\nentry Many f.desktop label=Phi\nmenu Empty label=Empty\n\
         entry  top.desktop label=Top",
        "a <Menuname>'s attributes apply to the submenu it names; libgnome-menu's search for it \
         stops one short of its list's end, so the first submenu declared never gets them (the \
         variant `inline-alias-not-first` agrees)",
    ),
    (
        "merges",
        "menu A label=A\nentry A a.desktop label=Alpha\nentry A b.desktop label=Beta\n\
         menu C label=C\nentry C d.desktop label=Delta",
        "<DefaultMergeDirs> puts the default locations earlier in the search path later, \"so that \
         they have priority\": the user's merged <Exclude> comes last and removes Gamma. \
         libgnome-menu inserts each merged file right after the element, so the last merged (the \
         user's) lands first and loses",
    ),
    (
        "legacy",
        "menu All legacy label=All legacy\nentry All legacy old-game.desktop label=Game\n\
         entry All legacy old-Top.desktop label=Top\nmenu Games label=Legacy games\n\
         entry Games old-game.desktop label=Game\nmenu Office label=Office\n\
         entry Office old-categorized.desktop label=Categorized\nentry  old-Top.desktop label=Top",
        "the spec: implementations \"must be able to load these old-style hierarchies\"; \
         libgnome-menu 3.38 ignores <LegacyDir> (the spec's own suite checks this crate's legacy \
         loading)",
    ),
    (
        "exec-program-missing",
        "entry  ghost.desktop label=Ghost\nentry  real.desktop label=Real",
        "the Desktop Entry spec ties availability to TryExec (MenuContext::try_exec); GLib also \
         refuses an entry whose Exec program is not on PATH",
    ),
];

fn vars_for(root: &Path, scenario: &Scenario) -> HashMap<String, OsString> {
    let at = |dir: &str| root.join(dir).into_os_string();
    let mut vars: HashMap<String, OsString> = HashMap::from([
        ("HOME".to_owned(), at("home")),
        ("XDG_CONFIG_HOME".to_owned(), at("config-home")),
        ("XDG_CONFIG_DIRS".to_owned(), at("config")),
        ("XDG_DATA_HOME".to_owned(), at("data-home")),
        ("XDG_DATA_DIRS".to_owned(), at("data")),
        ("XDG_CURRENT_DESKTOP".to_owned(), "GNOME".into()),
        ("LANG".to_owned(), "C.UTF-8".into()),
        (
            "PATH".to_owned(),
            std::env::var_os("PATH").unwrap_or_default(),
        ),
    ]);
    for (name, value) in scenario.vars {
        vars.insert((*name).to_owned(), (*value).into());
    }
    vars
}

fn ours(root: &Path, scenario: &Scenario) -> String {
    let vars = vars_for(root, scenario);
    let context = MenuContext::from_vars(|name| vars.get(name).cloned()).unwrap();
    let tree = MenuTree::load_file(&context, root.join(scenario.menu)).unwrap();
    let mut out = Vec::new();
    walk(tree.root(), &mut out);
    out.join("\n")
}

fn walk(menu: &Menu, out: &mut Vec<String>) {
    let here = menu.path();
    for item in menu.items() {
        match item {
            MenuItem::Menu(child) => {
                out.push(format!("menu {} label={}", child.path(), child.label()));
                walk(child, out);
            }
            MenuItem::Entry(entry) => out.push(format!(
                "entry {here} {} label={}",
                entry.id(),
                entry.label()
            )),
            MenuItem::Separator => out.push(format!("separator {here}")),
            MenuItem::Header(header) => out.push(format!("header {here} label={}", header.label())),
            MenuItem::Alias(alias) => match alias.item() {
                Aliased::Entry(entry) => {
                    out.push(format!(
                        "alias {here} label={} -> entry {}",
                        alias.label(),
                        entry.id()
                    ));
                }
                Aliased::Menu(child) => {
                    out.push(format!(
                        "alias {here} label={} -> menu {}",
                        alias.label(),
                        child.path()
                    ));
                    walk(child, out);
                }
            },
        }
    }
}

fn gnome(root: &Path, scenario: &Scenario) -> String {
    let typelibs = std::env::var_os("XDG_KIT_GI_TYPELIB_PATH")
        .expect("XDG_KIT_GI_TYPELIB_PATH is not set: run the tests in the dev shell (nix develop)");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/gmenu-dump.py");
    let output = Command::new("python3")
        .arg(script)
        .arg(root.join(scenario.menu))
        .env_clear()
        .envs(vars_for(root, scenario))
        .env("GI_TYPELIB_PATH", typelibs)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .trim_end()
        .to_owned()
}

fn build(scenario: &Scenario) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    for (path, contents) in scenario.files {
        let path = root.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let contents = contents.replace("@ROOT@", root.path().to_str().unwrap());
        fs::write(path, contents).unwrap();
    }
    root
}

#[test]
fn resolved_trees_agree_with_libgnome_menu() {
    let _ = APP;
    let mut failures = Vec::new();
    for scenario in SCENARIOS {
        let root = build(scenario);
        let (ours, theirs) = (ours(root.path(), scenario), gnome(root.path(), scenario));
        match DIFFERENCES
            .iter()
            .find(|(name, _, _)| *name == scenario.name)
        {
            Some((_, expected, _)) => {
                if ours == theirs || ours != *expected {
                    failures.push(format!(
                        "== {} (listed difference)\n-- ours\n{ours}\n-- expected\n{expected}\n-- gnome\n{theirs}",
                        scenario.name
                    ));
                }
            }
            None if ours != theirs => failures.push(format!(
                "== {}\n-- ours\n{ours}\n-- gnome\n{theirs}",
                scenario.name
            )),
            None => {}
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}
