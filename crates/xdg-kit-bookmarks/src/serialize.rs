//! Serializing the logical document: the original bytes, with each edit
//! spliced in as the smallest possible replacement. A document without
//! edits serializes byte-identically.

use std::fmt::Write as _;
use std::ops::Range;
use std::time::SystemTime;

use crate::edit::{AppState, Overlay, OwnedBookmark};
use crate::xml::{App, BOOKMARK_NS, MIME_NS, OWNER, Scope};
use crate::{BookmarkFile, work};

impl BookmarkFile {
    /// The serialized logical document: what a write would install. With
    /// no pending edits, exactly the bytes that were read.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        if self.edits.is_empty() {
            // Byte-identical, the XML declaration, comments and all.
            work::count(self.source.len());
            return self.source.clone();
        }
        let mut patches = Patches::default();
        for (&at, overlay) in &self.edits.overlays {
            if !self.edits.removed.contains(&at) {
                self.splice_overlay(&self.parsed.bookmarks[at], overlay, &mut patches);
            }
        }
        for &at in &self.edits.removed {
            let node = &self.parsed.bookmarks[at];
            patches.replace(node.lead.start..node.span.end, String::new());
        }
        let empty_scope = Scope::default();
        let root_scope: &Scope = self
            .parsed
            .root
            .as_ref()
            .map_or(&empty_scope, |root| &root.ns);
        let inserted: Vec<String> = self
            .edits
            .inserted
            .iter()
            .flatten()
            .map(|owned| Self::render_bookmark(owned, root_scope))
            .collect();
        // Edits exist only on editable documents, which have a root.
        if !inserted.is_empty()
            && let Some(root) = self.parsed.root.as_ref()
        {
            add_children(
                &mut patches,
                &self.source,
                &root.span,
                root.end_insert,
                root.self_closing,
                0,
                &inserted,
            );
        }
        patches.apply(&self.source)
    }

    /// The patches for one source bookmark's pending changes.
    fn splice_overlay(
        &self,
        node: &crate::xml::BookmarkNode,
        overlay: &Overlay,
        patches: &mut Patches,
    ) {
        for (new, old, name) in [
            (overlay.modified, &node.modified, "modified"),
            (overlay.visited, &node.visited, "visited"),
        ] {
            if let Some(time) = new {
                let text = timestamp(time);
                match old {
                    Some(stamp) => patches.replace(stamp.value.clone(), text),
                    None => insert_attr(patches, &self.source, node.span.start, name, &text),
                }
            }
        }
        let meta = node.info.as_ref().and_then(|info| info.metadata.as_ref());
        // What must live inside the freedesktop metadata block, rendered
        // for wherever that block (or its parents) must be created.
        let mut meta_children: Vec<String> = Vec::new();
        // The scope of wherever the metadata children will actually land:
        // an existing metadata block, a new one created inside an existing
        // <info> (which can redeclare prefixes), or a whole new chain in
        // the bookmark itself.
        let scope = meta.map_or_else(
            || node.info.as_ref().map_or(&node.ns, |info| &info.ns),
            |meta| &meta.ns,
        );
        let mime_elem = |patches: &mut Patches, children: &mut Vec<String>, text: &str| match meta
            .and_then(|meta| meta.mime.as_ref())
        {
            Some(found) => match &found.value {
                Some(attr) => patches.replace(attr.value.clone(), escape(text)),
                // The element exists without a type: the attribute goes
                // into its start tag, never beside the element.
                None => insert_attr(patches, &self.source, found.span.start, "type", text),
            },
            None => children.push(render_mime(scope, text)),
        };
        if let Some(mime) = &overlay.mime {
            mime_elem(patches, &mut meta_children, mime);
        }
        if !overlay.groups_added.is_empty() {
            // Rendered against the scope of wherever they actually land:
            // a container can redeclare prefixes.
            let render = |scope: &Scope| -> Vec<String> {
                overlay
                    .groups_added
                    .iter()
                    .map(|group| render_group(scope, group))
                    .collect()
            };
            match meta.and_then(|meta| meta.groups.as_ref()) {
                Some(found) => add_children(
                    patches,
                    &self.source,
                    &found.span,
                    found.end_insert,
                    found.self_closing,
                    4,
                    &render(&found.ns),
                ),
                None => meta_children.push(render_list(scope, "groups", 4, &render(scope))),
            }
        }
        if overlay.private_set && meta.is_none_or(|meta| meta.private.is_none()) {
            meta_children.push(render_private(scope));
        }
        let mut new_apps: Vec<&AppState> = Vec::new();
        for state in &overlay.apps {
            // The first stored record with the name is the application's
            // record; later ones are junk and stay as they are (reads
            // apply the same rule).
            let stored = meta
                .and_then(|meta| meta.applications.as_ref())
                .and_then(|apps| {
                    apps.list.iter().find(|app| {
                        work::count(1);
                        app.name.as_ref().map(|name| name.text.as_str())
                            == Some(state.name.as_str())
                    })
                });
            match stored {
                Some(app) => patch_app(patches, &self.source, app, state),
                None => new_apps.push(state),
            }
        }
        if !new_apps.is_empty() {
            // Rendered against the scope of wherever they actually land.
            let render = |scope: &Scope| -> Vec<String> {
                new_apps
                    .iter()
                    .map(|state| render_app(scope, state))
                    .collect()
            };
            match meta.and_then(|meta| meta.applications.as_ref()) {
                Some(found) => add_children(
                    patches,
                    &self.source,
                    &found.span,
                    found.end_insert,
                    found.self_closing,
                    4,
                    &render(&found.ns),
                ),
                None => {
                    meta_children.push(render_list(scope, "applications", 4, &render(scope)));
                }
            }
        }
        if !meta_children.is_empty() {
            self.place_meta_children(node, meta, &meta_children, patches);
        }
    }

    /// Puts `children` inside the bookmark's freedesktop metadata block,
    /// creating the block — and the `<info>` around it — where missing.
    fn place_meta_children(
        &self,
        node: &crate::xml::BookmarkNode,
        meta: Option<&crate::xml::Meta>,
        children: &[String],
        patches: &mut Patches,
    ) {
        let block = || {
            wrap(
                2,
                &format!("<metadata owner=\"{OWNER}\">"),
                "</metadata>",
                3,
                children,
            )
        };
        match (node.info.as_ref(), meta) {
            (Some(_), Some(meta)) => add_children(
                patches,
                &self.source,
                &meta.span,
                meta.end_insert,
                meta.self_closing,
                3,
                children,
            ),
            (Some(info), None) => {
                add_children(
                    patches,
                    &self.source,
                    &info.span,
                    info.end_insert,
                    info.self_closing,
                    2,
                    &[block()],
                );
            }
            (None, _) => {
                let info = wrap(1, "<info>", "</info>", 2, &[block()]);
                add_children(
                    patches,
                    &self.source,
                    &node.span,
                    node.end_insert,
                    node.self_closing,
                    1,
                    &[info],
                );
            }
        }
    }

    /// A whole inserted bookmark, rendered for the root's content (its
    /// own first line unindented; [`add_children`] places it).
    fn render_bookmark(owned: &OwnedBookmark, scope: &Scope) -> String {
        let mut meta_children = vec![render_mime(scope, &owned.mime)];
        if !owned.groups.is_empty() {
            let rendered: Vec<String> = owned
                .groups
                .iter()
                .map(|group| render_group(scope, group))
                .collect();
            meta_children.push(render_list(scope, "groups", 4, &rendered));
        }
        if !owned.apps.is_empty() {
            let rendered: Vec<String> = owned
                .apps
                .iter()
                .map(|state| render_app(scope, state))
                .collect();
            meta_children.push(render_list(scope, "applications", 4, &rendered));
        }
        if owned.private {
            meta_children.push(render_private(scope));
        }
        let metadata = wrap(
            2,
            &format!("<metadata owner=\"{OWNER}\">"),
            "</metadata>",
            3,
            &meta_children,
        );
        let info = wrap(1, "<info>", "</info>", 2, &[metadata]);
        let open = format!(
            "<bookmark href=\"{}\" added=\"{}\" modified=\"{}\" visited=\"{}\">",
            escape(&owned.href),
            timestamp(owned.added),
            timestamp(owned.modified),
            timestamp(owned.visited),
        );
        wrap(0, &open, "</bookmark>", 1, &[info])
    }
}

