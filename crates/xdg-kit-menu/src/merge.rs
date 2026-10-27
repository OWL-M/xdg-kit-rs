//! The spec's merging steps after the files are loaded ("Merging"):
//! consolidating menus of the same name, expanding the default
//! directories, dropping duplicate directories and `<Directory>` items,
//! then running `<Move>`s deepest menus first, consolidating again where
//! they changed anything.

use std::path::PathBuf;

use crate::model::{Item, MenuNode};
use crate::{Diagnostic, MenuError, work};

/// The directories `<DefaultAppDirs>` and `<DefaultDirectoryDirs>` stand
/// for, most important first.
pub(crate) struct Defaults {
    pub app_dirs: Vec<PathBuf>,
    pub directory_dirs: Vec<PathBuf>,
}

/// Consolidates `node` and every menu below it; returns how many items it
/// went through (moves charge it to their budget).
pub(crate) fn consolidate(node: &mut MenuNode, defaults: &Defaults) -> usize {
    let before = node.items.len();
    merge_same_named(node);
    expand_defaults(node, defaults);
    drop_duplicates(node);
    let mut visited = before + node.items.len();
    for item in &mut node.items {
        if let Item::Menu(child) = item {
            visited += consolidate(child, defaults);
        }
    }
    visited
}

/// Merges each group of child menus of one name into the last of them:
/// their items concatenated in order.
fn merge_same_named(node: &mut MenuNode) {
    // The last position of each name.
    let mut last: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (at, item) in node.items.iter().enumerate() {
        if let Item::Menu(child) = item
            && let Some(name) = &child.name
        {
            work::count(1 + name.len());
            last.insert(name.clone(), at);
        }
    }
    let mut gathered: std::collections::HashMap<String, (Vec<Item>, Vec<PathBuf>)> =
        std::collections::HashMap::new();
    let items = std::mem::take(&mut node.items);
    let mut kept = Vec::with_capacity(items.len());
    for (at, item) in items.into_iter().enumerate() {
        match item {
            Item::Menu(mut child) if child.name.is_some() => {
                let name = child.name.clone().unwrap_or_default();
                work::count(1 + name.len());
                let (gathered_items, gathered_sources) = gathered.entry(name.clone()).or_default();
                gathered_items.append(&mut child.items);
                gathered_sources.append(&mut child.sources);
                if last.get(&name) == Some(&at) {
                    let (items, sources) = gathered.remove(&name).unwrap_or_default();
                    child.items = items;
                    child.sources = dedup_sources(sources);
                    kept.push(Item::Menu(child));
                }
            }
            other => kept.push(other),
        }
    }
    node.items = kept;
}

/// Each source once, in order of first appearance.
fn dedup_sources(sources: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    sources
        .into_iter()
        .filter(|source| {
            work::count(1 + source.as_os_str().len());
            seen.insert(source.clone())
        })
        .collect()
}

/// `<DefaultAppDirs>` and `<DefaultDirectoryDirs>` as the directories they
/// stand for, the more important later (later ones take precedence).
fn expand_defaults(node: &mut MenuNode, defaults: &Defaults) {
    let items = std::mem::take(&mut node.items);
    for item in items {
        match item {
            Item::DefaultAppDirs => node
                .items
                .extend(defaults.app_dirs.iter().rev().cloned().map(Item::AppDir)),
            Item::DefaultDirectoryDirs => node.items.extend(
                defaults
                    .directory_dirs
                    .iter()
                    .rev()
                    .cloned()
                    .map(Item::DirectoryDir),
            ),
            other => node.items.push(other),
        }
    }
}

/// Of duplicate `<AppDir>`, `<DirectoryDir>` and `<Directory>` items (and
/// legacy application directories), only the last stays, where it is.
fn drop_duplicates(node: &mut MenuNode) {
    #[derive(PartialEq, Eq, Hash)]
    enum Key {
        AppDir(PathBuf),
        Legacy(PathBuf, String),
        DirectoryDir(PathBuf),
        Directory(String),
    }
    let key = |item: &Item| match item {
        Item::AppDir(path) => Some(Key::AppDir(path.clone())),
        Item::LegacyAppDir { path, prefix } => Some(Key::Legacy(path.clone(), prefix.clone())),
        Item::DirectoryDir(path) => Some(Key::DirectoryDir(path.clone())),
        Item::Directory(name) => Some(Key::Directory(name.clone())),
        _ => None,
    };
    let mut last = std::collections::HashMap::new();
    for (at, item) in node.items.iter().enumerate() {
        if let Some(key) = key(item) {
            work::count(1);
            last.insert(key, at);
        }
    }
    let items = std::mem::take(&mut node.items);
    node.items = items
        .into_iter()
        .enumerate()
        .filter(|(at, item)| key(item).is_none_or(|key| last.get(&key) == Some(at)))
        .map(|(_, item)| item)
        .collect();
}

