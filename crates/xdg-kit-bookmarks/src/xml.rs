//! Parsing XBEL into a span index: every structure the typed API knows,
//! each with the byte range it occupies in the source, so edits can
//! replace exactly the affected bytes and everything else survives
//! verbatim.
//!
//! Reading is lenient: whatever cannot be understood becomes a
//! [`Diagnostic`] and the rest is still served. Editing has a higher bar,
//! computed here as well ([`Parsed::not_editable`]): the document must be
//! well-formed XML end to end, with exactly one root element named
//! `xbel` (in no namespace), no duplicate attributes, every prefix in use
//! declared, and no internal DTD subset — entity declarations would change
//! expansion semantics this crate does not implement, so only the five
//! predefined entities and character references are expanded.

use std::ops::Range;
use std::sync::Arc;
use std::time::SystemTime;

use quick_xml::escape::resolve_xml_entity;
use quick_xml::events::attributes::Attribute;
use quick_xml::events::{BytesStart, Event};

use xdg_kit_basedir::__hashed::{Map, Set, Str};

use crate::work::Counted;
use crate::{Diagnostic, Limits, work};

/// The namespace of `bookmark:*` metadata elements.
pub(crate) const BOOKMARK_NS: &str = "http://www.freedesktop.org/standards/desktop-bookmarks";
/// The namespace of the `mime:mime-type` metadata element.
pub(crate) const MIME_NS: &str = "http://www.freedesktop.org/standards/shared-mime-info";
/// The owner whose `<metadata>` block this crate reads and edits.
pub(crate) const OWNER: &str = "http://freedesktop.org";

/// The namespace bindings in scope at a point, as a structurally shared
/// chain: each declaring element contributes one node holding only its
/// own `(prefix, uri)` bindings and sharing everything inherited, so a
/// snapshot costs the element's declarations, never a copy of the scope.
/// The empty prefix is the default namespace; an inner binding shadows an
/// outer one of the same prefix.
#[derive(Debug, Default)]
pub(crate) struct Scope {
    parent: Option<Arc<Scope>>,
    /// This element's declarations, in document order.
    bindings: Vec<(String, String)>,
}

impl Scope {
    /// The bindings from the innermost outward (within one element, the
    /// later first): the first binding seen for a prefix is the effective
    /// one.
    pub(crate) fn shadowing(&self) -> Shadowing<'_> {
        Shadowing {
            node: Some(self),
            at: self.bindings.len(),
        }
    }
}

/// See [`Scope::shadowing`].
pub(crate) struct Shadowing<'a> {
    node: Option<&'a Scope>,
    at: usize,
}

impl<'a> Iterator for Shadowing<'a> {
    type Item = &'a (String, String);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let node = self.node?;
            if self.at > 0 {
                self.at -= 1;
                return Some(&node.bindings[self.at]);
            }
            self.node = node.parent.as_deref();
            self.at = self.node.map_or(0, |parent| parent.bindings.len());
        }
    }
}

/// An attribute value: where its raw text is, and what it says.
#[derive(Debug, Clone)]
pub(crate) struct AttrSpan {
    /// The raw value's bytes in the source, between its quotes.
    pub value: Range<usize>,
    /// The value, unescaped.
    pub text: String,
}

/// A timestamp attribute: where it is, and the time if it parses.
#[derive(Debug, Clone)]
pub(crate) struct Stamp {
    pub value: Range<usize>,
    pub time: Option<SystemTime>,
}

/// The `<xbel>` root.
#[derive(Debug)]
pub(crate) struct Root {
    pub span: Range<usize>,
    /// Where new content goes: the start of `</xbel>`. Meaningless for a
    /// self-closing root, which the serializer expands.
    pub end_insert: usize,
    pub self_closing: bool,
    /// The bindings in scope inside the root.
    pub ns: Arc<Scope>,
}

/// One `<bookmark>`.
#[derive(Debug)]
pub(crate) struct BookmarkNode {
    pub span: Range<usize>,
    /// The whitespace-only text right before the element, which is its
    /// indentation and leaves with it when it is removed.
    pub lead: Range<usize>,
    pub end_insert: usize,
    pub self_closing: bool,
    pub href: AttrSpan,
    pub added: Option<Stamp>,
    pub modified: Option<Stamp>,
    pub visited: Option<Stamp>,
    pub info: Option<Info>,
    pub ns: Arc<Scope>,
}

/// A bookmark's `<info>`.
#[derive(Debug)]
pub(crate) struct Info {
    pub span: Range<usize>,
    pub end_insert: usize,
    pub self_closing: bool,
    /// The bindings in scope inside the element — what a metadata block
    /// created in it must resolve against, since `<info>` itself can
    /// redeclare prefixes.
    pub ns: Arc<Scope>,
    pub metadata: Option<Meta>,
}

/// The `<metadata owner="http://freedesktop.org">` block. Metadata of any
/// other owner is opaque: preserved, never read or edited.
#[derive(Debug)]
pub(crate) struct Meta {
    pub span: Range<usize>,
    pub end_insert: usize,
    pub self_closing: bool,
    pub ns: Arc<Scope>,
    pub mime: Option<MimeNode>,
    pub groups: Option<Groups>,
    pub applications: Option<Apps>,
    /// A `bookmark:private` element's span, with its indentation.
    pub private: Option<Range<usize>>,
}

