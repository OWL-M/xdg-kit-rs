//! Generating the menus (spec "Generating the menus") and laying them out
//! ("Layout"), over a merged menu. Pure: desktop entries and `.directory`
//! files come from a [`Provider`], so this runs the same over files and
//! over fixtures.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use xdg_kit_desktop_entry::{DesktopEntry, Desktops};
use xdg_kit_locale::Locales;

use crate::model::{Item, LayoutAttributes, LayoutNode, MenuNode, MergeKind, Rule};
use crate::tree::{Alias, Aliased, Directory, Entry, Header, Menu, MenuItem};
use crate::{Diagnostic, MenuError, ShowPolicy, work};

/// A desktop entry in a pool.
#[derive(Debug, Clone)]
pub(crate) struct PoolEntry {
    pub id: Arc<str>,
    pub entry: Arc<DesktopEntry>,
    pub categories: Arc<[String]>,
    pub app_dir: Arc<Path>,
    pub legacy: bool,
}

/// What a directory holds for an ID: an entry, or `Hidden=true` (the ID is
/// deleted here and in every less important directory).
#[derive(Debug, Clone)]
pub(crate) enum Slot {
    Entry(PoolEntry),
    Hidden,
}

/// One scanned directory's IDs, sorted.
pub(crate) type DirPool = Arc<Vec<(Arc<str>, Slot)>>;

/// Where entries and `.directory` files come from.
pub(crate) trait Provider {
    /// The applications of an `<AppDir>`.
    fn app_dir(&mut self, path: &Path) -> Result<DirPool, MenuError>;
    /// The applications of a legacy hierarchy.
    fn legacy_dir(&mut self, path: &Path, prefix: &str) -> Result<DirPool, MenuError>;
    /// The `.directory` file at `relative` below `dir`, when it exists, can
    /// be read and is not `Hidden`.
    ///
    /// # Errors
    ///
    /// When the file is over a limit: without it, a menu its `NoDisplay`
    /// would hide could show.
    fn directory(
        &mut self,
        dir: &Path,
        relative: &str,
    ) -> Result<Option<Arc<DesktopEntry>>, MenuError>;
}

/// What resolution is judged against.
pub(crate) struct Settings<'a> {
    pub desktops: &'a Desktops,
    pub locales: &'a Locales,
    pub show: ShowPolicy,
    pub max_matches: usize,
}

type Pool = Arc<BTreeMap<Arc<str>, PoolEntry>>;

/// A `<DefaultLayout>`: its attributes and nodes, shared by every menu
/// that inherits it.
type DefaultLayout = Arc<(LayoutAttributes, Vec<LayoutNode>)>;

/// A menu being generated.
struct Work {
    name: Option<String>,
    path: String,
    pool: Pool,
    /// Most important first: the menu's own, last-written first, then its
    /// parent's.
    directory_dirs: Arc<Vec<PathBuf>>,
    directories: Vec<String>,
    only_unallocated: bool,
    deleted: bool,
    rules: Vec<(bool, Vec<Rule>)>,
    layout: Option<Vec<LayoutNode>>,
    default_layout: Option<DefaultLayout>,
    sources: Vec<PathBuf>,
    included: BTreeSet<Arc<str>>,
    children: Vec<Work>,
}

/// Three-valued truth: a rule the format does not define is `Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Truth {
    True,
    False,
    Unknown,
}

impl Truth {
    fn not(self) -> Self {
        match self {
            Self::True => Self::False,
            Self::False => Self::True,
            Self::Unknown => Self::Unknown,
        }
    }
}

