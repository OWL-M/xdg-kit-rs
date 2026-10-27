//! A menu loaded from a relative path keeps reading the files it read
//! when the working directory changes: its merges, its folders, relative
//! KDE legacy folders, and its refresh. The working directory is process-global, so this binary holds
//! exactly one test and nothing else runs beside it.

use std::fs;
use std::path::Path;

use xdg_kit_menu::{MenuContext, MenuTree};

fn write(base: &Path, relative: &str, contents: &str) {
    let path = base.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn lay_out(base: &Path, label: &str) {
    write(
        base,
        "menu.menu",
        "<Menu><Name>R</Name><AppDir>apps</AppDir><MergeFile>part.menu</MergeFile>\
         <KDELegacyDirs/></Menu>",
    );
    write(
        base,
        "part.menu",
        "<Menu><Name>R</Name><DirectoryDir>dirs</DirectoryDir>\
         <Menu><Name>M</Name><Directory>m.directory</Directory><Include><All/></Include></Menu></Menu>",
    );
    write(
        base,
        "apps/a.desktop",
        &format!("[Desktop Entry]\nType=Application\nExec=true\nName={label}\n"),
    );
    write(
        base,
        "legacy/l.desktop",
        &format!("[Desktop Entry]\nType=Application\nExec=true\nName={label} legacy\n"),
    );
    write(
        base,
        "dirs/m.directory",
        &format!("[Desktop Entry]\nType=Directory\nName={label} menu\n"),
    );
}

#[test]
fn a_relative_menu_file_is_anchored_at_load() {
    let root = tempfile::tempdir().unwrap();
    let (here, elsewhere) = (root.path().join("here"), root.path().join("elsewhere"));
    lay_out(&here, "Here");
    lay_out(&elsewhere, "Elsewhere");
    let empty = root.path().join("empty");
    let vars = |name: &str| match name {
        "HOME" => Some(empty.clone().into_os_string()),
        name if name.starts_with("XDG_") && name.ends_with("_DIRS") => {
            Some(empty.clone().into_os_string())
        }
        name if name.starts_with("XDG_") && name.ends_with("_HOME") => {
            Some(empty.clone().into_os_string())
        }
        _ => None,
    };
    let mut context = MenuContext::from_vars(vars).unwrap();
    context.kde_legacy_dirs = vec!["legacy".into()];
    let legacy = |tree: &MenuTree| {
        tree.root()
            .entries()
            .find(|entry| entry.id() == "kde-l.desktop")
            .map(|entry| entry.label().to_owned())
    };

    std::env::set_current_dir(&here).unwrap();
    let tree = MenuTree::load_file(&context, "menu.menu").unwrap();
    assert!(tree.files()[0].is_absolute());
    let menu = tree.root().submenu("M").unwrap();
    assert_eq!(menu.label(), "Here menu");
    assert_eq!(menu.entries().next().unwrap().label(), "Here");
    assert_eq!(legacy(&tree).as_deref(), Some("Here legacy"));

    std::env::set_current_dir(&elsewhere).unwrap();
    assert!(tree.refresh().is_none(), "the decoy is not looked at");
    write(
        &here,
        "apps/a.desktop",
        "[Desktop Entry]\nType=Application\nExec=true\nName=Changed\n",
    );
    let after = tree.refresh().expect("the change is noticed").unwrap();
    let menu = after.root().submenu("M").unwrap();
    assert_eq!(menu.label(), "Here menu");
    assert_eq!(menu.entries().next().unwrap().label(), "Changed");
    assert_eq!(legacy(&after).as_deref(), Some("Here legacy"));
    assert!(after.files().iter().all(|file| file.starts_with(&here)));
}