/// A `mime:mime-type` element: where it is, and its `type` attribute —
/// `None` when the element has no such attribute, so a new one is
/// inserted into the start tag rather than anywhere near it.
#[derive(Debug)]
pub(crate) struct MimeNode {
    pub span: Range<usize>,
    pub value: Option<AttrSpan>,
}

/// A `bookmark:groups` list.
#[derive(Debug)]
pub(crate) struct Groups {
    pub span: Range<usize>,
    pub end_insert: usize,
    pub self_closing: bool,
    /// The bindings in scope inside the element — the scope insertions
    /// into it must resolve against, since the container itself can
    /// redeclare prefixes.
    pub ns: Arc<Scope>,
    pub list: Vec<Group>,
}

/// One `bookmark:group`.
#[derive(Debug)]
pub(crate) struct Group {
    pub name: String,
}

/// A `bookmark:applications` list.
#[derive(Debug)]
pub(crate) struct Apps {
    pub span: Range<usize>,
    pub end_insert: usize,
    pub self_closing: bool,
    /// The bindings in scope inside the element (see [`Groups::ns`]).
    pub ns: Arc<Scope>,
    pub list: Vec<App>,
}

/// One `bookmark:application` record.
#[derive(Debug)]
pub(crate) struct App {
    pub span: Range<usize>,
    pub name: Option<AttrSpan>,
    pub exec: Option<AttrSpan>,
    pub modified: Option<Stamp>,
    /// The count attribute, and the number if it parses.
    pub count: Option<(AttrSpan, Option<u64>)>,
}

/// The parsed document: the span index, what reading noted, and whether
/// editing is allowed.
#[derive(Debug, Default)]
pub(crate) struct Parsed {
    pub root: Option<Root>,
    pub bookmarks: Vec<BookmarkNode>,
    pub diagnostics: Vec<Diagnostic>,
    /// Why the document cannot be edited, when it cannot: the first
    /// reason found. Reading still serves what parsed before it.
    pub not_editable: Option<String>,
}

/// What an element is to this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Xbel,
    Bookmark,
    Info,
    Metadata,
    MimeType,
    Groups,
    Group,
    Applications,
    Application,
    Private,
    /// Anything else, foreign metadata included: an opaque subtree.
    Opaque,
}

/// What an open element is collecting until its end tag. Leaves
/// (applications, mime types, private markers) are built whole from their
/// attributes and attached when the element ends.
enum Build {
    Xbel,
    Bookmark(Box<BookmarkNode>),
    Info(Info),
    Meta(Meta),
    Groups(Groups),
    Group(String),
    Apps(Apps),
    Application(App),
    MimeType(MimeNode),
    Private,
    Opaque,
}

struct Frame {
    build: Build,
    start: usize,
    lead: Range<usize>,
    /// The scope outside this element, restored at its end.
    outer: Arc<Scope>,
    /// The prefixes this element declared, undone at its end.
    declared: Vec<String>,
}

/// Parses `source` into the span index.
pub(crate) fn parse(source: &[u8], limits: &Limits) -> Parsed {
    let mut parsed = Parsed::default();
    let mut reader = quick_xml::Reader::from_reader(source);
    reader.config_mut().check_comments = true;
    // quick-xml skips a UTF-8 byte order mark and counts positions past
    // it; the spans here index the whole source, BOM included, so every
    // reader position is shifted back by its length.
    let bom = if source.starts_with(b"\xEF\xBB\xBF") {
        3
    } else {
        0
    };
    let mut stack: Vec<Frame> = Vec::new();
    let mut scope: Arc<Scope> = Arc::new(Scope::default());
    let mut resolver = Resolver::default();
    let mut state = Prologue {
        document_start: bom,
        seen_doctype: false,
    };
    let mut seen_root = false;
    // The end of the last event: the start of the next one, and of the
    // whitespace that indents an element.
    let mut last_end = bom;
    let mut last_text_blank: Option<Range<usize>> = None;
    loop {
        let start = last_end;
        let event = match reader.read_event() {
            Ok(event) => event,
            Err(err) => {
                let at = usize::try_from(reader.error_position())
                    .unwrap_or(usize::MAX)
                    .saturating_add(bom);
                refuse(&mut parsed, Some(at), format!("bad XML: {err}"));
                return parsed;
            }
        };
        let end = usize::try_from(reader.buffer_position())
            .unwrap_or(usize::MAX)
            .saturating_add(bom);
        last_end = end;
        // The event, and each of its bytes.
        work::count(1 + end.saturating_sub(start));
        let lead = last_text_blank.take().unwrap_or(start..start);
        match event {
            Event::Eof => break,
            Event::Decl(_) | Event::PI(_) | Event::Comment(_) | Event::DocType(_) => {
                misc_event(
                    &event,
                    start,
                    &mut state,
                    seen_root || !stack.is_empty(),
                    &mut parsed,
                );
            }
            Event::CData(data) => {
                // `xml_content` normalizes line endings, as XML requires
                // before any application-level reading; a `&#13;` keeps
                // its carriage return, references being separate events.
                cdata_event(
                    &data.xml_content(quick_xml::XmlVersion::Implicit1_0),
                    start,
                    &mut parsed,
                    &mut stack,
                );
            }
            Event::Text(text) => {
                last_text_blank = text_event(
                    &text.xml_content(quick_xml::XmlVersion::Implicit1_0),
                    start..end,
                    &mut parsed,
                    &mut stack,
                );
            }
            Event::GeneralRef(reference) => {
                general_ref(&reference, start, &mut parsed, &mut stack);
            }
            Event::Start(ref tag) | Event::Empty(ref tag) => {
                let element = Element {
                    self_closing: matches!(event, Event::Empty(_)),
                    start,
                    end,
                    lead,
                };
                let went_on = open_element(
                    tag,
                    &element,
                    source,
                    limits,
                    &mut parsed,
                    &mut stack,
                    &mut scope,
                    &mut resolver,
                    &mut seen_root,
                );
                if !went_on {
                    return parsed;
                }
            }
            Event::End(_) => close_element(
                start,
                end,
                &mut parsed,
                &mut stack,
                &mut scope,
                &mut resolver,
            ),
        }
    }
    ended(&mut parsed, &stack, seen_root);
    parsed
}