/// Resolves the merged `root` into the laid-out menu.
pub(crate) fn resolve(
    root: &MenuNode,
    provider: &mut impl Provider,
    settings: &Settings<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Menu, MenuError> {
    let empty: Pool = Arc::new(BTreeMap::new());
    let mut budget = settings.max_matches;
    let mut work = build(
        root,
        String::new(),
        &empty,
        &Arc::new(Vec::new()),
        provider,
        &mut budget,
    )?;
    let mut allocated: HashSet<Arc<str>> = HashSet::new();
    // Pass one: every menu that may take any entry — deleted ones too, as
    // the spec's own test suite requires (`NoDisplay2`).
    match_pass(&mut work, false, &mut allocated, &mut budget)?;
    // Pass two: the menus restricted to what nothing else took.
    match_pass(&mut work, true, &mut allocated, &mut budget)?;
    let defaults: DefaultLayout = Arc::new((LayoutAttributes::default(), default_nodes()));
    let root_name = work.name.clone().unwrap_or_default();
    if work.deleted {
        diagnostics.push(Diagnostic::new(
            None,
            None,
            "the root menu is deleted: it shows nothing",
        ));
    }
    Ok(output(
        &work,
        &defaults,
        provider,
        settings,
        diagnostics,
        &mut budget,
    )?
    .unwrap_or_else(|| Menu {
        label: root_name.clone(),
        name: root_name,
        path: String::new(),
        icon: None,
        directory: None,
        items: Vec::new(),
        sources: work.sources.clone(),
        no_display: false,
    }))
}

/// The built-in default layout: submenus, then entries.
fn default_nodes() -> Vec<LayoutNode> {
    vec![
        LayoutNode::Merge(MergeKind::Menus),
        LayoutNode::Merge(MergeKind::Files),
    ]
}

/// The working menu of `node`: its pool, its directory directories and
/// its other settings gathered from the items.
fn build(
    node: &MenuNode,
    path: String,
    parent_pool: &Pool,
    parent_dirs: &Arc<Vec<PathBuf>>,
    provider: &mut impl Provider,
    budget: &mut usize,
) -> Result<Work, MenuError> {
    let mut sources = Vec::new();
    let mut own_dirs = Vec::new();
    let mut work = Work {
        name: node.name.clone(),
        path,
        pool: Arc::clone(parent_pool),
        directory_dirs: Arc::clone(parent_dirs),
        directories: Vec::new(),
        only_unallocated: false,
        deleted: false,
        rules: Vec::new(),
        layout: None,
        default_layout: None,
        sources: node.sources.clone(),
        included: BTreeSet::new(),
        children: Vec::new(),
    };
    for item in &node.items {
        work::count(1);
        match item {
            Item::AppDir(path) => sources.push(provider.app_dir(path)?),
            Item::LegacyAppDir { path, prefix } => sources.push(provider.legacy_dir(path, prefix)?),
            Item::DirectoryDir(path) => own_dirs.push(path.clone()),
            Item::Directory(name) => work.directories.push(name.clone()),
            Item::OnlyUnallocated(only) => work.only_unallocated = *only,
            Item::Deleted(deleted) => work.deleted = *deleted,
            Item::Include(rules) => work.rules.push((true, rules.clone())),
            Item::Exclude(rules) => work.rules.push((false, rules.clone())),
            Item::Layout(nodes) => {
                // An empty <Layout> asks for the default layout.
                work.layout = (!nodes.is_empty()).then(|| nodes.clone());
            }
            Item::DefaultLayout(attributes, nodes) => {
                work.default_layout = Some(Arc::new((*attributes, nodes.clone())));
            }
            _ => {}
        }
    }
    if !sources.is_empty() {
        // Later directories win; the menu's own win over its parent's. The
        // copy grows with menus × entries, as matching does: it spends the
        // same budget.
        spend(
            budget,
            parent_pool.len() + sources.iter().map(|source| source.len()).sum::<usize>(),
        )?;
        let mut pool = (**parent_pool).clone();
        for source in &sources {
            for (id, slot) in source.iter() {
                work::count(1 + id.len());
                match slot {
                    Slot::Entry(entry) => {
                        pool.insert(Arc::clone(id), entry.clone());
                    }
                    Slot::Hidden => {
                        pool.remove(id);
                    }
                }
            }
        }
        work.pool = Arc::new(pool);
    }
    if !own_dirs.is_empty() {
        let mut dirs: Vec<PathBuf> = own_dirs.into_iter().rev().collect();
        dirs.extend(parent_dirs.iter().cloned());
        work.directory_dirs = Arc::new(dirs);
    }
    for item in &node.items {
        if let Item::Menu(child) = item {
            let child_path = match &child.name {
                Some(name) => crate::merge::join(&work.path, name),
                None => crate::merge::join(&work.path, "(unnamed)"),
            };
            work.children.push(build(
                child,
                child_path,
                &work.pool,
                &work.directory_dirs,
                provider,
                budget,
            )?);
        }
    }
    Ok(work)
}

/// One matching pass: the menus whose `only_unallocated` is `only`, every
/// level, parents before children.
fn match_pass(
    work: &mut Work,
    only: bool,
    allocated: &mut HashSet<Arc<str>>,
    budget: &mut usize,
) -> Result<(), MenuError> {
    let mut stack: Vec<&mut Work> = vec![work];
    while let Some(menu) = stack.pop() {
        if menu.only_unallocated == only {
            run_rules(menu, only, allocated, budget)?;
        }
        stack.extend(menu.children.iter_mut().rev());
    }
    Ok(())
}

/// The `<Include>`s and `<Exclude>`s of `menu`, in order. An include adds
/// what its rules certainly match (and, outside the second pass, marks it
/// allocated — an exclusion later does not undo that); an exclude removes
/// what its rules match or might match.
fn run_rules(
    menu: &mut Work,
    second_pass: bool,
    allocated: &mut HashSet<Arc<str>>,
    budget: &mut usize,
) -> Result<(), MenuError> {
    let pool = Arc::clone(&menu.pool);
    for (include, rules) in &menu.rules {
        if *include {
            for (id, entry) in pool.iter() {
                work::count(1);
                // Every candidate visited is work, the ones passed over too.
                spend(budget, 1)?;
                if second_pass && allocated.contains(id) {
                    continue;
                }
                if any(rules, entry, budget)? == Truth::True {
                    menu.included.insert(Arc::clone(id));
                    if !second_pass {
                        allocated.insert(Arc::clone(id));
                    }
                }
            }
        } else {
            let mut removed = Vec::new();
            for id in &menu.included {
                work::count(1);
                spend(budget, 1)?;
                let Some(entry) = pool.get(id) else {
                    continue;
                };
                if any(rules, entry, budget)? != Truth::False {
                    removed.push(Arc::clone(id));
                }
            }
            for id in removed {
                menu.included.remove(&id);
            }
        }
    }
    Ok(())
}

/// The rules joined by "or".
fn any(rules: &[Rule], entry: &PoolEntry, budget: &mut usize) -> Result<Truth, MenuError> {
    let mut truth = Truth::False;
    for rule in rules {
        match eval(rule, entry, budget)? {
            Truth::True => return Ok(Truth::True),
            Truth::Unknown => truth = Truth::Unknown,
            Truth::False => {}
        }
    }
    Ok(truth)
}

/// The rules joined by "and"; no rules at all match everything.
fn all(rules: &[Rule], entry: &PoolEntry, budget: &mut usize) -> Result<Truth, MenuError> {
    let mut truth = Truth::True;
    for rule in rules {
        match eval(rule, entry, budget)? {
            Truth::False => return Ok(Truth::False),
            Truth::Unknown => truth = Truth::Unknown,
            Truth::True => {}
        }
    }
    Ok(truth)
}

/// Takes `steps` from the budget of matching work, or fails at its end.
fn spend(budget: &mut usize, steps: usize) -> Result<(), MenuError> {
    *budget = budget.checked_sub(steps).ok_or_else(|| MenuError::Limit {
        message: "more matching work than the limit allows (see Limits::max_matches)".to_owned(),
    })?;
    Ok(())
}

fn eval(rule: &Rule, entry: &PoolEntry, budget: &mut usize) -> Result<Truth, MenuError> {
    spend(budget, 1)?;
    work::count(1);
    let truth = |yes: bool| if yes { Truth::True } else { Truth::False };
    Ok(match rule {
        Rule::Filename(id) => {
            work::count(id.len());
            truth(*entry.id == **id)
        }
        Rule::Category(category) => truth(entry.categories.iter().any(|had| {
            work::count(1 + had.len());
            had == category
        })),
        Rule::All => Truth::True,
        Rule::And(rules) => all(rules, entry, budget)?,
        Rule::Or(rules) => any(rules, entry, budget)?,
        Rule::Not(rules) => any(rules, entry, budget)?.not(),
        Rule::Unknown => Truth::Unknown,
    })
}

/// The laid-out menu of `work`, `None` when it is not shown: unnamed,
/// deleted, or kept from display by its `.directory` file.
fn output(
    work: &Work,
    parent_default: &DefaultLayout,
    provider: &mut impl Provider,
    settings: &Settings<'_>,
    diagnostics: &mut Vec<Diagnostic>,
    budget: &mut usize,
) -> Result<Option<Menu>, MenuError> {
    let Some(name) = work.name.clone() else {
        return Ok(None);
    };
    if work.deleted {
        return Ok(None);
    }
    let directory = directory(work, provider)?;
    let mut no_display = false;
    if let Some(found) = &directory {
        no_display = found.entry.no_display();
        if (no_display && !settings.show.no_display) || !found.entry.shown_in(settings.desktops) {
            return Ok(None);
        }
    }
    let label = directory
        .as_ref()
        .and_then(|found| found.entry.name(settings.locales))
        .unwrap_or_else(|| name.clone());
    let icon = directory
        .as_ref()
        .and_then(|found| found.entry.icon(settings.locales));
    let default = Arc::clone(work.default_layout.as_ref().unwrap_or(parent_default));
    let mut children = Vec::new();
    for child in &work.children {
        children.extend(output(
            child,
            &default,
            provider,
            settings,
            diagnostics,
            budget,
        )?);
    }
    let entries: Vec<Entry> = work
        .included
        .iter()
        .filter_map(|id| work.pool.get(id))
        .filter(|found| {
            (settings.show.no_display || !found.entry.no_display())
                && found.entry.shown_in(settings.desktops)
        })
        .map(|found| entry(found, settings.locales))
        .collect();
    // Laying out walks the layout once per menu, an inherited one in each
    // menu that inherits it: that grows with menus × layout, so it spends
    // the budget.
    let nodes = work.layout.as_deref().unwrap_or(&default.1);
    spend(budget, nodes.len())?;
    let items = lay_out(
        &work.path,
        nodes,
        &default.0,
        entries,
        children,
        settings.show,
        diagnostics,
        budget,
    )?;
    Ok(Some(Menu {
        name,
        path: work.path.clone(),
        label,
        icon,
        directory,
        items,
        sources: work.sources.clone(),
        no_display,
    }))
}

fn entry(found: &PoolEntry, locales: &Locales) -> Entry {
    work::count(1 + found.id.len());
    Entry {
        id: Arc::clone(&found.id),
        label: found
            .entry
            .name(locales)
            .map_or_else(|| Arc::clone(&found.id), Arc::from),
        icon: found.entry.icon(locales).map(Arc::from),
        desktop: Arc::clone(&found.entry),
        app_dir: Arc::clone(&found.app_dir),
        legacy: found.legacy,
    }
}

/// The `.directory` file of `work`: the last `<Directory>` first, each
/// looked up in the directory directories, most important first; the
/// first found wins.
fn directory(work: &Work, provider: &mut impl Provider) -> Result<Option<Directory>, MenuError> {
    for name in work.directories.iter().rev() {
        work::count(1 + name.len());
        // Only `.directory` files are loaded (legacy hierarchies name
        // theirs `.directory`).
        if !relative_path(name) || !name.ends_with(".directory") {
            continue;
        }
        for dir in work.directory_dirs.iter() {
            if let Some(entry) = provider.directory(dir, name)? {
                return Ok(Some(Directory {
                    id: name.clone(),
                    entry,
                }));
            }
        }
    }
    Ok(None)
}

/// Whether `name` is a relative path staying below its directory.
pub(crate) fn relative_path(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && name
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// The effective attributes: the built-in defaults, then the default
/// layout's, then the reference's own.
fn attributes(default: &LayoutAttributes, own: &LayoutAttributes) -> Effective {
    Effective {
        show_empty: own.show_empty.or(default.show_empty).unwrap_or(false),
        inline: own.inline.or(default.inline).unwrap_or(false),
        inline_limit: own.inline_limit.or(default.inline_limit).unwrap_or(4),
        inline_header: own.inline_header.or(default.inline_header).unwrap_or(true),
        inline_alias: own.inline_alias.or(default.inline_alias).unwrap_or(false),
    }
}

/// A menu's layout attributes, its own over its default layout's.
#[allow(
    clippy::struct_excessive_bools,
    reason = "the spec's attributes, one each"
)]
struct Effective {
    show_empty: bool,
    inline: bool,
    inline_limit: usize,
    inline_header: bool,
    inline_alias: bool,
}

