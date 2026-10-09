//! The per-type `MEDIA/SUBTYPE.xml` files (spec "The MEDIA/SUBTYPE.xml
//! files"), and the root element of XML documents for `XMLnamespaces`.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use quick_xml::escape::resolve_xml_entity;
use quick_xml::events::attributes::{AttrError, Attribute};
use quick_xml::events::{BytesRef, BytesStart, Event};
use quick_xml::name::{Namespace, PrefixDeclaration, QName, ResolveResult};
use quick_xml::{NsReader, XmlVersion};

/// The namespace of the database's XML files.
const NAMESPACE: &str = "http://www.freedesktop.org/standards/shared-mime-info";
use xdg_kit_locale::Locales;

use crate::error::quoted;
use crate::read::Problems;
use crate::{Diagnostic, MimeDatabase, MimeType};

/// Values by language (`None` for the untranslated one), a later value
/// replacing an earlier one for the same language.
#[derive(Debug, Default)]
struct Localized(HashMap<Option<String>, String>);

impl Localized {
    fn set(&mut self, lang: Option<String>, text: String) {
        self.0.insert(lang, text);
    }

    /// The value for the first of `locales`' keys present, else the
    /// untranslated one.
    fn pick(&self, locales: &Locales) -> Option<String> {
        let get = |lang: Option<&str>| self.0.get(&lang.map(str::to_owned)).cloned();
        locales.select(|key| get(Some(key))).or_else(|| get(None))
    }
}

/// What a type's XML files say, merged over the folders.
///
/// Each file is read using the XML version declared at its start, or 1.0
/// if there is no declaration. The root must be `mime-type` in the spec's
/// namespace, with a valid `QName`. Files with another root or an invalid
/// name (`<1p:mime-type>`, `<:mime-type>`, `<x:y:mime-type>`) are ignored
/// with a diagnostic ([`structure_problem`]).
///
/// A field is an element directly inside the root, in the spec's namespace
/// or in none, named `comment`, `acronym` or `expanded-acronym`. For this
/// check, the name is the part after the last `:`, or the whole name if
/// there is no `:`. A field is dropped with a diagnostic when:
/// - its text, inside tags in it too, holds a reference that cannot be
///   replaced, a character the version does not allow written as it is, or
///   `]]>`. Only references to the five entities XML predefines and
///   character references to a character the version allows can be
///   replaced;
/// - any attribute of its start tag, before or after `xml:lang`, or of a
///   tag inside it, is not well formed, appears twice, has a name that is
///   not an XML `QName`, has a value holding a reference that cannot be
///   replaced or is not ended, a character the version does not allow
///   written as it is, or `<`, or declares a reserved namespace
///   (`http://www.w3.org/XML/1998/namespace` or
///   `http://www.w3.org/2000/xmlns/`) the default (`xmlns="…"`; Namespaces
///   in XML 1.0, 3), or, in an XML 1.0 file, a prefix's namespace empty
///   (`xmlns:p=""`). These two are checked on the field's own tag and the
///   tags inside it only: a declaration further up is not, so a field
///   whose name it binds is not recognised as one, and a tag inside a field
///   whose name it binds is read as any other (a known limit);
/// - its tag's name, or that of a tag inside it, is not a `QName`
///   (`<x:y:comment>`, `<1p:comment>`, `<:comment>`, or `<x:y:z>` inside
///   it; Namespaces in XML 1.0, 5 and 7) or has the prefix `xmlns` (3):
///   one check for both kinds of tag, made before their attributes are
///   looked at, and on the field's own tag whatever namespace its prefix
///   is bound to, or none;
/// - its namespace's declaration, on its tag or further up, has a value
///   that cannot be read as above: it is not taken for an element of
///   another namespace;
/// - its tag declares a namespace twice, under a name that is not a `QName`
///   (`xmlns:`), or, as above, a prefix's namespace empty
///   (`<p:comment xmlns:p="">`) or a reserved namespace the default; or it
///   has an attribute `quick-xml` cannot read, which stops `quick-xml`
///   reading the declarations after it (`<p:comment a xmlns:p="…">`):
///   whatever namespace `quick-xml` then puts it in, or none, it is not
///   taken for an element of another namespace;
/// - the file ends, or `quick-xml` finds it not well formed, before the
///   field's end: the rest of the file is skipped.
///
/// Entities declared in a file's own DTD are not expanded; fields that use
/// them are also dropped. Some problems go unreported because `quick-xml`
/// does not detect them: duplicate attributes under two prefixes bound to
/// the same namespace (`p:a` and `q:a`), unbound prefixes, and declarations
/// with no version. An attribute or a tag inside a field with an unbound
/// prefix is read as any other; an element with one is not a field, unless
/// its tag is as described above. These are known limits.
///
/// XML 1.1 makes U+0085 and U+2028 line ends (spec 1.1, 2.11): in text, in
/// attribute values and before or after the root element they are read as
/// such. Inside a tag or the declaration they are not white space here, as
/// `quick-xml` takes only ASCII white space there, so a tag that uses them
/// is misread (a known limit): an attribute name that takes one in is not
/// an XML name, so its field is dropped; an element name that takes one in
/// is another name, so the element is not the one meant, and for the root,
/// inside a field, or when its name after its last `:` is still a field's
/// (`<p\u{85}:comment>`), is not an XML name, so the file is ignored or the
/// field dropped.
#[derive(Debug, Default)]
pub(crate) struct TypeInfo {
    comment: Localized,
    /// Untranslated only: spec 0.22 dropped `xml:lang` on the acronyms.
    acronym: Option<String>,
    expanded_acronym: Option<String>,
    diagnostics: Vec<Diagnostic>,
    /// Whether any folder has a file for the type.
    found: bool,
}

impl TypeInfo {
    /// Reads `mime`'s XML file from each of `dirs`, least important first,
    /// so a more important folder's value replaces a less important one for
    /// the same language.
    pub(crate) fn read<'a>(mime: &MimeType, dirs: impl Iterator<Item = &'a Path>) -> Self {
        let mut info = Self::default();
        for dir in dirs {
            let Some(path) = type_file(dir, mime) else {
                continue;
            };
            info.found = true;
            match xdg_kit_basedir::read_regular_file(&path) {
                Ok(bytes) => info.parse(&path, &bytes),
                Err(err) => info.diagnostics.push(Diagnostic::new(
                    &path,
                    None,
                    format!("cannot be read: {err}"),
                )),
            }
        }
        info
    }

    /// Parses one file's contents, for the fuzz target.
    #[cfg(fuzzing)]
    pub(crate) fn parse_bytes(bytes: &[u8]) -> Self {
        let mut info = Self::default();
        info.parse(Path::new("type.xml"), bytes);
        info
    }

    /// Picks every value for a few languages and formats the diagnostics,
    /// for the fuzz target.
    #[cfg(fuzzing)]
    pub(crate) fn pick_all(&self) {
        for locales in [
            Locales::untranslated(),
            Locales::new(["de_AT", "sr@latin", "pt_BR.UTF-8"]),
        ] {
            let _ = self.comment.pick(&locales);
        }
        for diagnostic in &self.diagnostics {
            let _ = diagnostic.to_string();
        }
    }

    pub(crate) fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Whether any folder has a file for the type.
    pub(crate) fn found(&self) -> bool {
        self.found
    }

    fn parse(&mut self, path: &Path, bytes: &[u8]) {
        let mut problems = Problems::new(path, &mut self.diagnostics);
        let mut reader = NsReader::from_reader(bytes);
        // `<comment/>` is read as `<comment></comment>`: an empty value.
        reader.config_mut().expand_empty_elements = true;
        // `quick-xml` skips a byte order mark without counting it, so its
        // length is added to make reported offsets offsets in the file.
        let bom = skipped_mark(bytes);
        let offset = |at: u64| usize::try_from(at).unwrap_or(usize::MAX);
        let location = |at: usize| Some(crate::Location::Offset(at.saturating_add(bom)));
        let mut depth = 0usize;
        let mut seen = Seen::default();
        // The element being collected and its language.
        let mut field: Option<(Field, Option<String>, String)> = None;
        // The version the declaration at the start gives, if any.
        let mut version = XmlVersion::Implicit1_0;
        loop {
            let at = offset(reader.buffer_position());
            let (namespace, event) = match reader.read_resolved_event() {
                Ok(resolved) => resolved,
                Err(err) => {
                    // A namespace error is in the tag being read.
                    let at = match err {
                        quick_xml::Error::Namespace(_) => at,
                        _ => offset(reader.error_position()),
                    };
                    problems.report(
                        location(at),
                        format!("bad XML ({}); the rest is skipped", reader_error(&err)),
                    );
                    return;
                }
            };
            let problem = structure_problem(&event, &namespace, at, depth, &seen, version);
            seen.doctype_read |= matches!(event, Event::DocType(_));
            seen.run_reported = matches!(
                event,
                Event::Text(_) | Event::GeneralRef(_) | Event::CData(_)
            ) && (seen.run_reported || problem.is_some());
            if let Some(problem) = problem {
                problems.report(problem.at.and_then(location), problem.message);
                if problem.rest_skipped {
                    return;
                }
            }
            // A value that cannot be decoded is dropped, as an unfinished
            // one is.
            if let Some((_, _, value)) = &mut field
                && let Err(problem) = add_to_value(value, &event, version)
            {
                problems.report(
                    location(at),
                    format!("bad XML ({problem}); the value is left out"),
                );
                field = None;
            }
            match event {
                // Only a declaration at the start is one (only a byte order
                // mark, which `quick-xml` does not count, may come before).
                Event::Decl(declaration) if at == 0 => {
                    if let Ok(declared) = declaration.xml_version() {
                        version = declared;
                    }
                }
                Event::Start(start) => {
                    depth += 1;
                    let read = if depth == 2 {
                        Field::starting(&start, &namespace, version)
                    } else {
                        // A tag inside a field is read as the field's own.
                        field
                            .as_ref()
                            .map(|&(kind, ..)| (kind, inner_tag(&start, version)))
                    };
                    // A field whose tag, or a tag inside it, cannot be read
                    // is dropped, as one whose value cannot be decoded.
                    match read {
                        Some((kind, Ok(lang))) if depth == 2 => {
                            field = Some((kind, lang, String::new()));
                        }
                        Some((_, Err(problem))) => {
                            field = None;
                            problems.report(
                                location(at + problem.at.map_or(0, |inside| inside + 1)),
                                format!("bad XML ({}); the value is left out", problem.message),
                            );
                        }
                        _ => {}
                    }
                }
                Event::End(_) => {
                    if depth == 2
                        && let Some((kind, lang, value)) = field.take()
                    {
                        match kind {
                            Field::Comment => self.comment.set(lang, value),
                            // Translated acronyms (allowed before spec 0.22)
                            // are left out.
                            Field::Acronym if lang.is_none() => self.acronym = Some(value),
                            Field::ExpandedAcronym if lang.is_none() => {
                                self.expanded_acronym = Some(value);
                            }
                            Field::Acronym | Field::ExpandedAcronym => {}
                        }
                    }
                    depth = depth.saturating_sub(1);
                    seen.ended = depth == 0;
                }
                Event::Eof => return,
                _ => {}
            }
        }
    }
}

/// Adds what `event`, read inside a field in a document of XML `version`,
/// stands for to the field's `value`: text and CDATA as written, line ends
/// normalised, and what a reference stands for. Gives what is wrong when
/// it cannot be decoded: a reference that cannot be replaced, a character
/// the version allows only as a reference, or not at all, or `]]>` in text
/// (spec 2.4, `CharData`), which `quick-xml` reads as text.
fn add_to_value(value: &mut String, event: &Event<'_>, version: XmlVersion) -> Result<(), String> {
    let written = match event {
        Event::Text(text) if text.contains("]]>") => return Err("`]]>` in text".to_owned()),
        Event::Text(text) => text.xml_content(version),
        Event::CData(data) => data.xml_content(version),
        Event::GeneralRef(reference) => {
            let name = reference.xml10_content();
            value.push(replacement(reference, version).ok_or_else(|| not_replaced(&name))?);
            return Ok(());
        }
        _ => return Ok(()),
    };
    if let Some(problem) = written_problem(&written, version) {
        return Err(problem);
    }
    value.push_str(&written);
    Ok(())
}

/// What `reference` (a character reference or a reference to an entity
/// XML predefines) stands for, if it can be replaced in a document of XML
/// `version`. XML predefines five entities, each one character (spec 4.6);
/// `quick-xml`'s `resolve_predefined_entity` takes every HTML5 one too with
/// its `escape-html` feature, which another crate in a build may turn on.
fn replacement(reference: &BytesRef<'_>, version: XmlVersion) -> Option<char> {
    match reference.resolve_char_ref() {
        Ok(Some(c)) => is_xml_char(c, version).then_some(c),
        Ok(None) => resolve_xml_entity(&reference.xml10_content())?
            .chars()
            .next(),
        Err(_) => None,
    }
}

/// The problem with a reference to `name` that cannot be replaced.
fn not_replaced(name: &str) -> String {
    format!("the reference `&{};` cannot be replaced", quoted(name))
}

/// What `err`, which `quick-xml` met reading a file, says, the text from
/// the file it holds as [`quoted`] shows it.
fn reader_error(err: &quick_xml::Error) -> String {
    use quick_xml::Error;
    use quick_xml::errors::IllFormedError as Ill;
    use quick_xml::escape::EscapeError as Escape;
    use quick_xml::name::NamespaceError as Ns;
    let bound = |prefix: &str, namespace: &str| {
        format!("the namespace prefix '{prefix}' cannot be bound to '{namespace}'")
    };
    match err {
        Error::IllFormed(Ill::MissingDeclVersion(Some(name))) => format!(
            "ill-formed document: an XML declaration starts with `{}`, not `version`",
            quoted(name)
        ),
        Error::IllFormed(Ill::MissingEndTag(name)) => format!(
            "ill-formed document: start tag not closed: `</{}>` not found before end of input",
            quoted(name)
        ),
        Error::IllFormed(Ill::UnmatchedEndTag(name)) => format!(
            "ill-formed document: close tag `</{}>` does not match any open tag",
            quoted(name)
        ),
        Error::IllFormed(Ill::MismatchedEndTag { expected, found }) => {
            let (expected, found) = crate::error::quoted_pair(expected, found);
            format!("ill-formed document: expected `</{expected}>`, but `</{found}>` was found")
        }
        Error::Namespace(Ns::UnknownPrefix(prefix)) => {
            format!("unknown namespace prefix '{}'", quoted(prefix))
        }
        Error::Namespace(Ns::InvalidXmlPrefixBind(namespace)) => bound("xml", &quoted(namespace)),
        Error::Namespace(Ns::InvalidXmlnsPrefixBind(namespace)) => {
            bound("xmlns", &quoted(namespace))
        }
        Error::Namespace(Ns::InvalidPrefixForXml(prefix)) => {
            bound(&quoted(prefix), "http://www.w3.org/XML/1998/namespace")
        }
        Error::Namespace(Ns::InvalidPrefixForXmlns(prefix)) => {
            bound(&quoted(prefix), "http://www.w3.org/2000/xmlns/")
        }
        Error::Escape(Escape::UnrecognizedEntity(at, name)) => {
            format!("at {at:?}: unrecognized entity `{}`", quoted(name))
        }
        // The others hold no text from the file.
        Error::Io(_)
        | Error::Syntax(_)
        | Error::InvalidAttr(_)
        | Error::Encoding(_)
        | Error::IllFormed(
            Ill::MissingDeclVersion(None)
            | Ill::UnknownVersion
            | Ill::MissingDoctypeName
            | Ill::DoubleHyphenInComment
            | Ill::UnclosedReference,
        )
        | Error::Namespace(Ns::TooManyBindings(_) | Ns::TooDeeplyNested(_))
        | Error::Escape(
            Escape::UnterminatedEntity(_)
            | Escape::InvalidCharRef(_)
            | Escape::TooManyNestedEntities,
        ) => err.to_string(),
    }
}

/// The problem with `written`, text as written in a document of XML
/// `version`, if any: a character the version allows only as a reference,
/// or not at all ([`may_be_written`]).
fn written_problem(written: &str, version: XmlVersion) -> Option<String> {
    let bad = written.chars().find(|&c| !may_be_written(c, version))?;
    Some(format!(
        "a character XML does not allow, U+{:04X}",
        u32::from(bad)
    ))
}