/// Where the document's prologue stands: what a declaration or DOCTYPE
/// may still do.
struct Prologue {
    /// Where the document begins (after a byte order mark): the only
    /// place an XML declaration may stand.
    document_start: usize,
    seen_doctype: bool,
}

/// Validates the events that carry no indexed structure: an XML
/// declaration belongs at the very start and must declare a real version;
/// one DOCTYPE at most, before the root, without an internal subset (its
/// entities would not be expanded); a processing instruction may not be
/// targeted at `xml`; and comments and instructions may not hold
/// characters XML cannot.
fn misc_event(
    event: &Event<'_>,
    start: usize,
    state: &mut Prologue,
    past_root: bool,
    parsed: &mut Parsed,
) {
    match event {
        Event::DocType(doctype) => {
            if doctype.as_ref().contains('[') {
                refuse(
                    parsed,
                    Some(start),
                    "an internal DTD subset declares entities this crate does not expand"
                        .to_owned(),
                );
            }
            if state.seen_doctype {
                refuse(
                    parsed,
                    Some(start),
                    "more than one document type declaration".to_owned(),
                );
            }
            if past_root {
                refuse(
                    parsed,
                    Some(start),
                    "a document type declaration past the root element".to_owned(),
                );
            }
            state.seen_doctype = true;
        }
        Event::Decl(decl) => {
            if start != state.document_start {
                refuse(
                    parsed,
                    Some(start),
                    "an XML declaration appears past the start".to_owned(),
                );
            }
            if let Err(problem) = declaration(decl) {
                refuse(parsed, Some(start), problem);
            }
        }
        Event::PI(pi) => {
            if pi.target().eq_ignore_ascii_case("xml") {
                refuse(
                    parsed,
                    Some(start),
                    "a processing instruction may not be targeted at `xml`".to_owned(),
                );
            } else if !valid_name(pi.target()) {
                refuse(
                    parsed,
                    Some(start),
                    format!(
                        "`{}` is not a processing instruction target",
                        crate::error::quoted(pi.target())
                    ),
                );
            }
            valid_chars(pi.content(), start, parsed);
        }
        Event::Comment(comment) => valid_chars(comment.as_ref(), start, parsed),
        _ => {}
    }
}

/// Validates an XML declaration's whole grammar: `version` first and
/// required, then optionally `encoding`, then optionally `standalone`,
/// nothing else and nothing twice, each with a well-formed value.
fn declaration(decl: &quick_xml::events::BytesDecl<'_>) -> Result<(), String> {
    // In the spec's order; a field's index must strictly grow.
    const FIELDS: [&str; 3] = ["version", "encoding", "standalone"];
    let mut next = 0;
    // The declaration's inner text is tag-shaped ("xml version=…"), so
    // its fields iterate like a tag's attributes.
    let fields = BytesStart::from_content(decl.as_ref(), "xml".len());
    for attr in fields.attributes() {
        let attr = attr.map_err(|err| format!("a malformed XML declaration: {err}"))?;
        let name = attr.key.as_ref();
        let value: &str = attr.value.as_ref();
        work::count(name.len() + value.len());
        let at = FIELDS[next..]
            .iter()
            .position(|&field| field == name)
            .ok_or_else(|| {
                format!(
                    "`{}` cannot stand here in an XML declaration",
                    crate::error::quoted(name)
                )
            })?
            + next;
        if next == 0 && at != 0 {
            return Err("an XML declaration must start with `version`".to_owned());
        }
        let good = match at {
            // `1.` and digits (spec `VersionNum`).
            0 => {
                value.len() > 2
                    && value.starts_with("1.")
                    && value[2..].bytes().all(|byte| byte.is_ascii_digit())
            }
            // Spec `EncName`.
            1 => {
                let mut bytes = value.bytes();
                bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic())
                    && bytes.all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
                    })
            }
            _ => value == "yes" || value == "no",
        };
        if !good {
            return Err(format!(
                "`{}` is not a value {name} can have in an XML declaration",
                crate::error::quoted(value)
            ));
        }
        next = at + 1;
    }
    if next == 0 {
        return Err("an XML declaration without a version".to_owned());
    }
    Ok(())
}

/// Handles a text event: group content, indentation tracking (the
/// returned range is the whitespace that leads the next element), or a
/// refusal for text outside the root.
fn text_event(
    text: &str,
    span: Range<usize>,
    parsed: &mut Parsed,
    stack: &mut [Frame],
) -> Option<Range<usize>> {
    valid_chars(text, span.start, parsed);
    if let Some(Frame {
        build: Build::Group(name),
        ..
    }) = stack.last_mut()
    {
        // References are split out as their own events, so the text holds
        // no `&` to resolve.
        name.push_str(text);
        return None;
    }
    if text.chars().all(|c| c.is_ascii_whitespace()) {
        return Some(span);
    }
    if stack.is_empty() {
        refuse(parsed, Some(span.start), "text outside the root".to_owned());
    }
    None
}