/// The ordered splices into the source.
#[derive(Debug, Default)]
struct Patches {
    replace: Vec<(Range<usize>, String)>,
}

impl Patches {
    fn replace(&mut self, range: Range<usize>, text: String) {
        work::count(text.len());
        self.replace.push((range, text));
    }

    fn insert(&mut self, at: usize, text: String) {
        self.replace(at..at, text);
    }

    fn apply(mut self, source: &[u8]) -> Vec<u8> {
        // Insertions at one position stay in the order they were made.
        self.replace
            .sort_by_key(|(range, _)| (range.start, range.end));
        let mut out = Vec::with_capacity(source.len());
        let mut at = 0;
        for (range, text) in self.replace {
            debug_assert!(range.start >= at, "overlapping patches");
            work::count(range.start - at);
            out.extend_from_slice(&source[at..range.start]);
            out.extend_from_slice(text.as_bytes());
            at = range.end;
        }
        work::count(source.len() - at);
        out.extend_from_slice(&source[at..]);
        out
    }
}

/// Inserts `children` (each rendered without leading indentation) into
/// the element at `span`, before its end tag — or expands a self-closing
/// element. `parent_depth` is the element's own depth; its children are
/// indented one level deeper.
fn add_children(
    patches: &mut Patches,
    source: &[u8],
    span: &Range<usize>,
    end_insert: usize,
    self_closing: bool,
    parent_depth: usize,
    children: &[String],
) {
    if self_closing {
        let name = tag_name(source, span.start);
        let mut text = ">".to_owned();
        for child in children {
            let _ = write!(text, "\n{}{child}", indent(parent_depth + 1));
        }
        let _ = write!(text, "\n{}</{name}>", indent(parent_depth));
        patches.replace(span.end - 2..span.end, text);
    } else {
        let mut text = String::new();
        for child in children {
            // Two extra spaces: the end tag's own indentation, which the
            // insertion lands after, is one level shallower.
            let _ = write!(text, "  {child}\n{}", indent(parent_depth));
        }
        patches.insert(end_insert, text);
    }
}

