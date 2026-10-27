//! The resolved menu: what a launcher shows, in order, with where each
//! piece came from.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use xdg_kit_desktop_entry::DesktopEntry;

/// A menu after resolution and layout: its items in display order.
///
/// Four different strings say what a menu is, kept apart:
/// [`Menu::name`] is the internal `<Name>` (what menu paths and layouts
/// refer to), [`Menu::path`] the menu path from the root, [`Menu::label`]
/// the localized caption to show, and [`Menu::directory`] the `.directory`
/// file the caption and icon came from, if any.
#[derive(Debug, Clone, PartialEq)]
pub struct Menu {
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) label: String,
    pub(crate) icon: Option<String>,
    pub(crate) directory: Option<Directory>,
    pub(crate) items: Vec<MenuItem>,
    pub(crate) sources: Vec<PathBuf>,
    pub(crate) no_display: bool,
}

impl Menu {
    /// The menu's `<Name>`.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The menu path: the names from below the root to this menu,
    /// separated by `/`; empty for the root.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The caption: the `.directory` file's localized `Name`, else the
    /// menu's name.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The `.directory` file's `Icon` value: an icon name or path, for the
    /// caller to resolve (with `xdg-kit-icon-theme`, say).
    #[must_use]
    pub fn icon(&self) -> Option<&str> {
        self.icon.as_deref()
    }

    /// The `.directory` file describing the menu, if one was found.
    #[must_use]
    pub fn directory(&self) -> Option<&Directory> {
        self.directory.as_ref()
    }

    /// The items, in the order the layout gives them.
    #[must_use]
    pub fn items(&self) -> &[MenuItem] {
        &self.items
    }

    /// The menu files whose `<Menu>` elements made this menu.
    #[must_use]
    pub fn sources(&self) -> &[PathBuf] {
        &self.sources
    }

    /// Whether the `.directory` file says `NoDisplay=true`; such menus
    /// appear only when [`ShowPolicy::no_display`](crate::ShowPolicy::no_display)
    /// is set.
    #[must_use]
    pub fn no_display(&self) -> bool {
        self.no_display
    }

    /// The submenu named `name` among this menu's items (not inlined ones).
    #[must_use]
    pub fn submenu(&self, name: &str) -> Option<&Menu> {
        self.items.iter().find_map(|item| match item {
            MenuItem::Menu(menu) if menu.name == name => Some(menu),
            _ => None,
        })
    }

    /// Every entry this menu shows, its submenus' and inlined ones
    /// included, depth first in display order.
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        let mut stack: Vec<std::slice::Iter<'_, MenuItem>> = vec![self.items.iter()];
        std::iter::from_fn(move || {
            while let Some(top) = stack.last_mut() {
                let Some(item) = top.next() else {
                    stack.pop();
                    continue;
                };
                match item {
                    MenuItem::Entry(entry) => return Some(entry),
                    MenuItem::Menu(menu) => stack.push(menu.items.iter()),
                    MenuItem::Alias(alias) => match &alias.item {
                        Aliased::Entry(entry) => return Some(entry),
                        Aliased::Menu(menu) => stack.push(menu.items.iter()),
                    },
                    MenuItem::Separator | MenuItem::Header(_) => {}
                }
            }
            None
        })
    }
}

/// One item of a menu, in display order.
#[derive(Debug, Clone, PartialEq)]
pub enum MenuItem {
    /// An application.
    Entry(Entry),
    /// A submenu.
    Menu(Menu),
    /// A visual separator.
    Separator,
    /// The caption of an inlined submenu, before its items
    /// (`inline_header`).
    Header(Header),
    /// The single item of an inlined submenu, shown under the submenu's
    /// caption (`inline_alias`).
    Alias(Alias),
}

/// The caption an inlined submenu's items are shown under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub(crate) label: String,
    pub(crate) menu_path: String,
}

impl Header {
    /// The inlined submenu's caption.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The inlined submenu's menu path.
    #[must_use]
    pub fn menu_path(&self) -> &str {
        &self.menu_path
    }
}

/// An inlined submenu's only item, shown under the submenu's caption.
#[derive(Debug, Clone, PartialEq)]
pub struct Alias {
    pub(crate) label: String,
    pub(crate) menu_path: String,
    pub(crate) item: Aliased,
}

/// What an [`Alias`] shows.
#[derive(Debug, Clone, PartialEq)]
pub enum Aliased {
    /// An application.
    Entry(Entry),
    /// A submenu.
    Menu(Menu),
}

impl Alias {
    /// The caption to show: the inlined submenu's.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The inlined submenu's menu path.
    #[must_use]
    pub fn menu_path(&self) -> &str {
        &self.menu_path
    }

    /// The item shown.
    #[must_use]
    pub fn item(&self) -> &Aliased {
        &self.item
    }
}

/// An application of a menu.
///
/// [`Entry::id`] is the desktop-file ID (what `<Filename>` matches and
/// layouts name), [`Entry::path`] the file it was read from, and
/// [`Entry::label`] the localized caption. The full entry, read with its
/// path — as `%k` and D-Bus activation need — is [`Entry::entry`], ready for
/// [`prepare_launch`](xdg_kit_desktop_entry::prepare_launch); nothing is
/// launched here.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub(crate) id: Arc<str>,
    pub(crate) label: Arc<str>,
    pub(crate) icon: Option<Arc<str>>,
    pub(crate) desktop: Arc<DesktopEntry>,
    pub(crate) app_dir: Arc<Path>,
    pub(crate) legacy: bool,
}

impl Entry {
    /// The desktop-file ID.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The file the entry was read from.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.desktop.path().unwrap_or(&self.app_dir)
    }

    /// The caption: the entry's localized `Name`, else its ID.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The entry's localized `Icon` value: an icon name or path, for the
    /// caller to resolve.
    #[must_use]
    pub fn icon(&self) -> Option<&str> {
        self.icon.as_deref()
    }

    /// The desktop entry, read with its path.
    #[must_use]
    pub fn entry(&self) -> &DesktopEntry {
        &self.desktop
    }

    /// The `<AppDir>` (or legacy directory) the entry was found under.
    #[must_use]
    pub fn app_dir(&self) -> &Path {
        &self.app_dir
    }

    /// Whether it came from a `<LegacyDir>` hierarchy.
    #[must_use]
    pub fn is_legacy(&self) -> bool {
        self.legacy
    }

    /// Whether the entry says `NoDisplay=true`; such entries appear only
    /// when [`ShowPolicy::no_display`](crate::ShowPolicy::no_display) is set.
    #[must_use]
    pub fn no_display(&self) -> bool {
        self.desktop.no_display()
    }
}

/// The `.directory` file describing a menu.
#[derive(Debug, Clone, PartialEq)]
pub struct Directory {
    pub(crate) id: String,
    pub(crate) entry: Arc<DesktopEntry>,
}

impl Directory {
    /// Its relative path below the `<DirectoryDir>` it was found in — what
    /// `<Directory>` names.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The file.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.entry.path()
    }

    /// The entry, for the keys this crate does not surface.
    #[must_use]
    pub fn entry(&self) -> &DesktopEntry {
        &self.entry
    }
}