/// What moves may still do: menus nest at most `max_depth` deep (the root
/// at 0), at most `menus_left` more menus are made, and at most
/// `steps_left` steps of work are done — each item a path search passes,
/// each menu a depth check visits, each item moved or consolidated again
/// (the matching budget, whose rest resolution spends).
pub(crate) struct MoveLimits {
    pub max_depth: usize,
    pub menus_left: usize,
    pub steps_left: usize,
}

/// Runs the `<Move>`s of `node`, `depth` deep, and of every menu below it,
/// deepest first; returns how many moves changed something.
///
/// # Errors
///
/// When a move would nest menus deeper, or make more of them, than
/// `limits` allow; nothing is moved then.
pub(crate) fn run_moves(
    node: &mut MenuNode,
    defaults: &Defaults,
    diagnostics: &mut Vec<Diagnostic>,
    path: &str,
    depth: usize,
    limits: &mut MoveLimits,
) -> Result<usize, MenuError> {
    let mut changed = 0;
    for item in &mut node.items {
        if let Item::Menu(child) = item
            && let Some(name) = child.name.clone()
        {
            let child_path = join(path, &name);
            changed += run_moves(child, defaults, diagnostics, &child_path, depth + 1, limits)?;
        }
    }
    let moves: Vec<(Vec<String>, Vec<String>)> = node
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Move(pairs) => Some(pairs.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    node.items.retain(|item| !matches!(item, Item::Move(_)));
    let mut here = 0;
    for (old, new) in moves {
        work::count(1 + old.len() + new.len());
        take(&mut limits.steps_left, 1 + old.len() + new.len())?;
        if run_move(node, &old, &new, diagnostics, path, depth, limits)? {
            here += 1;
        }
    }
    if here > 0 {
        // Moves may have made menus of one name meet: consolidate again.
        let visited = consolidate(node, defaults);
        take(&mut limits.steps_left, visited)?;
    }
    Ok(changed + here)
}

/// Takes `steps` from the budget of moves, or fails at its end.
fn take(steps_left: &mut usize, steps: usize) -> Result<(), MenuError> {
    *steps_left = steps_left
        .checked_sub(steps)
        .ok_or_else(|| MenuError::Limit {
            message: "more matching work than the limit allows (see Limits::max_matches)"
                .to_owned(),
        })?;
    Ok(())
}

/// Moves the menu at `old` (relative to `node`) to `new`: renamed into
/// place when `new` does not exist (its missing parents made), merged into
/// it (its items first) when it does. Nothing when `old` does not exist.
fn run_move(
    node: &mut MenuNode,
    old: &[String],
    new: &[String],
    diagnostics: &mut Vec<Diagnostic>,
    path: &str,
    depth: usize,
    limits: &mut MoveLimits,
) -> Result<bool, MenuError> {
    if old == new {
        return Ok(false);
    }
    if new.starts_with(old) {
        diagnostics.push(Diagnostic::new(
            None,
            None,
            format!(
                "menu {}: moving {} into itself is refused",
                shown_path(path),
                crate::error::shown(&old.join("/"))
            ),
        ));
        return Ok(false);
    }
    // Checked before anything changes: where the moved menu's deepest
    // descendant lands, and how many menus the destination path makes.
    let steps = &mut limits.steps_left;
    let Some(moved) = find(node, old, steps)? else {
        return Ok(false);
    };
    let lands = depth + new.len() + height(moved, steps)?;
    if lands > limits.max_depth {
        return Err(MenuError::Limit {
            message: format!(
                "menu {}: a <Move> would nest menus {lands} deep, more than {}",
                shown_path(path),
                limits.max_depth
            ),
        });
    }
    // The menus made are the destination's missing parents: the moved
    // menu itself becomes the last one.
    let made = missing(node, &new[..new.len() - 1], &mut limits.steps_left)?;
    limits.menus_left = limits
        .menus_left
        .checked_sub(made)
        .ok_or_else(|| MenuError::Limit {
            message: format!(
                "menu {}: <Move>s would make more menus than the limit allows",
                shown_path(path)
            ),
        })?;
    let steps = &mut limits.steps_left;
    let Some(origin) = detach(node, old, steps)? else {
        return Ok(false);
    };
    let mut origin = *origin;
    if let Some(destination) = find_mut(node, new, steps)? {
        take(
            steps,
            origin.items.len()
                + destination.items.len()
                + origin.sources.len()
                + destination.sources.len(),
        )?;
        let mut items = std::mem::take(&mut origin.items);
        items.append(&mut destination.items);
        destination.items = items;
        let mut sources = origin.sources;
        sources.append(&mut destination.sources);
        destination.sources = dedup_sources(sources);
    } else {
        let (last, parents) = new.split_last().expect("menu paths are not empty");
        let parent = make_path(node, parents, steps)?;
        origin.name = Some(last.clone());
        parent.items.push(Item::Menu(Box::new(origin)));
    }
    Ok(true)
}

/// The menu at `path` below `node`.
fn find<'a>(
    node: &'a MenuNode,
    path: &[String],
    steps: &mut usize,
) -> Result<Option<&'a MenuNode>, MenuError> {
    let mut at = node;
    for name in path {
        let Some(index) = child_at(at, name, steps)? else {
            return Ok(None);
        };
        let Item::Menu(child) = &at.items[index] else {
            return Ok(None);
        };
        at = child;
    }
    Ok(Some(at))
}