/// Inserts the attribute `name="value"` into the open tag at `start`.
fn insert_attr(patches: &mut Patches, source: &[u8], start: usize, name: &str, value: &str) {
    let at = tag_end(source, start);
    patches.insert(at, format!(" {name}=\"{}\"", escape(value)));
}

/// Patches an existing application element to carry `state` (the name,
/// which matched it, stays).
fn patch_app(patches: &mut Patches, source: &[u8], app: &App, state: &AppState) {
    if let Some(found) = &app.exec {
        patches.replace(found.value.clone(), escape(&state.exec));
    } else {
        insert_attr(patches, source, app.span.start, "exec", &state.exec);
    }
    if let Some(found) = &app.modified {
        patches.replace(found.value.clone(), timestamp(state.modified));
    } else {
        insert_attr(
            patches,
            source,
            app.span.start,
            "modified",
            &timestamp(state.modified),
        );
    }
    if let Some((found, _)) = &app.count {
        patches.replace(found.value.clone(), state.count.to_string());
    } else {
        insert_attr(
            patches,
            source,
            app.span.start,
            "count",
            &state.count.to_string(),
        );
    }
}

/// Where the open tag starting at `start` takes another attribute: the
/// position of its closing `>` or `/>`, found outside quotes.
fn tag_end(source: &[u8], start: usize) -> usize {
    let mut quote: Option<u8> = None;
    for (offset, &byte) in source[start..].iter().enumerate() {
        work::count(1);
        match quote {
            Some(open) if byte == open => quote = None,
            Some(_) => {}
            None => match byte {
                b'"' | b'\'' => quote = Some(byte),
                b'>' if offset > 0 && source[start + offset - 1] == b'/' => {
                    return start + offset - 1;
                }
                b'>' => return start + offset,
                _ => {}
            },
        }
    }
    // Unreachable on a well-formed document, which editing guarantees.
    source.len()
}