/// Handles a CDATA section: part of a group's name where one is open,
/// content to refuse outside the root, preserved opaquely elsewhere.
fn cdata_event(data: &str, start: usize, parsed: &mut Parsed, stack: &mut [Frame]) {
    valid_chars(data, start, parsed);
    match stack.last_mut() {
        Some(Frame {
            build: Build::Group(name),
            ..
        }) => name.push_str(data),
        Some(_) => {}
        None => refuse(parsed, Some(start), "content outside the root".to_owned()),
    }
}

/// The checks only the end of input can settle: an unclosed start tag
/// (quick-xml does not flag those at EOF) and a document without a root.
fn ended(parsed: &mut Parsed, stack: &[Frame], seen_root: bool) {
    if let Some(frame) = stack.last() {
        refuse(
            parsed,
            Some(frame.start),
            "bad XML: a start tag is not closed before the end of input".to_owned(),
        );
    } else if !seen_root {
        refuse(parsed, None, "no root element".to_owned());
    }
}

/// Handles a reference event: only character references and the five
/// predefined entities are expanded (no internal DTD may declare more),
/// and a reference inside a group contributes its character to the name.
fn general_ref(
    reference: &quick_xml::events::BytesRef<'_>,
    start: usize,
    parsed: &mut Parsed,
    stack: &mut [Frame],
) {
    if stack.is_empty() {
        refuse(parsed, Some(start), "content outside the root".to_owned());
    }
    let named = reference.xml10_content().into_owned();
    match reference.resolve_char_ref().unwrap_or_default() {
        Some(c) if !is_xml_char(c) => refuse(
            parsed,
            Some(start),
            format!(
                "`&{};` names the character U+{:04X}, which XML cannot hold",
                crate::error::quoted(&named),
                u32::from(c)
            ),
        ),
        None if resolve_xml_entity(&named).is_none() => refuse(
            parsed,
            Some(start),
            format!(
                "the reference `&{};` is not a predefined entity",
                crate::error::quoted(&named)
            ),
        ),
        Some(_) | None => {}
    }
    if let Some(Frame {
        build: Build::Group(name),
        ..
    }) = stack.last_mut()
        && let Some(replaced) = replacement(reference)
    {
        name.push(replaced);
    }
}

/// Handles an `End` event: the frame closes, its declarations are undone,
/// and the finished element attaches to its parent.
fn close_element(
    start: usize,
    end: usize,
    parsed: &mut Parsed,
    stack: &mut Vec<Frame>,
    scope: &mut Arc<Scope>,
    resolver: &mut Resolver,
) {
    let Some(frame) = stack.pop() else {
        // quick-xml reports unmatched end tags itself.
        return;
    };
    let inner = std::mem::replace(scope, Arc::clone(&frame.outer));
    for prefix in frame.declared.iter().rev() {
        resolver.undeclare(prefix);
    }
    finish(
        frame.build,
        frame.start..end,
        frame.lead,
        start,
        false,
        &inner,
        stack,
        parsed,
    );
}

/// Indexed prefix resolution during the parse: each prefix's stack of
/// bindings, pushed when an element declares it and popped when that
/// element ends, so a lookup costs the prefix's bytes, never a walk over
/// every binding in scope. (The [`Scope`] snapshots on the nodes serve
/// the serializer, which resolves rarely, per edit.)
#[derive(Default)]
struct Resolver {
    bindings: Map<Str<Counted>, Vec<String>>,
}

impl Resolver {
    fn declare(&mut self, prefix: &str, uri: &str) {
        // The push; the map counts the prefix's bytes, and the binding is
        // copied.
        work::count(1 + uri.len());
        self.bindings
            .get_or_insert_with(prefix, Vec::new)
            .push(uri.to_owned());
    }

    fn undeclare(&mut self, prefix: &str) {
        work::count(1);
        if let Some(stack) = self.bindings.get_mut(prefix) {
            stack.pop();
        }
    }

    fn resolve(&self, prefix: &str) -> Option<&str> {
        if prefix == "xml" {
            return Some(XML_NS);
        }
        // The lookup; the map counts the prefix's bytes.
        work::count(1);
        self.bindings.get(prefix)?.last().map(String::as_str)
    }
}

/// Where one element's open (or whole self-closing) tag lies.
struct Element {
    self_closing: bool,
    start: usize,
    end: usize,
    lead: Range<usize>,
}

