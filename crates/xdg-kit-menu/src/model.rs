//! The menu format as typed nodes, read from the element tree: the
//! spec's elements, each path made absolute against the directory of the
//! file it was written in, so that merging only moves items around.

use std::path::{Path, PathBuf};

use crate::xml::{Child, Element};
use crate::{Diagnostic, work};

/// The deepest nesting ever handled, whatever the limits say: see
/// [`Limits::MAX_DEPTH`](crate::Limits::MAX_DEPTH).
pub(crate) const HARD_DEPTH: usize = crate::Limits::MAX_DEPTH;

/// One `<Menu>`: its name and its other children in document order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MenuNode {
    /// The `<Name>`; `None` when it is missing or unusable (holds a `/`):
    /// such a menu is never shown, moved or laid out, but its rules still
    /// allocate entries, so dropping it cannot free them for an
    /// `<OnlyUnallocated>` menu.
    pub name: Option<String>,
    pub items: Vec<Item>,
    /// The menu files whose `<Menu>` elements make up this menu.
    pub sources: Vec<PathBuf>,
}

/// A child of `<Menu>` other than `<Name>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Item {
    Menu(Box<MenuNode>),
    AppDir(PathBuf),
    DefaultAppDirs,
    /// The applications of a legacy hierarchy: IDs are the prefix and the
    /// file name, and every entry gets the `Legacy` category.
    LegacyAppDir {
        path: PathBuf,
        prefix: String,
    },
    DirectoryDir(PathBuf),
    DefaultDirectoryDirs,
    Directory(String),
    LegacyDir {
        path: PathBuf,
        prefix: String,
    },
    KdeLegacyDirs,
    MergeFile(MergeTarget),
    MergeDir(PathBuf),
    DefaultMergeDirs,
    OnlyUnallocated(bool),
    Deleted(bool),
    Include(Vec<Rule>),
    Exclude(Vec<Rule>),
    /// A `<Move>`'s `<Old>`/`<New>` pairs, run in order.
    Move(Vec<(Vec<String>, Vec<String>)>),
    Layout(Vec<LayoutNode>),
    DefaultLayout(LayoutAttributes, Vec<LayoutNode>),
}

/// What a `<MergeFile>` merges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MergeTarget {
    Path(PathBuf),
    /// `type="parent"`: the same relative file in the less important
    /// configuration directories, found from the containing file.
    Parent {
        containing: PathBuf,
    },
}

/// A matching rule. [`Rule::Unknown`] is an element the format does not
/// define (or a malformed one): it neither matches nor fails to match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Rule {
    Filename(String),
    Category(String),
    All,
    And(Vec<Rule>),
    Or(Vec<Rule>),
    Not(Vec<Rule>),
    Unknown,
}

/// One element of a `<Layout>` or `<DefaultLayout>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LayoutNode {
    Filename(String),
    Menuname(String, LayoutAttributes),
    Separator,
    Merge(MergeKind),
}

/// What a layout's `<Merge>` inserts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MergeKind {
    Menus,
    Files,
    All,
}

/// The presentation attributes of `<DefaultLayout>` and `<Menuname>`;
/// `None` takes the value in effect.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LayoutAttributes {
    pub show_empty: Option<bool>,
    pub inline: Option<bool>,
    pub inline_limit: Option<usize>,
    pub inline_header: Option<bool>,
    pub inline_alias: Option<bool>,
}

/// Where converting reports to.
pub(crate) struct Reader<'a> {
    pub file: &'a Path,
    /// The directory relative paths are resolved against.
    pub base: &'a Path,
    pub diagnostics: &'a mut Vec<Diagnostic>,
}

impl Reader<'_> {
    fn note(&mut self, line: usize, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic::new(
            Some(self.file.to_owned()),
            Some(line),
            message,
        ));
    }

    /// `text` as an absolute path, relative ones against the file's
    /// directory; `None` (with a diagnostic) when empty.
    fn path(&mut self, element: &Element) -> Option<PathBuf> {
        let text = element.text();
        if text.is_empty() {
            self.note(
                element.line,
                format!("<{}> is empty; ignored", element.name),
            );
            return None;
        }
        let path = Path::new(&text);
        work::count(text.len() + self.base.as_os_str().len());
        Some(if path.is_absolute() {
            path.to_owned()
        } else {
            self.base.join(path)
        })
    }
}

