//! A bounded reader of menu files into a small element tree.
//!
//! Only what the menu format needs: elements, their attributes and their
//! text. Comments, processing instructions and the XML declaration are
//! skipped; a document type declaration is accepted and ignored, but one
//! that declares entities is refused, since nothing is ever expanded but
//! the predefined entities and character references — no external
//! resource is ever fetched. Elements outside the menu namespace (a
//! prefix, or another default namespace) are kept under a name no menu
//! element has, so they are ignored as unknown, never read as menu
//! elements.

use quick_xml::escape::resolve_xml_entity;
use quick_xml::events::{BytesRef, BytesStart, Event};

use crate::work;

mod grammar;

/// The namespace of menu files.
pub(crate) const MENU_NAMESPACE: &str = "http://www.freedesktop.org/standards/menu";

/// The name given to elements outside the menu namespace.
pub(crate) const FOREIGN: &str = "{foreign}";

/// One element: its name, attributes, children and the line it starts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Element {
    pub name: String,
    pub attributes: Vec<(String, String)>,
    pub children: Vec<Child>,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Child {
    Element(Element),
    Text(String),
}

impl Element {
    /// The element's text, its pieces joined, surrounding whitespace
    /// trimmed (menu files indent element contents freely).
    pub(crate) fn text(&self) -> String {
        let mut text = String::new();
        for child in &self.children {
            if let Child::Text(piece) = child {
                text.push_str(piece);
            }
        }
        work::count(text.len());
        text.trim().to_owned()
    }

    /// The child elements, in order.
    pub(crate) fn elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|child| match child {
            Child::Element(element) => Some(element),
            Child::Text(_) => None,
        })
    }

    /// The value of attribute `name`.
    pub(crate) fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Why a file could not be read as XML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Failure {
    pub line: Option<usize>,
    pub message: String,
    /// Whether a resource limit was hit, rather than the file being
    /// malformed.
    pub limit: bool,
}

impl Failure {
    fn malformed(line: usize, message: impl Into<String>) -> Self {
        Self {
            line: Some(line),
            message: message.into(),
            limit: false,
        }
    }

    fn limit(line: usize, message: impl Into<String>) -> Self {
        Self {
            line: Some(line),
            message: message.into(),
            limit: true,
        }
    }
}

/// An element being read.
struct Open {
    element: Element,
    /// The default namespace in scope inside it.
    namespace: Option<String>,
}

/// Counts lines as the reader moves through the bytes.
struct Lines<'a> {
    bytes: &'a [u8],
    at: usize,
    line: usize,
}

impl Lines<'_> {
    /// The line of byte `to`, which never goes back.
    fn at(&mut self, to: usize) -> usize {
        let to = to.min(self.bytes.len());
        if to > self.at {
            #[allow(
                clippy::naive_bytecount,
                reason = "once per byte, no dependency for it"
            )]
            let newlines = self.bytes[self.at..to]
                .iter()
                .filter(|&&b| b == b'\n')
                .count();
            work::count(to - self.at);
            self.line += newlines;
            self.at = to;
        }
        self.line
    }
}

