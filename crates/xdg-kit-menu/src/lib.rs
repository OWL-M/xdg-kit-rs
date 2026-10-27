//! The freedesktop.org Desktop Menu Specification: `.menu` files resolved
//! into the application menu a launcher, panel or menu editor shows —
//! merges, moves, deletions, `<Include>`/`<Exclude>` rules with
//! `<OnlyUnallocated>`, legacy hierarchies, `.directory` captions and the
//! `<Layout>` presentation hints.
//!
//! Loading reads; it never launches anything. Each [`Entry`] carries its
//! desktop entry, read with its path, ready for
//! [`prepare_launch`](xdg_kit_desktop_entry::prepare_launch).
//!
//! ```no_run
//! use xdg_kit_menu::{MenuContext, MenuItem, MenuTree};
//!
//! let tree = MenuTree::load(&MenuContext::from_env()?)?;
//! for item in tree.root().items() {
//!     match item {
//!         MenuItem::Menu(menu) => println!("{}/", menu.label()),
//!         MenuItem::Entry(entry) => println!("{} ({})", entry.label(), entry.id()),
//!         MenuItem::Separator => println!("----"),
//!         MenuItem::Header(header) => println!("[{}]", header.label()),
//!         MenuItem::Alias(alias) => println!("{}", alias.label()),
//!     }
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Policies
//!
//! - **Visibility.** Matching and allocation come first, over every menu —
//!   deleted ones and ones hidden by their `.directory` file included, as
//!   the spec's own test suite requires. Then entries with `NoDisplay=true`,
//!   or not shown in [`MenuContext::desktops`] (`OnlyShowIn`/`NotShowIn`),
//!   are left out, and so are menus that are deleted, or whose `.directory`
//!   file says `NoDisplay=true` or is not shown in the desktops.
//!   [`ShowPolicy`] can keep `NoDisplay` items and empty menus.
//! - **Executables.** With [`MenuContext::try_exec`] set, a desktop entry
//!   whose `TryExec` names no installed program is ignored as if absent —
//!   the Desktop Entry spec's "may be ignored", and what GNOME does — so a
//!   less important copy of its ID may stand in. The check is the Desktop
//!   Entry crate's GLib-compatible one
//!   ([`DesktopEntry::try_exec_available`](xdg_kit_desktop_entry::DesktopEntry::try_exec_available)).
//! - **Malformed input.** A root menu file that cannot be read, is not
//!   well-formed XML or is no `<Menu>` is an error. A merged file like that
//!   is skipped with a diagnostic and marks the tree degraded
//!   ([`MenuTree::is_degraded`]). Unknown elements are ignored with a
//!   diagnostic, as the spec asks. A rule element that is unknown or
//!   malformed neither includes nor spares: an `<Include>` takes only what
//!   its rules certainly match, an `<Exclude>` removes what they might —
//!   so an invalid construct can never widen what a menu shows.
//! - **Limits.** Every [`Limits`] guard on the configuration fails the
//!   load ([`MenuError::Limit`]) rather than build a menu without the part
//!   it could not read, which might be what hides something.
//! - **Determinism.** Merge directories are merged in byte order of their
//!   file names; layouts sort by caption case-folded, then as written, then
//!   by ID or name.
//!
//! # Reference
//!
//! The tests run the spec's own regression suite against this crate and
//! compare full trees with GNOME's libgnome-menu, recording each
//! deliberate difference with its reason.

#[cfg(not(unix))]
compile_error!("xdg-kit-menu supports Unix systems only");

mod error;
#[cfg(fuzzing)]
#[doc(hidden)]
pub mod fuzzing;
mod limits;
mod load;
mod merge;
mod model;
mod resolve;
#[cfg(test)]
mod tests;
mod tree;
mod work;
mod xml;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use xdg_kit_basedir::{BaseDirs, FileStamp};
use xdg_kit_desktop_entry::{AppIndex, DesktopEntry};

pub use error::{Diagnostic, MenuError};
pub use limits::Limits;
pub use tree::{Alias, Aliased, Directory, Entry, Header, Menu, MenuItem};
pub use xdg_kit_desktop_entry::Desktops;
pub use xdg_kit_locale::Locales;