/// The `<Menu>` `element` is, read for its file. `None` when the element is
/// no `<Menu>`.
pub(crate) fn menu(element: &Element, reader: &mut Reader<'_>) -> Option<MenuNode> {
    (element.name == "Menu").then(|| read_menu(element, reader, 0))
}

fn read_menu(element: &Element, reader: &mut Reader<'_>, depth: usize) -> MenuNode {
    let mut node = MenuNode {
        name: None,
        items: Vec::new(),
        sources: vec![reader.file.to_owned()],
    };
    let mut named = false;
    for child in &element.children {
        let child = match child {
            Child::Element(child) => child,
            Child::Text(text) => {
                if !text.trim().is_empty() {
                    reader.note(element.line, "text directly in <Menu>; ignored");
                }
                continue;
            }
        };
        work::count(1);
        if child.name == "Name" {
            if named {
                reader.note(child.line, "a second <Name>; ignored");
                continue;
            }
            named = true;
            let name = child.text();
            if name.is_empty() || name.contains('/') {
                reader.note(
                    child.line,
                    format!(
                        "the menu name \"{}\" is empty or holds a /: the menu is not shown",
                        crate::error::shown(&name)
                    ),
                );
            } else {
                node.name = Some(name);
            }
            continue;
        }
        if let Some(item) = read_item(child, reader, depth) {
            node.items.push(item);
        }
    }
    if !named {
        reader.note(
            element.line,
            "a <Menu> without <Name>: the menu is not shown",
        );
    }
    node
}

/// One child of `<Menu>`, `None` when it is ignored.
fn read_item(child: &Element, reader: &mut Reader<'_>, depth: usize) -> Option<Item> {
    let flag = |reader: &mut Reader<'_>, item: Item| {
        if !child.text().is_empty() {
            reader.note(
                child.line,
                format!("<{}> holds content; it is ignored", child.name),
            );
        }
        Some(item)
    };
    match child.name.as_str() {
        "Menu" => Some(Item::Menu(Box::new(read_menu(child, reader, depth + 1)))),
        "AppDir" => reader.path(child).map(Item::AppDir),
        "DefaultAppDirs" => flag(reader, Item::DefaultAppDirs),
        "DirectoryDir" => reader.path(child).map(Item::DirectoryDir),
        "DefaultDirectoryDirs" => flag(reader, Item::DefaultDirectoryDirs),
        "Directory" => {
            let text = child.text();
            if text.is_empty() {
                reader.note(child.line, "<Directory> is empty; ignored");
                None
            } else if !text.ends_with(".directory") {
                // The spec: only files ending in `.directory` are loaded.
                reader.note(
                    child.line,
                    format!(
                        "<Directory>{}</Directory> does not name a .directory file; ignored",
                        crate::error::shown(&text)
                    ),
                );
                None
            } else {
                Some(Item::Directory(text))
            }
        }
        "LegacyDir" => {
            let prefix = child.attribute("prefix").unwrap_or_default().to_owned();
            if prefix.contains('/') {
                reader.note(
                    child.line,
                    "a <LegacyDir> prefix holds a /; the legacy directory is not loaded",
                );
                return None;
            }
            reader
                .path(child)
                .map(|path| Item::LegacyDir { path, prefix })
        }
        "KDELegacyDirs" => flag(reader, Item::KdeLegacyDirs),
        "MergeFile" => merge_file(child, reader),
        "MergeDir" => reader.path(child).map(Item::MergeDir),
        "DefaultMergeDirs" => flag(reader, Item::DefaultMergeDirs),
        "OnlyUnallocated" => flag(reader, Item::OnlyUnallocated(true)),
        "NotOnlyUnallocated" => flag(reader, Item::OnlyUnallocated(false)),
        "Deleted" => flag(reader, Item::Deleted(true)),
        "NotDeleted" => flag(reader, Item::Deleted(false)),
        "Include" => Some(Item::Include(rules(child, reader, depth))),
        "Exclude" => Some(Item::Exclude(rules(child, reader, depth))),
        "Move" => moves(child, reader),
        "Layout" => Some(Item::Layout(layout(child, reader))),
        "DefaultLayout" => {
            let attributes = layout_attributes(child, reader, true);
            Some(Item::DefaultLayout(attributes, layout(child, reader)))
        }
        other => {
            reader.note(
                child.line,
                format!(
                    "<{}> is not a menu element; ignored",
                    crate::error::shown(other)
                ),
            );
            None
        }
    }
}