/// Whether XML `version` allows `c` in a document (its `Char` production);
/// a character reference to any other is bad. `quick-xml` only refuses
/// `&#0;` and what is not a Unicode scalar value. XML 1.1 adds the controls
/// but U+0000 (spec 1.1, 2.2).
fn is_xml_char(c: char, version: XmlVersion) -> bool {
    match version {
        XmlVersion::Explicit1_1 => {
            matches!(c, '\u{1}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..)
        }
        XmlVersion::Implicit1_0 | XmlVersion::Explicit1_0 => {
            matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..)
        }
    }
}

/// Whether XML `version` allows `c` written as it is, not as a character
/// reference: XML 1.1 allows its `RestrictedChar`s (the controls other than
/// tab, line feed, carriage return and U+0085) only as references (spec
/// 1.1, 2.1 production `document` and 2.2). `quick-xml` checks neither
/// rule.
fn may_be_written(c: char, version: XmlVersion) -> bool {
    let restricted = version == XmlVersion::Explicit1_1
        && matches!(
            c,
            '\u{1}'..='\u{8}'
                | '\u{B}'..='\u{C}'
                | '\u{E}'..='\u{1F}'
                | '\u{7F}'..='\u{84}'
                | '\u{86}'..='\u{9F}'
        );
    is_xml_char(c, version) && !restricted
}

/// How many bytes at the start of `bytes` `quick-xml` skips as a byte order
/// mark without counting them in its offsets.
///
/// Measured rather than assumed, as it depends on `quick-xml`'s features,
/// which another crate in the build may turn on: a UTF-8 mark is always
/// skipped, UTF-16 ones only with `encoding` (`quick-xml` 0.42
/// `src/reader/mod.rs:294-304`, `src/encoding.rs:153-161`), by moving past
/// it in the input without counting it (`src/reader/slice_reader.rs:253-268`).
/// A reader is run over the text up to the first `<` or `&`, which holds
/// any mark. With no markup or reference in it, the reader reads that text
/// whole, as text or as an error if it is not UTF-8, and counts each byte
/// it moves past (`src/reader/slice_reader.rs:273`, `296-303`), so what it
/// moved past but did not count is the mark.
fn skipped_mark(bytes: &[u8]) -> usize {
    let text = bytes
        .iter()
        .position(|&byte| matches!(byte, b'<' | b'&'))
        .map_or(bytes, |end| &bytes[..end]);
    let mut reader = quick_xml::Reader::from_reader(text);
    let _ = reader.read_event();
    let counted = usize::try_from(reader.buffer_position()).unwrap_or(usize::MAX);
    let read = text.len() - reader.get_ref().len();
    read.saturating_sub(counted)
}

/// What was read of a per-type file so far, for [`structure_problem`].
#[derive(Default)]
struct Seen {
    /// Whether the root element has ended.
    ended: bool,
    /// Whether the run of text being read before the root was reported.
    run_reported: bool,
    /// Whether a document type was read before the root.
    doctype_read: bool,
}

/// A problem with the structure of a per-type file.
struct Problem {
    /// Where, as `quick-xml` counts offsets: from after any byte order mark.
    at: Option<usize>,
    message: String,
    /// Whether the rest of the file is skipped.
    rest_skipped: bool,
}

impl Problem {
    fn new(at: Option<usize>, message: impl Into<String>, rest_skipped: bool) -> Self {
        Self {
            at,
            message: message.into(),
            rest_skipped,
        }
    }
}

/// The problem with the structure of a per-type file that `event` shows, if
/// any.
///
/// `event` is in `namespace` and was read at offset `at` (as `quick-xml`
/// counts) with `depth` elements open, after what `seen` records (its
/// `run_reported` is about the run of text before the root that `event`
/// may continue). `version` is the document's XML version, by which white
/// space is told (XML 1.1 reads U+0085 and U+2028 as line ends, spec 1.1,
/// 2.11).
///
/// A per-type file is, in order:
/// - optionally, an XML declaration at the very start (only a byte order
///   mark may come before it);
/// - white space, comments, processing instructions and at most one
///   document type;
/// - the root element: `mime-type` in the spec's namespace;
/// - white space, comments and processing instructions.
///
/// A file with any other root, or one whose name is not a `QName`
/// (`<1p:mime-type>`, `<:mime-type>`, `<x:y:mime-type>`; Namespaces in XML
/// 1.0, 3rd ed., 5 and 7), is not a per-type file, and is ignored. An
/// XML declaration or a document type anywhere else is a problem: inside
/// the root, the values are still read; after it, the rest is skipped, as
/// for other content there.
fn structure_problem(
    event: &Event<'_>,
    namespace: &ResolveResult<'_>,
    at: usize,
    depth: usize,
    seen: &Seen,
    version: XmlVersion,
) -> Option<Problem> {
    const BEFORE: &str = "bad XML (content before the root element)";
    const AFTER: &str = "bad XML (content after the root element); the rest is skipped";
    const LATE: &str = "bad XML (an XML declaration not at the start of the file)";
    const DOCTYPE: &str = "bad XML (a document type declaration not before the root element)";
    let Seen {
        ended,
        run_reported,
        doctype_read,
    } = *seen;
    // The offset of the first character other than white space.
    let text_at = |text: &str| {
        let blank: usize = text
            .chars()
            .take_while(|&c| {
                matches!(c, ' ' | '\t' | '\r' | '\n')
                    || version == XmlVersion::Explicit1_1 && matches!(c, '\u{85}' | '\u{2028}')
            })
            .map(char::len_utf8)
            .sum();
        (blank < text.len()).then_some(at + blank)
    };
    let problem = |message: &str, rest_skipped| Some(Problem::new(Some(at), message, rest_skipped));
    // The arms are checked in order: inside the root (`depth > 0`), then
    // before its end (`!ended`), then after it.
    match event {
        // What was completed is kept; a value cut off is not.
        Event::Eof if depth > 0 => problem(
            "bad XML (the file ends before the root element is closed)",
            false,
        ),
        Event::Eof if !ended => Some(Problem::new(None, "bad XML (no root element)", false)),
        Event::Decl(_) if depth > 0 => problem(LATE, false),
        Event::DocType(_) if depth > 0 => problem(DOCTYPE, false),
        _ if depth > 0 => None,
        Event::Start(element) if !ended => {
            let name = element.name();
            let what = if !is_qname(name.as_ref()) {
                format!(
                    "the root element's name `{}` is not an XML name",
                    quoted(name.as_ref())
                )
            } else if element.local_name().as_ref() != "mime-type" {
                format!(
                    "the root element is `{}`, not `mime-type`",
                    quoted(name.as_ref())
                )
            } else if matches!(namespace, ResolveResult::Bound(bound) if is_ours(bound, version)) {
                return None;
            } else {
                format!(
                    "the root element `{}` is not in the namespace `{NAMESPACE}`",
                    quoted(name.as_ref())
                )
            };
            problem(&format!("not a per-type file: {what}; it is ignored"), true)
        }
        Event::Decl(_) if !ended && at != 0 => problem(LATE, false),
        Event::DocType(_) if !ended && doctype_read => {
            problem("bad XML (a second document type declaration)", false)
        }
        // One problem for each run of text, references, CDATA and all.
        Event::Text(_) | Event::GeneralRef(_) | Event::CData(_) if !ended && run_reported => None,
        Event::Text(text) if !ended => {
            text_at(text).map(|at| Problem::new(Some(at), BEFORE, false))
        }
        Event::CData(_) | Event::GeneralRef(_) if !ended => problem(BEFORE, false),
        _ if !ended => None,
        Event::Start(_) => problem("bad XML (a second root element); the rest is skipped", true),
        Event::Decl(_) => problem(&format!("{LATE}; the rest is skipped"), true),
        Event::DocType(_) => problem(&format!("{DOCTYPE}; the rest is skipped"), true),
        Event::Text(text) => text_at(text).map(|at| Problem::new(Some(at), AFTER, true)),
        Event::Comment(_) | Event::PI(_) | Event::Eof => None,
        _ => problem(AFTER, true),
    }
}

/// The value of `bound`, a namespace as `quick-xml` resolved it (the
/// declaring attribute's value as written), in a document of XML `version`,
/// normalised as an attribute value ([`attribute_value`]); or what is wrong
/// with it.
fn namespace_value<'a>(bound: &Namespace<'a>, version: XmlVersion) -> Result<Cow<'a, str>, String> {
    // Built as read: `Attribute::from` would escape the value.
    let attribute = Attribute {
        key: QName("xmlns"),
        value: bound.0.into(),
    };
    attribute_value(&attribute, version)
}

/// Whether `bound` ([`namespace_value`]) is the spec's namespace: one that
/// cannot be read is another namespace.
fn is_ours(bound: &Namespace<'_>, version: XmlVersion) -> bool {
    namespace_value(bound, version).is_ok_and(|value| value == NAMESPACE)
}

#[derive(Debug, Clone, Copy)]
enum Field {
    Comment,
    Acronym,
    ExpandedAcronym,
}

impl Field {
    /// The field that `start`, an element in `namespace` directly inside
    /// the root of a document of XML `version`, begins, if any, and its
    /// language ([`language`]).
    ///
    /// A field is named by what follows the last `:` in the element's
    /// name, or all of it without one (`<x:y:comment>` is a comment, whose
    /// name is not a `QName`; `<comment:>` is not a field). Only the spec's
    /// own elements (or ones in no namespace) are fields; an extension's
    /// `<x:comment>` is not the comment. An element whose name is not a
    /// `QName` or has the prefix `xmlns` ([`name_problem`]), or whose
    /// namespace cannot be read, is a field whose tag cannot be read; so is
    /// one in another namespace, or a prefix no declaration binds, with an
    /// attribute `quick-xml` cannot read ([`unreadable_attribute`], the
    /// problem given) or own namespace declarations that cannot be read
    /// ([`declaration_problem`]), as which namespace it is in is then not
    /// known.
    fn starting(
        start: &BytesStart<'_>,
        namespace: &ResolveResult<'_>,
        version: XmlVersion,
    ) -> Option<(Self, Result<Option<String>, TagProblem>)> {
        // Named by what follows the last `:`, so `<x:y:comment>`, not a
        // `QName`, is a field whose tag cannot be read.
        let name = start.name();
        let kind = match name.as_ref().rsplit(':').next() {
            Some("comment") => Self::Comment,
            Some("acronym") => Self::Acronym,
            Some("expanded-acronym") => Self::ExpandedAcronym,
            _ => return None,
        };
        if let Some(problem) = name_problem(start) {
            return Some((kind, Err(problem)));
        }
        let namespace = match namespace {
            ResolveResult::Bound(bound) => match namespace_value(bound, version) {
                Ok(value) if value != NAMESPACE => None,
                read => Some(read.map(drop)),
            },
            ResolveResult::Unbound => Some(Ok(())),
            ResolveResult::Unknown(_) => None,
        };
        let Some(namespace) = namespace else {
            // `quick-xml` reads the tag's namespace declarations only up to
            // an attribute it cannot read, so past one which namespace the
            // element is in is not known.
            return unreadable_attribute(start)
                .or_else(|| declaration_problem(start, version))
                .map(|problem| (kind, Err(problem)));
        };
        // A problem in the tag's own attributes is reported where it is,
        // even when the namespace's declaration is one of them; one in a
        // declaration further up at the tag.
        let lang = language(start, version).and_then(|lang| {
            namespace
                .map(|()| lang)
                .map_err(|message| TagProblem { at: None, message })
        });
        Some((kind, lang))
    }
}

/// What is wrong with a field's start tag.
struct TagProblem {
    /// Where, as an offset in the tag from after its `<` (`None` for the
    /// `<`).
    at: Option<usize>,
    message: String,
}

/// How a problem with an attribute starts.
const UNREADABLE: &str = "an attribute cannot be read: ";

/// The problem with an attribute whose name is not a `QName`.
fn not_a_name() -> String {
    format!("{UNREADABLE}a name is not an XML name")
}

/// The problem with an attribute `name` that appears twice.
fn twice(name: &str) -> String {
    format!("{UNREADABLE}an attribute appears twice: {}", quoted(name))
}

/// The first problem with `start`'s own namespace declarations (its
/// `xmlns` and `xmlns:` attributes) in a document of XML `version`, if
/// any: one appears twice, its name is not a `QName` (`xmlns:`), in XML
/// 1.0 it declares a prefix's namespace empty ([`empty_prefix`]), or it
/// declares a reserved namespace the default ([`reserved_default`]). Other
/// attributes are not looked at.
fn declaration_problem(start: &BytesStart<'_>, version: XmlVersion) -> Option<TagProblem> {
    let mut seen = HashSet::new();
    start
        .attributes()
        .with_checks(false)
        .filter_map(Result::ok)
        .map(|attribute| (attribute.key.into_inner(), attribute.value))
        .filter(|(name, _)| *name == "xmlns" || name.starts_with("xmlns:"))
        .find_map(|(name, value)| {
            let message = if !is_qname(name) {
                not_a_name()
            } else if !seen.insert(name) {
                twice(name)
            } else {
                empty_prefix(name, &value, version).or_else(|| {
                    reserved_default(name, &namespace_value(&Namespace(&value), version).ok()?)
                })?
            };
            Some(TagProblem {
                at: offset_in(start, name),
                message,
            })
        })
}

/// The problem with an attribute `name` that declares a prefix's namespace
/// as `value` in a document of XML `version`, if it is empty and the
/// version XML 1.0 (Namespaces in XML 1.0, 3rd ed., 5, "No Prefix
/// Undeclaring"; 1.1 allows it, Namespaces in XML 1.1, 2nd ed., 6.1). A
/// reference always stands for a character, so only a value written empty
/// is empty.
fn empty_prefix(name: &str, value: &str, version: XmlVersion) -> Option<String> {
    (value.is_empty() && name.starts_with("xmlns:") && version != XmlVersion::Explicit1_1)
        .then(|| format!("{UNREADABLE}a prefix's namespace declaration is empty in XML 1.0"))
}

/// The language `start`'s `xml:lang` gives in a document of XML `version`
/// (`None` without one, or for an empty one), normalised as an attribute
/// value. Gives what is wrong when any of `start`'s attributes, before or
/// after it, cannot be read: it is not well formed or appears twice
/// ([`attribute_error`]), its name is not a `QName` ([`is_qname`]), its
/// value cannot be read ([`attribute_value`]), in XML 1.0 it declares a
/// prefix's namespace empty ([`empty_prefix`]), or it declares a reserved
/// namespace the default ([`reserved_default`]). The problem is where
/// `quick-xml` finds it, or at the start of the attribute.
fn language(start: &BytesStart<'_>, version: XmlVersion) -> Result<Option<String>, TagProblem> {
    let mut lang = None;
    // Where the attribute being read starts, at the earliest: after the
    // element's name, then after each value's closing quote.
    let mut after = start.name().as_ref().len();
    for attribute in start.attributes().with_checks(true) {
        let attribute = attribute.map_err(|err| attribute_error(start, &err, after))?;
        let name = attribute.key.as_ref();
        let problem = |message| TagProblem {
            at: offset_in(start, name),
            message,
        };
        if !is_qname(name) {
            return Err(problem(not_a_name()));
        }
        let value = attribute_value(&attribute, version).map_err(problem)?;
        if let Some(message) =
            empty_prefix(name, &value, version).or_else(|| reserved_default(name, &value))
        {
            return Err(problem(message));
        }
        if name == "xml:lang" {
            lang = Some(value.into_owned());
        }
        after =
            offset_in(start, &attribute.value).map_or(after, |at| at + attribute.value.len() + 1);
    }
    Ok(lang.filter(|lang| !lang.is_empty()))
}

/// The problem with the first of `start`'s attributes that `quick-xml`
/// cannot read, an attribute that appears twice aside, if any
/// ([`attribute_error`]).
fn unreadable_attribute(start: &BytesStart<'_>) -> Option<TagProblem> {
    // As in `language`.
    let mut after = start.name().as_ref().len();
    for attribute in start.attributes().with_checks(false) {
        let attribute = match attribute {
            Ok(attribute) => attribute,
            Err(err) => return Some(attribute_error(start, &err, after)),
        };
        after =
            offset_in(start, &attribute.value).map_or(after, |at| at + attribute.value.len() + 1);
    }
    None
}