/// The name of the tag starting at `start`, as written.
fn tag_name(source: &[u8], start: usize) -> String {
    let name: Vec<u8> = source[start + 1..]
        .iter()
        .inspect(|_| work::count(1))
        .take_while(|&&byte| !byte.is_ascii_whitespace() && byte != b'>' && byte != b'/')
        .copied()
        .collect();
    String::from_utf8_lossy(&name).into_owned()
}

/// `2 * depth` spaces.
fn indent(depth: usize) -> String {
    "  ".repeat(depth)
}

/// A parent element with `children` on their own lines.
fn wrap(depth: usize, open: &str, close: &str, child_depth: usize, children: &[String]) -> String {
    let mut text = open.to_owned();
    for child in children {
        let _ = write!(text, "\n{}{child}", indent(child_depth));
    }
    let _ = write!(text, "\n{}{close}", indent(depth));
    text
}

/// The timestamp spelling of `time`, validated by the mutation that
/// recorded it.
fn timestamp(time: SystemTime) -> String {
    crate::time::format(time).expect("validated when the edit was recorded")
}

/// `text` escaped for an attribute value or element content: the five
/// metacharacters, plus tab, newline and carriage return as character
/// references — a parser's attribute-value normalization would otherwise
/// turn them into spaces, changing an accepted value on its round trip.
fn escape(text: &str) -> String {
    work::count(text.len());
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            '\t' => escaped.push_str("&#9;"),
            '\n' => escaped.push_str("&#10;"),
            '\r' => escaped.push_str("&#13;"),
            c => escaped.push(c),
        }
    }
    escaped
}

/// The prefix (with its colon) that names `uri` at this point, and the
/// declaration to put on the inserted element when none does: prefixes
/// are resolved by namespace URI, never assumed by name, and a missing
/// one is declared on the inserted element itself, so no ancestor tag is
/// rewritten and no existing prefix changes meaning.
fn prefix_for(scope: &Scope, uri: &str, preferred: &str) -> (String, String) {
    let mut shadowed = xdg_kit_basedir::__hashed::Set::<
        xdg_kit_basedir::__hashed::Str<crate::work::Counted>,
    >::new();
    for (prefix, bound) in scope.shadowing() {
        // The binding looked at; the set counts the prefix's bytes.
        work::count(1);
        if !shadowed.insert(prefix) {
            continue;
        }
        if bound == uri {
            return if prefix.is_empty() {
                (String::new(), String::new())
            } else {
                (format!("{prefix}:"), String::new())
            };
        }
    }
    (
        format!("{preferred}:"),
        format!(" xmlns:{preferred}=\"{uri}\""),
    )
}

fn render_mime(scope: &Scope, mime: &str) -> String {
    let (prefix, decl) = prefix_for(scope, MIME_NS, "mime");
    format!("<{prefix}mime-type type=\"{}\"{decl}/>", escape(mime))
}

fn render_group(scope: &Scope, group: &str) -> String {
    let (prefix, decl) = prefix_for(scope, BOOKMARK_NS, "bookmark");
    format!("<{prefix}group{decl}>{}</{prefix}group>", escape(group))
}

fn render_private(scope: &Scope) -> String {
    let (prefix, decl) = prefix_for(scope, BOOKMARK_NS, "bookmark");
    format!("<{prefix}private{decl}/>")
}

fn render_app(scope: &Scope, state: &AppState) -> String {
    let (prefix, decl) = prefix_for(scope, BOOKMARK_NS, "bookmark");
    format!(
        "<{prefix}application name=\"{}\" exec=\"{}\" modified=\"{}\" count=\"{}\"{decl}/>",
        escape(&state.name),
        escape(&state.exec),
        timestamp(state.modified),
        state.count,
    )
}

/// A `groups` or `applications` list element with `children`.
fn render_list(scope: &Scope, local: &str, depth: usize, children: &[String]) -> String {
    let (prefix, decl) = prefix_for(scope, BOOKMARK_NS, "bookmark");
    wrap(
        depth,
        &format!("<{prefix}{local}{decl}>"),
        &format!("</{prefix}{local}>"),
        depth + 1,
        children,
    )
}