/// Handles one `Start` or `Empty` event. `false` means parsing cannot go
/// on (the caller returns what was indexed so far).
#[allow(clippy::too_many_arguments)]
fn open_element(
    tag: &BytesStart<'_>,
    element: &Element,
    source: &[u8],
    limits: &Limits,
    parsed: &mut Parsed,
    stack: &mut Vec<Frame>,
    scope: &mut Arc<Scope>,
    resolver: &mut Resolver,
    seen_root: &mut bool,
) -> bool {
    let (start, end, lead) = (element.start, element.end, element.lead.clone());
    if stack.is_empty() {
        if *seen_root {
            refuse(parsed, Some(start), "more than one root element".to_owned());
            return true;
        }
        *seen_root = true;
    }
    if stack.len() >= limits.max_depth {
        refuse(
            parsed,
            Some(start),
            format!("elements nested deeper than {}", limits.max_depth),
        );
        return false;
    }
    let outer = Arc::clone(scope);
    let (attrs, declared) = match read_tag(tag, source, start) {
        Ok(read) => read,
        Err(problem) => {
            refuse(parsed, Some(start), problem);
            // The names in the tag cannot be trusted; stop.
            return false;
        }
    };
    if !declared.is_empty() {
        // Only this element's bindings are stored; the rest is shared.
        work::count(
            declared
                .iter()
                .map(|(p, u)| p.len() + u.len())
                .sum::<usize>(),
        );
        *scope = Arc::new(Scope {
            parent: Some(Arc::clone(scope)),
            bindings: declared.clone(),
        });
    }
    for (prefix, uri) in &declared {
        resolver.declare(prefix, uri);
    }
    // Declarations on this tag bind its own attributes too, so names
    // resolve only now.
    if let Err(problem) = check_attrs(&attrs, resolver) {
        refuse(parsed, Some(start), problem);
        return false;
    }
    let parent = stack.last().map(kind_of);
    let Some(kind) = classify(tag, resolver, &attrs, parent, parsed, start) else {
        // The root is not xbel; nothing more can be indexed.
        return false;
    };
    if kind == Kind::Bookmark && parsed.bookmarks_full(limits) {
        refuse(
            parsed,
            Some(start),
            format!("more than {} bookmarks", limits.max_bookmarks),
        );
        return false;
    }
    let build = begin(kind, &attrs, scope, parsed, start, lead.clone());
    if element.self_closing {
        finish(build, start..end, lead, end, true, scope, stack, parsed);
        *scope = outer;
        for (prefix, _) in declared.iter().rev() {
            resolver.undeclare(prefix);
        }
    } else {
        stack.push(Frame {
            build,
            start,
            lead,
            outer,
            declared: declared.into_iter().map(|(prefix, _)| prefix).collect(),
        });
    }
    true
}

impl Parsed {
    fn bookmarks_full(&self, limits: &Limits) -> bool {
        self.bookmarks.len() >= limits.max_bookmarks
    }
}

/// Notes `message` as a diagnostic and, the first time, as the reason the
/// document cannot be edited.
fn refuse(parsed: &mut Parsed, at: Option<usize>, message: String) {
    if parsed.not_editable.is_none() {
        parsed.not_editable = Some(message.clone());
    }
    parsed.diagnostics.push(Diagnostic::new(at, message));
}

/// What `kind` an open [`Frame`] is building.
fn kind_of(frame: &Frame) -> Kind {
    match &frame.build {
        Build::Xbel => Kind::Xbel,
        Build::Bookmark(_) => Kind::Bookmark,
        Build::Info(_) => Kind::Info,
        Build::Meta(_) => Kind::Metadata,
        Build::Groups(_) => Kind::Groups,
        Build::Group(_) => Kind::Group,
        Build::Apps(_) => Kind::Applications,
        Build::Application(_) => Kind::Application,
        Build::MimeType(_) => Kind::MimeType,
        Build::Private => Kind::Private,
        Build::Opaque => Kind::Opaque,
    }
}

/// One read attribute: the name as written, the resolved value, and where
/// the raw value is.
struct ReadAttr {
    name: String,
    value: AttrSpan,
}

/// Reads a tag's attributes and namespace declarations, syntax-checked:
/// malformed or invalid names and values make the document uneditable,
/// which the caller reports. The declarations are returned, not applied —
/// the caller owns the scope, and resolution happens once the tag's own
/// declarations are in it ([`check_attrs`]).
/// What [`read_tag`] found: the ordinary attributes, and the namespace
/// declarations as `(prefix, uri)`.
type TagContents = (Vec<ReadAttr>, Vec<(String, String)>);

fn read_tag(tag: &BytesStart<'_>, source: &[u8], at: usize) -> Result<TagContents, String> {
    let mut read = Vec::new();
    let mut declared: Vec<(String, String)> = Vec::new();
    for attr in tag.attributes() {
        let attr = attr.map_err(|err| format!("bad attribute: {err}"))?;
        // The attribute's bytes examined.
        work::count(attr.key.as_ref().len() + attr.value.len());
        if attr.value.contains('<') {
            // A literal `<` in a value is not well-formed; after
            // unescaping it would be indistinguishable from `&lt;`, so it
            // is caught raw — spans spliced next to one would otherwise
            // produce exactly such a value.
            return Err("a value holds a literal `<`".to_owned());
        }
        let text = attr
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map_err(|err| format!("a value cannot be unescaped: {err}"))?
            .into_owned();
        if let Some(bad) = text.chars().find(|&c| !is_xml_char(c)) {
            return Err(format!(
                "a value holds the character U+{:04X}, which XML cannot",
                u32::from(bad)
            ));
        }
        let name = attr.key.as_ref().to_owned();
        if name == "xmlns" {
            reserved_binding("", &text)?;
            declared.push((String::new(), text));
            continue;
        }
        if let Some(prefix) = name.strip_prefix("xmlns:") {
            if !valid_name(prefix) {
                return Err(format!(
                    "`{}` is not an XML name",
                    crate::error::quoted(&name)
                ));
            }
            reserved_binding(prefix, &text)?;
            declared.push((prefix.to_owned(), text));
            continue;
        }
        let value = value_span(&attr, source, at, tag.len());
        read.push(ReadAttr {
            name,
            value: AttrSpan { value, text },
        });
    }
    Ok((read, declared))
}