/// The problem `err`, which `quick-xml` met reading `start`'s attributes
/// from `after` on, and where it is.
fn attribute_error(start: &BytesStart<'_>, err: &AttrError, after: usize) -> TagProblem {
    let (at, what) = match *err {
        // `quick-xml` gives what follows the name: the name is the first
        // thing after the attribute before.
        AttrError::ExpectedEq(at) => {
            let name = start
                .get(after..)
                .and_then(|rest| rest.find(|c| !matches!(c, ' ' | '\t' | '\r' | '\n')))
                .map_or(at, |blank| after + blank);
            (name, "a name is not followed by `=`".to_owned())
        }
        AttrError::ExpectedValue(at) => (at, "`=` is not followed by a value".to_owned()),
        AttrError::UnquotedValue(at) => (at, "a value is not quoted".to_owned()),
        AttrError::ExpectedQuote(at, _) => (at, "a value is not closed".to_owned()),
        AttrError::Duplicated(at, _) => {
            let name = start
                .get(at..)
                .and_then(|key| key.split(['=', ' ', '\t', '\r', '\n']).next())
                .unwrap_or_default();
            return TagProblem {
                at: Some(at),
                message: twice(name),
            };
        }
    };
    TagProblem {
        at: Some(at),
        message: format!("{UNREADABLE}{what}"),
    }
}

/// Where `part`, a slice of `whole`, starts in it, if it is one.
fn offset_in(whole: &str, part: &str) -> Option<usize> {
    let at = part.as_ptr().addr().checked_sub(whole.as_ptr().addr())?;
    (at + part.len() <= whole.len()).then_some(at)
}

/// Whether `name` is a `QName` (Namespaces in XML 1.0, 3rd ed., 4; 1.1 has
/// the same): a `Name` (XML 1.0, 5th ed., and 1.1, 2.3, the same production
/// in both) with at most one `:`, neither first nor last.
fn is_qname(name: &str) -> bool {
    let is_part = |part: &str| {
        let mut chars = part.chars();
        chars.next().is_some_and(|c| c != ':' && is_name_start(c))
            && chars.all(|c| c != ':' && is_name_char(c))
    };
    match name.split_once(':') {
        Some((prefix, local)) => is_part(prefix) && is_part(local),
        None => is_part(name),
    }
}

/// Whether `c` may start a `Name` (XML 1.0, 5th ed., and 1.1, 2.3,
/// `NameStartChar`).
fn is_name_start(c: char) -> bool {
    matches!(c,
        ':' | 'A'..='Z' | '_' | 'a'..='z' | '\u{C0}'..='\u{D6}' | '\u{D8}'..='\u{F6}'
        | '\u{F8}'..='\u{2FF}' | '\u{370}'..='\u{37D}' | '\u{37F}'..='\u{1FFF}'
        | '\u{200C}'..='\u{200D}' | '\u{2070}'..='\u{218F}' | '\u{2C00}'..='\u{2FEF}'
        | '\u{3001}'..='\u{D7FF}' | '\u{F900}'..='\u{FDCF}' | '\u{FDF0}'..='\u{FFFD}'
        | '\u{10000}'..='\u{EFFFF}'
    )
}

/// Whether `c` may be in a `Name` (XML 1.0, 5th ed., and 1.1, 2.3,
/// `NameChar`).
fn is_name_char(c: char) -> bool {
    is_name_start(c)
        || matches!(c, '-' | '.' | '0'..='9' | '\u{B7}' | '\u{300}'..='\u{36F}' | '\u{203F}'..='\u{2040}')
}

/// The problem with an attribute `name` that declares the default namespace
/// as `value`, normalised as an attribute value, if it is one of the two
/// that no default may be (Namespaces in XML 1.0, 3rd ed., 3; 1.1 has the
/// same).
fn reserved_default(name: &str, value: &str) -> Option<String> {
    const RESERVED: [&str; 2] = [
        "http://www.w3.org/XML/1998/namespace",
        "http://www.w3.org/2000/xmlns/",
    ];
    (name == "xmlns" && RESERVED.contains(&value))
        .then(|| format!("{UNREADABLE}the default namespace cannot be `{value}`"))
}

/// The problem with `start`'s name, at it, if it is not a `QName`
/// (Namespaces in XML 1.0, 3rd ed., 5 and 7) or has the prefix `xmlns`,
/// which no element's may (3; 1.1 has the same), the first checked first.
fn name_problem(start: &BytesStart<'_>) -> Option<TagProblem> {
    let name = start.name();
    let message = if !is_qname(name.as_ref()) {
        "an element's name is not an XML name"
    } else if name.prefix()?.as_ref() == "xmlns" {
        "an element's name has the prefix `xmlns`"
    } else {
        return None;
    };
    Some(TagProblem {
        at: Some(0),
        message: message.to_owned(),
    })
}

/// [`language`] for `start`, a tag inside a field, after the same check
/// of its name as a field's own tag has ([`name_problem`]).
fn inner_tag(start: &BytesStart<'_>, version: XmlVersion) -> Result<Option<String>, TagProblem> {
    name_problem(start).map_or_else(|| language(start, version), Err)
}

/// `attribute`'s value in a document of XML `version`, normalised as an
/// attribute value; or what is wrong with it, as with a field's text
/// ([`add_to_value`]): a reference that cannot be replaced or is not ended,
/// or a character the version allows only as a reference, or not at all;
/// or `<`, which XML does not allow there (spec 2.3, `AttValue`) but
/// `quick-xml` reads. Entities declared in a document's own DTD are not
/// replaced (a known limit).
fn attribute_value<'a>(
    attribute: &Attribute<'a>,
    version: XmlVersion,
) -> Result<Cow<'a, str>, String> {
    // References are looked at as `normalized_value_with` finds them: from
    // each `&` to the next `;`. What they stand for is not kept here:
    // `normalized_value_with` gives it.
    let mut rest = &*attribute.value;
    loop {
        let (written, reference) = match rest.split_once('&') {
            Some((written, after)) => (written, Some(after)),
            None => (rest, None),
        };
        if written.contains('<') {
            return Err(format!("{UNREADABLE}`<` in a value"));
        }
        if let Some(problem) = written_problem(written, version) {
            return Err(problem);
        }
        let Some(reference) = reference else {
            break;
        };
        let Some((name, after)) = reference.split_once(';') else {
            return Err(format!(
                "the reference `&{}` is not ended",
                quoted(reference)
            ));
        };
        if replacement(&BytesRef::new(name), version).is_none() {
            return Err(not_replaced(name));
        }
        rest = after;
    }
    // Only XML's own entities, which stand for characters that are not
    // normalised, so one level is enough (as for `normalized_value`).
    attribute
        .normalized_value_with(version, 1, resolve_xml_entity)
        .map_err(|err| reader_error(&err))
}

/// Where `mime`'s file in `dir` may be: `<dir>/<media>/<subtype>.xml` as
/// spelled, then lowercased (as `update-mime-database` names the files).
pub(crate) fn type_file_paths(dir: &Path, mime: &MimeType) -> [PathBuf; 2] {
    [
        dir.join(mime.media())
            .join(format!("{}.xml", mime.subtype())),
        dir.join(mime.media().to_ascii_lowercase())
            .join(format!("{}.xml", mime.subtype().to_ascii_lowercase())),
    ]
}

/// The first of [`type_file_paths`] that exists.
fn type_file(dir: &Path, mime: &MimeType) -> Option<PathBuf> {
    type_file_paths(dir, mime)
        .into_iter()
        .find(|path| path.is_file())
}

/// The types with an XML file in `dir`, for a folder without a `types` file.
pub(crate) fn types_in(dir: &Path) -> Vec<MimeType> {
    let mut types = Vec::new();
    let Ok(media_dirs) = std::fs::read_dir(dir) else {
        return types;
    };
    for media_dir in media_dirs.flatten() {
        let media = media_dir.file_name();
        let Some(media) = media.to_str() else {
            continue;
        };
        if media == "packages" || !media_dir.path().is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(media_dir.path()) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name();
            let Some(subtype) = name.to_str().and_then(|name| name.strip_suffix(".xml")) else {
                continue;
            };
            let Ok(mime) = MimeType::new(&format!("{media}/{subtype}")) else {
                continue;
            };
            // Regular files only (links followed), as `type_file` finds them;
            // only a link needs another look.
            let regular = match file.file_type() {
                Ok(kind) if kind.is_symlink() => file.path().is_file(),
                Ok(kind) => kind.is_file(),
                Err(_) => false,
            };
            if regular {
                types.push(mime);
            }
        }
    }
    types
}

/// The namespace URI (empty when there is none) and local name of the root
/// element of the XML document `bytes` starts, if it gets that far.
///
/// The namespace is the declaring attribute's value, normalised using the
/// XML version declared at the start, or 1.0 if there is no declaration.
/// References to the five predefined XML entities and character references
/// are replaced (`urn:a&amp;b` becomes `urn:a&b`), matching the value stored
/// by `update-mime-database`.
///
/// No root element is returned if the value fails the same checks as a
/// field's attribute value ([`attribute_value`]): an unresolved or
/// unterminated reference, a character the version does not allow written
/// literally, or `<`. Entities from the document's internal DTD are not
/// replaced, so a namespace that uses one gets no XML-root match (a known
/// limit). A root with an unbound prefix or a name that is not a `QName`
/// also returns no root element (`<1p:doc xmlns:1p="urn:x">`,
/// `<x:y:doc xmlns:x="urn:x">`; Namespaces in XML 1.0, 3rd ed., 5 and 7).
pub(crate) fn root_element(bytes: &[u8]) -> Option<(String, String)> {
    let mut reader = NsReader::from_reader(bytes);
    let mut version = XmlVersion::Implicit1_0;
    loop {
        let at_start = reader.buffer_position() == 0;
        let (namespace, event) = reader.read_resolved_event().ok()?;
        match event {
            // Only a declaration at the start is one (only a byte order
            // mark, which `quick-xml` does not count, may come before).
            Event::Decl(declaration) if at_start => {
                if let Ok(declared) = declaration.xml_version() {
                    version = declared;
                }
            }
            Event::Start(element) | Event::Empty(element) => {
                let namespace = match namespace {
                    ResolveResult::Bound(namespace) => match binding_attribute(&element) {
                        Some(attribute) => attribute_value(&attribute, version).ok()?.into_owned(),
                        // Bound without a declaration: the `xml` prefix.
                        None => namespace.into_inner().to_owned(),
                    },
                    ResolveResult::Unbound => String::new(),
                    ResolveResult::Unknown(_) => return None,
                };
                if !is_qname(element.name().as_ref()) {
                    return None;
                }
                let local = element.local_name().as_ref().to_owned();
                return Some((namespace, local));
            }
            Event::Eof => return None,
            _ => {}
        }
    }
}

/// The root `element`'s attribute that binds its prefix (or the default
/// namespace), if it has one. Where it repeats, the last counts, as for the
/// namespace `quick-xml` resolves.
fn binding_attribute<'a>(element: &'a BytesStart<'_>) -> Option<Attribute<'a>> {
    let prefix = element.name().prefix();
    element
        .attributes()
        .with_checks(false)
        .map_while(Result::ok)
        .filter(
            |attribute| match (attribute.key.as_namespace_binding(), prefix) {
                (Some(PrefixDeclaration::Default), None) => true,
                (Some(PrefixDeclaration::Named(name)), Some(prefix)) => name == prefix.as_ref(),
                _ => false,
            },
        )
        .last()
}

impl MimeDatabase {
    /// A human-readable description of `mime` ("PNG image") in the first of
    /// `locales` that has one, else untranslated. Read from the type's XML
    /// files on first use and kept in this snapshot. An empty `<comment/>`
    /// is an empty description, which replaces a less important folder's.
    #[must_use]
    pub fn description(&self, mime: &MimeType, locales: &Locales) -> Option<String> {
        self.type_info(mime).comment.pick(locales)
    }

    /// The type's acronym ("PNG"), from its XML files like
    /// [`MimeDatabase::description`]. Not translated: spec 0.22 dropped
    /// translated acronyms, and any an older database has are ignored.
    #[must_use]
    pub fn acronym(&self, mime: &MimeType) -> Option<String> {
        self.type_info(mime).acronym.clone()
    }

    /// The expanded acronym ("Portable Network Graphics"); not translated,
    /// as [`MimeDatabase::acronym`].
    #[must_use]
    pub fn expanded_acronym(&self, mime: &MimeType) -> Option<String> {
        self.type_info(mime).expanded_acronym.clone()
    }