fn merge_file(child: &Element, reader: &mut Reader<'_>) -> Option<Item> {
    match child.attribute("type") {
        Some("parent") => Some(Item::MergeFile(MergeTarget::Parent {
            containing: reader.file.to_owned(),
        })),
        None | Some("path") => reader
            .path(child)
            .map(|path| Item::MergeFile(MergeTarget::Path(path))),
        Some(other) => {
            // Older implementations ignore `type` and merge the named file:
            // doing the same keeps what the file merges.
            reader.note(
                child.line,
                format!(
                    "<MergeFile type=\"{}\"> is not a known type; the file named is merged",
                    crate::error::shown(other)
                ),
            );
            reader
                .path(child)
                .map(|path| Item::MergeFile(MergeTarget::Path(path)))
        }
    }
}

/// The rules of an `<Include>`, `<Exclude>`, `<And>`, `<Or>` or `<Not>`.
fn rules(element: &Element, reader: &mut Reader<'_>, depth: usize) -> Vec<Rule> {
    let mut rules = Vec::new();
    for child in &element.children {
        match child {
            Child::Text(text) if text.trim().is_empty() => {}
            Child::Text(_) => {
                reader.note(
                    element.line,
                    format!(
                        "text in <{}>; it is no rule and matches nothing either way",
                        element.name
                    ),
                );
                rules.push(Rule::Unknown);
            }
            Child::Element(child) => rules.push(rule(child, reader, depth)),
        }
    }
    rules
}

fn rule(child: &Element, reader: &mut Reader<'_>, depth: usize) -> Rule {
    work::count(1);
    match child.name.as_str() {
        "Filename" | "Category" | "All" => leaf(child, reader),
        "And" => Rule::And(rules(child, reader, depth + 1)),
        "Or" => Rule::Or(rules(child, reader, depth + 1)),
        "Not" => Rule::Not(rules(child, reader, depth + 1)),
        other => {
            reader.note(
                child.line,
                format!(
                    "<{}> is not a matching rule: it neither includes nor spares anything",
                    crate::error::shown(other)
                ),
            );
            Rule::Unknown
        }
    }
}

/// A `<Filename>`, `<Category>` or `<All>`: text alone, a name in the
/// first two, nothing in `<All>`. Anything else in one is a malformed rule:
/// unknown, so it neither includes nor spares (its negation included).
fn leaf(child: &Element, reader: &mut Reader<'_>) -> Rule {
    let text = child.text();
    let all = child.name == "All";
    let problem = if child.elements().next().is_some() {
        Some("holds elements")
    } else if all && !text.is_empty() {
        Some("holds text")
    } else if !all && text.is_empty() {
        Some("is empty")
    } else {
        None
    };
    if let Some(problem) = problem {
        reader.note(
            child.line,
            format!(
                "<{}> {problem}: a malformed rule, it neither includes nor spares anything",
                child.name
            ),
        );
        return Rule::Unknown;
    }
    match child.name.as_str() {
        "Filename" => Rule::Filename(text),
        "Category" => Rule::Category(text),
        _ => Rule::All,
    }
}