/// Checks a tag's attributes once its own declarations are in effect:
/// well-formed `QName`s, declared prefixes, and uniqueness by expanded
/// (namespace, local) name — quick-xml catches only spelled-alike
/// duplicates (hashed itself, so its check stays linear too).
fn check_attrs(read: &[ReadAttr], resolver: &Resolver) -> Result<(), String> {
    let mut expanded = Set::<Str<Counted>>::new();
    for attr in read {
        let (prefix, local) = split_qname(&attr.name)
            .ok_or_else(|| format!("`{}` is not an XML name", crate::error::quoted(&attr.name)))?;
        // An unprefixed attribute is in no namespace, not the default one.
        let uri = if prefix.is_empty() {
            ""
        } else {
            resolver.resolve(prefix).ok_or_else(|| {
                format!(
                    "the prefix `{}` is not declared",
                    crate::error::quoted(prefix)
                )
            })?
        };
        // A space separates the parts: it can appear in neither. The set
        // counts the key's bytes hashed, compared and copied; building
        // the key copies them once more.
        work::count(uri.len() + 1 + local.len());
        if !expanded.insert(&format!("{uri} {local}")) {
            return Err(format!(
                "two attributes resolve to the same name `{}`",
                crate::error::quoted(local)
            ));
        }
    }
    Ok(())
}

/// Splits a `QName` into its prefix (empty for none) and local part; `None`
/// when it is not one (a bare or edge colon, several colons, or invalid
/// name characters).
fn split_qname(name: &str) -> Option<(&str, &str)> {
    work::count(name.len());
    match name.find(':') {
        None => valid_name(name).then_some(("", name)),
        Some(colon) => {
            let (prefix, local) = (&name[..colon], &name[colon + 1..]);
            (valid_name(prefix) && valid_name(local)).then_some((prefix, local))
        }
    }
}

/// The namespace of the `xml:` prefix, the only one that may bind it.
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
/// The namespace of declarations themselves: never declared, never bound.
const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";

/// Refuses the declarations the namespaces spec reserves: `xmlns` is
/// never declared, `xml` binds only to its one namespace, and neither
/// reserved namespace may be bound to anything else.
fn reserved_binding(prefix: &str, uri: &str) -> Result<(), String> {
    if prefix == "xmlns" {
        return Err("the prefix `xmlns` may not be declared".to_owned());
    }
    // Only the default namespace may be reset to nothing (`xmlns=""`);
    // Namespaces 1.0 has no undeclaring of a named prefix.
    if !prefix.is_empty() && uri.is_empty() {
        return Err(format!(
            "the prefix `{}` cannot be bound to nothing",
            crate::error::quoted(prefix)
        ));
    }
    if prefix == "xml" && uri != XML_NS {
        return Err("the prefix `xml` may bind only to the XML namespace".to_owned());
    }
    if prefix != "xml" && uri == XML_NS {
        return Err("the XML namespace may bind only to the prefix `xml`".to_owned());
    }
    if uri == XMLNS_NS {
        return Err("the xmlns namespace may not be bound".to_owned());
    }
    Ok(())
}

/// Whether `c` may appear in an XML 1.0 document at all.
pub(crate) fn is_xml_char(c: char) -> bool {
    matches!(c, '\u{9}' | '\u{A}' | '\u{D}')
        || ('\u{20}'..='\u{D7FF}').contains(&c)
        || ('\u{E000}'..='\u{FFFD}').contains(&c)
        || c >= '\u{10000}'
}

/// Whether `text` is an XML `Name` (the spec's production, with the
/// widely used character classes).
fn valid_name(text: &str) -> bool {
    work::count(text.len());
    let start = |c: char| {
        c == '_'
            || c.is_ascii_alphabetic()
            || matches!(c,
                '\u{C0}'..='\u{D6}' | '\u{D8}'..='\u{F6}' | '\u{F8}'..='\u{2FF}'
                | '\u{370}'..='\u{37D}' | '\u{37F}'..='\u{1FFF}'
                | '\u{200C}'..='\u{200D}' | '\u{2070}'..='\u{218F}'
                | '\u{2C00}'..='\u{2FEF}' | '\u{3001}'..='\u{D7FF}'
                | '\u{F900}'..='\u{FDCF}' | '\u{FDF0}'..='\u{FFFD}'
                | '\u{10000}'..='\u{EFFFF}')
    };
    let follow = |c: char| {
        start(c)
            || c.is_ascii_digit()
            || matches!(c, '-' | '.' | '\u{B7}' | '\u{300}'..='\u{36F}' | '\u{203F}'..='\u{2040}')
    };
    let mut chars = text.chars();
    chars.next().is_some_and(start) && chars.all(follow)
}

/// Refuses characters XML cannot hold where they appear literally.
fn valid_chars(text: &str, at: usize, parsed: &mut Parsed) {
    work::count(text.len());
    if let Some(bad) = text.chars().find(|&c| !is_xml_char(c)) {
        refuse(
            parsed,
            Some(at),
            format!(
                "the character U+{:04X} cannot appear in XML",
                u32::from(bad)
            ),
        );
    }
}