    /// Problems met reading `mime`'s XML files (reading them if not done
    /// yet).
    #[must_use]
    pub fn type_diagnostics(&self, mime: &MimeType) -> Vec<Diagnostic> {
        self.type_info(mime).diagnostics().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::MAX_DIAGNOSTICS;

    /// A per-type file's root start tag, as `update-mime-database` writes it
    /// (less the type).
    const OPEN: &str =
        r#"<mime-type xmlns="http://www.freedesktop.org/standards/shared-mime-info">"#;

    /// Makes a FIFO at `path`.
    #[cfg(not(target_vendor = "apple"))]
    fn fifo(path: &Path) {
        rustix::fs::mknodat(
            rustix::fs::CWD,
            path,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        )
        .unwrap();
    }

    #[test]
    fn root_elements_with_and_without_namespaces() {
        let svg = br#"<?xml version="1.0"?><!-- c --><!DOCTYPE svg><svg xmlns="http://www.w3.org/2000/svg" width="1">"#;
        assert_eq!(
            root_element(svg),
            Some(("http://www.w3.org/2000/svg".into(), "svg".into()))
        );
        let prefixed = br#"<x:doc xmlns:x="urn:x"/>"#;
        assert_eq!(root_element(prefixed), Some(("urn:x".into(), "doc".into())));
        assert_eq!(
            root_element(b"<plain>"),
            Some((String::new(), "plain".into()))
        );
        assert_eq!(root_element(b"<y:doc>"), None, "an undeclared prefix");
        assert_eq!(
            root_element(b"<?xml version=\"1.0\"?>"),
            None,
            "cut before the root"
        );
        assert_eq!(root_element(b"\x00\xff garbage"), None);
    }

    #[test]
    fn white_space_in_a_namespace_is_normalised_as_in_any_attribute() {
        // A character reference gives its character; white space written
        // as it is becomes a space (XML 1.0, 3.3.3).
        let namespace = |root: &[u8]| root_element(root).unwrap().0;
        assert_eq!(namespace(b"<doc xmlns=\"urn:a&#10;b\"/>"), "urn:a\nb");
        assert_eq!(namespace(b"<doc xmlns=\"urn:a\nb\"/>"), "urn:a b");
        assert_eq!(namespace(b"<doc xmlns=\"urn:a&#9;b\"/>"), "urn:a\tb");
        assert_eq!(namespace(b"<doc xmlns=\"urn:a\tb\"/>"), "urn:a b");
        assert_eq!(namespace(b"<x:doc xmlns:x=\"urn:a\r\nb\"/>"), "urn:a b");
    }

    #[test]
    fn descriptions_merge_by_language_and_pick_by_locale() {
        let low = tempfile::tempdir().unwrap();
        let high = tempfile::tempdir().unwrap();
        let write = |dir: &Path, text: &str| {
            std::fs::create_dir_all(dir.join("image")).unwrap();
            std::fs::write(dir.join("image/png.xml"), text).unwrap();
        };
        write(
            low.path(),
            r#"<mime-type xmlns="http://www.freedesktop.org/standards/shared-mime-info" type="image/png">
                 <comment>PNG image</comment>
                 <comment xml:lang="de">PNG-Bild</comment>
                 <comment xml:lang="fr">image PNG</comment>
                 <acronym xml:lang="de">PNG-de</acronym>
                 <acronym>PNG</acronym>
                 <expanded-acronym>Portable Network Graphics &amp; more&#x21;</expanded-acronym>
               </mime-type>"#,
        );
        write(
            high.path(),
            r#"<mime-type xmlns="http://www.freedesktop.org/standards/shared-mime-info" type="image/png">
                 <comment xml:lang="de">Ein PNG-Bild</comment>
               </mime-type>"#,
        );
        let png = MimeType::new("image/png").unwrap();
        let info = TypeInfo::read(&png, [low.path(), high.path()].into_iter());
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(
            info.comment.pick(&Locales::new(["de_AT"])).as_deref(),
            Some("Ein PNG-Bild")
        );
        assert_eq!(
            info.comment.pick(&Locales::new(["fr"])).as_deref(),
            Some("image PNG")
        );
        assert_eq!(
            info.comment.pick(&Locales::new(["ja"])).as_deref(),
            Some("PNG image")
        );
        assert_eq!(
            info.acronym.as_deref(),
            Some("PNG"),
            "a translated one is ignored"
        );
        assert_eq!(
            info.expanded_acronym.as_deref(),
            Some("Portable Network Graphics & more!")
        );
    }

    #[test]
    fn elements_of_other_namespaces_are_not_the_fields() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("text")).unwrap();
        std::fs::write(
            dir.path().join("text/plain.xml"),
            r#"<mime-type xmlns="http://www.freedesktop.org/standards/shared-mime-info" xmlns:x="urn:extension" type="text/plain">
                 <comment>Plain text</comment>
                 <x:comment>Extension text</x:comment>
                 <x:acronym>EXT</x:acronym>
               </mime-type>"#,
        )
        .unwrap();
        let plain = MimeType::new("text/plain").unwrap();
        let info = TypeInfo::read(&plain, [dir.path()].into_iter());
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(
            info.comment.pick(&Locales::untranslated()).as_deref(),
            Some("Plain text")
        );
        assert_eq!(info.acronym, None);
    }

    #[test]
    fn an_empty_element_is_an_empty_value() {
        let low = tempfile::tempdir().unwrap();
        let high = tempfile::tempdir().unwrap();
        let write = |dir: &Path, fields: &str| {
            std::fs::create_dir_all(dir.join("text")).unwrap();
            std::fs::write(
                dir.join("text/x-t.xml"),
                format!(
                    r#"<mime-type xmlns="http://www.freedesktop.org/standards/shared-mime-info" type="text/x-t">{fields}</mime-type>"#
                ),
            )
            .unwrap();
        };
        write(
            low.path(),
            r#"<comment>T file</comment><comment xml:lang="de">T-Datei</comment>
               <acronym>T</acronym><expanded-acronym>Tee</expanded-acronym>"#,
        );
        let t = MimeType::new("text/x-t").unwrap();
        // `update-mime-database` writes an empty element self-closed.
        for fields in [
            r#"<glob pattern="*.t"/><comment/><comment xml:lang="de"/><acronym/><expanded-acronym/>"#,
            r#"<glob pattern="*.t"/><comment></comment><comment xml:lang="de"></comment>
               <acronym></acronym><expanded-acronym></expanded-acronym>"#,
        ] {
            write(high.path(), fields);
            let info = TypeInfo::read(&t, [low.path(), high.path()].into_iter());
            assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
            assert_eq!(
                info.comment.pick(&Locales::untranslated()).as_deref(),
                Some(""),
                "{fields}"
            );
            assert_eq!(
                info.comment.pick(&Locales::new(["de"])).as_deref(),
                Some(""),
                "{fields}"
            );
            assert_eq!(info.acronym.as_deref(), Some(""), "{fields}");
            assert_eq!(info.expanded_acronym.as_deref(), Some(""), "{fields}");
        }
        // A self-closed element of another kind leaves the next value in
        // place.
        write(
            high.path(),
            r#"<sub-class-of type="text/plain"/><acronym>TT</acronym>"#,
        );
        let info = TypeInfo::read(&t, [high.path()].into_iter());
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(info.acronym.as_deref(), Some("TT"));
        assert_eq!(info.comment.pick(&Locales::untranslated()), None);
    }

    /// Where a diagnostic is expected to point.
    #[derive(Clone, Copy)]
    enum At {
        /// Nowhere in particular.
        Nowhere,
        /// The end of the file.
        End,
        /// The last occurrence of this text.
        Last(&'static str),
    }

    /// Parses `text` as a per-type file named `t.xml`.
    fn parse(text: &str) -> TypeInfo {
        let mut info = TypeInfo::default();
        info.parse(Path::new("t.xml"), text.as_bytes());
        info
    }

    #[test]
    fn a_structure_problem_is_reported_and_values_completed_before_it_kept() {
        let ends = "bad XML (the file ends before the root element is closed)";
        let none = "bad XML (no root element)";
        let second = "bad XML (a second root element); the rest is skipped";
        let after = "bad XML (content after the root element); the rest is skipped";
        let cases = [
            (
                format!("{OPEN}<comment>unfinished"),
                None,
                None,
                At::End,
                ends,
            ),
            (
                format!("{OPEN}<comment>done</comment><acronym>AC"),
                Some("done"),
                None,
                At::End,
                ends,
            ),
            (String::new(), None, None, At::Nowhere, none),
            (
                "<?xml version=\"1.0\"?>\n<!-- none -->\n".to_owned(),
                None,
                None,
                At::Nowhere,
                none,
            ),
            (
                format!(
                    "{OPEN}<comment>a</comment></mime-type>\n{OPEN}<comment>b</comment><acronym>B</acronym></mime-type>"
                ),
                Some("a"),
                None,
                At::Last("<mime-type"),
                second,
            ),
            (
                format!("{OPEN}<acronym>A</acronym></mime-type><mime-type/>"),
                None,
                Some("A"),
                At::Last("<mime-type"),
                second,
            ),
            (
                format!("{OPEN}<comment>a</comment></mime-type>\n junk"),
                Some("a"),
                None,
                At::Last("junk"),
                after,
            ),
            (
                format!("{OPEN}<comment>a</comment></mime-type><![CDATA[x]]>"),
                Some("a"),
                None,
                At::Last("<![CDATA["),
                after,
            ),
            (
                format!("{OPEN}<comment>a</comment></mime-type>&amp;"),
                Some("a"),
                None,
                At::Last("&amp;"),
                after,
            ),
        ];
        // Offsets count a byte order mark too.
        for bom in ["", "\u{feff}"] {
            for (text, comment, acronym, at, message) in &cases {
                let text = format!("{bom}{text}");
                let info = parse(&text);
                assert_eq!(
                    info.comment.pick(&Locales::untranslated()).as_deref(),
                    *comment,
                    "{text}"
                );
                assert_eq!(info.acronym.as_deref(), *acronym, "{text}");
                let offset = match at {
                    At::Nowhere => None,
                    At::End => Some(text.len()),
                    At::Last(mark) => text.rfind(mark),
                };
                assert_eq!(
                    info.diagnostics,
                    [Diagnostic::new(
                        "t.xml",
                        offset.map(crate::Location::Offset),
                        *message
                    )],
                    "{text}"
                );
            }
        }
        // White space, comments and processing instructions may follow.
        let info = parse(&format!(
            "{OPEN}<comment>a</comment></mime-type>\n<!-- c --><?x y?>\r\n\t "
        ));
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
    }

    /// Checks that each case's text, with and without a byte order mark,
    /// gives the comment `a` and the problems listed (offsets without the
    /// mark).
    fn check_misplaced(cases: &[(String, Vec<(usize, &str)>)]) {
        // Offsets count a byte order mark too.
        for bom in ["", "\u{feff}"] {
            for (text, problems) in cases {
                let text = format!("{bom}{text}");
                let info = parse(&text);
                assert_eq!(
                    info.comment.pick(&Locales::untranslated()).as_deref(),
                    Some("a"),
                    "{text}"
                );
                let expected = problems
                    .iter()
                    .map(|&(offset, message)| {
                        Diagnostic::new(
                            "t.xml",
                            Some(crate::Location::Offset(bom.len() + offset)),
                            message,
                        )
                    })
                    .collect::<Vec<_>>();
                assert_eq!(info.diagnostics, expected, "{text}");
            }
        }
    }

    #[test]
    fn a_misplaced_document_type_is_reported() {
        let doctype_late = "bad XML (a document type declaration not before the root element)";
        let doctype_after = "bad XML (a document type declaration not before the root element); the rest is skipped";
        let doctype_second = "bad XML (a second document type declaration)";
        let root = format!("{OPEN}<comment>a</comment></mime-type>");
        let declaration = "<?xml version=\"1.0\"?>";
        let doctype = "<!DOCTYPE mime-type>";
        let cases = [
            // A second document type before the root is reported too.
            (
                format!("{doctype}<!-- c -->{doctype}{root}"),
                vec![(doctype.len() + 10, doctype_second)],
            ),
            (
                format!("{declaration}{doctype}\n{doctype}{doctype}{root}"),
                vec![
                    (declaration.len() + doctype.len() + 1, doctype_second),
                    (declaration.len() + 2 * doctype.len() + 1, doctype_second),
                ],
            ),
            // A document type inside the root is reported as a misplaced
            // declaration is, and after it the rest is skipped.
            (
                format!("{OPEN}{doctype}<comment>a</comment></mime-type>"),
                vec![(OPEN.len(), doctype_late)],
            ),
            (
                format!("{OPEN}<comment>a{doctype}</comment></mime-type>"),
                vec![(OPEN.len() + 10, doctype_late)],
            ),
            (
                format!("{root}{doctype}<!-- c -->"),
                vec![(root.len(), doctype_after)],
            ),
        ];
        check_misplaced(&cases);
    }

    #[test]
    fn content_before_the_root_and_a_late_declaration_are_reported() {
        let before = "bad XML (content before the root element)";
        let late = "bad XML (an XML declaration not at the start of the file)";
        let late_after =
            "bad XML (an XML declaration not at the start of the file); the rest is skipped";
        let root = format!("{OPEN}<comment>a</comment></mime-type>");
        let declaration = "<?xml version=\"1.0\"?>";
        let cases = [
            (format!("junk{root}"), vec![(0, before)]),
            (format!("\n <![CDATA[x]]>{root}"), vec![(2, before)]),
            (format!("<!-- c -->\n  &amp;{root}"), vec![(13, before)]),
            // One run of text is one problem, references and all.
            (format!("ab&amp;cd&#x21;{root}"), vec![(0, before)]),
            (format!("\n&lt;x{root}"), vec![(1, before)]),
            (
                format!("ab<!-- c -->cd{root}"),
                vec![(0, before), (12, before)],
            ),
            // CDATA joins the run too.
            (
                format!("ab&amp;<![CDATA[x]]>cd&lt;{root}"),
                vec![(0, before)],
            ),
            (format!("<![CDATA[x]]>&amp;{root}"), vec![(0, before)]),
            (format!("\n <![CDATA[x]]>\n{root}"), vec![(2, before)]),
            (
                format!("ab<?x y?><![CDATA[x]]>{root}"),
                vec![(0, before), (9, before)],
            ),
            (
                format!("<![CDATA[x]]><!-- c -->&amp;{root}"),
                vec![(0, before), (23, before)],
            ),
            (format!("<!-- c -->{declaration}{root}"), vec![(10, late)]),
            (format!(" {declaration}{root}"), vec![(1, late)]),
            // Inside the root, the values are still read.
            (
                format!("{OPEN}{declaration}<comment>a</comment></mime-type>"),
                vec![(OPEN.len(), late)],
            ),
            (
                format!("{OPEN}<comment>a</comment>{declaration}</mime-type>"),
                vec![(OPEN.len() + 20, late)],
            ),
            // After it, the rest is skipped as for other content there.
            (
                format!("{root}{declaration}<!-- c -->"),
                vec![(root.len(), late_after)],
            ),
        ];
        check_misplaced(&cases);
        // A byte order mark, a declaration, white space, comments, a
        // document type and processing instructions may come first.
        let info = parse(&format!(
            "\u{feff}{declaration}\n<!-- c --><!DOCTYPE mime-type>\r\n\t<?x y?>{root}"
        ));
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(
            info.comment.pick(&Locales::untranslated()).as_deref(),
            Some("a")
        );
    }

    #[test]
    fn a_bad_reference_in_a_field_is_reported_and_the_field_dropped() {
        for bad in [
            "&#xZZ;",
            "&#0;",
            "&#xD800;",
            "&#x110000;",
            "&#-1;",
            "&undefined;",
            "&;",
            // Characters XML 1.0 does not allow.
            "&#1;",
            "&#x1F;",
            "&#xFFFE;",
            "&#xFFFF;",
        ] {
            for bom in ["", "\u{feff}"] {
                let head = format!("{bom}{OPEN}<comment xml:lang=\"de\">D</comment><comment>a");
                let text = format!(
                    "{head}{bad}b&amp;</comment><acronym>A</acronym><comment xml:lang=\"fr\">F</comment></mime-type>"
                );
                let info = parse(&text);
                assert_eq!(
                    info.diagnostics,
                    [Diagnostic::new(
                        "t.xml",
                        Some(crate::Location::Offset(head.len())),
                        format!(
                            "bad XML (the reference `{bad}` cannot be replaced); the value is left out"
                        )
                    )],
                    "{text}"
                );
                // The field is dropped as unfinished; the others are kept.
                assert_eq!(info.comment.pick(&Locales::untranslated()), None, "{text}");
                assert_eq!(
                    info.comment.pick(&Locales::new(["de"])).as_deref(),
                    Some("D"),
                    "{text}"
                );
                assert_eq!(
                    info.comment.pick(&Locales::new(["fr"])).as_deref(),
                    Some("F"),
                    "{text}"
                );
                assert_eq!(info.acronym.as_deref(), Some("A"), "{text}");
            }
        }
        // One problem for each field, however many references are bad.
        let info = parse(&format!(
            "{OPEN}<comment>&bad;&#0;</comment><acronym>&#xZZ;</acronym></mime-type>"
        ));
        assert_eq!(info.diagnostics.len(), 2, "{:?}", info.diagnostics);
        assert_eq!(info.comment.pick(&Locales::untranslated()), None);
        assert_eq!(info.acronym, None);
        // Good references are replaced, and one outside the fields is not
        // a problem.
        let info = parse(&format!(
            "{OPEN}<x>&bad;</x><comment>&#x21;&#65;&lt;&gt;&amp;&apos;&quot;&#x9;&#xA;&#xD;&#x20;&#xD7FF;&#xE000;&#xFFFD;&#x10000;&#x10FFFF;</comment></mime-type>"
        ));
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(
            info.comment.pick(&Locales::untranslated()).as_deref(),
            Some("!A<>&'\"\t\n\r \u{D7FF}\u{E000}\u{FFFD}\u{10000}\u{10FFFF}")
        );
    }

    /// The diagnostic for `c`, written as it is in a field.
    fn not_allowed(c: char) -> String {
        format!(
            "bad XML (a character XML does not allow, U+{:04X}); the value is left out",
            u32::from(c)
        )
    }

    #[test]
    fn a_character_xml_does_not_allow_in_a_field_is_reported_and_the_field_dropped() {
        for bad in ['\u{1}', '\u{8}', '\u{B}', '\u{1F}', '\u{FFFE}', '\u{FFFF}'] {
            for (open, close) in [("", ""), ("<![CDATA[", "]]>")] {
                for bom in ["", "\u{feff}"] {
                    let head = format!("{bom}{OPEN}<comment xml:lang=\"de\">D</comment><comment>");
                    let text = format!(
                        "{head}{open}a{bad}b{close}&amp;</comment><acronym>A</acronym><comment xml:lang=\"fr\">F</comment></mime-type>"
                    );
                    let info = parse(&text);
                    assert_eq!(
                        info.diagnostics,
                        [Diagnostic::new(
                            "t.xml",
                            Some(crate::Location::Offset(head.len())),
                            not_allowed(bad)
                        )],
                        "{text:?}"
                    );
                    // The field is dropped as unfinished; the others are kept.
                    assert_eq!(
                        info.comment.pick(&Locales::untranslated()),
                        None,
                        "{text:?}"
                    );
                    assert_eq!(
                        info.comment.pick(&Locales::new(["de"])).as_deref(),
                        Some("D"),
                        "{text:?}"
                    );
                    assert_eq!(
                        info.comment.pick(&Locales::new(["fr"])).as_deref(),
                        Some("F"),
                        "{text:?}"
                    );
                    assert_eq!(info.acronym.as_deref(), Some("A"), "{text:?}");
                }
            }
        }
        // One problem for each field, naming the first character.
        let info = parse(&format!(
            "{OPEN}<comment>a\u{1}b\u{FFFE}c</comment><acronym><![CDATA[a\u{1}b]]></acronym></mime-type>"
        ));
        assert_eq!(
            info.diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            [not_allowed('\u{1}'), not_allowed('\u{1}')]
        );
        assert_eq!(info.comment.pick(&Locales::untranslated()), None);
        assert_eq!(info.acronym, None);
        // The problems count towards the file's cap.
        let info = parse(&format!(
            "{OPEN}{}</mime-type>",
            "<comment>\u{1}</comment>".repeat(MAX_DIAGNOSTICS + 5)
        ));
        assert_eq!(info.diagnostics.len(), MAX_DIAGNOSTICS + 1);
        assert_eq!(
            info.diagnostics[MAX_DIAGNOSTICS].message,
            format!("more than {MAX_DIAGNOSTICS} problems; the rest are not listed")
        );
        // XML 1.0 allows the C1 controls, and characters outside the fields
        // are not looked at.
        let info = parse(&format!(
            "{OPEN}<x>\u{1}</x><comment>\u{7F}\u{80}\u{85}\u{9F}\t\r\n</comment></mime-type>"
        ));
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(
            info.comment.pick(&Locales::untranslated()).as_deref(),
            Some("\u{7F}\u{80}\u{85}\u{9F}\t\n")
        );
    }

    #[test]
    fn a_file_is_read_by_its_declared_xml_version() {
        let field = |declaration: &str, comment: &str| {
            let head = format!("{declaration}{OPEN}<comment>");
            let info = parse(&format!(
                "{head}{comment}</comment><acronym>A</acronym></mime-type>"
            ));
            assert_eq!(info.acronym.as_deref(), Some("A"));
            let value = info.comment.pick(&Locales::untranslated());
            let problems = info
                .diagnostics
                .iter()
                .map(|diagnostic| (diagnostic.location, diagnostic.message.clone()))
                .collect::<Vec<_>>();
            (value, problems, head.len())
        };
        let kept = |declaration: &str, comment: &str, value: &str| {
            let (read, problems, _) = field(declaration, comment);
            assert!(
                problems.is_empty(),
                "{declaration}{comment:?}: {problems:?}"
            );
            assert_eq!(read.as_deref(), Some(value), "{declaration}{comment:?}");
        };
        // The problem is at the start of the text, or at the reference
        // after its first character.
        let dropped = |declaration: &str, comment: &str, message: &str| {
            let (read, problems, head) = field(declaration, comment);
            assert_eq!(read, None, "{declaration}{comment:?}");
            let at = head + usize::from(message.contains("reference"));
            assert_eq!(
                problems,
                [(Some(crate::Location::Offset(at)), message.to_owned())],
                "{declaration}{comment:?}"
            );
        };
        let reference = |bad: &str| {
            format!("bad XML (the reference `{bad}` cannot be replaced); the value is left out")
        };
        for v11 in [
            "<?xml version=\"1.1\"?>",
            "<?xml version='1.1' encoding=\"UTF-8\"?>\n",
            "\u{feff}<?xml version=\"1.1\"?>",
        ] {
            // XML 1.1 allows references to the controls (spec 1.1, 2.2 and
            // 4.1), but not the controls written as they are (2.1, [1]).
            kept(v11, "a&#1;&#x1F;&#x7F;&#x85;b", "a\u{1}\u{1F}\u{7F}\u{85}b");
            dropped(v11, "a\u{1}b", &not_allowed('\u{1}'));
            dropped(v11, "<![CDATA[a\u{8}b]]>", &not_allowed('\u{8}'));
            dropped(v11, "a\u{80}b", &not_allowed('\u{80}'));
            dropped(v11, "a\u{9F}b", &not_allowed('\u{9F}'));
            dropped(v11, "a\u{FFFE}b", &not_allowed('\u{FFFE}'));
            for bad in ["&#0;", "&#xFFFE;", "&#xD800;"] {
                dropped(v11, &format!("a{bad}b"), &reference(bad));
            }
            // Its line ends (2.11).
            kept(
                v11,
                "a\u{85}b\u{2028}c\r\u{85}d\r\ne\rf",
                "a\nb\nc\nd\ne\nf",
            );
            kept(v11, "<![CDATA[a\u{85}b\r\u{85}c]]>", "a\nb\nc");
            kept(v11, "a&#xD;&#x85;&#x2028;b", "a\r\u{85}\u{2028}b");
        }
        // XML 1.0, declared or not, and a declaration not at the start
        // (which is reported) or with another version.
        for v10 in [
            "",
            "<?xml version=\"1.0\"?>",
            "\u{feff}",
            "\u{feff}<?xml version=\"1.0\"?>",
            "<?xml version=\"2.0\"?>",
        ] {
            kept(
                v10,
                "a\u{85}b\u{2028}c\r\u{85}d\u{7F}\u{80}\u{9F}",
                "a\u{85}b\u{2028}c\n\u{85}d\u{7F}\u{80}\u{9F}",
            );
            dropped(v10, "a&#1;b", &reference("&#1;"));
            dropped(v10, "a\u{1}b", &not_allowed('\u{1}'));
        }
        let late = parse(&format!(
            " <?xml version=\"1.1\"?>{OPEN}<comment>a\u{85}&#1;</comment></mime-type>"
        ));
        assert_eq!(late.comment.pick(&Locales::untranslated()), None);
        assert_eq!(
            late.diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            [
                "bad XML (an XML declaration not at the start of the file)".to_owned(),
                reference("&#1;")
            ]
        );
        // A language is normalised by the version too.
        for (declaration, lang) in [("", "de\u{85}"), ("<?xml version=\"1.1\"?>", "de ")] {
            let info = parse(&format!(
                "{declaration}{OPEN}<comment xml:lang=\"de\u{85}\">D</comment></mime-type>"
            ));
            assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
            assert_eq!(comments(&info), [(Some(lang), "D")], "{declaration}");
        }
    }

    #[test]
    fn a_field_whose_language_cannot_be_read_is_reported_and_dropped() {
        // Before, it was read as untranslated, and replaced that value.
        let info = parse(&format!(
            "{OPEN}<comment>English</comment><comment xml:lang=\"&e;\">Deutsch</comment></mime-type>"
        ));
        assert_eq!(comments(&info), [(None, "English")]);
        assert_eq!(
            info.diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            ["bad XML (the reference `&e;` cannot be replaced); the value is left out"]
        );
        let reference = |bad: &str| {
            format!("bad XML (the reference `{bad}` cannot be replaced); the value is left out")
        };
        let v11 = "<?xml version=\"1.1\"?>";
        let cases = [
            ("", "&bogus;", reference("&bogus;")),
            ("", "d&#0;e", reference("&#0;")),
            (v11, "d&#0;e", reference("&#0;")),
            ("", "d&#xD800;e", reference("&#xD800;")),
            (v11, "d&#xD800;e", reference("&#xD800;")),
            // XML 1.0 allows no reference to a control, nor either version
            // one to U+FFFE.
            ("", "d&#1;e", reference("&#1;")),
            (v11, "d&#xFFFE;e", reference("&#xFFFE;")),
            // Written as they are: XML 1.1 allows the controls only as
            // references.
            ("", "d\u{1}e", not_allowed('\u{1}')),
            (v11, "d\u{1}e", not_allowed('\u{1}')),
            (v11, "d\u{80}e", not_allowed('\u{80}')),
            ("", "de\u{FFFF}", not_allowed('\u{FFFF}')),
        ];
        for bom in ["", "\u{feff}"] {
            for (declaration, lang, message) in &cases {
                let head = format!("{bom}{declaration}{OPEN}<comment>a</comment>");
                let text = format!(
                    "{head}<comment xml:lang=\"{lang}\">b</comment><acronym>A</acronym><comment xml:lang=\"fr\">F</comment></mime-type>"
                );
                let info = parse(&text);
                // The problem is at the attribute.
                assert_eq!(
                    info.diagnostics,
                    [Diagnostic::new(
                        "t.xml",
                        Some(crate::Location::Offset(head.len() + "<comment ".len())),
                        message.as_str()
                    )],
                    "{text:?}"
                );
                // The field is dropped; the others are kept.
                assert_eq!(
                    comments(&info),
                    [(None, "a"), (Some("fr"), "F")],
                    "{text:?}"
                );
                assert_eq!(info.acronym.as_deref(), Some("A"), "{text:?}");
            }
        }
    }

    #[test]
    fn a_field_whose_attributes_cannot_be_read_is_reported_and_dropped() {
        // Attributes that cannot be read, or a reference not ended: the
        // problem in our words, where `quick-xml` finds it or at the
        // attribute (the last occurrence of the mark in the tag).
        for (tag, mark, problem) in [
            (
                "<comment xml:lang=de>",
                "de",
                "an attribute cannot be read: a value is not quoted",
            ),
            (
                "<comment x xml:lang=\"de\">",
                "x x",
                "an attribute cannot be read: a name is not followed by `=`",
            ),
            (
                "<comment xml:lang=\"de\" a=>",
                ">",
                "an attribute cannot be read: `=` is not followed by a value",
            ),
            (
                "<comment xml:lang=\"d&e\">",
                "xml:lang",
                "the reference `&e` is not ended",
            ),
        ] {
            let head = format!("{OPEN}<comment>a</comment>");
            let info = parse(&format!(
                "{head}{tag}b</comment><acronym>A</acronym></mime-type>"
            ));
            assert_eq!(
                info.diagnostics,
                [Diagnostic::new(
                    "t.xml",
                    Some(crate::Location::Offset(
                        head.len() + tag.rfind(mark).unwrap()
                    )),
                    format!("bad XML ({problem}); the value is left out")
                )],
                "{tag}"
            );
            assert_eq!(comments(&info), [(None, "a")], "{tag}");
            assert_eq!(info.acronym.as_deref(), Some("A"), "{tag}");
        }
        // XML 1.1 allows a reference to a control.
        let v11 = "<?xml version=\"1.1\"?>";
        let info = parse(&format!(
            "{v11}{OPEN}<comment xml:lang=\"d&#1;e\">D</comment></mime-type>"
        ));
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(comments(&info), [(Some("d\u{1}e"), "D")]);
        // The problems count towards the file's cap.
        let info = parse(&format!(
            "{OPEN}{}</mime-type>",
            "<comment xml:lang=\"&e;\">x</comment>".repeat(MAX_DIAGNOSTICS + 5)
        ));
        assert_eq!(info.diagnostics.len(), MAX_DIAGNOSTICS + 1);
        assert_eq!(
            info.diagnostics[MAX_DIAGNOSTICS].message,
            format!("more than {MAX_DIAGNOSTICS} problems; the rest are not listed")
        );
        assert!(info.comment.0.is_empty(), "{:?}", info.comment);
    }

    #[test]
    fn a_problem_anywhere_in_a_fields_tag_drops_it() {
        // Every attribute is read, not only up to `xml:lang`. The problem is
        // at the last occurrence of the mark in the tag.
        let twice =
            |name: &str| format!("an attribute cannot be read: an attribute appears twice: {name}");
        let no_eq = "an attribute cannot be read: a name is not followed by `=`".to_owned();
        for (tag, mark, problem) in [
            (
                "<comment xml:lang=\"de\" xml:lang=\"fr\">",
                "xml:lang",
                twice("xml:lang"),
            ),
            (
                "<comment a=\"1\" xml:lang=\"de\" a=\"2\">",
                "a=",
                twice("a"),
            ),
            ("<comment a=\"1\" a='2'>", "a=", twice("a")),
            ("<comment xml:lang=\"de\" x>", "x>", no_eq.clone()),
            (
                "<comment xml:lang=\"de\" x=y>",
                "y",
                "an attribute cannot be read: a value is not quoted".to_owned(),
            ),
            ("<comment a='1' b xml:lang=\"de\">", "b ", no_eq.clone()),
            // Right after a value, and before white space.
            ("<comment a='1'b c=\"2\">", "b ", no_eq.clone()),
            ("<comment xml:lang=\"de\" x\t\n>", "x\t", no_eq),
        ] {
            let head = format!("{OPEN}<comment>a</comment><comment xml:lang=\"fr\">F</comment>");
            let info = parse(&format!(
                "{head}{tag}b</comment><acronym>A</acronym></mime-type>"
            ));
            assert_eq!(
                info.diagnostics,
                [Diagnostic::new(
                    "t.xml",
                    Some(crate::Location::Offset(
                        head.len() + tag.rfind(mark).unwrap()
                    )),
                    format!("bad XML ({problem}); the value is left out")
                )],
                "{tag}"
            );
            assert_eq!(comments(&info), [(None, "a"), (Some("fr"), "F")], "{tag}");
            assert_eq!(info.acronym.as_deref(), Some("A"), "{tag}");
        }
        // The acronyms too.
        for field in ["acronym", "expanded-acronym"] {
            let head = format!("{OPEN}<comment>a</comment>");
            let tag = format!("<{field} x=\"1\" x=\"2\">");
            let info = parse(&format!("{head}{tag}B</{field}></mime-type>"));
            assert_eq!(
                info.diagnostics,
                [Diagnostic::new(
                    "t.xml",
                    Some(crate::Location::Offset(
                        head.len() + tag.rfind("x=").unwrap()
                    )),
                    format!("bad XML ({}); the value is left out", twice("x"))
                )],
                "{field}"
            );
            assert_eq!(info.acronym, None, "{field}");
            assert_eq!(info.expanded_acronym, None, "{field}");
            assert_eq!(comments(&info), [(None, "a")], "{field}");
        }
        // Other elements are not looked at.
        let info = parse(&format!(
            "{OPEN}<generic-icon name=\"a\" name=\"b\"/><x:comment xmlns:x=\"urn:x\" a=\"1\" a=\"2\">X</x:comment><comment>a</comment></mime-type>"
        ));
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(comments(&info), [(None, "a")]);
        // The problems count towards the file's cap.
        let info = parse(&format!(
            "{OPEN}{}</mime-type>",
            "<comment xml:lang=\"de\" xml:lang=\"de\">x</comment>".repeat(MAX_DIAGNOSTICS + 5)
        ));
        assert_eq!(info.diagnostics.len(), MAX_DIAGNOSTICS + 1);
        assert_eq!(
            info.diagnostics[MAX_DIAGNOSTICS].message,
            format!("more than {MAX_DIAGNOSTICS} problems; the rest are not listed")
        );
        assert!(info.comment.0.is_empty(), "{:?}", info.comment);
    }

    #[test]
    fn white_space_outside_the_root_is_by_the_xml_version() {
        let root = format!("{OPEN}<comment>a</comment></mime-type>");
        // XML 1.1 reads U+0085 and U+2028 as line ends (spec 1.1, 2.11), so
        // they are white space there.
        let v11 = "<?xml version=\"1.1\"?>";
        for text in [
            format!("{v11}\u{85}{root}\u{2028}"),
            format!("{v11}\r\u{85}<!-- c -->\u{2028}\n{root}\u{85}<?x y?>\r\u{2028}"),
        ] {
            let info = parse(&text);
            assert!(
                info.diagnostics.is_empty(),
                "{text:?}: {:?}",
                info.diagnostics
            );
            assert_eq!(
                info.comment.pick(&Locales::untranslated()).as_deref(),
                Some("a"),
                "{text:?}"
            );
        }
        let before = "bad XML (content before the root element)";
        let after = "bad XML (content after the root element); the rest is skipped";
        check_misplaced(&[
            // Content after them is still content.
            (
                format!("{v11}\u{85}\u{2028}x{root}"),
                vec![(v11.len() + 5, before)],
            ),
            (
                format!("{v11}{root}\u{2028}x"),
                vec![(v11.len() + root.len() + 3, after)],
            ),
        ]);
        // In XML 1.0 they are content.
        for v10 in ["", "<?xml version=\"1.0\"?>"] {
            check_misplaced(&[
                (format!("{v10}\u{85}{root}"), vec![(v10.len(), before)]),
                (
                    format!("{v10}{root}\u{2028}"),
                    vec![(v10.len() + root.len(), after)],
                ),
                (
                    format!("{v10}\n\u{2028}{root}"),
                    vec![(v10.len() + 1, before)],
                ),
            ]);
        }
    }

    #[test]
    fn a_file_with_any_byte_order_mark_is_read_by_its_declared_xml_version() {
        // `quick-xml` reads only UTF-8, but with its `encoding` feature
        // (which another crate in a build may turn on) it skips a UTF-16
        // byte order mark too, and the declaration after it still counts.
        // Without it, such a mark is not UTF-8.
        let rest = format!(
            "<?xml version=\"1.1\" encoding=\"UTF-16\"?>{OPEN}<comment>a&#1;\u{85}b</comment><acronym>\u{1}</acronym></mime-type>"
        );
        for mark in [&b"\xEF\xBB\xBF"[..], b"\xFE\xFF", b"\xFF\xFE", b""] {
            let mut bytes = mark.to_vec();
            bytes.extend_from_slice(rest.as_bytes());
            let mut info = TypeInfo::default();
            info.parse(Path::new("t.xml"), &bytes);
            let shown = String::from_utf8_lossy(&bytes);
            let messages = info
                .diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.clone())
                .collect::<Vec<_>>();
            if let [message] = messages.as_slice()
                && message.contains("UTF-8")
            {
                assert_ne!(mark, b"", "{shown}: {message}");
                assert_ne!(mark, b"\xEF\xBB\xBF", "{shown}: {message}");
                continue;
            }
            assert_eq!(messages, [not_allowed('\u{1}')], "{shown}");
            assert_eq!(
                info.comment.pick(&Locales::untranslated()).as_deref(),
                Some("a\u{1}\nb"),
                "{shown}"
            );
            assert_eq!(info.acronym, None, "{shown}");
        }
    }

    #[test]
    fn offsets_count_whatever_byte_order_mark_quick_xml_skips() {
        // `quick-xml` skips a UTF-8 byte order mark, and with its
        // `encoding` feature (which another crate in a build may turn on)
        // UTF-16 ones too. Either way an offset is one in the file.
        for mark in [&b"\xEF\xBB\xBF"[..], b"\xFE\xFF", b"\xFF\xFE", b""] {
            let after = format!("{OPEN}<comment>a</comment></mime-type> x");
            for (rest, at) in [
                (format!(" x{OPEN}<comment>a</comment></mime-type>"), 1),
                (format!("{OPEN}<comment>Text</commen>"), OPEN.len() + 13),
                (after.clone(), after.len() - 1),
                ("<!DOCTYPE".to_owned(), 0),
            ] {
                let mut bytes = mark.to_vec();
                bytes.extend_from_slice(rest.as_bytes());
                let mut info = TypeInfo::default();
                info.parse(Path::new("t.xml"), &bytes);
                let shown = String::from_utf8_lossy(&bytes);
                assert_eq!(info.diagnostics.len(), 1, "{shown}: {:?}", info.diagnostics);
                let diagnostic = &info.diagnostics[0];
                let Some(crate::Location::Offset(offset)) = diagnostic.location else {
                    panic!("{shown}: {diagnostic:?}");
                };
                if diagnostic.message.contains("utf-8") || diagnostic.message.contains("UTF-8") {
                    // Not skipped (no `encoding` feature): the mark is
                    // text that is not UTF-8, which starts the file.
                    assert_eq!(offset, 0, "{shown}: {diagnostic:?}");
                    continue;
                }
                let expected = mark.len() + at;
                assert_eq!(offset, expected, "{shown}: {diagnostic:?}");
            }
        }
    }

    #[test]
    fn a_namespace_written_with_references_is_the_namespace() {
        // `h` written as a character reference, as an attribute may be.
        let referenced = NAMESPACE.replacen('h', "&#104;", 1);
        let info = parse(&format!(
            r#"<mime-type xmlns="{referenced}"><comment>a</comment></mime-type>"#
        ));
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(
            info.comment.pick(&Locales::untranslated()).as_deref(),
            Some("a")
        );
        let info = parse(&format!(
            r#"<m:mime-type xmlns:m="{referenced}"><m:comment>b</m:comment><m:acronym>B</m:acronym></m:mime-type>"#
        ));
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(
            info.comment.pick(&Locales::untranslated()).as_deref(),
            Some("b")
        );
        assert_eq!(info.acronym.as_deref(), Some("B"));
        // A field whose own prefix is bound so.
        let info = parse(&format!(
            r#"{OPEN}<f:comment xmlns:f="{referenced}">c</f:comment></mime-type>"#
        ));
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(
            info.comment.pick(&Locales::untranslated()).as_deref(),
            Some("c")
        );
        // A namespace with a reference that cannot be replaced cannot be
        // read: such a field is dropped with a problem, not taken for an
        // element of another namespace.
        let text =
            format!(r#"{OPEN}<f:comment xmlns:f="{NAMESPACE}&unknown;">c</f:comment></mime-type>"#);
        let info = parse(&text);
        assert_eq!(
            info.diagnostics,
            [Diagnostic::new(
                "t.xml",
                text.rfind("xmlns:f").map(crate::Location::Offset),
                "bad XML (the reference `&unknown;` cannot be replaced); the value is left out"
            )]
        );
        assert_eq!(info.comment.pick(&Locales::untranslated()), None);
        // A root element with one is not in the spec's namespace.
        let info = parse(&format!(
            r#"<mime-type xmlns="{NAMESPACE}&unknown;"><comment>c</comment></mime-type>"#
        ));
        assert_eq!(info.comment.pick(&Locales::untranslated()), None);
        assert_eq!(info.diagnostics.len(), 1, "{:?}", info.diagnostics);
        assert!(
            info.diagnostics[0]
                .message
                .starts_with("not a per-type file"),
            "{:?}",
            info.diagnostics
        );
    }

    #[test]
    fn a_bad_xml_offset_counts_a_byte_order_mark() {
        let text = format!("{OPEN}<comment>Text</commen>");
        let offset = |text: &str| {
            let info = parse(text);
            assert_eq!(info.diagnostics.len(), 1, "{:?}", info.diagnostics);
            assert!(info.diagnostics[0].message.starts_with("bad XML ("));
            match info.diagnostics[0].location {
                Some(crate::Location::Offset(offset)) => offset,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(offset(&format!("\u{feff}{text}")), offset(&text) + 3);
    }

    #[test]
    fn diagnostics_are_capped_per_file() {
        let text = format!(
            "{}{OPEN}</mime-type>",
            "x<!-- -->".repeat(MAX_DIAGNOSTICS + 5)
        );
        let mut info = TypeInfo::default();
        info.parse(Path::new("t.xml"), text.as_bytes());
        assert_eq!(info.diagnostics.len(), MAX_DIAGNOSTICS + 1);
        assert!(
            info.diagnostics[..MAX_DIAGNOSTICS]
                .iter()
                .all(|diagnostic| diagnostic.message == "bad XML (content before the root element)")
        );
        assert_eq!(
            info.diagnostics[MAX_DIAGNOSTICS].message,
            format!("more than {MAX_DIAGNOSTICS} problems; the rest are not listed")
        );
        // The count starts again in each file.
        let dir = tempfile::tempdir().unwrap();
        for folder in ["a", "b"] {
            std::fs::create_dir_all(dir.path().join(folder).join("text")).unwrap();
            std::fs::write(dir.path().join(folder).join("text/x-t.xml"), &text).unwrap();
        }
        let info = TypeInfo::read(
            &MimeType::new("text/x-t").unwrap(),
            [dir.path().join("a"), dir.path().join("b")]
                .iter()
                .map(PathBuf::as_path),
        );
        assert_eq!(info.diagnostics.len(), 2 * (MAX_DIAGNOSTICS + 1));
    }

    #[test]
    fn a_file_whose_root_is_not_mime_type_is_ignored() {
        let low = tempfile::tempdir().unwrap();
        let high = tempfile::tempdir().unwrap();
        let write = |dir: &Path, text: &str| {
            std::fs::create_dir_all(dir.join("text")).unwrap();
            std::fs::write(dir.join("text/x-t.xml"), text).unwrap();
        };
        write(
            low.path(),
            r#"<mime-type xmlns="http://www.freedesktop.org/standards/shared-mime-info" type="text/x-t"><comment>Low</comment><acronym>L</acronym></mime-type>"#,
        );
        let t = MimeType::new("text/x-t").unwrap();
        let fields =
            "<comment>High</comment><acronym>H</acronym><expanded-acronym>Hi</expanded-acronym>";
        let name = |name: &str| format!("the root element is `{name}`, not `mime-type`");
        let namespace =
            |name: &str| format!("the root element `{name}` is not in the namespace `{NAMESPACE}`");
        for (text, what) in [
            (
                format!(r#"<mime-info xmlns="{NAMESPACE}">{fields}</mime-info>"#),
                name("mime-info"),
            ),
            ("<x:other xmlns:x=\"urn:x\"/>".to_owned(), name("x:other")),
            (
                format!("<mime-type>{fields}</mime-type>"),
                namespace("mime-type"),
            ),
            (
                format!(r#"<mime-type xmlns="urn:other">{fields}</mime-type>"#),
                namespace("mime-type"),
            ),
            (
                format!(
                    r#"<x:mime-type xmlns:x="urn:x" xmlns="{NAMESPACE}">{fields}</x:mime-type>"#
                ),
                namespace("x:mime-type"),
            ),
            (
                format!("<y:mime-type>{fields}</y:mime-type>"),
                namespace("y:mime-type"),
            ),
        ] {
            let text = text.as_str();
            write(high.path(), text);
            let alone = TypeInfo::read(&t, [high.path()].into_iter());
            assert!(alone.found);
            assert_eq!(alone.comment.pick(&Locales::untranslated()), None, "{text}");
            assert_eq!(alone.acronym, None, "{text}");
            assert_eq!(alone.expanded_acronym, None, "{text}");
            let message = format!("not a per-type file: {what}; it is ignored");
            assert_eq!(
                alone
                    .diagnostics
                    .iter()
                    .map(|diagnostic| (diagnostic.location, diagnostic.message.as_str()))
                    .collect::<Vec<_>>(),
                [(Some(crate::Location::Offset(0)), message.as_str())],
                "{text}"
            );
            let both = TypeInfo::read(&t, [low.path(), high.path()].into_iter());
            assert_eq!(
                both.comment.pick(&Locales::untranslated()).as_deref(),
                Some("Low"),
                "{text}"
            );
            assert_eq!(both.acronym.as_deref(), Some("L"), "{text}");
            assert_eq!(both.expanded_acronym, None, "{text}");
        }
    }

    #[test]
    fn a_file_whose_root_name_is_not_a_qname_is_ignored() {
        // Namespaces in XML 1.0, 3rd ed., 5 and 7. Before, `<1p:mime-type>`
        // was read as a per-type file, `<:mime-type xmlns=…>` said not to be
        // in the spec's namespace, and `<x:y:mime-type>` to be named
        // `x:y:mime-type`, not `mime-type`.
        let fields = "<comment>C</comment><acronym>A</acronym>";
        let name = |name: &str| {
            format!(
                "not a per-type file: the root element's name `{name}` is not an XML name; it is ignored"
            )
        };
        let ours = |prefix: &str| format!(" xmlns{prefix}=\"{NAMESPACE}\"");
        for (head, root, declaration, shown) in [
            ("", "1p:mime-type", ours(":1p"), "1p:mime-type"),
            ("", ":mime-type", ours(""), ":mime-type"),
            ("", "x:y:mime-type", ours(":x"), "x:y:mime-type"),
            ("", "-p:mime-type", ours(":-p"), "-p:mime-type"),
            ("", "-p:mime-type", String::new(), "-p:mime-type"),
            ("", "mime-type:", ours(""), "mime-type:"),
            // U+0085 is a line end in XML 1.1, not a name character.
            (
                "<?xml version=\"1.1\"?>",
                "p\u{85}:mime-type",
                ours(":p\u{85}"),
                r"p\u{85}:mime-type",
            ),
        ] {
            let text = format!("{head}<{root}{declaration}>{fields}</{root}>");
            let info = parse(&text);
            assert_eq!(
                info.comment.pick(&Locales::untranslated()),
                None,
                "{text:?}"
            );
            assert_eq!(info.acronym, None, "{text:?}");
            assert_eq!(
                info.diagnostics,
                [Diagnostic::new(
                    "t.xml",
                    Some(crate::Location::Offset(head.len())),
                    name(shown)
                )],
                "{text:?}"
            );
        }
    }

    #[test]
    fn a_root_name_that_is_not_a_qname_gives_no_root_element() {
        for root in [
            "<1p:doc xmlns:1p=\"urn:x\"/>",
            "<x:y:doc xmlns:x=\"urn:x\"/>",
            "<:doc xmlns=\"urn:x\"/>",
            "<-p:doc xmlns:-p=\"urn:x\"/>",
            "<doc: xmlns=\"urn:x\"/>",
            "<1doc/>",
        ] {
            assert_eq!(root_element(root.as_bytes()), None, "{root}");
        }
        assert_eq!(
            root_element(b"<?xml version=\"1.1\"?><p\xc2\x85:doc xmlns:p\xc2\x85=\"urn:x\"/>"),
            None
        );
    }

    #[test]
    fn broken_xml_is_reported_and_types_are_listed_from_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("text")).unwrap();
        std::fs::create_dir_all(dir.path().join("packages")).unwrap();
        std::fs::write(
            dir.path().join("text/plain.xml"),
            format!("{OPEN}<comment>Text</commen>"),
        )
        .unwrap();
        std::fs::write(dir.path().join("packages/x.xml"), "").unwrap();
        let plain = MimeType::new("text/plain").unwrap();
        let info = TypeInfo::read(&plain, [dir.path()].into_iter());
        assert_eq!(info.diagnostics.len(), 1);
        assert_eq!(types_in(dir.path()), [plain]);
    }

    #[test]
    fn types_are_listed_from_regular_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let text = dir.path().join("text");
        std::fs::create_dir_all(text.join("fake.xml")).unwrap();
        // `mknodat` is not in rustix on Apple systems.
        #[cfg(not(target_vendor = "apple"))]
        fifo(&text.join("fifo.xml"));
        std::fs::write(text.join("real.xml"), "").unwrap();
        std::os::unix::fs::symlink("real.xml", text.join("linked.xml")).unwrap();
        let mut types = types_in(dir.path());
        types.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(
            types,
            [
                MimeType::new("text/linked").unwrap(),
                MimeType::new("text/real").unwrap()
            ]
        );
    }

    /// The comments `info` holds, by language, sorted.
    fn comments(info: &TypeInfo) -> Vec<(Option<&str>, &str)> {
        let mut comments = info
            .comment
            .0
            .iter()
            .map(|(lang, text)| (lang.as_deref(), text.as_str()))
            .collect::<Vec<_>>();
        comments.sort_unstable();
        comments
    }

    /// Checks that `text` gives the comments `expected` and the acronym `A`,
    /// with one problem, `message`, at the last occurrence of `mark` (offset
    /// `0` when `mark` is empty).
    fn check_dropped(text: &str, expected: &[(Option<&str>, &str)], mark: &str, message: &str) {
        let info = parse(text);
        assert_eq!(comments(&info), expected, "{text:?}");
        assert_eq!(info.acronym.as_deref(), Some("A"), "{text:?}");
        let at = text.rfind(mark).unwrap();
        assert_eq!(
            info.diagnostics,
            [Diagnostic::new(
                "t.xml",
                Some(crate::Location::Offset(at)),
                message
            )],
            "{text:?}"
        );
    }

    #[test]
    fn only_the_entities_xml_predefines_are_replaced() {
        // `quick-xml` replaces every HTML5 entity with its `escape-html`
        // feature, which another crate in a build may turn on. XML predefines
        // five (spec 4.6); any other reference that no DTD here declares
        // cannot be replaced.
        let reference = |bad: &str| {
            format!("bad XML (the reference `{bad}` cannot be replaced); the value is left out")
        };
        let head = format!("{OPEN}<comment>en</comment>");
        let tail = "<acronym>A</acronym></mime-type>";
        check_dropped(
            &format!("{head}<comment>a&nbsp;b</comment>{tail}"),
            &[(None, "en")],
            "&nbsp;",
            &reference("&nbsp;"),
        );
        check_dropped(
            &format!("{head}<comment xml:lang=\"de&nbsp;\">D</comment>{tail}"),
            &[(None, "en")],
            "xml:lang=\"de&nbsp;",
            &reference("&nbsp;"),
        );
        check_dropped(
            &format!("{head}<comment>&copy;</comment>{tail}"),
            &[(None, "en")],
            "&copy;",
            &reference("&copy;"),
        );
        // A namespace with one is not the spec's.
        let colon = NAMESPACE.replacen(':', "&colon;", 1);
        let info = parse(&format!(
            r#"<mime-type xmlns="{colon}"><comment>a</comment></mime-type>"#
        ));
        assert!(comments(&info).is_empty(), "{:?}", info.comment);
        assert_eq!(
            info.diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            [format!(
                "not a per-type file: the root element `mime-type` is not in the namespace `{NAMESPACE}`; it is ignored"
            )]
        );
        // Nor is a root element's namespace with one read.
        assert_eq!(root_element(b"<r xmlns=\"urn&colon;x\"/>"), None);
        assert_eq!(root_element(b"<x:r xmlns:x=\"urn:&nbsp;\"/>"), None);
        assert_eq!(
            root_element(b"<r xmlns=\"urn:&lt;&gt;&amp;&apos;&quot;\"/>"),
            Some(("urn:<>&'\"".into(), "r".into()))
        );
    }

    #[test]
    fn a_field_with_an_attribute_whose_name_is_not_an_xml_name_is_dropped() {
        // `quick-xml` takes any bytes up to `=` or ASCII white space as an
        // attribute's name. Before, the German text replaced the
        // untranslated one, its language not read.
        let v11 = "<?xml version=\"1.1\"?>";
        let message = "bad XML (an attribute cannot be read: a name is not an XML name); the value is left out";
        for (declaration, name) in [
            (v11, "xml:lang\u{2028}"),
            (v11, "xml:lang\u{85}"),
            ("", "xml:lang\u{85}"),
            ("", "xml:lang\u{2028}"),
            ("", "1a"),
            ("", "-a"),
            ("", "a\u{D7}"),
            ("", "a:b:c"),
            ("", ":a"),
            ("", "a:"),
        ] {
            for bom in ["", "\u{feff}"] {
                let text = format!(
                    "{bom}{declaration}{OPEN}<comment>English</comment><comment {name}=\"de\">Deutsch</comment><acronym>A</acronym></mime-type>"
                );
                check_dropped(&text, &[(None, "English")], &format!("{name}="), message);
            }
        }
        // Names XML allows (spec 2.3, the same in XML 1.0 and 1.1).
        let info = parse(&format!(
            "{OPEN}<comment _a=\"1\" a.b-c\u{B7}9=\"2\" \u{C0}\u{300}=\"3\" \u{10000}=\"4\" x:y=\"5\" xml:lang=\"de\">D</comment></mime-type>"
        ));
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(comments(&info), [(Some("de"), "D")]);
    }

    #[test]
    fn a_field_with_any_attribute_value_that_cannot_be_read_is_dropped() {
        let reference = |bad: &str| {
            format!("bad XML (the reference `{bad}` cannot be replaced); the value is left out")
        };
        let v11 = "<?xml version=\"1.1\"?>";
        let dtd = "<!DOCTYPE mime-type [<!ENTITY e \"x\">]>";
        let cases = [
            ("", "a=\"&bogus;\"", reference("&bogus;")),
            ("", "a='x&#0;'", reference("&#0;")),
            ("", "a=\"&#1;\"", reference("&#1;")),
            (v11, "a=\"&#xFFFE;\"", reference("&#xFFFE;")),
            ("", "a=\"x\u{1}\"", not_allowed('\u{1}')),
            ("", "a=\"\u{FFFF}\"", not_allowed('\u{FFFF}')),
            (v11, "a=\"\u{80}\"", not_allowed('\u{80}')),
            // An entity the document's own DTD declares (a known limit).
            (dtd, "a=\"&e;\"", reference("&e;")),
            (
                "",
                "a=\"x&y\"",
                "bad XML (the reference `&y` is not ended); the value is left out".to_owned(),
            ),
            // A namespace declaration too.
            ("", "xmlns:p=\"&bogus;\"", reference("&bogus;")),
            ("", "xmlns=\"&bogus;\"", reference("&bogus;")),
            (
                "",
                &format!("xmlns=\"{NAMESPACE}&unknown;\""),
                reference("&unknown;"),
            ),
        ];
        for bom in ["", "\u{feff}"] {
            for (declaration, attribute, message) in &cases {
                let text = format!(
                    "{bom}{declaration}{OPEN}<comment>en</comment><comment xml:lang=\"fr\" {attribute}>F</comment><acronym>A</acronym></mime-type>"
                );
                check_dropped(&text, &[(None, "en")], attribute, message);
            }
        }
        // A prefix bound further up to a namespace that cannot be read: the
        // problem is at the field's tag.
        let text = format!(
            r#"<mime-type xmlns="{NAMESPACE}" xmlns:f="{NAMESPACE}&bogus;"><comment>en</comment><f:comment>F</f:comment><acronym>A</acronym></mime-type>"#
        );
        check_dropped(&text, &[(None, "en")], "<f:comment", &reference("&bogus;"));
        // What the version allows is kept, and the elements of other
        // namespaces are not looked at.
        let info = parse(&format!(
            "{v11}{OPEN}<comment a=\"&#1;&lt;&#x41;\" b='\"' xmlns:p=\"urn:p\">C</comment><x:comment xmlns:x=\"urn:x\" a=\"&bogus;\">X</x:comment></mime-type>"
        ));
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(comments(&info), [(None, "C")]);
    }

    #[test]
    fn a_field_with_lt_in_an_attribute_or_cdata_end_in_its_text_is_dropped() {
        // XML allows neither (spec 2.3 [10] `AttValue`, 2.4 [14] `CharData`);
        // `quick-xml` reads both.
        let head = format!("{OPEN}<comment>en</comment>");
        let tail = "<acronym>A</acronym></mime-type>";
        let lt = "bad XML (an attribute cannot be read: `<` in a value); the value is left out";
        for attribute in ["a=\"d<e\"", "xml:lang='d<e'"] {
            check_dropped(
                &format!("{head}<comment {attribute}>X</comment>{tail}"),
                &[(None, "en")],
                attribute,
                lt,
            );
        }
        let end = "bad XML (`]]>` in text); the value is left out";
        check_dropped(
            &format!("{head}<comment xml:lang=\"de\">a]]>b</comment>{tail}"),
            &[(None, "en")],
            "a]]>",
            end,
        );
        check_dropped(
            &format!("{head}<comment xml:lang=\"de\">a<b>]]></b></comment>{tail}"),
            &[(None, "en")],
            "]]>",
            end,
        );
        // What comes close is kept.
        let info = parse(&format!(
            "{OPEN}<comment a=\"&lt;\">a]]b]>c]]&gt;<![CDATA[]]]]><![CDATA[>]]></comment></mime-type>"
        ));
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(comments(&info), [(None, "a]]b]>c]]>]]>")]);
    }

    #[test]
    fn the_root_element_is_read_by_the_version_declared_at_the_start() {
        // XML 1.1 normalises U+0085 in an attribute value to a space (spec
        // 1.1, 2.11 and 3.3.3); only a declaration at the start gives it.
        let root = "<r xmlns=\"urn:a\u{85}b\"/>";
        for (start, namespace) in [
            ("<?xml version=\"1.1\"?>", "urn:a b"),
            ("\u{feff}<?xml version=\"1.1\"?>", "urn:a b"),
            ("", "urn:a\u{85}b"),
            (" <?xml version=\"1.1\"?>", "urn:a\u{85}b"),
            ("<!-- c --><?xml version=\"1.1\"?>", "urn:a\u{85}b"),
        ] {
            let text = format!("{start}{root}");
            assert_eq!(
                root_element(text.as_bytes()),
                Some((namespace.to_owned(), "r".to_owned())),
                "{text:?}"
            );
        }
    }

    /// The messages `info` holds.
    fn messages(info: &TypeInfo) -> Vec<&str> {
        info.diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect()
    }

    #[test]
    fn text_from_the_file_is_shown_escaped_and_cut_in_a_message() {
        let unended = |shown: &str| {
            format!("bad XML (the reference `&{shown}` is not ended); the value is left out")
        };
        let head = format!("{OPEN}<comment>en</comment>");
        let tail = "<acronym>A</acronym></mime-type>";
        let long = "x".repeat(5000);
        for (lang, shown) in [
            // Control characters that would clear and colour a terminal.
            (
                "&\u{1b}[2J\u{1b}[31mFAKE",
                unended(r"\u{1b}[2J\u{1b}[31mFAKE"),
            ),
            (
                &format!("&{long}"),
                unended(&format!("{}…", "x".repeat(32))),
            ),
            ("&\u{1}\nab", unended(r"\u{1}\nab")),
            (
                &format!("&{long};"),
                format!(
                    "bad XML (the reference `&{}…;` cannot be replaced); the value is left out",
                    "x".repeat(32)
                ),
            ),
        ] {
            let text = format!("{head}<comment xml:lang=\"{lang}\">X</comment>{tail}");
            check_dropped(&text, &[(None, "en")], "xml:lang", &shown);
            let shown = parse(&text).diagnostics[0].to_string();
            assert!(!shown.chars().any(char::is_control), "{shown:?}");
        }
        check_dropped(
            &format!("{head}<comment>&{long};</comment>{tail}"),
            &[(None, "en")],
            "&x",
            &format!(
                "bad XML (the reference `&{}…;` cannot be replaced); the value is left out",
                "x".repeat(32)
            ),
        );
        // A name given twice.
        let name = "a".repeat(40);
        check_dropped(
            &format!("{head}<comment {name}=\"1\" {name}=\"2\">X</comment>{tail}"),
            &[(None, "en")],
            &format!("{name}="),
            &format!(
                "bad XML (an attribute cannot be read: an attribute appears twice: {}…); the value is left out",
                "a".repeat(32)
            ),
        );
        // The root's name: names that are not XML names, and names that
        // are, with a zero-width joiner (a name character).
        for (root, shown) in [
            (
                "<\u{1b}[2Jr/>",
                r"the root element's name `\u{1b}[2Jr` is not an XML name",
            ),
            (
                "<p\u{1b}:mime-type/>",
                r"the root element's name `p\u{1b}:mime-type` is not an XML name",
            ),
            (
                "<\u{200d}r/>",
                r"the root element is `\u{200d}r`, not `mime-type`",
            ),
            (
                "<\u{200d}p:mime-type/>",
                &format!(
                    r"the root element `\u{{200d}}p:mime-type` is not in the namespace `{NAMESPACE}`"
                ),
            ),
            (
                &format!("<{name}/>"),
                &format!("the root element is `{}…`, not `mime-type`", "a".repeat(32)),
            ),
        ] {
            assert_eq!(
                messages(&parse(root)),
                [format!("not a per-type file: {shown}; it is ignored")],
                "{root:?}"
            );
        }
        // Names in what `quick-xml` finds.
        let info = parse(&format!("{OPEN}<comment>a</comment\u{1b}[2J></mime-type>"));
        assert_eq!(
            messages(&info),
            [
                r"bad XML (ill-formed document: expected `</comment>`, but `</comment\u{1b}[2J>` was found); the rest is skipped"
            ]
        );
        let info = parse(&format!("{OPEN}<comment>a</{name}></mime-type>"));
        assert_eq!(
            messages(&info),
            [format!(
                "bad XML (ill-formed document: expected `</comment>`, but `</{}…>` was found); the rest is skipped",
                "a".repeat(32)
            )]
        );
    }

    #[test]
    fn a_declaration_not_starting_with_its_version_is_worded_correctly() {
        use quick_xml::errors::IllFormedError;
        let err = quick_xml::Error::IllFormed(IllFormedError::MissingDeclVersion(Some(
            "encoding\u{1b}".to_owned(),
        )));
        assert_eq!(
            reader_error(&err),
            r"ill-formed document: an XML declaration starts with `encoding\u{1b}`, not `version`"
        );
    }

    #[test]
    fn mismatched_tags_are_shown_where_they_differ() {
        use quick_xml::errors::IllFormedError;
        let long = "a".repeat(40);
        let err = quick_xml::Error::IllFormed(IllFormedError::MismatchedEndTag {
            expected: format!("{long}1"),
            found: format!("{long}2\u{1b}"),
        });
        assert_eq!(
            reader_error(&err),
            format!(
                r"ill-formed document: expected `</…{0}1>`, but `</…{0}2\u{{1b}}>` was found",
                "a".repeat(8)
            )
        );
        // Names that differ only in how an accent is written look
        // different.
        let info = parse(&format!(
            "{OPEN}<comment>x</comment><caf\u{e9}>y</cafe\u{301}></mime-type>"
        ));
        assert_eq!(
            messages(&info),
            [
                "bad XML (ill-formed document: expected `</caf\\u{e9}>`, but `</cafe\\u{301}>` was found); the rest is skipped"
            ]
        );
    }

    #[test]
    fn mismatched_tags_differing_in_look_alike_characters_look_different() {
        for (start, end, shown) in [
            (
                "\u{c5}",
                "\u{212b}",
                r"expected `</\u{c5}>`, but `</\u{212b}>`",
            ),
            ("K", "\u{212a}", r"expected `</K>`, but `</\u{212a}>`"),
            (
                "\u{ac00}",
                "\u{1100}\u{1161}",
                "expected `</\\u{ac00}>`, but `</\\u{1100}\u{1161}>`",
            ),
        ] {
            let info = parse(&format!(
                "{OPEN}<comment>x</comment><{start}>y</{end}></mime-type>"
            ));
            assert_eq!(
                messages(&info),
                [format!(
                    "bad XML (ill-formed document: {shown} was found); the rest is skipped"
                )]
            );
        }
    }

    #[test]
    fn an_empty_prefixed_namespace_declaration_drops_a_field_in_xml_1_0() {
        // Namespaces in XML 1.0, 5, "No Prefix Undeclaring"; 1.1 allows it
        // (Namespaces in XML 1.1, 6.1).
        let message = "bad XML (an attribute cannot be read: a prefix's namespace declaration is empty in XML 1.0); the value is left out";
        for declaration in ["", "<?xml version=\"1.0\"?>", "<?xml version=\"2.0\"?>"] {
            for attribute in ["xmlns:p=\"\"", "xmlns:p=''"] {
                check_dropped(
                    &format!(
                        "{declaration}{OPEN}<comment>en</comment><comment {attribute} xml:lang=\"de\">X</comment><acronym>A</acronym></mime-type>"
                    ),
                    &[(None, "en")],
                    "xmlns:p",
                    message,
                );
            }
        }
        // On a field's tag that binds its own prefix so, whatever it then
        // is in.
        for declaration in ["", "<?xml version=\"1.0\"?>"] {
            check_dropped(
                &format!(
                    "{declaration}{OPEN}<comment>en</comment><p:comment xmlns:p=\"\">X</p:comment><acronym>A</acronym></mime-type>"
                ),
                &[(None, "en")],
                "xmlns:p",
                message,
            );
        }
        let info = parse(&format!(
            "<?xml version=\"1.1\"?>{OPEN}<p:comment xmlns:p=\"\">X</p:comment></mime-type>"
        ));
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        // XML 1.1 allows it, and either version an empty default
        // namespace.
        for (declaration, attribute) in [
            ("<?xml version=\"1.1\"?>", "xmlns:p=\"\""),
            ("", "xmlns=\"\""),
            ("<?xml version=\"1.1\"?>", "xmlns=\"\""),
        ] {
            let info = parse(&format!(
                "{declaration}{OPEN}<comment>en</comment><comment {attribute} xml:lang=\"de\">X</comment></mime-type>"
            ));
            assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
            assert_eq!(comments(&info), [(None, "en"), (Some("de"), "X")]);
        }
    }

    #[test]
    fn a_root_namespace_that_cannot_be_read_gives_no_root_element() {
        for root in [
            &b"<r xmlns=\"urn:&#1;\"/>"[..],
            b"<r xmlns=\"urn:&#xFFFE;\"/>",
            b"<?xml version=\"1.1\"?><r xmlns=\"urn:&#xFFFE;\"/>",
            b"<r xmlns=\"urn:<\"/>",
            b"<x:r xmlns:x=\"urn:\x01\"/>",
            b"<?xml version=\"1.1\"?><r xmlns=\"urn:\x01\"/>",
            b"<r xmlns=\"urn:&x\"/>",
        ] {
            assert_eq!(
                root_element(root),
                None,
                "{}",
                String::from_utf8_lossy(root)
            );
        }
        // XML 1.1 allows a reference to a control.
        assert_eq!(
            root_element(b"<?xml version=\"1.1\"?><r xmlns=\"urn:&#1;\"/>"),
            Some(("urn:\u{1}".into(), "r".into()))
        );
    }

    #[test]
    fn a_tag_inside_a_field_is_checked_as_the_fields_own() {
        // The problem is at the attribute in the inner tag.
        let head = format!("{OPEN}<comment>en</comment>");
        let tail = "<acronym>A</acronym></mime-type>";
        let reference =
            "bad XML (the reference `&bogus;` cannot be replaced); the value is left out";
        let twice = "bad XML (an attribute cannot be read: an attribute appears twice: x); the value is left out";
        let name = "bad XML (an attribute cannot be read: a name is not an XML name); the value is left out";
        let lt = "bad XML (an attribute cannot be read: `<` in a value); the value is left out";
        for (inner, mark, message) in [
            (
                "<b x=\"&bogus;\" x=\"1\" 1y=\"<\">c</b>",
                "x=\"&",
                reference,
            ),
            ("<b x=\"1\" x=\"2\">c</b>", "x=\"2", twice),
            ("<b><i 1y=\"2\"/>c</b>", "1y", name),
            ("<b y='<'>c</b>", "y=", lt),
        ] {
            check_dropped(
                &format!("{head}<comment xml:lang=\"de\">a{inner}d</comment>{tail}"),
                &[(None, "en")],
                mark,
                message,
            );
        }
        // Clean markup inside is still read as its text.
        let info = parse(&format!(
            "{OPEN}<comment xml:lang=\"de\">a<b x=\"1\" y='&amp;' xmlns:p=\"urn:p\"><p:i/>c</b>d</comment></mime-type>"
        ));
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(comments(&info), [(Some("de"), "acd")]);
    }

    #[test]
    fn a_fields_namespace_declarations_are_checked_in_any_order() {
        let head = format!("{OPEN}<comment>en</comment>");
        let tail = "<acronym>A</acronym></mime-type>";
        let twice = |name: &str| {
            format!(
                "bad XML (an attribute cannot be read: an attribute appears twice: {name}); the value is left out"
            )
        };
        let name = "bad XML (an attribute cannot be read: a name is not an XML name); the value is left out";
        for (tag, mark, message) in [
            (
                "<comment xmlns:=\"urn:x\" xml:lang=\"de\">",
                "xmlns:=",
                name.to_owned(),
            ),
            (
                &format!("<comment xmlns=\"{NAMESPACE}\" xmlns=\"urn:x\">"),
                "xmlns=",
                twice("xmlns"),
            ),
            (
                &format!("<comment xmlns=\"urn:x\" xmlns=\"{NAMESPACE}\">"),
                "xmlns=",
                twice("xmlns"),
            ),
            (
                &format!("<p:comment xmlns:p=\"{NAMESPACE}\" xmlns:p=\"urn:x\">"),
                "xmlns:p=",
                twice("xmlns:p"),
            ),
            (
                &format!("<p:comment xmlns:p=\"urn:x\" xmlns:p=\"{NAMESPACE}\">"),
                "xmlns:p=",
                twice("xmlns:p"),
            ),
        ] {
            let close = if tag.starts_with("<p:") {
                "</p:comment>"
            } else {
                "</comment>"
            };
            check_dropped(
                &format!("{head}{tag}X{close}{tail}"),
                &[(None, "en")],
                mark,
                &message,
            );
        }
        // A name not followed by `=` is where it starts, in a self-closed
        // tag too.
        check_dropped(
            &format!("{head}<comment xml:lang=\"de\" key/>{tail}"),
            &[(None, "en")],
            "key",
            "bad XML (an attribute cannot be read: a name is not followed by `=`); the value is left out",
        );
        // An element of another namespace is still not looked at otherwise.
        let info = parse(&format!(
            "{OPEN}<x:comment xmlns:x=\"urn:x\" a=\"1\" a=\"2\" xmlns:y=\"&bogus;\">X</x:comment><comment>a</comment></mime-type>"
        ));
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(comments(&info), [(None, "a")]);
    }

    #[test]
    fn a_namespace_error_is_at_its_tag() {
        // `quick-xml` stops at these; the problem is at the tag's `<`.
        let head = format!("{OPEN}<comment>en</comment>");
        for (attribute, problem) in [
            (
                "xmlns:xmlns=\"urn:\u{1b}[2J\"",
                r"the namespace prefix 'xmlns' cannot be bound to 'urn:\u{1b}[2J'",
            ),
            (
                "xmlns:xml=\"urn:x\"",
                "the namespace prefix 'xml' cannot be bound to 'urn:x'",
            ),
            (
                "xmlns:p=\"http://www.w3.org/XML/1998/namespace\"",
                "the namespace prefix 'p' cannot be bound to 'http://www.w3.org/XML/1998/namespace'",
            ),
        ] {
            let text = format!("{head}<comment {attribute}>X</comment></mime-type>");
            let info = parse(&text);
            assert_eq!(
                info.diagnostics,
                [Diagnostic::new(
                    "t.xml",
                    Some(crate::Location::Offset(head.len())),
                    format!("bad XML ({problem}); the rest is skipped")
                )],
                "{text:?}"
            );
        }
    }

    #[test]
    fn an_attribute_before_a_fields_own_prefix_declaration_is_reported() {
        // `quick-xml` reads a tag's namespace declarations only up to an
        // attribute it cannot read, so which namespace the field is in is
        // not known. Before, it was dropped without a word.
        let head = format!("{OPEN}<comment>en</comment>");
        let tail = "<acronym>A</acronym></mime-type>";
        let no_eq = "bad XML (an attribute cannot be read: a name is not followed by `=`); the value is left out";
        let unquoted =
            "bad XML (an attribute cannot be read: a value is not quoted); the value is left out";
        for (text, mark, message) in [
            (
                format!("{head}<x:comment a xmlns:x=\"{NAMESPACE}\">X</x:comment>{tail}"),
                "a xmlns",
                no_eq,
            ),
            (
                format!("{head}<x:comment a=b xmlns:x='{NAMESPACE}'>X</x:comment>{tail}"),
                "b xmlns",
                unquoted,
            ),
            // The root binds the prefix to another namespace.
            (
                format!(
                    "<mime-type xmlns=\"{NAMESPACE}\" xmlns:x=\"urn:y\"><comment>en</comment><x:comment a xmlns:x=\"{NAMESPACE}\">X</x:comment>{tail}"
                ),
                "a xmlns",
                no_eq,
            ),
            // In another namespace, the attribute that cannot be read is
            // the one reported, not another problem before it.
            (
                format!("{head}<x:comment xmlns:x=\"urn:x\" k=\"&bogus;\" a>X</x:comment>{tail}"),
                "a>X",
                no_eq,
            ),
        ] {
            check_dropped(&text, &[(None, "en")], mark, message);
        }
    }

    #[test]
    fn a_reserved_default_namespace_or_element_name_drops_a_field() {
        // Namespaces in XML 1.0, 3rd ed., 3: neither reserved namespace may
        // be declared the default one, and no element name has the prefix
        // `xmlns`. Before, the field was taken for an element of another
        // namespace, or the field kept.
        let head = format!("{OPEN}<comment>en</comment>");
        let tail = "<acronym>A</acronym></mime-type>";
        let reserved = |namespace: &str| {
            format!(
                "bad XML (an attribute cannot be read: the default namespace cannot be `{namespace}`); the value is left out"
            )
        };
        let xml = "http://www.w3.org/XML/1998/namespace";
        let xmlns = "http://www.w3.org/2000/xmlns/";
        let prefix = "bad XML (an element's name has the prefix `xmlns`); the value is left out";
        let name = "bad XML (an element's name is not an XML name); the value is left out";
        for (fields, mark, message) in [
            (
                format!("<comment xmlns=\"{xml}\">X</comment>"),
                "xmlns=",
                reserved(xml),
            ),
            (
                format!("<comment xml:lang=\"de\" xmlns='{xmlns}'>X</comment>"),
                "xmlns=",
                reserved(xmlns),
            ),
            // Written with a reference, and on a tag in the spec's
            // namespace.
            (
                "<comment xmlns=\"&#104;ttp://www.w3.org/2000/xmlns/\">X</comment>".to_owned(),
                "xmlns=",
                reserved(xmlns),
            ),
            (
                format!("<f:comment xmlns:f=\"{NAMESPACE}\" xmlns=\"{xml}\">X</f:comment>"),
                "xmlns=",
                reserved(xml),
            ),
            (
                format!("<expanded-acronym xmlns=\"{xml}\">X</expanded-acronym>"),
                "xmlns=",
                reserved(xml),
            ),
            (
                "<xmlns:comment>X</xmlns:comment>".to_owned(),
                "xmlns:comment>X",
                prefix.to_owned(),
            ),
            // Inside a field.
            (
                "<comment xml:lang=\"de\">a<x:y:z/>d</comment>".to_owned(),
                "x:y:z",
                name.to_owned(),
            ),
            (
                "<comment xml:lang=\"de\">a<b><xmlns:i>c</xmlns:i></b>d</comment>".to_owned(),
                "xmlns:i>c",
                prefix.to_owned(),
            ),
            (
                format!("<comment xml:lang=\"de\">a<b xmlns=\"{xmlns}\">c</b>d</comment>"),
                "xmlns=",
                reserved(xmlns),
            ),
        ] {
            check_dropped(
                &format!("{head}{fields}{tail}"),
                &[(None, "en")],
                mark,
                &message,
            );
        }
        // The `xml` prefix may name an element, in its own namespace.
        let info = parse(&format!(
            "{OPEN}<xml:comment>X</xml:comment><comment>a<xml:b>c</xml:b>d</comment></mime-type>"
        ));
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(comments(&info), [(None, "acd")]);
    }

    #[test]
    fn a_field_named_tag_whose_name_is_not_a_qname_drops_the_field() {
        // Namespaces in XML 1.0, 3rd ed., 5 and 7, as for a tag inside a
        // field. Before, the field was kept when the root bound the prefix,
        // and left out without a word when no declaration did.
        let name = "bad XML (an element's name is not an XML name); the value is left out";
        let tail = "<acronym>A</acronym>";
        let bound = |prefix: &str| {
            format!(
                "<mime-type xmlns=\"{NAMESPACE}\" xmlns:{prefix}=\"{NAMESPACE}\"><comment>en</comment>"
            )
        };
        for (text, mark) in [
            (
                format!(
                    "{}<1p:comment xml:lang=\"de\">X</1p:comment>{tail}</mime-type>",
                    bound("1p")
                ),
                "1p:comment xml",
            ),
            (
                format!(
                    "{}<-p:comment xml:lang=\"de\">X</-p:comment>{tail}</mime-type>",
                    bound("-p")
                ),
                "-p:comment xml",
            ),
            // U+0085 is a line end in XML 1.1, not a name character.
            (
                format!(
                    "<?xml version=\"1.1\"?>{}<p\u{85}:comment xml:lang=\"de\">X</p\u{85}:comment>{tail}</mime-type>",
                    bound("p\u{85}")
                ),
                "p\u{85}:comment xml",
            ),
            (
                format!(
                    "{OPEN}<comment>en</comment><:comment xml:lang=\"de\">X</:comment>{tail}</mime-type>"
                ),
                ":comment xml",
            ),
            (
                format!(
                    "<m:mime-type xmlns:m=\"{NAMESPACE}\"><comment>en</comment><:comment xml:lang=\"de\">X</:comment>{tail}</m:mime-type>"
                ),
                ":comment xml",
            ),
            // The name is looked at before the tag's attributes.
            (
                format!(
                    "{OPEN}<comment>en</comment><1p:comment xml:lang=\"de\" xmlns:1p=\"{NAMESPACE}\">X</1p:comment>{tail}</mime-type>"
                ),
                "1p:comment xml",
            ),
            // Inside a field.
            (
                format!(
                    "<mime-type xmlns=\"{NAMESPACE}\" xmlns:1p=\"urn:q\"><comment>en</comment><comment xml:lang=\"de\">a<1p:b/>d</comment>{tail}</mime-type>"
                ),
                "1p:b",
            ),
        ] {
            check_dropped(&text, &[(None, "en")], mark, name);
        }
        // `p.` is a name.
        let info = parse(&format!(
            "{}<p.:comment xml:lang=\"de\">X</p.:comment></mime-type>",
            bound("p.")
        ));
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(comments(&info), [(None, "en"), (Some("de"), "X")]);
    }

    #[test]
    fn a_field_is_named_by_what_follows_the_last_colon() {
        // Before, `quick-xml`'s local name (after the first `:`) was looked
        // at, so `<x:y:comment>` was left out without a word.
        let name = "bad XML (an element's name is not an XML name); the value is left out";
        let head = format!(
            "<mime-type xmlns=\"{NAMESPACE}\" xmlns:x=\"{NAMESPACE}\" xmlns:o=\"urn:o\"><comment>en</comment>"
        );
        let tail = "<acronym>A</acronym></mime-type>";
        for (field, mark) in [
            (
                "<x:y:comment xml:lang=\"de\">X</x:y:comment>",
                "x:y:comment xml",
            ),
            ("<x:y:acronym n=\"1\">B</x:y:acronym>", "x:y:acronym n"),
            (
                "<x:y:expanded-acronym n=\"1\">B</x:y:expanded-acronym>",
                "x:y:expanded-acronym n",
            ),
            (
                "<x:y:z:comment xml:lang=\"de\">X</x:y:z:comment>",
                "x:y:z:comment xml",
            ),
            // Whatever namespace the prefix is bound to, or none.
            (
                "<o:y:comment xml:lang=\"de\">X</o:y:comment>",
                "o:y:comment xml",
            ),
            (
                "<p:y:comment xml:lang=\"de\">X</p:y:comment>",
                "p:y:comment xml",
            ),
        ] {
            let text = format!("{head}{field}{tail}");
            check_dropped(&text, &[(None, "en")], mark, name);
            assert_eq!(parse(&text).expanded_acronym, None, "{text:?}");
        }
        // Other names are not fields, `:` or not: nothing is said of them.
        for other in [
            "<comment: xml:lang=\"de\">X</comment:>",
            "<x:y:z>X</x:y:z>",
            "<x:comment:y>X</x:comment:y>",
            "<comment:y>X</comment:y>",
            "<o:comment>X</o:comment>",
        ] {
            let text = format!("{head}{other}{tail}");
            let info = parse(&text);
            assert!(
                info.diagnostics.is_empty(),
                "{text:?}: {:?}",
                info.diagnostics
            );
            assert_eq!(comments(&info), [(None, "en")], "{text:?}");
            assert_eq!(info.acronym.as_deref(), Some("A"), "{text:?}");
        }
    }

    #[test]
    #[ignore = "timing: run with `--release -- --ignored`"]
    fn many_languages_take_linear_time() {
        use std::fmt::Write as _;
        // Before, each value was looked for among all the languages seen:
        // quadratic in their number.
        let time = |n: usize| {
            let mut text = OPEN.to_owned();
            for at in 0..n {
                let _ = write!(text, "<comment xml:lang=\"l{at}\">x</comment>");
            }
            text.push_str("</mime-type>");
            (0..3)
                .map(|_| {
                    let start = std::time::Instant::now();
                    let info = parse(&text);
                    assert_eq!(info.comment.0.len(), n);
                    start.elapsed()
                })
                .min()
                .unwrap()
        };
        let (single, quadrupled) = (time(20_000), time(80_000));
        assert!(
            quadrupled < single * 8,
            "{quadrupled:?} for 80000 languages against {single:?} for 20000"
        );
    }
}