/// What to show that the defaults leave out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ShowPolicy {
    /// Keep entries and menus that say `NoDisplay=true` (flagged:
    /// [`Entry::no_display`], [`Menu::no_display`]) — for menu editors.
    pub no_display: bool,
    /// Keep empty menus whatever their layout's `show_empty` says.
    pub empty: bool,
}

/// Everything resolution depends on, explicit: no environment variable is
/// read except by [`MenuContext::from_env`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct MenuContext {
    /// Where menus, merge directories, desktop entries and `.directory`
    /// files are looked for.
    pub base_dirs: BaseDirs,
    /// `$XDG_MENU_PREFIX`: the main menu is `${prefix}applications.menu`.
    pub menu_prefix: String,
    /// The session's desktops, for `OnlyShowIn`/`NotShowIn`.
    pub desktops: Desktops,
    /// The languages captions are taken in.
    pub locales: Locales,
    /// A `PATH`-style search path to check `TryExec` against; `None` checks
    /// nothing.
    pub try_exec: Option<OsString>,
    /// The directories `<KDELegacyDirs>` stands for, most important first
    /// (KDE's `kde-config --path apps`, which loading never runs). Empty by
    /// default: the element then loads nothing. Relative ones are anchored
    /// to the working directory when a menu is loaded.
    pub kde_legacy_dirs: Vec<PathBuf>,
    /// What to show beyond the defaults.
    pub show: ShowPolicy,
    /// Bounds on loading and resolving.
    pub limits: Limits,
}

impl MenuContext {
    /// A context over `base_dirs`, with no menu prefix, no desktops,
    /// untranslated captions, no `TryExec` check and the default limits.
    #[must_use]
    pub fn new(base_dirs: BaseDirs) -> Self {
        Self {
            base_dirs,
            menu_prefix: String::new(),
            desktops: Desktops::default(),
            locales: Locales::untranslated(),
            try_exec: None,
            kde_legacy_dirs: Vec::new(),
            show: ShowPolicy::default(),
            limits: Limits::default(),
        }
    }

    /// This process's base directories, `XDG_MENU_PREFIX`,
    /// `XDG_CURRENT_DESKTOP`, languages and `PATH` (for `TryExec`).
    ///
    /// # Errors
    ///
    /// When the base directories cannot be found (no usable `HOME`).
    pub fn from_env() -> Result<Self, xdg_kit_basedir::Error> {
        Self::from_vars(|name| std::env::var_os(name))
    }

    /// As [`MenuContext::from_env`], with the variables `vars` returns.
    ///
    /// # Errors
    ///
    /// When the base directories cannot be found (no usable `HOME`).
    pub fn from_vars(
        vars: impl Fn(&str) -> Option<OsString>,
    ) -> Result<Self, xdg_kit_basedir::Error> {
        let mut context = Self::new(BaseDirs::from_vars(&vars)?);
        context.menu_prefix = vars("XDG_MENU_PREFIX")
            .and_then(|prefix| prefix.into_string().ok())
            .unwrap_or_default();
        context.desktops = Desktops::from_vars(&vars);
        context.locales = Locales::from_vars(&vars);
        context.try_exec = Some(vars("PATH").unwrap_or_default());
        Ok(context)
    }
}

/// Which menu a tree was loaded from, for its refresh.
#[derive(Debug, Clone)]
enum Target {
    Named(String),
    File(PathBuf),
}

/// A resolved menu: the tree, what loading reported, and what it read.
#[derive(Debug)]
pub struct MenuTree {
    root: Menu,
    diagnostics: Vec<Diagnostic>,
    files: Vec<PathBuf>,
    degraded: bool,
    stamps: Vec<(PathBuf, FileStamp)>,
    indices: Vec<AppIndex>,
    /// The entries whose `TryExec` was checked, with the verdict.
    try_execs: Vec<(Arc<DesktopEntry>, bool)>,
    context: MenuContext,
    target: Target,
}

impl MenuTree {
    /// The main application menu: the first
    /// `menus/${XDG_MENU_PREFIX}applications.menu` in the configuration
    /// directories, the user's first.
    ///
    /// # Errors
    ///
    /// See [`MenuError`].
    pub fn load(context: &MenuContext) -> Result<Self, MenuError> {
        Self::load_named(
            context,
            &format!("{}applications.menu", context.menu_prefix),
        )
    }