/// Reads `bytes` as one XML document whose elements are at most
/// `max_depth` deep, taking each element from `elements_left`.
///
/// # Errors
///
/// When the document is not well-formed, declares entities, holds content
/// outside its root, or goes over a limit.
pub(crate) fn parse(
    bytes: &[u8],
    max_depth: usize,
    elements_left: &mut usize,
) -> Result<Element, Failure> {
    // A byte order mark is no part of the document. Taken off here, the
    // reader's positions are positions in `bytes` (it would skip the mark
    // and count from after it).
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    // A second one is text before the root element; the reader would skip
    // it as the first.
    if bytes.starts_with(b"\xEF\xBB\xBF") {
        return Err(Failure::malformed(1, "a second byte order mark"));
    }
    let mut reader = quick_xml::Reader::from_reader(bytes);
    reader.config_mut().check_comments = true;
    let mut lines = Lines {
        bytes,
        at: 0,
        line: 1,
    };
    let mut stack: Vec<Open> = Vec::new();
    let mut root: Option<Element> = None;
    // The prolog: an XML declaration only as the first thing (the reader
    // passes over a byte order mark itself; anything else, whitespace too,
    // comes as an event before it), a document type declaration at most
    // once, before the root element.
    let mut first = true;
    let mut doctype = false;
    loop {
        let start = usize::try_from(reader.buffer_position()).unwrap_or(usize::MAX);
        let event = reader.read_event().map_err(|err| {
            let at = usize::try_from(reader.error_position()).unwrap_or(usize::MAX);
            Failure::malformed(lines.at(at), format!("not well-formed XML: {err}"))
        })?;
        let line = lines.at(start);
        let was_first = std::mem::replace(&mut first, false);
        match event {
            Event::Eof => break,
            Event::Decl(_) | Event::PI(_) | Event::Comment(_) => ignored(&event, line, was_first)?,
            Event::DocType(_) => {
                // Checked whole, as written: the reader trims it and takes
                // `<!doctype` in any case.
                let end = usize::try_from(reader.buffer_position()).unwrap_or(usize::MAX);
                let raw = bytes
                    .get(start..end)
                    .and_then(|raw| std::str::from_utf8(raw).ok())
                    .unwrap_or_default();
                let in_prolog = root.is_none() && stack.is_empty();
                document_type(raw, line, in_prolog, &mut doctype)?;
            }
            Event::Start(ref tag) | Event::Empty(ref tag) => {
                if root.is_some() {
                    return Err(Failure::malformed(line, "content after the root element"));
                }
                grammar::start_tag(tag, line)?;
                if stack.len() >= max_depth {
                    return Err(Failure::limit(
                        line,
                        format!("elements nested more than {max_depth} deep"),
                    ));
                }
                if *elements_left == 0 {
                    return Err(Failure::limit(line, "more elements than the limit allows"));
                }
                *elements_left -= 1;
                let inherited = stack.last().and_then(|open| open.namespace.clone());
                let open = open_element(tag, line, inherited)?;
                if matches!(event, Event::Empty(_)) {
                    close(open.element, &mut stack, &mut root);
                } else {
                    stack.push(open);
                }
            }
            Event::End(_) => {
                // quick-xml checks that end names match their start.
                if let Some(open) = stack.pop() {
                    close(open.element, &mut stack, &mut root);
                }
            }
            Event::Text(text) => {
                grammar::text(&text, line)?;
                let content = text.xml_content(quick_xml::XmlVersion::Implicit1_0);
                push_text(&content, line, &mut stack)?;
            }
            Event::CData(data) => {
                let content = data.xml_content(quick_xml::XmlVersion::Implicit1_0);
                if stack.is_empty() {
                    return Err(Failure::malformed(
                        line,
                        "character data outside the root element",
                    ));
                }
                push_text(&content, line, &mut stack)?;
            }
            Event::GeneralRef(reference) => {
                if stack.is_empty() {
                    return Err(Failure::malformed(
                        line,
                        "a reference outside the root element",
                    ));
                }
                push_text(&replacement(&reference, line)?, line, &mut stack)?;
            }
        }
    }
    if !stack.is_empty() {
        return Err(Failure::malformed(
            lines.at(bytes.len()),
            "the document ends inside an element",
        ));
    }
    root.ok_or_else(|| Failure::malformed(lines.at(bytes.len()), "no root element"))
}

/// Checks markup that holds nothing for the tree: an XML declaration
/// (only as the first thing), a processing instruction or a comment.
fn ignored(event: &Event<'_>, line: usize, first: bool) -> Result<(), Failure> {
    match event {
        Event::Decl(declaration) => {
            if !first {
                return Err(Failure::malformed(
                    line,
                    "an XML declaration that is not at the start of the document",
                ));
            }
            grammar::characters(declaration, line, "the XML declaration")?;
            grammar::declaration(declaration, line)
        }
        Event::PI(instruction) => {
            grammar::characters(instruction, line, "a processing instruction")?;
            grammar::instruction(instruction, line)
        }
        Event::Comment(comment) => grammar::characters(comment, line, "a comment"),
        _ => Ok(()),
    }
}

