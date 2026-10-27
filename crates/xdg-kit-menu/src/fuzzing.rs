//! Entry points for the fuzz targets in `fuzz/`. Built only with
//! `--cfg fuzzing`, which `cargo fuzz` sets; not part of the API.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use xdg_kit_desktop_entry::{DesktopEntry, Desktops, KeyFile};
use xdg_kit_locale::Locales;

use crate::merge::Defaults;
use crate::resolve::{DirPool, PoolEntry, Provider, Settings, Slot};
use crate::{Aliased, Menu, MenuError, MenuItem, ShowPolicy};

/// Fixed entries and `.directory` files, nothing read from disk.
struct Fixtures {
    pool: DirPool,
    directory: Arc<DesktopEntry>,
}

impl Fixtures {
    fn new() -> Self {
        let app = |id: &str, extra: &str| {
            let entry = DesktopEntry::from_key_file(KeyFile::parse(
                format!("[Desktop Entry]\nType=Application\nExec=true\nName={id}\nName[de]=De {id}\n{extra}")
                    .as_bytes(),
            ));
            let id: Arc<str> = Arc::from(id);
            let slot = Slot::Entry(PoolEntry {
                id: Arc::clone(&id),
                categories: entry.categories().into(),
                entry: Arc::new(entry),
                app_dir: Arc::from(Path::new("/fuzz/apps")),
                legacy: false,
            });
            (id, slot)
        };
        // Sorted by ID, as a scanned directory is.
        let pool = vec![
            app("a.desktop", "Categories=Game;X-A;\nIcon=a\n"),
            app("b.desktop", "Categories=Office;\n"),
            app("c.desktop", "Categories=Game;Office;\nNoDisplay=true\n"),
            (Arc::from("d.desktop"), Slot::Hidden),
            app("e.desktop", "OnlyShowIn=KDE;\n"),
            app("kde-f.desktop", "Categories=X-A;\n"),
        ];
        let directory = DesktopEntry::from_key_file(KeyFile::parse(
            b"[Desktop Entry]\nType=Directory\nName=Folder\nName[de]=Mappe\nIcon=folder\n",
        ));
        Self {
            pool: Arc::new(pool),
            directory: Arc::new(directory),
        }
    }
}

impl Provider for Fixtures {
    fn app_dir(&mut self, _: &Path) -> Result<DirPool, MenuError> {
        Ok(Arc::clone(&self.pool))
    }

    fn legacy_dir(&mut self, _: &Path, _: &str) -> Result<DirPool, MenuError> {
        Ok(Arc::clone(&self.pool))
    }

    fn directory(
        &mut self,
        _: &Path,
        relative: &str,
    ) -> Result<Option<Arc<DesktopEntry>>, MenuError> {
        Ok(relative
            .ends_with(".directory")
            .then(|| Arc::clone(&self.directory)))
    }
}

/// Parses `bytes` as a menu file and resolves it over fixed entries, as
/// loading does once merges are expanded (merge elements are left
/// unexpanded: nothing is read), and holds the result to the crate's
/// contracts: nothing panics; a limit is the only error resolution gives;
/// the same input resolves to the same tree; every entry shown is one of
/// the pool's, never a hidden or `NoDisplay` one unless asked; and no menu
/// starts, ends, or doubles a separator.
pub fn menu(bytes: &[u8]) {
    let mut elements_left = 20_000;
    let Ok(element) = crate::xml::parse(bytes, 48, &mut elements_left) else {
        return;
    };
    let mut diagnostics = Vec::new();
    let file = PathBuf::from("/fuzz/fuzz.menu");
    let mut reader = crate::model::Reader {
        file: &file,
        base: Path::new("/fuzz"),
        diagnostics: &mut diagnostics,
    };
    let Some(mut root) = crate::model::menu(&element, &mut reader) else {
        return;
    };
    let defaults = Defaults {
        app_dirs: vec![
            PathBuf::from("/fuzz/apps"),
            PathBuf::from("/fuzz/more-apps"),
        ],
        directory_dirs: vec![PathBuf::from("/fuzz/dirs")],
    };
    crate::merge::consolidate(&mut root, &defaults);
    let mut limits = crate::merge::MoveLimits {
        max_depth: 48,
        menus_left: 20_000,
        steps_left: 200_000,
    };
    if crate::merge::run_moves(&mut root, &defaults, &mut diagnostics, "", 0, &mut limits).is_err()
    {
        return;
    }
    let desktops = Desktops::new(["GNOME"]);
    let locales = Locales::new(["de_DE"]);
    for no_display in [false, true] {
        let settings = Settings {
            desktops: &desktops,
            locales: &locales,
            show: ShowPolicy {
                no_display,
                empty: no_display,
            },
            max_matches: 200_000,
        };
        let resolve = |diagnostics: &mut Vec<_>| {
            crate::resolve::resolve(&root, &mut Fixtures::new(), &settings, diagnostics)
        };
        let first = resolve(&mut diagnostics);
        let again = resolve(&mut Vec::new());
        match (&first, &again) {
            (Ok(first), Ok(again)) => {
                assert_eq!(first, again, "deterministic");
                check(first, no_display);
            }
            (Err(MenuError::Limit { .. }), Err(MenuError::Limit { .. })) => {}
            other => panic!("only a limit fails resolution, the same each time: {other:?}"),
        }
    }
}

/// The shape every resolved menu has.
fn check(menu: &Menu, no_display: bool) {
    let items = menu.items();
    assert!(
        !matches!(items.first(), Some(MenuItem::Separator)),
        "{}",
        menu.path()
    );
    assert!(
        !matches!(items.last(), Some(MenuItem::Separator)),
        "{}",
        menu.path()
    );
    for pair in items.windows(2) {
        assert!(
            !matches!(pair, [MenuItem::Separator, MenuItem::Separator]),
            "{}",
            menu.path()
        );
    }
    for item in items {
        match item {
            MenuItem::Entry(entry) => {
                assert!(
                    ["a.desktop", "b.desktop", "c.desktop", "kde-f.desktop"].contains(&entry.id()),
                    "{} is not a shown entry of the pool",
                    entry.id()
                );
                assert!(
                    no_display || !entry.no_display(),
                    "{} is NoDisplay",
                    entry.id()
                );
            }
            MenuItem::Menu(child) => check(child, no_display),
            MenuItem::Alias(alias) => {
                if let Aliased::Menu(child) = alias.item() {
                    check(child, no_display);
                }
            }
            MenuItem::Header(_) | MenuItem::Separator => {}
        }
    }
}