/// Where `attr`'s raw value lies in `source`. The event's bytes borrow
/// from the source, so the value's address places it; a value that does
/// not borrow (it cannot here) falls back to the tag's span, which makes
/// an edit of it replace more than needed but never corrupt.
fn value_span(
    attr: &Attribute<'_>,
    source: &[u8],
    tag_start: usize,
    tag_len: usize,
) -> Range<usize> {
    let value: &str = attr.value.as_ref();
    let offset = (value.as_ptr() as usize).wrapping_sub(source.as_ptr() as usize);
    if offset <= source.len() && offset + value.len() <= source.len() {
        offset..offset + value.len()
    } else {
        tag_start..tag_start + tag_len
    }
}

/// What `tag` is: its kind under `parent` (`None`: it is the root).
/// `None` means parsing cannot go on (the root is not `xbel`).
fn classify(
    tag: &BytesStart<'_>,
    resolver: &Resolver,
    attrs: &[ReadAttr],
    parent: Option<Kind>,
    parsed: &mut Parsed,
    at: usize,
) -> Option<Kind> {
    let name = tag.name();
    let name: &str = name.as_ref();
    let Some((prefix, local)) = split_qname(name) else {
        refuse(
            parsed,
            Some(at),
            format!("`{}` is not an XML name", crate::error::quoted(name)),
        );
        return Some(Kind::Opaque);
    };
    let uri = if prefix.is_empty() {
        // An element without a prefix is in the default namespace.
        resolver.resolve("").unwrap_or("")
    } else if let Some(uri) = resolver.resolve(prefix) {
        uri
    } else {
        refuse(
            parsed,
            Some(at),
            format!(
                "the prefix `{}` is not declared",
                crate::error::quoted(prefix)
            ),
        );
        return Some(Kind::Opaque);
    };
    let Some(parent) = parent else {
        if uri.is_empty() && local == "xbel" {
            return Some(Kind::Xbel);
        }
        refuse(parsed, Some(at), "the root element is not xbel".to_owned());
        return None;
    };
    let named = |wanted: &str| uri.is_empty() && local == wanted;
    let bookmark_ns = |wanted: &str| uri == BOOKMARK_NS && local == wanted;
    Some(match parent {
        Kind::Xbel if named("bookmark") => Kind::Bookmark,
        Kind::Bookmark if named("info") => Kind::Info,
        Kind::Info if named("metadata") => {
            let owner = attrs
                .iter()
                .find(|attr| attr.name == "owner")
                .map(|attr| attr.value.text.as_str());
            if owner == Some(OWNER) {
                Kind::Metadata
            } else {
                Kind::Opaque
            }
        }
        Kind::Metadata if uri == MIME_NS && local == "mime-type" => Kind::MimeType,
        Kind::Metadata if bookmark_ns("groups") => Kind::Groups,
        Kind::Groups if bookmark_ns("group") => Kind::Group,
        Kind::Metadata if bookmark_ns("applications") => Kind::Applications,
        Kind::Applications if bookmark_ns("application") => Kind::Application,
        Kind::Metadata if bookmark_ns("private") => Kind::Private,
        _ => Kind::Opaque,
    })
}

/// Starts building `kind` from its attributes.
#[allow(clippy::too_many_arguments)]
fn begin(
    kind: Kind,
    attrs: &[ReadAttr],
    scope: &Arc<Scope>,
    parsed: &mut Parsed,
    at: usize,
    lead: Range<usize>,
) -> Build {
    match kind {
        Kind::Xbel => Build::Xbel,
        Kind::Bookmark => {
            let href = find(attrs, "href").map(|found| found.value.clone());
            let href = href.unwrap_or_else(|| {
                parsed.diagnostics.push(Diagnostic::new(
                    Some(at),
                    "a bookmark has no href".to_owned(),
                ));
                AttrSpan {
                    value: at..at,
                    text: String::new(),
                }
            });
            Build::Bookmark(Box::new(BookmarkNode {
                span: 0..0,
                lead,
                end_insert: 0,
                self_closing: false,
                href,
                added: stamp_of(parsed, attrs, "added"),
                modified: stamp_of(parsed, attrs, "modified"),
                visited: stamp_of(parsed, attrs, "visited"),
                info: None,
                ns: Arc::clone(scope),
            }))
        }
        Kind::Info => Build::Info(Info {
            span: 0..0,
            end_insert: 0,
            self_closing: false,
            ns: Arc::clone(scope),
            metadata: None,
        }),
        Kind::Metadata => Build::Meta(Meta {
            span: 0..0,
            end_insert: 0,
            self_closing: false,
            ns: Arc::clone(scope),
            mime: None,
            groups: None,
            applications: None,
            private: None,
        }),
        Kind::Groups => Build::Groups(Groups {
            span: 0..0,
            end_insert: 0,
            self_closing: false,
            ns: Arc::clone(scope),
            list: Vec::new(),
        }),
        Kind::Group => Build::Group(String::new()),
        Kind::Applications => Build::Apps(Apps {
            span: 0..0,
            end_insert: 0,
            self_closing: false,
            ns: Arc::clone(scope),
            list: Vec::new(),
        }),
        Kind::Application => {
            // Applications are leaves: built whole from their attributes
            // here, attached by `finish` when the element ends.
            let count = find(attrs, "count").map(|found| {
                let number = found.value.text.parse::<u64>().ok();
                if number.is_none() {
                    parsed.diagnostics.push(Diagnostic::new(
                        Some(found.value.value.start),
                        format!(
                            "`{}` is not a count",
                            crate::error::quoted(&found.value.text)
                        ),
                    ));
                }
                (found.value.clone(), number)
            });
            Build::Application(App {
                span: 0..0,
                name: find(attrs, "name").map(|found| found.value.clone()),
                exec: find(attrs, "exec").map(|found| found.value.clone()),
                modified: stamp_of(parsed, attrs, "modified"),
                count,
            })
        }
        Kind::MimeType => {
            let value = find(attrs, "type").map(|found| found.value.clone());
            if value.is_none() {
                parsed.diagnostics.push(Diagnostic::new(
                    Some(at),
                    "a mime-type element has no type".to_owned(),
                ));
            }
            Build::MimeType(MimeNode { span: 0..0, value })
        }
        Kind::Private => Build::Private,
        Kind::Opaque => Build::Opaque,
    }
}