/// Checks a document type declaration (`raw`, all of it): in the prolog,
/// the only one, its characters and grammar.
fn document_type(raw: &str, line: usize, in_prolog: bool, seen: &mut bool) -> Result<(), Failure> {
    if !in_prolog {
        return Err(Failure::malformed(
            line,
            "a document type declaration inside the document",
        ));
    }
    if std::mem::replace(seen, true) {
        return Err(Failure::malformed(
            line,
            "a second document type declaration",
        ));
    }
    grammar::characters(raw, line, "the document type declaration")?;
    grammar::document_type(raw, line)
}

/// The text a reference stands for: a predefined entity or an XML
/// character.
fn replacement(reference: &BytesRef<'_>, line: usize) -> Result<String, Failure> {
    let replaced = match reference.resolve_char_ref() {
        Ok(Some(c)) if is_xml_char(c) => Some(c.to_string()),
        Ok(Some(_)) | Err(_) => None,
        Ok(None) => resolve_xml_entity(&reference.xml10_content()).map(str::to_owned),
    };
    replaced.ok_or_else(|| {
        Failure::malformed(
            line,
            format!(
                "the reference `&{};` is not a predefined entity or an XML character",
                crate::error::shown(&reference.xml10_content())
            ),
        )
    })
}

/// The element `tag` opens, its name made foreign when it is outside the
/// menu namespace.
fn open_element(
    tag: &BytesStart<'_>,
    line: usize,
    inherited: Option<String>,
) -> Result<Open, Failure> {
    let raw: String = tag.name().as_ref().to_owned();
    work::count(raw.len());
    let mut attributes = Vec::new();
    let mut namespace = inherited;
    for attribute in tag.attributes() {
        let attribute =
            attribute.map_err(|err| Failure::malformed(line, format!("bad attribute: {err}")))?;
        let key: String = attribute.key.as_ref().to_owned();
        let value = attribute
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map_err(|err| {
                Failure::malformed(line, format!("an attribute value cannot be read: {err}"))
            })?
            .into_owned();
        work::count(key.len() + value.len());
        if let Some(bad) = value.chars().find(|&c| !is_xml_char(c)) {
            return Err(Failure::malformed(
                line,
                format!(
                    "an attribute value holds U+{:04X}, which XML cannot",
                    u32::from(bad)
                ),
            ));
        }
        if key == "xmlns" {
            // An empty declaration undeclares the default namespace: names
            // are then in none, as with no declaration at all.
            namespace = (!value.is_empty()).then(|| value.clone());
        }
        attributes.push((key, value));
    }
    let foreign = raw.contains(':') || namespace.as_deref().is_some_and(|ns| ns != MENU_NAMESPACE);
    let name = if foreign {
        format!("{FOREIGN}{raw}")
    } else {
        raw
    };
    Ok(Open {
        element: Element {
            name,
            attributes,
            children: Vec::new(),
            line,
        },
        namespace,
    })
}

/// Adds a finished element to its parent, or makes it the root.
fn close(element: Element, stack: &mut [Open], root: &mut Option<Element>) {
    match stack.last_mut() {
        Some(parent) => parent.element.children.push(Child::Element(element)),
        None => *root = Some(element),
    }
}

/// Adds text to the open element; outside the root only whitespace may be.
fn push_text(text: &str, line: usize, stack: &mut [Open]) -> Result<(), Failure> {
    work::count(text.len());
    if let Some(bad) = text.chars().find(|&c| !is_xml_char(c)) {
        return Err(Failure::malformed(
            line,
            format!("the text holds U+{:04X}, which XML cannot", u32::from(bad)),
        ));
    }
    match stack.last_mut() {
        Some(open) => {
            if let Some(Child::Text(last)) = open.element.children.last_mut() {
                last.push_str(text);
            } else {
                open.element.children.push(Child::Text(text.to_owned()));
            }
            Ok(())
        }
        // Outside the root only XML's own whitespace (not every Unicode
        // space) may stand.
        None if grammar::is_space(text) => Ok(()),
        None => Err(Failure::malformed(line, "text outside the root element")),
    }
}

/// Whether `c` is a character XML documents may hold.
fn is_xml_char(c: char) -> bool {
    matches!(c, '\u{9}' | '\u{A}' | '\u{D}')
        || ('\u{20}'..='\u{D7FF}').contains(&c)
        || ('\u{E000}'..='\u{FFFD}').contains(&c)
        || c >= '\u{10000}'
}