/// What a layout places: an entry or a submenu.
enum Candidate {
    Entry(Entry),
    Menu(Menu),
}

impl Candidate {
    fn label(&self) -> &str {
        match self {
            Self::Entry(entry) => &entry.label,
            Self::Menu(menu) => &menu.label,
        }
    }

    /// Sorting: the caption case-folded, then as written, submenus before
    /// entries of the same caption, then the ID or name — a total order.
    fn key(&self) -> (String, String, u8, String) {
        let label = self.label();
        work::count(2 * label.len());
        let (kind, tie) = match self {
            Self::Menu(menu) => (0, menu.name.clone()),
            Self::Entry(entry) => (1, entry.id.to_string()),
        };
        (label.to_lowercase(), label.to_owned(), kind, tie)
    }
}

/// What a `<Merge>` of `kind` places: the entries and submenus the layout
/// does not name, taken out, in sort order. Each candidate looked at is a
/// step from `budget`, the ones the layout names (left in place) too.
fn merged(
    kind: MergeKind,
    entries: &mut BTreeMap<Arc<str>, Entry>,
    menus: &mut BTreeMap<String, Menu>,
    mentioned_files: &HashSet<&str>,
    mentioned_menus: &HashSet<&str>,
    budget: &mut usize,
) -> Result<Vec<Candidate>, MenuError> {
    let mut candidates = Vec::new();
    if matches!(kind, MergeKind::Files | MergeKind::All) {
        spend(budget, entries.len())?;
        let ids: Vec<Arc<str>> = entries
            .keys()
            .filter(|id| !mentioned_files.contains(&***id))
            .cloned()
            .collect();
        for id in ids {
            if let Some(entry) = entries.remove(&id) {
                candidates.push(Candidate::Entry(entry));
            }
        }
    }
    if matches!(kind, MergeKind::Menus | MergeKind::All) {
        spend(budget, menus.len())?;
        let names: Vec<String> = menus
            .keys()
            .filter(|name| !mentioned_menus.contains(name.as_str()))
            .cloned()
            .collect();
        for name in names {
            if let Some(menu) = menus.remove(&name) {
                candidates.push(Candidate::Menu(menu));
            }
        }
    }
    let mut keyed: Vec<_> = candidates.into_iter().map(|c| (c.key(), c)).collect();
    keyed.sort_by(|(a, _), (b, _)| {
        work::count(1 + a.0.len().min(b.0.len()));
        a.cmp(b)
    });
    Ok(keyed.into_iter().map(|(_, candidate)| candidate).collect())
}