/// How many of the menus on `path` below `node` do not exist yet.
fn missing(node: &MenuNode, path: &[String], steps: &mut usize) -> Result<usize, MenuError> {
    let mut at = node;
    for (found, name) in path.iter().enumerate() {
        let Some(index) = child_at(at, name, steps)? else {
            return Ok(path.len() - found);
        };
        let Item::Menu(child) = &at.items[index] else {
            return Ok(path.len() - found);
        };
        at = child;
    }
    Ok(0)
}

/// How many levels of menus `node` holds below it (0 for none), each item
/// looked at a step from `steps_left`. The tree is never deeper than the
/// limit, so the recursion is bounded.
fn height(node: &MenuNode, steps_left: &mut usize) -> Result<usize, MenuError> {
    take(steps_left, 1 + node.items.len())?;
    let mut deepest = 0;
    for item in &node.items {
        work::count(1);
        if let Item::Menu(child) = item {
            deepest = deepest.max(1 + height(child, steps_left)?);
        }
    }
    Ok(deepest)
}

/// The position of the named child menu `name` of `node` (the last one, as
/// consolidation keeps). Each item passed is a step, found or not (and
/// covers removing at that position).
fn child_at(node: &MenuNode, name: &str, steps: &mut usize) -> Result<Option<usize>, MenuError> {
    take(steps, 1 + node.items.len())?;
    Ok(node.items.iter().rposition(|item| {
        work::count(1);
        matches!(item, Item::Menu(child) if child.name.as_deref() == Some(name))
    }))
}

fn find_mut<'a>(
    node: &'a mut MenuNode,
    path: &[String],
    steps: &mut usize,
) -> Result<Option<&'a mut MenuNode>, MenuError> {
    let mut at = node;
    for name in path {
        let Some(index) = child_at(at, name, steps)? else {
            return Ok(None);
        };
        let Item::Menu(child) = &mut at.items[index] else {
            return Ok(None);
        };
        at = child;
    }
    Ok(Some(at))
}

/// Takes the menu at `path` out of the tree.
fn detach(
    node: &mut MenuNode,
    path: &[String],
    steps: &mut usize,
) -> Result<Option<Box<MenuNode>>, MenuError> {
    let Some((last, parents)) = path.split_last() else {
        return Ok(None);
    };
    let Some(parent) = find_mut(node, parents, steps)? else {
        return Ok(None);
    };
    let Some(index) = child_at(parent, last, steps)? else {
        return Ok(None);
    };
    Ok(match parent.items.remove(index) {
        Item::Menu(menu) => Some(menu),
        _ => None,
    })
}

/// The menu at `path`, made (empty) where missing.
fn make_path<'a>(
    node: &'a mut MenuNode,
    path: &[String],
    steps: &mut usize,
) -> Result<&'a mut MenuNode, MenuError> {
    let mut at = node;
    for name in path {
        let index = if let Some(index) = child_at(at, name, steps)? {
            index
        } else {
            at.items.push(Item::Menu(Box::new(MenuNode {
                name: Some(name.clone()),
                items: Vec::new(),
                sources: Vec::new(),
            })));
            at.items.len() - 1
        };
        let Item::Menu(child) = &mut at.items[index] else {
            unreachable!("the index was found or made as a menu");
        };
        at = child;
    }
    Ok(at)
}

pub(crate) fn join(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_owned()
    } else {
        format!("{path}/{name}")
    }
}

fn shown_path(path: &str) -> String {
    if path.is_empty() {
        "(root)".to_owned()
    } else {
        crate::error::shown(path)
    }
}