/// The `<Old>`/`<New>` pairs of a `<Move>`; `None` when it has none
/// usable.
fn moves(child: &Element, reader: &mut Reader<'_>) -> Option<Item> {
    let mut pending: Option<Option<Vec<String>>> = None;
    let mut pairs = Vec::new();
    for element in child.elements() {
        match element.name.as_str() {
            "Old" => {
                if pending.is_some() {
                    reader.note(element.line, "an <Old> without its <New>; ignored");
                }
                pending = Some(menu_path(element, reader));
            }
            "New" => {
                let new = menu_path(element, reader);
                match pending.take() {
                    Some(Some(old)) => {
                        if let Some(new) = new {
                            pairs.push((old, new));
                        }
                    }
                    // The <Old> was unusable: reported already.
                    Some(None) => {}
                    None => reader.note(element.line, "a <New> without an <Old>; ignored"),
                }
            }
            other => reader.note(
                element.line,
                format!("<{}> in <Move>; ignored", crate::error::shown(other)),
            ),
        }
    }
    if pending.is_some() {
        reader.note(child.line, "an <Old> without its <New>; ignored");
    }
    // One <Move> is a set of renames: a later pair for the same <Old>
    // replaces an earlier one, as everywhere in the format the last word
    // counts (the spec's suite, test `Move`, expects exactly this).
    let mut last: std::collections::HashMap<&[String], usize> = std::collections::HashMap::new();
    for (at, (old, _)) in pairs.iter().enumerate() {
        // Hashed.
        work::count(1 + old.iter().map(String::len).sum::<usize>());
        if last.insert(old.as_slice(), at).is_some() {
            reader.note(
                child.line,
                format!(
                    "a later pair moves {} again; the earlier one is dropped",
                    crate::error::shown(&old.join("/"))
                ),
            );
        }
    }
    let keep: Vec<bool> = pairs
        .iter()
        .enumerate()
        .map(|(at, (old, _))| last.get(old.as_slice()) == Some(&at))
        .collect();
    let kept: Vec<(Vec<String>, Vec<String>)> = pairs
        .into_iter()
        .zip(keep)
        .filter_map(|(pair, keep)| keep.then_some(pair))
        .collect();
    (!kept.is_empty()).then_some(Item::Move(kept))
}

/// A menu path: names separated by `/`, none empty, none `.` or `..`.
fn menu_path(element: &Element, reader: &mut Reader<'_>) -> Option<Vec<String>> {
    let text = element.text();
    let parts: Vec<String> = text.split('/').map(str::to_owned).collect();
    if text.is_empty()
        || parts
            .iter()
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        reader.note(
            element.line,
            format!(
                "\"{}\" is not a menu path; the move is ignored",
                crate::error::shown(&text)
            ),
        );
        return None;
    }
    Some(parts)
}

fn layout(element: &Element, reader: &mut Reader<'_>) -> Vec<LayoutNode> {
    let mut nodes = Vec::new();
    for child in element.elements() {
        match child.name.as_str() {
            "Filename" => nodes.push(LayoutNode::Filename(child.text())),
            "Menuname" => {
                let name = child.text();
                let attributes = layout_attributes(child, reader, false);
                if name.is_empty() || name.contains('/') {
                    reader.note(
                        child.line,
                        "a <Menuname> that is empty or holds a /; ignored",
                    );
                } else {
                    nodes.push(LayoutNode::Menuname(name, attributes));
                }
            }
            "Separator" => nodes.push(LayoutNode::Separator),
            "Merge" => match child.attribute("type") {
                Some("menus") => nodes.push(LayoutNode::Merge(MergeKind::Menus)),
                Some("files") => nodes.push(LayoutNode::Merge(MergeKind::Files)),
                Some("all") => nodes.push(LayoutNode::Merge(MergeKind::All)),
                other => reader.note(
                    child.line,
                    format!(
                        "<Merge type=\"{}\"> is not a known type; ignored",
                        crate::error::shown(other.unwrap_or_default())
                    ),
                ),
            },
            other => reader.note(
                child.line,
                format!("<{}> in a layout; ignored", crate::error::shown(other)),
            ),
        }
    }
    nodes
}

fn layout_attributes(
    element: &Element,
    reader: &mut Reader<'_>,
    default_layout: bool,
) -> LayoutAttributes {
    let flag = |reader: &mut Reader<'_>, name: &str| match element.attribute(name) {
        None => None,
        Some("true") => Some(true),
        Some("false") => Some(false),
        Some(other) => {
            reader.note(
                element.line,
                format!(
                    "{name}=\"{}\" is not true or false; ignored",
                    crate::error::shown(other)
                ),
            );
            None
        }
    };
    let show_empty = if default_layout || element.attribute("show_empty").is_some() {
        flag(reader, "show_empty")
    } else {
        None
    };
    let inline = flag(reader, "inline");
    let inline_header = flag(reader, "inline_header");
    let inline_alias = flag(reader, "inline_alias");
    let inline_limit = match element.attribute("inline_limit") {
        None => None,
        Some(text) => {
            let limit = text.trim().parse::<usize>().ok();
            if limit.is_none() {
                reader.note(
                    element.line,
                    format!(
                        "inline_limit=\"{}\" is not a count; ignored",
                        crate::error::shown(text)
                    ),
                );
            }
            limit
        }
    };
    LayoutAttributes {
        show_empty,
        inline,
        inline_limit,
        inline_header,
        inline_alias,
    }
}