/// The items of a menu as its layout orders them.
#[allow(clippy::too_many_arguments)]
fn lay_out(
    path: &str,
    nodes: &[LayoutNode],
    default: &LayoutAttributes,
    entries: Vec<Entry>,
    menus: Vec<Menu>,
    show: ShowPolicy,
    diagnostics: &mut Vec<Diagnostic>,
    budget: &mut usize,
) -> Result<Vec<MenuItem>, MenuError> {
    let mut entries: BTreeMap<Arc<str>, Entry> = entries
        .into_iter()
        .map(|entry| (Arc::clone(&entry.id), entry))
        .collect();
    let mut menus: BTreeMap<String, Menu> = menus
        .into_iter()
        .map(|menu| (menu.name.clone(), menu))
        .collect();
    let mut mentioned_files: HashSet<&str> = HashSet::new();
    let mut mentioned_menus: HashSet<&str> = HashSet::new();
    let (mut merges_files, mut merges_menus) = (false, false);
    for node in nodes {
        work::count(1);
        match node {
            LayoutNode::Filename(id) => {
                mentioned_files.insert(id);
            }
            LayoutNode::Menuname(name, _) => {
                mentioned_menus.insert(name);
            }
            LayoutNode::Merge(kind) => {
                merges_files |= matches!(kind, MergeKind::Files | MergeKind::All);
                merges_menus |= matches!(kind, MergeKind::Menus | MergeKind::All);
            }
            LayoutNode::Separator => {}
        }
    }
    let mut out = Vec::new();
    for node in nodes {
        match node {
            LayoutNode::Filename(id) => {
                if let Some(entry) = entries.remove(id.as_str()) {
                    out.push(MenuItem::Entry(entry));
                }
            }
            LayoutNode::Menuname(name, own) => {
                if let Some(menu) = menus.remove(name) {
                    place(menu, &attributes(default, own), show, &mut out);
                }
            }
            LayoutNode::Separator => out.push(MenuItem::Separator),
            LayoutNode::Merge(kind) => {
                let merged = merged(
                    *kind,
                    &mut entries,
                    &mut menus,
                    &mentioned_files,
                    &mentioned_menus,
                    budget,
                )?;
                let effective = attributes(default, &LayoutAttributes::default());
                for candidate in merged {
                    match candidate {
                        Candidate::Entry(entry) => out.push(MenuItem::Entry(entry)),
                        Candidate::Menu(menu) => place(menu, &effective, show, &mut out),
                    }
                }
            }
        }
    }
    let unplaced_entries = entries
        .keys()
        .filter(|id| !mentioned_files.contains(&***id))
        .count();
    let unplaced_menus = menus
        .keys()
        .filter(|name| !mentioned_menus.contains(name.as_str()))
        .count();
    if (!merges_files && unplaced_entries > 0) || (!merges_menus && unplaced_menus > 0) {
        diagnostics.push(Diagnostic::new(
            None,
            None,
            format!(
                "the layout of menu {} has no <Merge> for some of its items; {} entries and {} \
                 submenus it does not name are not shown",
                if path.is_empty() { "(root)" } else { path },
                unplaced_entries,
                unplaced_menus
            ),
        ));
    }
    Ok(tidy_separators(out))
}