    /// The menu file `name` (such as `preferences.menu`), the first in
    /// the configuration directories' `menus` folders. Its default merge
    /// directories are `<name without .menu and the menu prefix>-merged`.
    ///
    /// # Errors
    ///
    /// See [`MenuError`].
    pub fn load_named(context: &MenuContext, name: &str) -> Result<Self, MenuError> {
        Self::run(context, Target::Named(name.to_owned()))
    }

    /// The menu file at `path`. A relative path is anchored to the working
    /// directory now: refreshes read the same file whatever the working
    /// directory is then.
    ///
    /// # Errors
    ///
    /// [`MenuError::Path`] when `path` cannot be made absolute; otherwise
    /// see [`MenuError`].
    pub fn load_file(context: &MenuContext, path: impl AsRef<Path>) -> Result<Self, MenuError> {
        let path = std::path::absolute(path).map_err(MenuError::Path)?;
        Self::run(context, Target::File(path))
    }

    fn run(context: &MenuContext, target: Target) -> Result<Self, MenuError> {
        // Relative KDE legacy folders are anchored now, as a relative menu
        // file is: the tree and its refreshes keep reading the same ones.
        let mut context = context.clone();
        for dir in &mut context.kde_legacy_dirs {
            *dir = std::path::absolute(&*dir).map_err(MenuError::Path)?;
        }
        let context = &context;
        let name = match &target {
            Target::Named(name) => name.clone(),
            Target::File(path) => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };
        let stem = name.strip_suffix(".menu").unwrap_or(&name);
        let base = stem
            .strip_prefix(context.menu_prefix.as_str())
            .unwrap_or(stem);
        let mut loader = load::Loader::new(context, base.to_owned());
        let path = match &target {
            Target::Named(name) => loader.find(name)?,
            Target::File(path) => path.clone(),
        };
        let mut root = loader.load_root(&path)?;
        let defaults = loader.defaults();
        merge::consolidate(&mut root, &defaults);
        let mut limits = loader.move_limits();
        merge::run_moves(
            &mut root,
            &defaults,
            &mut loader.diagnostics,
            "",
            0,
            &mut limits,
        )?;
        let settings = resolve::Settings {
            desktops: &context.desktops,
            locales: &context.locales,
            show: context.show,
            max_matches: limits.steps_left,
        };
        let mut diagnostics = Vec::new();
        let menu = resolve::resolve(&root, &mut loader, &settings, &mut diagnostics)?;
        loader.diagnostics.append(&mut diagnostics);
        Ok(Self {
            root: menu,
            diagnostics: loader.diagnostics,
            files: loader.files,
            degraded: loader.degraded,
            stamps: loader.stamps,
            indices: loader.indices,
            try_execs: loader.try_execs,
            context: context.clone(),
            target,
        })
    }

    /// The root menu.
    #[must_use]
    pub fn root(&self) -> &Menu {
        &self.root
    }

    /// What loading and resolution reported.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// The menu files read, the root first, each merged file once.
    #[must_use]
    pub fn files(&self) -> &[PathBuf] {
        &self.files
    }

    /// Whether a merged file or a directory could not be used (each a
    /// diagnostic): the menu then lacks what it would have contributed.
    #[must_use]
    pub fn is_degraded(&self) -> bool {
        self.degraded
    }

    /// A fresh load, when any file or directory this one consulted — menu
    /// files, merge and legacy directories, desktop entries, `.directory`
    /// files, and every place a file was looked for and not found —
    /// changed since, or a `TryExec` program checked appeared, went or
    /// changed whether it can run; `None` while all look the same.
    #[must_use]
    pub fn refresh(&self) -> Option<Result<Self, MenuError>> {
        let search_path = self.context.try_exec.as_deref().unwrap_or_default();
        let unchanged = self
            .stamps
            .iter()
            .all(|(path, stamp)| FileStamp::of(path) == *stamp)
            && self.indices.iter().all(|index| index.refresh().is_none())
            && self
                .try_execs
                .iter()
                .all(|(entry, available)| entry.try_exec_available(search_path) == *available);
        if unchanged {
            return None;
        }
        Some(Self::run(&self.context, self.target.clone()))
    }
}

// Trees are shared between threads; keep them that way.
const _: fn() = || {
    fn shareable<T: Send + Sync>() {}
    shareable::<MenuTree>();
    shareable::<Menu>();
    shareable::<Entry>();
    shareable::<MenuContext>();
    shareable::<Limits>();
};