/// The attribute `name` among `attrs`, if present.
fn find<'a>(attrs: &'a [ReadAttr], name: &str) -> Option<&'a ReadAttr> {
    attrs.iter().find(|attr| attr.name == name)
}

/// The timestamp attribute `name`, with a diagnostic when it does not
/// parse.
fn stamp_of(parsed: &mut Parsed, attrs: &[ReadAttr], name: &str) -> Option<Stamp> {
    let found = find(attrs, name)?;
    let time = crate::time::parse(&found.value.text);
    if time.is_none() {
        parsed.diagnostics.push(Diagnostic::new(
            Some(found.value.value.start),
            format!(
                "`{}` is not a timestamp",
                crate::error::quoted(&found.value.text)
            ),
        ));
    }
    Some(Stamp {
        value: found.value.value.clone(),
        time,
    })
}

/// Closes a finished element and attaches it to its parent. `span` is the
/// whole element, `end_insert` where its end tag starts (for a
/// self-closing element, its end — the serializer expands those), and
/// `scope` the bindings that were in effect inside it.
#[allow(clippy::too_many_arguments)]
fn finish(
    build: Build,
    span: Range<usize>,
    lead: Range<usize>,
    end_insert: usize,
    self_closing: bool,
    scope: &Arc<Scope>,
    stack: &mut [Frame],
    parsed: &mut Parsed,
) {
    let parent = stack.last_mut().map(|frame| &mut frame.build);
    match (build, parent) {
        (Build::Xbel, _) => {
            parsed.root = Some(Root {
                span,
                end_insert,
                self_closing,
                ns: Arc::clone(scope),
            });
        }
        (Build::Bookmark(mut bookmark), parent) => {
            bookmark.span = span;
            bookmark.end_insert = end_insert;
            bookmark.self_closing = self_closing;
            // Only a direct child of the root is classified Bookmark, so
            // the parent can only be the root frame (or gone, for the
            // degenerate `<bookmark>` root, which classify refused).
            if matches!(parent, Some(Build::Xbel)) {
                parsed.bookmarks.push(*bookmark);
            }
        }
        (Build::Info(mut info), Some(Build::Bookmark(bookmark))) => {
            info.span = span.clone();
            info.end_insert = end_insert;
            info.self_closing = self_closing;
            if bookmark.info.is_none() {
                bookmark.info = Some(info);
            } else {
                parsed.diagnostics.push(Diagnostic::new(
                    Some(span.start),
                    "a bookmark has more than one info; the first is used".to_owned(),
                ));
            }
        }
        (Build::Meta(mut meta), Some(Build::Info(info))) => {
            meta.span = span.clone();
            meta.end_insert = end_insert;
            meta.self_closing = self_closing;
            if info.metadata.is_none() {
                info.metadata = Some(meta);
            } else {
                parsed.diagnostics.push(Diagnostic::new(
                    Some(span.start),
                    "an info has more than one freedesktop metadata; the first is used".to_owned(),
                ));
            }
        }
        (Build::MimeType(mut mime), Some(Build::Meta(meta))) => {
            if meta.mime.is_none() {
                mime.span = span;
                meta.mime = Some(mime);
            }
        }
        (Build::Groups(mut groups), Some(Build::Meta(meta))) => {
            groups.span = span.clone();
            groups.end_insert = end_insert;
            groups.self_closing = self_closing;
            if meta.groups.is_none() {
                meta.groups = Some(groups);
            }
        }
        (Build::Group(name), Some(Build::Groups(groups))) => {
            groups.list.push(Group { name });
        }
        (Build::Apps(mut apps), Some(Build::Meta(meta))) => {
            apps.span = span.clone();
            apps.end_insert = end_insert;
            apps.self_closing = self_closing;
            if meta.applications.is_none() {
                meta.applications = Some(apps);
            }
        }
        (Build::Application(mut app), Some(Build::Apps(apps))) => {
            app.span = span;
            apps.list.push(app);
        }
        (Build::Private, Some(Build::Meta(meta))) if meta.private.is_none() => {
            meta.private = Some(lead.start..span.end);
        }
        _ => {}
    }
}

/// What a character reference or predefined entity stands for.
fn replacement(reference: &quick_xml::events::BytesRef<'_>) -> Option<char> {
    match reference.resolve_char_ref() {
        Ok(Some(c)) => is_xml_char(c).then_some(c),
        Ok(None) => resolve_xml_entity(&reference.xml10_content())?
            .chars()
            .next(),
        Err(_) => None,
    }
}