/// Places a submenu: as itself, inlined (with its caption as a header), as
/// an alias of its single item, or not at all when empty and empty menus
/// are not shown.
fn place(menu: Menu, attributes: &Effective, show: ShowPolicy, out: &mut Vec<MenuItem>) {
    let count = menu
        .items
        .iter()
        .filter(|item| !matches!(item, MenuItem::Separator))
        .count();
    if count == 0 {
        if attributes.show_empty || show.empty {
            out.push(MenuItem::Menu(menu));
        }
        return;
    }
    if !attributes.inline {
        out.push(MenuItem::Menu(menu));
        return;
    }
    if attributes.inline_alias && count == 1 {
        let label = menu.label.clone();
        let menu_path = menu.path.clone();
        let only = menu
            .items
            .into_iter()
            .find(|item| !matches!(item, MenuItem::Separator));
        let item = match only {
            Some(MenuItem::Entry(entry)) => Aliased::Entry(entry),
            Some(MenuItem::Menu(inner)) => Aliased::Menu(inner),
            Some(MenuItem::Alias(inner)) => inner.item,
            // A header always comes with items, and the count is one.
            Some(MenuItem::Header(_) | MenuItem::Separator) | None => return,
        };
        out.push(MenuItem::Alias(Alias {
            label,
            menu_path,
            item,
        }));
        return;
    }
    if attributes.inline_limit == 0 || count <= attributes.inline_limit {
        if attributes.inline_header {
            out.push(MenuItem::Header(Header {
                label: menu.label.clone(),
                menu_path: menu.path.clone(),
            }));
        }
        out.extend(menu.items);
        return;
    }
    out.push(MenuItem::Menu(menu));
}

/// Drops separators at the start, at the end and right after another.
fn tidy_separators(items: Vec<MenuItem>) -> Vec<MenuItem> {
    let mut out: Vec<MenuItem> = Vec::with_capacity(items.len());
    for item in items {
        work::count(1);
        if matches!(item, MenuItem::Separator)
            && matches!(out.last(), None | Some(MenuItem::Separator))
        {
            continue;
        }
        out.push(item);
    }
    if matches!(out.last(), Some(MenuItem::Separator)) {
        out.pop();
    }
    out
}
