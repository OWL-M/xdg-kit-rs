//! The per-type `MEDIA/SUBTYPE.xml` files (spec "The MEDIA/SUBTYPE.xml
//! files"), and the root element of XML documents for `XMLnamespaces`.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use quick_xml::XmlVersion;
use quick_xml::escape::resolve_xml_entity;
use quick_xml::events::attributes::{AttrError, Attribute};
use quick_xml::events::{BytesDecl, BytesRef, BytesStart, Event};
use quick_xml::name::{PrefixDeclaration, QName};

/// The namespace of the database's XML files.
const NAMESPACE: &str = "http://www.freedesktop.org/standards/shared-mime-info";
use xdg_kit_basedir::FileStamp;
use xdg_kit_locale::Locales;

use crate::error::quoted;
use crate::read::Problems;
use crate::{Diagnostic, MimeDatabase, MimeType};

/// Values by language (`None` for the untranslated one), a later value
/// replacing an earlier one for the same language. Each language is stored
/// once, and values refer to it by an ID, so a long language that many
/// fields share is neither copied nor hashed again for each.
#[derive(Debug, Default)]
struct Localized {
    /// Each language's ID, by name.
    ids: HashMap<String, usize>,
    /// The values, by language ID.
    values: HashMap<Option<usize>, String>,
}

impl Localized {
    /// The ID of `lang`, a new one if it has none yet.
    fn intern(&mut self, lang: &str) -> usize {
        looked_up_language();
        // Hashed and compared.
        crate::work::count(1 + 2 * lang.len());
        if let Some(&id) = self.ids.get(lang) {
            return id;
        }
        // Copied.
        crate::work::count(lang.len());
        copied_language();
        let id = self.ids.len();
        self.ids.insert(lang.to_owned(), id);
        id
    }

    /// The ID of a field's language `lang` ([`Lang`]), `None` for none.
    fn resolve(&mut self, lang: Lang) -> Option<usize> {
        match lang {
            Lang::Own(own) if own.is_empty() => None,
            Lang::Own(own) => Some(self.intern(&own)),
            Lang::Id(id) => id,
        }
    }

    fn set(&mut self, lang: Option<usize>, text: String) {
        // Hashed.
        crate::work::count(1);
        self.values.insert(lang, text);
    }

    /// The value for the first of `locales`' keys present, else the
    /// untranslated one.
    fn pick(&self, locales: &Locales) -> Option<String> {
        let get = |lang: Option<&str>| {
            let id = match lang {
                Some(lang) => Some(*self.ids.get(lang)?),
                None => None,
            };
            self.values.get(&id).cloned()
        };
        locales.select(|key| get(Some(key))).or_else(|| get(None))
    }
}

// Test-only counters: languages copied and looked up on this thread;
// outside tests nothing is counted or kept.
#[cfg(test)]
thread_local! {
    #[allow(
        clippy::missing_const_for_thread_local,
        reason = "already const; clippy flags it on targets without native thread locals"
    )]
    static LANGUAGE_COPIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    #[allow(
        clippy::missing_const_for_thread_local,
        reason = "already const; clippy flags it on targets without native thread locals"
    )]
    static LANGUAGE_LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Notes, in tests, that a language was copied (a test sees that an
/// inherited one is not copied per field).
#[inline]
fn copied_language() {
    #[cfg(test)]
    LANGUAGE_COPIES.with(|copies| copies.set(copies.get() + 1));
}

/// Notes, in tests only, that a language was looked up to be interned (a
/// field that copied an inherited language and found it interned would
/// look it up); does nothing outside tests.
#[inline]
fn looked_up_language() {
    #[cfg(test)]
    LANGUAGE_LOOKUPS.with(|lookups| lookups.set(lookups.get() + 1));
}

/// The languages looked up on this thread so far ([`looked_up_language`]).
#[cfg(test)]
fn language_lookups() -> usize {
    LANGUAGE_LOOKUPS.with(std::cell::Cell::get)
}

/// The languages copied on this thread so far ([`copied_language`]).
#[cfg(test)]
fn language_copies() -> usize {
    LANGUAGE_COPIES.with(std::cell::Cell::get)
}

/// The problem with a namespace declaration whose value is too large to
/// hold, or to copy.
fn too_large() -> String {
    "an entity expansion is too large to hold".to_owned()
}

#[cfg(test)]
thread_local! {
    #[allow(
        clippy::missing_const_for_thread_local,
        reason = "already const; clippy flags it on targets without native thread locals"
    )]
    static COPY_REFUSED: std::cell::Cell<(usize, usize)> =
        const { std::cell::Cell::new((0, usize::MAX)) };
    #[allow(
        clippy::missing_const_for_thread_local,
        reason = "already const; clippy flags it on targets without native thread locals"
    )]
    static LAST_RESERVED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Refuses the copy by [`try_copy`] on this thread numbered `refused`,
/// counting from 0 now, and no other (tests only; `usize::MAX`: none).
#[cfg(test)]
fn set_copy_refused(refused: usize) {
    COPY_REFUSED.with(|copy| copy.set((0, refused)));
}

/// The bytes the last expansion on this thread asked for (tests only).
#[cfg(test)]
fn last_reserved() -> usize {
    LAST_RESERVED.with(std::cell::Cell::get)
}

/// Copies `text`, reserving memory first; returns `None` if allocation fails.
/// Without a budget ([`crate::Limits::entity_expansion_per_byte`]), namespace
/// expansions have no size limit. Every copy therefore reserves memory this
/// way, and a failed copy makes the binding unreadable.
/// Only allocations sized by an expansion use this check. Small allocations
/// around it, such as map entries and binding slots, can still abort when
/// memory is almost exhausted ([`crate::Limits::entity_expansion_per_byte`]).
/// Tests can also force a failure through `set_copy_refused`.
fn try_copy(text: &str) -> Option<String> {
    #[cfg(test)]
    if COPY_REFUSED.with(|copy| {
        let (made, refused) = copy.get();
        copy.set((made.saturating_add(1), refused));
        made == refused
    }) {
        return None;
    }
    let mut copy = String::new();
    copy.try_reserve_exact(text.len()).ok()?;
    copy.push_str(text);
    Some(copy)
}

/// A field's language: its own `xml:lang` (empty: untranslated), or the
/// root's, by ID ([`Localized::intern`]; `None`: untranslated).
enum Lang {
    Own(String),
    Id(Option<usize>),
}

/// What a type's XML files say, merged over the folders.
///
/// Each file is read using the XML version declared at its start, or 1.0
/// if there is no declaration or its version is not 1.0 or 1.1. A
/// declaration outside XML's grammar ([`declaration_grammar_problem`]),
/// or a processing instruction named `xml` in any casing at the start
/// ([`named_xml`]), is ignored with a diagnostic, and the file read as
/// 1.0. The root must be `mime-type` in the spec's namespace, with a
/// valid `QName`. Files with another root or an invalid name
/// (`<1p:mime-type>`, `<:mime-type>`, `<x:y:mime-type>`) are ignored with
/// a diagnostic ([`structure_problem`]).
///
/// A field is an element directly inside the root, in the spec's namespace
/// or in none, named `comment`, `acronym` or `expanded-acronym`. For this
/// check, the name is the part after the last `:`, or the whole name if
/// there is no `:`. Its language is its own `xml:lang`, else the root's
/// (XML 1.0, 2.12); an empty one means untranslated. A field is dropped
/// with a diagnostic when:
/// - it takes the root's `xml:lang` and that cannot be read: any attribute
///   of the root's tag fails the checks a field's tag has (below), so the
///   language is not known ([`root_language`]);
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
///   (`xmlns:p=""`), has a prefix no declaration binds, or has the
///   expanded name of one before it (`p:a` and `q:a`, both prefixes bound
///   to one namespace; 6);
/// - its tag's name, or that of a tag inside it, is not a `QName`
///   (`<x:y:comment>`, `<1p:comment>`, `<:comment>`, or `<x:y:z>` inside
///   it; Namespaces in XML 1.0, 5 and 7) or has the prefix `xmlns` (3):
///   one check for both kinds of tag, made before their attributes are
///   looked at, and on the field's own tag whatever namespace its prefix
///   is bound to, or none; or a tag inside it has a prefix no declaration
///   binds;
/// - its namespace's declaration, on its tag or further up, has a value
///   that cannot be read as above: it is not taken for an element of
///   another namespace;
/// - its tag declares a namespace twice, under a name that is not a `QName`
///   (`xmlns:`), or, as above, a prefix's namespace empty
///   (`<p:comment xmlns:p="">`) or a reserved namespace the default; or it
///   has an attribute that cannot be read, which stops the declarations
///   after it being read (`<p:comment a xmlns:p="…">`): whatever namespace
///   it is then in, or none, it is not taken for an element of another
///   namespace;
/// - the file ends, or `quick-xml` finds it not well formed, before the
///   field's end: the rest of the file is skipped.
///
/// Namespaces are resolved here ([`Scopes`]). A tag that binds `xml` to
/// another namespace than its own, declares `xmlns`, or binds another
/// prefix to either's namespace (Namespaces in XML 1.0, 3), compared after
/// references are replaced ([`forbidden`]), or that has more bindings in
/// scope than [`crate::Limits::max_namespace_bindings`], skips the rest of
/// the file with a diagnostic. On an element that is not a field or the
/// root, nor inside one, a problem as libxml2 reports one ([`tag_problem`]:
/// a name that is not a `QName`, a prefix no declaration binds, the prefix
/// `xmlns`, a reserved default namespace, in XML 1.0 a prefix's namespace
/// empty, two attributes with one name or one expanded name, or any other
/// attribute a field's tag could not have) is reported at its tag, and
/// that element and what is in it are skipped; reading goes on. So is an
/// element named as a field whose prefix no declaration binds
/// (`<q:comment>`), which is not read as one. On the root's tag such an
/// attribute makes its language unknown (above).
/// Inside a skipped element, a misplaced declaration or document type is
/// still reported, as anywhere in the root.
///
/// Usable internal entities the first document type before the root
/// declares in its internal subset, the text of internal parameter entities
/// it refers to between declarations included where they stand (with
/// `standalone="yes"`, for a reference outside such text, not one declared
/// only in it) ([`declared_entities`]; one with no `<` whose references name
/// usable entities, declared before or after it, with no loop) are replaced
/// in namespace declarations only ([`Entities::expand`]), and the value then
/// checked as one written out; so `xmlns="&ns_svg;"` declares the namespace
/// it stands for. Their expansions, over all the file's declarations, may
/// not pass the budget, by default the file's length and 64 KiB
/// ([`crate::Limits::entity_expansion_per_byte`]): past that a declaration
/// cannot be read. Elsewhere an attribute value that uses one is unknown,
/// not an error; so is one that uses an entity not declared there, or one
/// whose text does, when the document has an external subset or a
/// parameter-entity reference and is not `standalone="yes"`
/// ([`Entities::open`]). A field whose text or language (its own or the
/// root's) uses one is dropped with a diagnostic. A namespace declared with
/// an entity not declared there is unknown: an element named in it is
/// handled as one in a namespace that cannot be read (a field dropped, the
/// file not a per-type file, another element skipped), while an attribute
/// named in it is read, with no check for a repeat; an unknown namespace is
/// not a reserved one and equals no other. A reference to any other entity
/// is an error: one not usable, external or unparsed, or, in a document that
/// is not open, not declared. So is, in every document, a character
/// reference to no legal character or an entity reference whose name is not
/// an XML name. The document type declares nothing when its internal subset
/// holds a literal with such a reference or a `&` not starting one; an
/// entity's literal with a `%` (a parameter-entity reference, which may not
/// stand in a declaration there, or a bare one); a markup declaration with
/// one outside its literals; a reference to an internal parameter entity
/// that includes itself, whose text is not whole declarations, or past the
/// bytes included allowed (by default the document type's length and 64
/// KiB, [`crate::Limits::entity_expansion_per_byte`]); an `ATTLIST` default
/// with `<`; a processed default referring, directly or through other
/// entities, to one a value could not use where the default stands
/// (undeclared there in a closed document, external, unparsed or not
/// usable; 4.1); a processing instruction named `xml` in any casing, or
/// whose target is missing or not followed by white space
/// ([`instruction_problem`]); a head whose name is not an XML name or is not
/// followed by white space, `[` or `>`, with other text after the name or
/// external ID, or with more than white space between the subset's `]` and
/// `>`, or a subset holding what its grammar does not allow; a markup
/// declaration not written whole as XML's grammar says (an `ENTITY`,
/// `ELEMENT`, `ATTLIST` or `NOTATION` one; another or a lower-case keyword,
/// a conditional section or a nested document type), a comment with `--`
/// inside, or a public literal, in the head too, with other than
/// `PubidChar`s ([`grammatical_end`]); a character the version does not
/// allow written as it is ([`may_be_written`]: not a `Char`, or in XML 1.1 a
/// restricted one) in a literal, comment or processing instruction of the
/// subset, or in the head's system literal; or any other form libxml2
/// refuses ([`declared_entities`]), written other than `<!DOCTYPE` and white
/// space included. Such a document type, the first before the root, is
/// reported once, at it, with a diagnostic of its own, besides those where
/// its entities are used.
///
/// A processing instruction libxml2 refuses ([`instruction_problem`]) is
/// reported as an XML declaration out of place is, at the start too, and so
/// is a comment or processing instruction holding a character the version
/// does not allow written as it is: before or inside the root the values are
/// still read; after it the rest is skipped.
///
/// XML 1.1 makes U+0085 and U+2028 line ends (spec 1.1, 2.11): in text, in
/// attribute values and before or after the root element they are read as
/// such. Inside a tag or the declaration they are not white space here, as
/// `quick-xml` takes only ASCII white space there, so a tag that uses them
/// is misread (a known limit), with a diagnostic: nothing after one in a
/// start tag can be read, so its field is dropped, its element skipped, or,
/// on the root, the language unknown, or the file ignored when a namespace
/// declaration after it is lost
/// (`<mime-type xml:lang="de"\u{85} xmlns="…">`); an element name that
/// takes one in is another name, so the element is not the one meant, and
/// is not an XML name, so the file is ignored, the field dropped, or the
/// element skipped; and an end tag misread so that it no longer matches its
/// start tag skips the rest of the file. In a document type they are not
/// white space either (the same known limit; libxml2, which reads XML 1.1
/// as 1.0, refuses them there too): one after its name, external ID or
/// subset refuses the document type.
#[derive(Debug, Default)]
pub(crate) struct TypeInfo {
    comment: Localized,
    /// Untranslated only: spec 0.22 dropped `xml:lang` on the acronyms.
    acronym: Option<String>,
    expanded_acronym: Option<String>,
    diagnostics: Vec<Diagnostic>,
    /// Whether any folder has a file for the type.
    found: bool,
    /// Whether a path could not be checked (an error other than its
    /// absence), so the answer may change without any file changing.
    unchecked: bool,
}

impl TypeInfo {
    /// Reads the type's XML file from each folder, least important first,
    /// so a more important folder's value replaces a less important one for
    /// the same language. Each folder gives the paths its file may have
    /// ([`type_file_paths`]); the first that is a file is read, within
    /// `limits`. A path that cannot be checked, for another reason than its
    /// absence, is reported as a file that cannot be read is, and ends that
    /// folder's search. The diagnostics start with `listing`, from listing
    /// the folders; one the listing already made is not made again.
    pub(crate) fn read(
        folders: impl IntoIterator<Item = Vec<PathBuf>>,
        listing: Vec<Diagnostic>,
        limits: &crate::Limits,
    ) -> Self {
        let mut info = Self {
            diagnostics: listing,
            ..Self::default()
        };
        for paths in folders {
            let mut file = None;
            for path in paths {
                // Each path looked at.
                crate::work::count(1);
                match probe(&path, std::fs::Metadata::is_file) {
                    Ok(false) => {}
                    Ok(true) => {
                        file = Some(path);
                        break;
                    }
                    Err(err) => {
                        info.unchecked = true;
                        let diagnostic =
                            Diagnostic::new(&path, None, format!("cannot be read: {err}"));
                        // Compared with each diagnostic so far.
                        crate::work::count(info.diagnostics.len());
                        if !info.diagnostics.contains(&diagnostic) {
                            info.diagnostics.push(diagnostic);
                        }
                        break;
                    }
                }
            }
            let Some(path) = file else {
                continue;
            };
            info.found = true;
            match xdg_kit_basedir::read_regular_file_with(&path, limits.max_file_len) {
                Ok(bytes) => info.parse_within(&path, &bytes, limits),
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

    /// Whether a path could not be checked ([`TypeInfo::read`]).
    pub(crate) fn unchecked(&self) -> bool {
        self.unchecked
    }

    /// Parses one file within the default limits.
    #[cfg(any(test, fuzzing))]
    fn parse(&mut self, path: &Path, bytes: &[u8]) {
        self.parse_within(path, bytes, &crate::Limits::default());
    }

    /// Parses one file within `limits`: the problems listed and the
    /// namespace bindings in scope.
    fn parse_within(&mut self, path: &Path, bytes: &[u8], limits: &crate::Limits) {
        let mut problems = Problems::new(path, &mut self.diagnostics, limits.max_diagnostics);
        let mut reader = quick_xml::Reader::from_reader(bytes);
        // `<comment/>` is read as `<comment></comment>`: an empty value.
        reader.config_mut().expand_empty_elements = true;
        // `quick-xml` skips a byte order mark without counting it, so its
        // length is added to make reported offsets offsets in the file.
        let bom = skipped_mark(bytes);
        let offset = |at: u64| usize::try_from(at).unwrap_or(usize::MAX);
        let location = |at: usize| Some(crate::Location::Offset(at.saturating_add(bom)));
        let left_out = |problem: &str| format!("bad XML ({problem}); the value is left out");
        let mut depth = 0usize;
        let mut seen = Seen::default();
        // The element being collected and its language.
        let mut field: Option<(Field, Option<usize>, String)> = None;
        // The root's language, by ID, which a field without one takes.
        let mut inherited: Result<Option<usize>, String> = Ok(None);
        // The version the declaration at the start gives, if any.
        let mut version = XmlVersion::Implicit1_0;
        let mut scopes = Scopes::new(limits);
        // The depth of the field, or of the element skipped, whose inside
        // is not checked as other elements are.
        let mut unchecked = None;
        loop {
            let at = offset(reader.buffer_position());
            let event = match read_next(&mut reader, &mut scopes, &mut seen, version) {
                Ok(event) => event,
                Err((problem_at, message)) => {
                    problems.report(location(problem_at.map_or(at, offset)), message);
                    return;
                }
            };
            let namespace = scopes.of(&event);
            let problem = structure_problem(&event, &namespace, at, depth, &seen, version);
            seen.note(&event, problem.is_some());
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
                problems.report(location(at), left_out(&problem));
                field = None;
            }
            match event {
                // Only a declaration at the start is one (only a byte order
                // mark, which `quick-xml` does not count, may come before).
                Event::Decl(_) | Event::PI(_) if at == 0 => match declared_at_start(&event) {
                    Some(Ok(declared)) => version = declared,
                    Some(Err(problem)) => problems.report(location(at), problem),
                    None => {}
                },
                Event::Start(start) => {
                    depth += 1;
                    if depth == 1 {
                        inherited = root_language(&start, version, &scopes)
                            .map(|lang| self.comment.resolve(Lang::Own(lang.unwrap_or_default())));
                    }
                    let read = if depth == 2 {
                        Field::starting(&start, &namespace, version, &inherited, &scopes)
                    } else {
                        // A tag inside a field is read as the field's own.
                        field.as_ref().map(|&(kind, ..)| {
                            let read = inner_tag(&start, &namespace, version, &scopes);
                            (kind, read.map(|_| Lang::Id(None)))
                        })
                    };
                    // A field whose tag, or a tag inside it, cannot be read
                    // is dropped, as one whose value cannot be decoded.
                    match read {
                        Some((kind, Ok(lang))) if depth == 2 => {
                            field = Some((kind, self.comment.resolve(lang), String::new()));
                            unchecked = Some(depth);
                        }
                        Some((_, Err(problem))) => {
                            field = None;
                            // The field's, if it is not yet.
                            unchecked.get_or_insert(depth);
                            problems.report(
                                location(at + problem.at.map_or(0, |inside| inside + 1)),
                                left_out(&problem.message),
                            );
                        }
                        // Another element, not inside a field or one skipped.
                        None if depth > 1 && unchecked.is_none() => {
                            if let Some(problem) = tag_problem(&start, &namespace, version, &scopes)
                            {
                                problems.report(
                                    location(at),
                                    format!("bad XML ({problem}); the element is skipped"),
                                );
                                unchecked = Some(depth);
                            }
                        }
                        _ => {}
                    }
                }
                Event::End(_) => {
                    if depth == 2
                        && let Some((kind, lang, value)) = field.take()
                    {
                        let (comment, acronym) = (&mut self.comment, &mut self.acronym);
                        kind.keep(lang, value, comment, acronym, &mut self.expanded_acronym);
                    }
                    unchecked = unchecked.filter(|&from| from != depth);
                    depth = depth.saturating_sub(1);
                    seen.ended = depth == 0;
                    scopes.pop();
                }
                Event::Eof => return,
                _ => {}
            }
        }
    }
}

/// Reads the next event of a per-type file of XML `version` with `reader`;
/// for a start tag, opens its scope in `scopes` ([`Scopes::push`]), and
/// for a document type notes the entities it declares, and in `seen`
/// whether it was refused ([`Scopes::declare_entities`]). Gives the
/// problem otherwise, which skips the rest, and where as `reader` counts
/// (`None`: at the tag being read).
fn read_next<'a>(
    reader: &mut quick_xml::Reader<&'a [u8]>,
    scopes: &mut Scopes,
    seen: &mut Seen,
    version: XmlVersion,
) -> Result<Event<'a>, (Option<u64>, String)> {
    let skipped = |problem: String| format!("bad XML ({problem}); the rest is skipped");
    let exact = written_doctype(reader.get_ref());
    let at_start = reader.buffer_position() == 0;
    let event = reader
        .read_event()
        .map_err(|err| (Some(reader.error_position()), skipped(reader_error(&err))))?;
    match &event {
        Event::Decl(declaration) if at_start && declared_version(declaration).is_ok() => {
            scopes.declare_standalone(declaration);
        }
        Event::Start(start) => scopes
            .push(start, version)
            .map_err(|problem| (None, skipped(problem)))?,
        Event::DocType(doctype) => {
            let length = usize::try_from(reader.buffer_position()).unwrap_or(usize::MAX);
            let length = length.saturating_add(reader.get_ref().len());
            if scopes.declare_entities(doctype, exact, version, length) {
                seen.doctype = Doctype::Refused;
            }
        }
        _ => {}
    }
    Ok(event)
}

/// The problem with the first document type before the root of a per-type
/// file, read and refused as libxml2 refuses it: reported once, at it,
/// besides where its entities are used.
const REFUSED_DOCTYPE: &str =
    "bad XML (the document type declaration cannot be read); it declares nothing";

/// Whether `rest`, the input still to read, starts with `<!DOCTYPE` and
/// white space as XML writes it (after a UTF-8 byte order mark, which
/// `quick-xml` skips). `quick-xml` takes the keyword in any case, and with
/// no white space after it.
fn written_doctype(rest: &[u8]) -> bool {
    rest.strip_prefix(b"\xEF\xBB\xBF")
        .unwrap_or(rest)
        .strip_prefix(b"<!DOCTYPE")
        .and_then(|after| after.first())
        .is_some_and(|after| matches!(after, b' ' | b'\t' | b'\r' | b'\n'))
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
        Error::Escape(Escape::UnrecognizedEntity(at, name)) => {
            format!("at {at:?}: unrecognized entity `{}`", quoted(name))
        }
        // The others hold no text from the file. Namespace errors are not
        // met: the plain reader resolves no namespaces ([`Scopes`]).
        Error::Io(_)
        | Error::Namespace(_)
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
    doctype: Doctype,
}

/// What [`Seen`] knows of the document types read before the root.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Doctype {
    /// None was read.
    #[default]
    None,
    /// The one being read, the first, is refused, as libxml2 refuses it
    /// ([`Scopes::declare_entities`]).
    Refused,
    /// One was read.
    Read,
}

/// A problem with the structure of a per-type file.
struct Problem {
    /// Where, as `quick-xml` counts offsets: from after any byte order mark.
    at: Option<usize>,
    message: String,
    /// Whether the rest of the file is skipped.
    rest_skipped: bool,
}

impl Seen {
    /// Notes `event`, read after what this records, which gave a problem
    /// if `reported`.
    fn note(&mut self, event: &Event<'_>, reported: bool) {
        if matches!(event, Event::DocType(_)) {
            self.doctype = Doctype::Read;
        }
        self.run_reported = matches!(
            event,
            Event::Text(_) | Event::GeneralRef(_) | Event::CData(_)
        ) && (self.run_reported || reported);
    }
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
/// for other content there. A processing instruction named `xml` in any
/// casing ([`named_xml`]) counts as a declaration here. Another one libxml2
/// refuses ([`instruction_problem`]) is reported as such a declaration is,
/// at the start too.
fn structure_problem(
    event: &Event<'_>,
    namespace: &Resolved<'_>,
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
        doctype,
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
    let declaration = match event {
        Event::Decl(_) => true,
        Event::PI(instruction) => named_xml(instruction).is_some(),
        _ => false,
    };
    // A processing instruction libxml2 refuses, or a comment or one holding
    // a character the version does not allow written as it is, is reported
    // as a declaration out of place is, at the start too.
    if let Some(message) = markup_problem(event, declaration, version) {
        return if ended && depth == 0 {
            problem(&format!("{message}; the rest is skipped"), true)
        } else {
            problem(&message, false)
        };
    }
    // The arms are checked in order: inside the root (`depth > 0`), then
    // before its end (`!ended`), then after it.
    match event {
        // Only the first before the root is read, so it has no other
        // problem: reported once, at it.
        Event::DocType(_) if doctype == Doctype::Refused => problem(REFUSED_DOCTYPE, false),
        // What was completed is kept; a value cut off is not.
        Event::Eof if depth > 0 => problem(
            "bad XML (the file ends before the root element is closed)",
            false,
        ),
        Event::Eof if !ended => Some(Problem::new(None, "bad XML (no root element)", false)),
        _ if declaration && depth > 0 => problem(LATE, false),
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
            } else if matches!(namespace, Resolved::Bound(Ok(bound)) if *bound == NAMESPACE) {
                return None;
            } else {
                format!(
                    "the root element `{}` is not in the namespace `{NAMESPACE}`",
                    quoted(name.as_ref())
                )
            };
            problem(&format!("not a per-type file: {what}; it is ignored"), true)
        }
        _ if declaration && !ended && at != 0 => problem(LATE, false),
        Event::DocType(_) if !ended && doctype == Doctype::Read => {
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
        _ if declaration => problem(&format!("{LATE}; the rest is skipped"), true),
        Event::DocType(_) => problem(&format!("{DOCTYPE}; the rest is skipped"), true),
        Event::Text(text) => text_at(text).map(|at| Problem::new(Some(at), AFTER, true)),
        Event::Comment(_) | Event::PI(_) | Event::Eof => None,
        _ => problem(AFTER, true),
    }
}

/// The problem with `event`, if it is a processing instruction libxml2
/// refuses ([`instruction_problem`]), or a comment or processing
/// instruction holding a character XML `version` does not allow written as
/// it is ([`written_problem`]). `declaration`: it is an XML declaration,
/// reported as such ([`structure_problem`]).
fn markup_problem(event: &Event<'_>, declaration: bool, version: XmlVersion) -> Option<String> {
    let (what, text) = match event {
        Event::Comment(comment) => ("comment", &**comment),
        Event::PI(instruction) if !declaration => {
            if let Some(what) = instruction_problem(instruction) {
                return Some(format!(
                    "bad XML (a processing instruction's target {what})"
                ));
            }
            ("processing instruction", &**instruction)
        }
        _ => return None,
    };
    // Each byte looked at: most text is printable ASCII, tab or a line
    // end, which every version allows written as it is, so the characters
    // are looked at only past that. A fold, through every byte, is
    // vectorised; `all`, which may stop early, is not, and is about four
    // times slower on a long comment.
    crate::work::count(1 + text.len());
    let other = text.bytes().fold(0u8, |other, b| {
        other | u8::from(!matches!(b, b'\t' | b'\n' | b'\r' | 0x20..=0x7E))
    });
    if other == 0 {
        return None;
    }
    // Each character looked at again.
    crate::work::count(text.len());
    written_problem(text, version).map(|held| format!("bad XML (a {what} with {held})"))
}

/// The namespace a declaration's value `written` names, in a document of
/// XML `version`: the value normalised as an attribute value
/// ([`attribute_value`]); or what is wrong with it.
fn namespace_value<'a>(
    written: &'a str,
    version: XmlVersion,
    entities: &Entities,
) -> Result<Value<'a>, String> {
    // Built as read: `Attribute::from` would escape the value.
    let attribute = Attribute {
        key: QName("xmlns"),
        value: written.into(),
    };
    attribute_value(&attribute, version, entities)
}

/// The namespace `xml` is bound to by definition (Namespaces in XML 1.0,
/// 3rd ed., 3; 1.1 has the same).
const XML_NAMESPACE: &str = "http://www.w3.org/XML/1998/namespace";

/// The namespace `xmlns` is bound to by definition, as [`XML_NAMESPACE`].
const XMLNS_NAMESPACE: &str = "http://www.w3.org/2000/xmlns/";

/// A declaration's namespace ([`namespace_value`]; empty for none), or what
/// is wrong with its value.
type Binding = Result<String, String>;

/// What an element's name is in, by its prefix or the default namespace.
enum Resolved<'a> {
    /// No namespace: no prefix, and no default namespace in scope.
    Unbound,
    /// A namespace, or what is wrong with its declaration's value.
    Bound(Result<&'a str, &'a str>),
    /// A prefix that no declaration in scope binds.
    Unknown,
}

/// The namespace declarations in scope while a document is read (Namespaces
/// in XML 1.0, 3rd ed., 5; 1.1, 2nd ed., 6). Each prefix has its own list,
/// so looking a name up takes time linear in its prefix, not in the
/// declarations in scope.
struct Scopes {
    /// The default namespace's bindings in scope, the innermost last.
    default: Vec<Binding>,
    /// Each prefix's, as `default`, each with its namespace's ID (`ids`).
    /// A prefix leaves the map when its last binding goes out of scope.
    prefixed: HashMap<String, Vec<(usize, Binding)>>,
    /// An ID for each namespace a prefix was bound to, given when first
    /// declared, so namespaces are compared without reading them again.
    /// Kept until the file is read, under any limit: it grows with the
    /// distinct namespaces bound in the file, so with its size.
    ids: HashMap<String, usize>,
    /// The general entities the document's internal subset declares
    /// ([`Scopes::declare_entities`]).
    entities: Entities,
    /// The namespace declarations of the tag last opened that use
    /// entities, by attribute name, with their values as bound
    /// ([`Scopes::declared_value`]).
    tag: HashMap<String, Binding>,
    /// Whether the XML declaration says `standalone="yes"`.
    standalone: bool,
    /// Whether a document type, or the root, was read: entities are
    /// declared only by the first document type before the root.
    typed: bool,
    /// The prefixes bound (`None` for the default), in order, with the
    /// depth of the element whose tag binds each.
    declared: Vec<(usize, Option<String>)>,
    /// The elements open.
    depth: usize,
    /// The most declarations on the open elements, hidden ones too, and
    /// not `xmlns:xml` ones ([`crate::Limits::max_namespace_bindings`]).
    max: Option<usize>,
    /// The bytes entities may expand to, or parameter entities include,
    /// per byte of the document or its document type, and besides
    /// ([`crate::Limits::entity_expansion_per_byte`]).
    expansion: (Option<usize>, usize),
}

impl Scopes {
    fn new(limits: &crate::Limits) -> Self {
        Self {
            default: Vec::new(),
            prefixed: HashMap::new(),
            ids: HashMap::new(),
            entities: Entities::default(),
            tag: HashMap::new(),
            standalone: false,
            typed: false,
            declared: Vec::new(),
            depth: 0,
            max: limits.max_namespace_bindings,
            expansion: (
                limits.entity_expansion_per_byte,
                limits.entity_expansion_extra,
            ),
        }
    }

    /// The bytes entities may expand to, or include, in `length` bytes of
    /// a document or its document type ([`Scopes::expansion`]); all if no
    /// budget.
    fn budget(&self, length: usize) -> usize {
        let (per_byte, extra) = self.expansion;
        per_byte.map_or(usize::MAX, |per_byte| {
            length.saturating_mul(per_byte).saturating_add(extra)
        })
    }

    /// Opens the scope of `start`, a tag in a document of XML `version`,
    /// with its declarations, read in order up to an attribute that cannot
    /// be read (those after it are not looked at). Gives the first problem:
    /// a binding Namespaces in XML forbids ([`forbidden`]), or one more
    /// than the limit. Bindings are compared after references are replaced.
    fn push(&mut self, start: &BytesStart<'_>, version: XmlVersion) -> Result<(), String> {
        use crate::work::count;
        self.depth += 1;
        self.typed = true;
        self.new_tag();
        // The tag scanned for its declarations.
        count(1 + start.len());
        for attribute in start.attributes().with_checks(false) {
            let Ok(attribute) = attribute else {
                break;
            };
            let Some(declared) = attribute.key.as_namespace_binding() else {
                continue;
            };
            // The value scanned to be checked, then to be normalised.
            count(2 * attribute.value.len());
            // Entities are replaced; a value still unknown is kept as one
            // that cannot be read: an error only where a name uses it.
            let value = match attribute_value(&attribute, version, &self.entities) {
                Ok(Value::Known(value)) => Ok(value),
                Ok(Value::Unknown(why)) => {
                    let mut value = match self.entities.expand(&attribute.value, version) {
                        Ok(Some(expanded)) => Ok(expanded),
                        Ok(None) => Err(why),
                        Err(why) => Err(why),
                    };
                    // Kept for the checks of this tag's attributes: hashed
                    // and copied, the first binding. The copy asks for its
                    // memory first, as the expansion did; refused, the
                    // binding cannot be read, as a refused expansion's.
                    // Counted in tests only, so that the sum cannot
                    // overflow elsewhere.
                    let name = attribute.key.into_inner();
                    crate::work::count_by(|| {
                        let length = value.as_ref().map_or_else(String::len, String::len);
                        name.len()
                            .saturating_mul(3)
                            .saturating_add(length)
                            .saturating_add(1)
                    });
                    if !self.tag.contains_key(name) {
                        let kept = match &value {
                            Ok(expanded) => try_copy(expanded).ok_or_else(too_large),
                            Err(why) => Err(why.clone()),
                        };
                        if let Err(why) = &kept {
                            value = Err(why.clone());
                        }
                        self.tag.insert(name.to_owned(), kept);
                    }
                    value.map(Cow::Owned)
                }
                Err(why) => Err(why),
            };
            let prefix = match declared {
                PrefixDeclaration::Default => None,
                PrefixDeclaration::Named(prefix) => {
                    if let Some(problem) =
                        forbidden(prefix, &attribute.value, value.as_deref().ok())
                    {
                        return Err(problem);
                    }
                    // Bound so by definition.
                    if prefix == "xml" {
                        continue;
                    }
                    Some(prefix)
                }
            };
            if let Some(max) = self.max
                && self.declared.len() >= max
            {
                return Err(format!(
                    "more than {max} namespace declarations on the open elements"
                ));
            }
            let mut value = value.map(Cow::into_owned);
            // Copied: the namespace, or a message quoting it.
            crate::work::count_by(|| value.as_ref().map_or_else(String::len, String::len));
            match prefix {
                None => self.default.push(value),
                Some(prefix) => {
                    // Hashed and copied (three), and copied again.
                    count(1 + 4 * prefix.len());
                    // A namespace whose copy for its ID is refused cannot
                    // be read, as one whose expansion is; such a binding
                    // has the ID of an empty one, as any binding that
                    // cannot be read.
                    let id = if let Some(id) = self.id(value.as_deref().unwrap_or_default()) {
                        id
                    } else {
                        value = Err(too_large());
                        self.id("").unwrap_or(usize::MAX)
                    };
                    self.prefixed
                        .entry(prefix.to_owned())
                        .or_default()
                        .push((id, value));
                }
            }
            self.declared.push((self.depth, prefix.map(str::to_owned)));
        }
        Ok(())
    }

    /// Notes the general entities `doctype` declares ([`declared_entities`]),
    /// in a document of XML `version`, if it is the first document type,
    /// before the root. One written other than `<!DOCTYPE` (`exact`), which
    /// XML does not allow, declares nothing. `length` is the document's,
    /// which bounds what entities may expand to ([`Entities::expand`];
    /// [`Scopes::budget`]), as the document type's length bounds what
    /// parameter entities include. In a `standalone="yes"` document an
    /// undeclared entity is an error whatever its subsets (XML 1.0, 4.1).
    /// Whether the document type was read and refused, as libxml2 refuses it:
    /// then it declares nothing.
    fn declare_entities(
        &mut self,
        doctype: &str,
        exact: bool,
        version: XmlVersion,
        length: usize,
    ) -> bool {
        if self.typed {
            return false;
        }
        self.typed = true;
        if !exact {
            return true;
        }
        let included = self.budget(doctype.len());
        let declared = declared_entities(doctype, version, self.standalone, included);
        let refused = declared.is_none();
        self.entities = declared.unwrap_or_default();
        self.entities.walk(version);
        // A default refers to entities as a value does, judged where it
        // stands (XML 1.0, 4.1): one that would be an error there refuses
        // the document type.
        let refused = refused || self.entities.a_default_is_an_error(version);
        if refused {
            self.entities = Entities::default();
        }
        self.entities.budget = self.budget(length);
        refused
    }

    /// Notes `declaration`, an XML declaration at the start of a document
    /// within XML's grammar: whether it says `standalone="yes"`.
    fn declare_standalone(&mut self, declaration: &BytesDecl<'_>) {
        // Its value looked for.
        crate::work::count(declaration.len());
        self.standalone = matches!(declaration.standalone(), Some(Ok(value)) if value == "yes");
    }

    /// Starts the cache of the tag being opened ([`Scopes::declared_value`])
    /// with a new map, not a cleared one: clearing walks every slot of a
    /// map once grown large, on every tag.
    fn new_tag(&mut self) {
        if !self.tag.is_empty() {
            // Each slot dropped.
            crate::work::count(self.tag.capacity());
            self.tag = HashMap::new();
        }
    }

    /// The value the tag last opened declares a namespace with in its
    /// attribute `name` (`xmlns` or `xmlns:` and a prefix), if it uses
    /// entities, as bound: entities replaced, or why it cannot be read.
    fn declared_value(&self, name: &str) -> Option<&Binding> {
        // Hashed and compared.
        crate::work::count(1 + 2 * name.len());
        self.tag.get(name)
    }

    /// The ID of `namespace`, a new one if it has none yet; `None` if
    /// memory to copy it is refused ([`try_copy`]).
    fn id(&mut self, namespace: &str) -> Option<usize> {
        // Hashed and compared, and copied if new; counted in tests only, so
        // that it cannot overflow elsewhere (an expansion may be large).
        crate::work::count_by(|| namespace.len().saturating_mul(3).saturating_add(1));
        if let Some(&id) = self.ids.get(namespace) {
            return Some(id);
        }
        let next = self.ids.len();
        self.ids.insert(try_copy(namespace)?, next);
        Some(next)
    }

    /// Closes the scope of the innermost open element.
    fn pop(&mut self) {
        while let Some((depth, _)) = self.declared.last()
            && *depth == self.depth
        {
            match self.declared.pop() {
                Some((_, Some(prefix))) => {
                    // Hashed and compared, and again to be removed.
                    crate::work::count(2 + 4 * prefix.len());
                    if let Some(bound) = self.prefixed.get_mut(&prefix) {
                        bound.pop();
                        if bound.is_empty() {
                            self.prefixed.remove(&prefix);
                        }
                    }
                }
                _ => {
                    self.default.pop();
                }
            }
        }
        self.depth = self.depth.saturating_sub(1);
    }

    /// What `name`, an element's, is in (Namespaces in XML 1.0, 3rd ed., 6.2;
    /// 1.1, 2nd ed., 6.2): its prefix's namespace, or with none the default
    /// one. `xml` and `xmlns` are bound by definition; an empty binding
    /// undeclares (1.1, 2nd ed., 6.1).
    fn resolve(&self, name: QName<'_>) -> Resolved<'_> {
        let prefix = name.prefix().map(quick_xml::name::Prefix::into_inner);
        // Hashed and compared.
        crate::work::count(1 + 2 * prefix.map_or(0, str::len));
        let bound = match prefix {
            None => self.default.last(),
            Some("xml") => return Resolved::Bound(Ok(XML_NAMESPACE)),
            Some("xmlns") => return Resolved::Bound(Ok(XMLNS_NAMESPACE)),
            Some(prefix) => self
                .prefixed
                .get(prefix)
                .and_then(|bound| bound.last())
                .map(|(_, bound)| bound),
        };
        match bound.map(Result::as_deref) {
            Some(Ok("")) | None if prefix.is_some() => Resolved::Unknown,
            Some(Ok("")) | None => Resolved::Unbound,
            Some(bound) => Resolved::Bound(bound.map_err(String::as_str)),
        }
    }

    /// What `event`'s name is in, if it is a start tag ([`Scopes::resolve`]);
    /// [`Resolved::Unbound`] for another event.
    fn of(&self, event: &Event<'_>) -> Resolved<'_> {
        match event {
            Event::Start(start) => self.resolve(start.name()),
            _ => Resolved::Unbound,
        }
    }

    /// The ID of the namespace an attribute's `prefix` puts it in, if it
    /// is bound to one that can be read; `None` for `xmlns`, which
    /// declares. `xml`'s is `usize::MAX`.
    fn namespace_of(&self, prefix: &str) -> Option<usize> {
        // Hashed and compared.
        crate::work::count(1 + 2 * prefix.len());
        match prefix {
            "xml" => Some(usize::MAX),
            "xmlns" => None,
            _ => {
                let (id, bound) = self.prefixed.get(prefix)?.last()?;
                bound.as_ref().ok().filter(|bound| !bound.is_empty())?;
                Some(*id)
            }
        }
    }

    /// Whether `prefix` is bound to a namespace: by a declaration in scope
    /// that does not undeclare it, or by definition (`xml`, `xmlns`).
    fn binds(&self, prefix: &str) -> bool {
        // Hashed and compared.
        crate::work::count(1 + 2 * prefix.len());
        match prefix {
            "xml" | "xmlns" => true,
            _ => self
                .prefixed
                .get(prefix)
                .and_then(|bound| bound.last())
                .is_some_and(|(_, bound)| bound.as_deref() != Ok("")),
        }
    }
}

/// The first problem with `start`, a tag that is not a field's, the root's
/// or one inside a field, whose name is in `namespace`, in a document of XML
/// `version` with `scopes` open, if any, as libxml2 reports them: its name
/// is not a `QName` or has the prefix `xmlns`; its prefix is not bound,
/// unless one of its own declarations cannot be read, which is reported
/// instead, as for a field's tag ([`unreadable_attribute`],
/// [`declaration_problem`]); or an attribute a field's or the root's tag
/// could not have ([`language`]), in attribute order; or its name is in a
/// namespace that cannot be read or is unknown. So all three kinds of tag
/// are checked alike.
fn tag_problem(
    start: &BytesStart<'_>,
    namespace: &Resolved<'_>,
    version: XmlVersion,
    scopes: &Scopes,
) -> Option<String> {
    // The tag's attributes read, each value checked and normalised.
    crate::work::count(1 + 3 * start.len());
    let name = start.name();
    if !is_qname(name.as_ref()) {
        return Some("an element's name is not an XML name".to_owned());
    }
    let prefix = name.prefix().map(quick_xml::name::Prefix::into_inner);
    if prefix == Some("xmlns") {
        return Some("an element's name has the prefix `xmlns`".to_owned());
    }
    if let (Resolved::Unknown, Some(prefix)) = (namespace, prefix) {
        let problem =
            unreadable_attribute(start).or_else(|| declaration_problem(start, version, scopes));
        return Some(problem.map_or_else(|| unbound_element(prefix), |problem| problem.message));
    }
    let problem = language(start, version, scopes, false).err();
    problem
        .map(|problem| problem.message)
        .or_else(|| match namespace {
            // Its name in a namespace that cannot be read.
            Resolved::Bound(Err(why)) => Some((*why).to_owned()),
            _ => None,
        })
}

/// The expanded names (namespace and local part) of a tag's prefixed
/// attributes read so far, namespace declarations aside.
#[derive(Default)]
struct Expanded<'a>(HashSet<(usize, &'a str)>);

impl<'a> Expanded<'a> {
    /// The problem with an attribute `name` if one read before it has the
    /// same expanded name, its prefix bound to the same namespace in
    /// `scopes` (Namespaces in XML 1.0, 3rd ed., 6.3); notes it otherwise.
    fn repeated(&mut self, name: &'a str, scopes: &Scopes) -> Option<String> {
        let (prefix, local) = name.split_once(':')?;
        let namespace = scopes.namespace_of(prefix)?;
        // Hashed and compared.
        crate::work::count(1 + 2 * local.len());
        (!self.0.insert((namespace, local))).then(|| {
            format!(
                "{UNREADABLE}an attribute appears twice in its namespace: {}",
                quoted(name)
            )
        })
    }
}

/// The problem with an element whose name has `prefix`, which no
/// declaration binds.
fn unbound_element(prefix: &str) -> String {
    format!("an element's prefix is not bound: {}", quoted(prefix))
}

/// The problem with an attribute `name` whose prefix no declaration in
/// `scopes` binds ([`Scopes::binds`]; Namespaces in XML 1.0, 3rd ed., 6),
/// if it has a prefix.
fn unbound_prefix(name: &str, scopes: &Scopes) -> Option<String> {
    let (prefix, _) = name.split_once(':')?;
    (!scopes.binds(prefix))
        .then(|| format!("{UNREADABLE}its prefix is not bound: {}", quoted(name)))
}

/// The problem with a declaration binding `prefix` to the namespace written
/// `written`, `value` when normalised (`None` if it cannot be), if
/// Namespaces in XML forbids it (1.0, 3rd ed., 3; 1.1 has the same): `xml`
/// bound to another namespace than [`XML_NAMESPACE`], `xmlns` declared at
/// all, or another prefix bound to either's namespace. Namespaces are
/// compared normalised; the message shows a namespace as written.
fn forbidden(prefix: &str, written: &str, value: Option<&str>) -> Option<String> {
    let bound = |prefix: &str, namespace: &str| {
        format!("the namespace prefix '{prefix}' cannot be bound to '{namespace}'")
    };
    match prefix {
        "xml" if value == Some(XML_NAMESPACE) => None,
        "xml" | "xmlns" => Some(bound(prefix, &quoted(written))),
        _ => [XML_NAMESPACE, XMLNS_NAMESPACE]
            .into_iter()
            .find(|&reserved| value == Some(reserved))
            .map(|reserved| bound(&quoted(prefix), reserved)),
    }
}

#[derive(Debug, Clone, Copy)]
enum Field {
    Comment,
    Acronym,
    ExpandedAcronym,
}

impl Field {
    /// Keeps `value`, this field read in `lang`, in `comment`, `acronym` or
    /// `expanded`.
    fn keep(
        self,
        lang: Option<usize>,
        value: String,
        comment: &mut Localized,
        acronym: &mut Option<String>,
        expanded: &mut Option<String>,
    ) {
        match self {
            Self::Comment => comment.set(lang, value),
            // Translated acronyms (allowed before spec 0.22) are left out.
            Self::Acronym if lang.is_none() => *acronym = Some(value),
            Self::ExpandedAcronym if lang.is_none() => *expanded = Some(value),
            Self::Acronym | Self::ExpandedAcronym => {}
        }
    }

    /// The field that `start`, an element in `namespace` directly inside
    /// the root of a document of XML `version`, begins, if any, and its
    /// language: its own ([`language`]), else the root's, `inherited`
    /// ([`field_language`]).
    ///
    /// A field is named by what follows the last `:` in the element's
    /// name, or all of it without one (`<x:y:comment>` is a comment, whose
    /// name is not a `QName`; `<comment:>` is not a field). Only the spec's
    /// own elements (or ones in no namespace) are fields; an extension's
    /// `<x:comment>` is not the comment. An element whose name is not a
    /// `QName` or has the prefix `xmlns` ([`name_problem`]), or whose
    /// namespace cannot be read, is a field whose tag cannot be read; so is
    /// one in another namespace, or a prefix no declaration binds, with an
    /// attribute that cannot be read ([`unreadable_attribute`], the
    /// problem given) or own namespace declarations that cannot be read
    /// ([`declaration_problem`]), as which namespace it is in is then not
    /// known.
    fn starting(
        start: &BytesStart<'_>,
        namespace: &Resolved<'_>,
        version: XmlVersion,
        inherited: &Result<Option<usize>, String>,
        scopes: &Scopes,
    ) -> Option<(Self, Result<Lang, TagProblem>)> {
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
            Resolved::Bound(Ok(value)) if *value != NAMESPACE => None,
            Resolved::Bound(read) => Some(read.map(drop).map_err(str::to_owned)),
            Resolved::Unbound => Some(Ok(())),
            Resolved::Unknown => None,
        };
        let Some(namespace) = namespace else {
            // The tag's namespace declarations are read only up to an
            // attribute that cannot be read ([`Scopes::push`]), so past one
            // which namespace the element is in is not known.
            return unreadable_attribute(start)
                .or_else(|| declaration_problem(start, version, scopes))
                .map(|problem| (kind, Err(problem)));
        };
        // A problem in the tag's own attributes is reported where it is,
        // even when the namespace's declaration is one of them; one in a
        // declaration further up at the tag.
        let lang = language(start, version, scopes, true).and_then(|lang| {
            namespace.map(|()| lang).map_err(|message| TagProblem {
                at: None,
                name: None,
                message,
            })
        });
        Some((kind, lang.and_then(|own| field_language(own, inherited))))
    }
}

/// What is wrong with a field's start tag.
struct TagProblem {
    /// Where, as an offset in the tag from after its `<` (`None` for the
    /// `<`).
    at: Option<usize>,
    /// Where the name of the attribute it is in starts, as `at`, if it is
    /// in one.
    name: Option<usize>,
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
fn declaration_problem(
    start: &BytesStart<'_>,
    version: XmlVersion,
    scopes: &Scopes,
) -> Option<TagProblem> {
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
                // Entities replaced, as bound.
                let value = match namespace_value(&value, version, &scopes.entities).ok()? {
                    Value::Known(value) => value,
                    Value::Unknown(_) => {
                        Cow::Borrowed(scopes.declared_value(name)?.as_deref().ok()?)
                    }
                };
                empty_prefix(name, &value, version).or_else(|| reserved_default(name, &value))?
            };
            let at = offset_in(start, name);
            Some(TagProblem {
                at,
                name: at,
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
/// (`None` without one; empty for an empty one), normalised as an attribute
/// value. Gives what is wrong when any of `start`'s attributes, before or
/// after it, cannot be read: it is not well formed or appears twice
/// ([`attribute_error`]), its name is not a `QName` ([`is_qname`]) or has
/// no white space before it, its value cannot be read ([`attribute_value`]),
/// in XML 1.0 it declares a prefix's namespace empty ([`empty_prefix`]), it
/// declares a reserved namespace the default ([`reserved_default`]), its
/// prefix is not bound ([`unbound_prefix`]), or one before it has its
/// expanded name ([`Expanded`]). The problem is where `quick-xml` finds it,
/// or at the start of the attribute.
fn language(
    start: &BytesStart<'_>,
    version: XmlVersion,
    scopes: &Scopes,
    lang_needed: bool,
) -> Result<Option<String>, TagProblem> {
    let mut lang = None;
    let mut expanded = Expanded::default();
    // Where the attribute being read starts, at the earliest: after the
    // element's name, then after each value's closing quote.
    let mut after = start.name().as_ref().len();
    for attribute in start.attributes().with_checks(true) {
        let attribute = attribute.map_err(|err| attribute_error(start, &err, after))?;
        let name = attribute.key.into_inner();
        let problem = |message| TagProblem {
            at: offset_in(start, name),
            name: offset_in(start, name),
            message,
        };
        if !is_qname(name) {
            return Err(problem(not_a_name()));
        }
        // XML 1.0, 3.1: white space before each attribute, which
        // `quick-xml` does not ask for after a value's closing quote.
        let blank_before = offset_in(start, name)
            .and_then(|at| start.get(..at))
            .is_none_or(|before| before.ends_with([' ', '\t', '\r', '\n']));
        if !blank_before {
            return Err(problem(format!(
                "{UNREADABLE}a name is not preceded by white space"
            )));
        }
        let value = match attribute_value(&attribute, version, &scopes.entities) {
            Ok(Value::Known(value)) => Some(value),
            // An unknown language makes the field's unknown.
            Ok(Value::Unknown(why)) if lang_needed && name == "xml:lang" => {
                return Err(problem(why));
            }
            // A namespace declaration's entities are replaced; another
            // value is not empty, as it holds a reference, nor reserved.
            Ok(Value::Unknown(_)) => match scopes.declared_value(name) {
                Some(Ok(value)) => Some(Cow::Borrowed(value.as_str())),
                _ => None,
            },
            Err(why) => return Err(problem(why)),
        };
        if let Some(message) = value
            .as_ref()
            .and_then(|value| {
                empty_prefix(name, value, version).or_else(|| reserved_default(name, value))
            })
            .or_else(|| unbound_prefix(name, scopes))
            .or_else(|| expanded.repeated(name, scopes))
        {
            return Err(problem(message));
        }
        if name == "xml:lang" {
            copied_language();
            lang = value.map(Cow::into_owned);
        }
        after =
            offset_in(start, &attribute.value).map_or(after, |at| at + attribute.value.len() + 1);
    }
    Ok(lang)
}

/// The field's language: its own `xml:lang` (`own`) if present, otherwise
/// the root's, `inherited`, by ID ([`root_language`], [`Localized::intern`];
/// XML 1.0, 2.12), not copied. An empty one means untranslated. If the field
/// inherits an unreadable root language, reports the field as unreadable at
/// its `<`.
fn field_language(
    own: Option<String>,
    inherited: &Result<Option<usize>, String>,
) -> Result<Lang, TagProblem> {
    match (own, inherited) {
        (Some(own), _) => Ok(Lang::Own(own)),
        (None, Ok(id)) => Ok(Lang::Id(*id)),
        (None, Err(problem)) => Err(TagProblem {
            at: None,
            name: None,
            message: problem.clone(),
        }),
    }
}

/// Reads and normalises `root`'s `xml:lang` using the document's XML `version`,
/// for fields without their own language. Returns `None` if absent and an empty
/// string if empty. Checks the root's attributes as [`language`] checks a
/// field's. Any invalid attribute makes the language unknown; the error
/// identifies whether the problem is in `xml:lang` or another attribute.
fn root_language(
    root: &BytesStart<'_>,
    version: XmlVersion,
    scopes: &Scopes,
) -> Result<Option<String>, String> {
    language(root, version, scopes, true).map_err(|problem| {
        let at_lang = problem
            .name
            .and_then(|at| root.get(at..))
            .and_then(|rest| rest.strip_prefix("xml:lang"))
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(['=', ' ', '\t', '\r', '\n']));
        if at_lang {
            format!(
                "the root element's `xml:lang` cannot be read: {}",
                problem.message
            )
        } else {
            format!(
                "the root element's tag cannot be read, so its language is unknown: {}",
                problem.message
            )
        }
    })
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
    // Where the attribute's name starts: the first thing after the
    // attribute before.
    let name = start
        .get(after..)
        .and_then(|rest| rest.find(|c| !matches!(c, ' ' | '\t' | '\r' | '\n')))
        .map(|blank| after + blank);
    let (at, what) = match *err {
        // `quick-xml` gives what follows the name.
        AttrError::ExpectedEq(at) => (
            name.unwrap_or(at),
            "a name is not followed by `=`".to_owned(),
        ),
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
                name: Some(at),
                message: twice(name),
            };
        }
    };
    TagProblem {
        at: Some(at),
        name,
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
        name: None,
        message: message.to_owned(),
    })
}

/// [`language`] for `start`, a tag inside a field, after the same check
/// of its name as a field's own tag has ([`name_problem`]); its prefix
/// must be bound, and its name not in a namespace that cannot be read or
/// is unknown.
fn inner_tag(
    start: &BytesStart<'_>,
    namespace: &Resolved<'_>,
    version: XmlVersion,
    scopes: &Scopes,
) -> Result<Option<String>, TagProblem> {
    if let Some(problem) = name_problem(start) {
        return Err(problem);
    }
    // At the name: its prefix not bound, or its namespace not readable
    // (after the tag's attributes, which may be why).
    let at_name = |message| TagProblem {
        at: Some(0),
        name: None,
        message,
    };
    if let (Resolved::Unknown, Some(prefix)) = (namespace, start.name().prefix()) {
        return Err(at_name(unbound_element(prefix.into_inner())));
    }
    let lang = language(start, version, scopes, false)?;
    if let Resolved::Bound(Err(why)) = namespace {
        return Err(at_name((*why).to_owned()));
    }
    Ok(lang)
}

/// `attribute`'s value in a document of XML `version`, normalised as an
/// attribute value; or what is wrong with it, as with a field's text
/// ([`add_to_value`]): a reference that cannot be replaced or is not ended,
/// or a character the version allows only as a reference, or not at all;
/// or `<`, which XML does not allow there (spec 2.3, `AttValue`) but
/// `quick-xml` reads. A reference to an entity the document's internal
/// subset declares (`entities`, [`declared_entities`]) is not replaced:
/// the value is then unknown.
fn attribute_value<'a>(
    attribute: &Attribute<'a>,
    version: XmlVersion,
    entities: &Entities,
) -> Result<Value<'a>, String> {
    // What the first reference to a declared entity cannot be replaced by.
    let mut unknown = None;
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
            // Looked up: hashed and compared.
            crate::work::count(1 + 2 * name.len());
            // Only an entity whose name is an XML name may be unknown: a
            // character reference to no legal character, or a name that is
            // not one, is an error in every document.
            if !is_xml_name(name) || !entities.unknown(name) {
                return Err(not_replaced(name));
            }
            unknown.get_or_insert_with(|| not_replaced(name));
        }
        rest = after;
    }
    if let Some(unknown) = unknown {
        return Ok(Value::Unknown(unknown));
    }
    // Only XML's own entities, which stand for characters that are not
    // normalised, so one level is enough (as for `normalized_value`).
    attribute
        .normalized_value_with(version, 1, resolve_xml_entity)
        .map(Value::Known)
        .map_err(|err| reader_error(&err))
}

/// An attribute's value ([`attribute_value`]).
enum Value<'a> {
    /// Normalised.
    Known(Cow<'a, str>),
    /// With a reference to an entity the internal subset declares, which
    /// is not replaced: why it is not known.
    Unknown(String),
}

/// The general entities a document's first document type declares, and
/// what a reference to another stands for.
#[derive(Default)]
struct Entities {
    /// Each declared general entity's place in `list`, by name, the first
    /// declaration binding (XML 1.0, 4.2).
    index: HashMap<String, usize>,
    /// The declared entities, in order.
    list: Vec<Entity>,
    /// Whether an entity not declared here may be declared where it is
    /// not read: the document has an external subset or a parameter-entity
    /// reference in its internal subset, and is not `standalone="yes"`
    /// (XML 1.0, 4.1). A reference to one is then unknown (a validity
    /// problem only), not an error.
    open: bool,
    /// Whether the document says `standalone="yes"` ([`Entities::declared`]).
    standalone: bool,
    /// What the document's namespace declarations may still read while
    /// they are expanded, shared by all of them: bytes read, references
    /// included, the budget at first ([`Scopes::budget`];
    /// [`Entities::expand`]).
    budget: usize,
    /// The references in processed `ATTLIST` defaults: the entity, by place
    /// in `list` (`None` for one not declared before the default, which
    /// counts as undeclared, XML 1.0, 4.1), and the entities declared
    /// before the default ([`Entities::a_default_is_an_error`]).
    defaults: Vec<(Option<usize>, usize)>,
}

/// What a value that refers to an entity is ([`Entities::walk`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Use {
    /// Known: replaced in a namespace declaration.
    Usable,
    /// Not known here: it refers, maybe through others, to an entity not
    /// declared in an open document.
    Unknown,
    /// An error.
    #[default]
    Unusable,
}

impl Use {
    /// What a value that refers to both `self` and `other` is.
    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unusable, _) | (_, Self::Unusable) => Self::Unusable,
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            (Self::Usable, Self::Usable) => Self::Usable,
        }
    }
}

/// A declared general entity.
#[derive(Default)]
struct Entity {
    /// An internal entity's replacement text ([`replacement_text`]);
    /// `None` for an external or unparsed entity, or one whose literal
    /// cannot be read.
    text: Option<String>,
    /// What a value that refers to it is ([`Entities::walk`]).
    use_: Use,
    /// The bytes read to expand it, its references' included (saturating),
    /// if usable: what the budget is charged with ([`Entities::expand`]).
    size: usize,
    /// The length of what it expands to (saturating), if usable: what is
    /// asked for to hold it.
    length: usize,
    /// Whether it has a declaration outside parameter-entity text, binding
    /// or not, and whether its binding one is in such text.
    outside: bool,
    included: bool,
    /// With standalone, whether a default outside parameter-entity text
    /// refers to it, so that it needs a declaration outside such text,
    /// anywhere (a flag, not a list, so it costs no memory per reference).
    outside_needed: bool,
}

impl Entities {
    /// What a reference to `name` in the document (not in parameter-entity
    /// text), not a character reference nor one of XML's own entities,
    /// makes a value: an undeclared one is unknown in an open document
    /// ([`Entities::open`]), an error otherwise; with standalone, so is
    /// one declared only in parameter-entity text ([`Entities::declared`]).
    fn use_of(&self, name: &str) -> Use {
        // Hashed and compared.
        crate::work::count(1 + 2 * name.len());
        match self.index.get(name) {
            Some(&at) if self.declared(at, false) => self.list[at].use_,
            _ if self.open => Use::Unknown,
            _ => Use::Unusable,
        }
    }

    /// Whether entity `at` is declared for a reference that is in
    /// parameter-entity text or not (`included`): with standalone, one
    /// that is not needs a declaration outside such text (XML 1.0, 4.1,
    /// WFC: Entity Declared, which a reference in such text is exempt
    /// from).
    fn declared(&self, at: usize, included: bool) -> bool {
        !self.standalone || included || self.list[at].outside
    }

    /// Whether a reference to `name` gives an unknown value rather than an
    /// error.
    fn unknown(&self, name: &str) -> bool {
        self.use_of(name) != Use::Unusable
    }

    /// The references in `text`, the replacement text of entity `from`, to
    /// entities other than XML's own: by place in `list`, or `None` for one
    /// not declared for them ([`Entities::declared`]: they are in
    /// parameter-entity text if `from`'s binding declaration is). `Err` if
    /// one cannot be read: a character reference to a character XML does
    /// not allow, or `&` not ending in `;`.
    fn references(
        &self,
        from: usize,
        text: &str,
        version: XmlVersion,
    ) -> Result<Vec<Option<usize>>, ()> {
        let included = self.list[from].included;
        // Each byte looked at.
        crate::work::count(1 + text.len());
        let mut found = Vec::new();
        for part in text.split('&').skip(1) {
            let (name, _) = part.split_once(';').ok_or(())?;
            if name.starts_with('#') {
                replacement(&BytesRef::new(name), version).ok_or(())?;
            } else if resolve_xml_entity(name).is_none() {
                if !is_xml_name(name) {
                    return Err(());
                }
                // Hashed and compared.
                crate::work::count(1 + 2 * name.len());
                let at = self.index.get(name).copied();
                found.push(at.filter(|&at| self.declared(at, included)));
            }
        }
        Ok(found)
    }

    /// Decides what each entity is in a value of a document of XML `version`,
    /// and its size. An entity is unusable if it is external or unparsed, its
    /// replacement text cannot be read or has `<` (WFC: No `<` in Attribute
    /// Values), it is on a loop of references of any length, or it refers,
    /// forward or back, to an unusable entity or, in a document not open, an
    /// undeclared one. Of the rest, one that refers, through any number of
    /// others, to an undeclared entity in an open document is unknown, and
    /// the others are usable.
    ///
    /// A depth-first walk over the references, iterative (no recursion, so
    /// no limit on their depth), visits each entity and follows each
    /// reference once. An entity's result is final when the walk leaves
    /// it, and is passed to the entity that referred to it then, whatever
    /// branch ends it; a reference to an entity still on the path is a
    /// loop, and makes the referrer unusable, so each entity on the loop
    /// is too.
    fn walk(&mut self, version: XmlVersion) {
        // Not visited, on the path, or done.
        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            New,
            Open,
            Done,
        }
        let edges = self.edges(version);
        let open = self.open;
        let mut marks = vec![Mark::New; self.list.len()];
        for start in 0..self.list.len() {
            if marks[start] != Mark::New {
                continue;
            }
            // Each entity on the path, and how many of its references
            // were followed.
            let mut path = vec![(start, 0)];
            marks[start] = Mark::Open;
            self.begin(start, &edges, version);
            while let Some(&mut (at, ref mut next)) = path.last_mut() {
                crate::work::count(1);
                let child = edges[at].as_ref().ok().and_then(|refs| refs.get(*next));
                if let Some(&child) = child {
                    *next += 1;
                    let got = match child {
                        None if open => (Use::Unknown, 0, 0),
                        None => (Use::Unusable, 0, 0),
                        Some(child) => match marks[child] {
                            Mark::New => {
                                marks[child] = Mark::Open;
                                self.begin(child, &edges, version);
                                path.push((child, 0));
                                continue;
                            }
                            // A loop.
                            Mark::Open => (Use::Unusable, 0, 0),
                            Mark::Done => {
                                let child = &self.list[child];
                                (child.use_, child.size, child.length)
                            }
                        },
                    };
                    self.take(at, got);
                    continue;
                }
                // Left: final, and passed to the referrer.
                marks[at] = Mark::Done;
                path.pop();
                if let Some(&(parent, _)) = path.last() {
                    let done = &self.list[at];
                    let got = (done.use_, done.size, done.length);
                    self.take(parent, got);
                }
            }
        }
    }

    /// Each entity's references ([`Entities::references`]); `Err` for one
    /// unusable by itself: external, unparsed, or a replacement text that
    /// cannot be read or holds `<`.
    fn edges(&self, version: XmlVersion) -> Vec<Result<Vec<Option<usize>>, ()>> {
        self.list
            .iter()
            .enumerate()
            .map(|(at, entity)| match entity.text.as_deref() {
                Some(text) if !text.contains('<') => self.references(at, text, version),
                _ => Err(()),
            })
            .collect()
    }

    /// Whether a reference in a processed `ATTLIST` default would be an
    /// error, judged where the default stands (XML 1.0, 4.1), as libxml2
    /// expands a default at its declaration: an entity it reaches, directly
    /// or through others, that is not declared before it is undeclared there
    /// (an error in a closed document, unknown in an open one), and usability
    /// is judged over the entities declared before it.
    ///
    /// Judged for all defaults at once, without walking again for each
    /// default: linear in the entities and references, but finding loops
    /// in an open document takes about log2 n passes over the n of them
    /// ([`cycle_times`]), so n log n. The memory is linear: each distinct
    /// reference is held a few times, as a word, repeats dropped. With
    /// `G(k)` the graph of the first `k` entities declared and the
    /// references among them:
    /// - `reach(e)`, the latest entity `e` reaches in the whole graph (an
    ///   undeclared one counting as later than all): if it is before the
    ///   default, `e`'s whole reach is, and the walk's result
    ///   ([`Entities::walk`]) is `e`'s there;
    /// - otherwise `e` reaches one not declared yet: an error in a closed
    ///   document; in an open one, only if in `G(k)` it already reaches
    ///   something unusable: an entity unusable by itself, or a loop.
    ///   `bad(e)`, the first `k` at which it does, is found by adding the
    ///   entities in order and marking each that reaches a marked one (or
    ///   becomes one), each entity once and each reference once; loops
    ///   come from the first `k` at which each entity lies on one in
    ///   `G(k)` ([`cycle_times`]).
    fn a_default_is_an_error(&mut self, version: XmlVersion) -> bool {
        use crate::work::{hold, size};
        // Each entity looked at.
        crate::work::count(self.list.len());
        if self
            .list
            .iter()
            .any(|entity| entity.outside_needed && !entity.outside)
        {
            return true;
        }
        let defaults = std::mem::take(&mut self.defaults);
        if defaults.is_empty() {
            return false;
        }
        let _defaults = hold(|| size::<(Option<usize>, usize)>(defaults.capacity()));
        let n = self.list.len();
        // Whether each is unusable by itself, and whether it refers to an
        // undeclared one; and its distinct references to declared ones.
        let (mut alone, mut undeclared) = (vec![false; n], vec![false; n]);
        let mut seen = vec![usize::MAX; n];
        let (mut start, mut to) = (Vec::with_capacity(n + 1), Vec::new());
        start.push(0);
        for at in 0..n {
            let refs = match self.list[at].text.as_deref() {
                Some(text) if !text.contains('<') => self.references(at, text, version),
                _ => Err(()),
            };
            let Ok(refs) = refs else {
                alone[at] = true;
                start.push(to.len());
                continue;
            };
            let _refs = hold(|| size::<Option<usize>>(refs.capacity()));
            for reference in refs {
                crate::work::count(1);
                match reference {
                    None => undeclared[at] = true,
                    Some(next) if seen[next] != at => {
                        seen[next] = at;
                        to.push(next);
                    }
                    Some(_) => {}
                }
            }
            start.push(to.len());
        }
        drop(seen);
        let succ = Graph { start, to };
        let _succ = hold(|| succ.bytes() + size::<bool>(alone.capacity() + undeclared.capacity()));
        // The latest entity each reaches: by components, successors first
        // (numbered in the order their members are popped).
        let reach = {
            let (component, popped) = components(&succ);
            let _held = hold(|| size::<usize>(component.capacity() + popped.capacity()));
            let mut latest: Vec<usize> = Vec::new();
            let mut members = popped.chunk_by(|&a, &b| component[a] == component[b]);
            for group in &mut members {
                let mut most = 0usize;
                for &at in group {
                    // Each entity and reference looked at.
                    crate::work::count(1 + succ.next(at).len());
                    most = most.max(if undeclared[at] { usize::MAX } else { at });
                    for &next in succ.next(at) {
                        if component[next] != component[at] {
                            most = most.max(latest[component[next]]);
                        }
                    }
                }
                latest.push(most);
            }
            let _latest = hold(|| size::<usize>(latest.capacity()));
            let reach: Vec<usize> = component.iter().map(|&c| latest[c]).collect();
            let _reach = hold(|| size::<usize>(reach.capacity()));
            reach
        };
        let _reach = hold(|| size::<usize>(reach.capacity()));
        let open = self.open;
        let bad = if open {
            reaches_bad_from(&succ, &alone)
        } else {
            Vec::new()
        };
        let _bad = hold(|| size::<usize>(bad.capacity()));
        defaults.iter().any(|&(entity, before)| {
            crate::work::count(1);
            match entity {
                None => !open,
                Some(e) if reach[e] < before => self.list[e].use_ == Use::Unusable,
                Some(_) if !open => true,
                Some(e) => bad[e] <= before,
            }
        })
    }

    /// Starts the walk's result for entity `at` from its own text.
    fn begin(&mut self, at: usize, edges: &[Result<Vec<Option<usize>>, ()>], version: XmlVersion) {
        let entity = &mut self.list[at];
        entity.use_ = if edges[at].is_ok() {
            Use::Usable
        } else {
            Use::Unusable
        };
        entity.size = entity.text.as_ref().map_or(0, String::len);
        entity.length = entity
            .text
            .as_deref()
            .map_or(0, |text| own_length(text, version));
    }

    /// Adds to entity `at`'s result what one of its references gives.
    fn take(&mut self, at: usize, (use_, size, length): (Use, usize, usize)) {
        let entity = &mut self.list[at];
        entity.use_ = entity.use_.and(use_);
        entity.size = entity.size.saturating_add(size);
        entity.length = entity.length.saturating_add(length);
    }

    /// `written`, a namespace declaration's value whose checks passed in a
    /// document of XML `version`, with usable entities replaced by their
    /// replacement texts, recursively, and normalised (XML 1.0, 3.3.3: white
    /// space a space, character references as they are). `None` if it refers
    /// to an unknown entity ([`Use::Unknown`]), as another value would be.
    /// An error if it refers to an unusable one, or if the document's budget
    /// ([`Entities::budget`]), shared by all its namespace declarations, is
    /// used up, even by a short one. The budget is charged with the bytes
    /// read, references included, whatever the outcome; they are counted as
    /// they are read too, so it holds whatever the sizes say. An error too
    /// if what it expands to is too large to hold (its size past `usize`, or
    /// memory refused): with no budget, the only bound.
    fn expand(&mut self, written: &str, version: XmlVersion) -> Result<Option<String>, String> {
        const TOO_LONG: &str = "the document's entity expansions together pass its length";
        let mut size = written.len();
        // What the value's length adds for the entities it refers to.
        let mut referred = 0usize;
        for part in written.split('&').skip(1) {
            let Some((name, _)) = part.split_once(';') else {
                return Ok(None);
            };
            if replacement(&BytesRef::new(name), version).is_none() {
                // `attribute_value` checked the names before.
                match self.use_of(name) {
                    Use::Usable => {}
                    Use::Unknown => return Ok(None),
                    Use::Unusable => return Err(not_replaced(name)),
                }
                let at = self.index[name];
                size = size.saturating_add(self.list[at].size);
                referred = referred.saturating_add(self.list[at].length);
            }
        }
        // Each byte looked at.
        crate::work::count(1 + written.len());
        if size > self.budget {
            return Err(TOO_LONG.to_owned());
        }
        // Line ends first (2.11), as in a value without entities.
        let written = line_ends(written, version);
        // What it expands to is held at once, its length asked for (not
        // the bytes read, references included, which the budget is charged
        // with): one too large to hold (its length or size saturated, or
        // memory refused) cannot be read, as one past the budget, which with
        // no budget is the only bound.
        let length = own_length(&written, version).saturating_add(referred);
        let mut value = String::new();
        if size == usize::MAX || length == usize::MAX || value.try_reserve_exact(length).is_err() {
            return Err(too_large());
        }
        #[cfg(test)]
        LAST_RESERVED.with(|last| last.set(value.capacity()));
        let mut pending = vec![&*written];
        let space = |c: char| {
            if matches!(c, '\t' | '\n' | '\r') {
                ' '
            } else {
                c
            }
        };
        // The bytes read so far, references included: the work done, which
        // the budget is charged with, whatever the outcome. Each reference
        // is at least three bytes, so it is charged even if it stands for
        // nothing.
        let mut read = 0usize;
        while let Some(rest) = pending.pop() {
            let at = rest.find('&').unwrap_or(rest.len());
            value.extend(rest[..at].chars().map(space));
            let reference = rest[at..].strip_prefix('&').and_then(|r| r.split_once(';'));
            read = read.saturating_add(rest.len() - reference.map_or(0, |(_, after)| after.len()));
            // Each byte read; never past what was allowed.
            crate::work::count(1 + at);
            if read > size {
                self.budget -= read.min(self.budget);
                return Err(TOO_LONG.to_owned());
            }
            let Some((name, after)) = reference else {
                continue;
            };
            pending.push(after);
            if let Some(c) = replacement(&BytesRef::new(name), version) {
                value.push(c);
            } else {
                // Only usable entities are followed.
                let text = self
                    .index
                    .get(name)
                    .map(|&at| &self.list[at])
                    .filter(|entity| entity.use_ == Use::Usable)
                    .and_then(|entity| entity.text.as_deref());
                let Some(text) = text else {
                    self.budget -= read.min(self.budget);
                    return Err(not_replaced(name));
                };
                pending.push(text);
            }
        }
        self.budget -= read.min(self.budget);
        Ok(Some(value))
    }
}

/// A graph whose nodes are numbered from 0, in compressed rows: node `i`'s
/// successors are `to[start[i]..start[i + 1]]`. One buffer for all edges,
/// so each costs a word, and each node one more.
struct Graph {
    start: Vec<usize>,
    to: Vec<usize>,
}

impl Graph {
    /// The graph of `nodes` nodes with the edges `edges` gives, called
    /// twice (to count them, then to place them), each edge (from, to).
    fn of<I: Iterator<Item = (usize, usize)>>(nodes: usize, edges: impl Fn() -> I) -> Self {
        let mut start = vec![0usize; nodes + 1];
        for (from, _) in edges() {
            crate::work::count(1);
            start[from] += 1;
        }
        // Each row's end, then moved back to its start as it is filled.
        let mut total = 0usize;
        for row in &mut start {
            total += *row;
            *row = total;
        }
        let mut to = vec![0usize; total];
        for (from, next) in edges() {
            crate::work::count(1);
            start[from] -= 1;
            to[start[from]] = next;
        }
        Self { start, to }
    }

    fn nodes(&self) -> usize {
        self.start.len() - 1
    }

    /// The successors of `node`.
    fn next(&self, node: usize) -> &[usize] {
        &self.to[self.start[node]..self.start[node + 1]]
    }

    /// The bytes its buffers take ([`crate::work::hold`]).
    fn bytes(&self) -> usize {
        crate::work::size::<usize>(self.start.capacity() + self.to.capacity())
    }
}

/// The strongly connected components of `graph` (Tarjan's algorithm,
/// iterative): each node's, numbered successors first, and the nodes in
/// the order they were assigned one (each component's together, in its
/// number's order). Each node and edge is looked at a few times.
fn components(graph: &Graph) -> (Vec<usize>, Vec<usize>) {
    const NONE: usize = usize::MAX;
    let n = graph.nodes();
    let (mut order, mut low) = (vec![NONE; n], vec![0usize; n]);
    let mut component = vec![NONE; n];
    let mut popped = Vec::with_capacity(n);
    let (mut stack, mut path) = (Vec::new(), Vec::new());
    let (mut next_order, mut count) = (0usize, 0usize);
    for start in 0..n {
        if order[start] != NONE {
            continue;
        }
        // The path: each node and how many of its edges were followed.
        path.push((start, 0usize));
        order[start] = next_order;
        low[start] = next_order;
        next_order += 1;
        stack.push(start);
        while let Some(&mut (at, ref mut followed)) = path.last_mut() {
            crate::work::count(1);
            if let Some(&next) = graph.next(at).get(*followed) {
                *followed += 1;
                if order[next] == NONE {
                    order[next] = next_order;
                    low[next] = next_order;
                    next_order += 1;
                    stack.push(next);
                    path.push((next, 0));
                } else if component[next] == NONE {
                    low[at] = low[at].min(order[next]);
                }
                continue;
            }
            path.pop();
            if let Some(&(parent, _)) = path.last() {
                low[parent] = low[parent].min(low[at]);
            }
            if low[at] == order[at] {
                while let Some(member) = stack.pop() {
                    crate::work::count(1);
                    component[member] = count;
                    popped.push(member);
                    if member == at {
                        break;
                    }
                }
                count += 1;
            }
        }
    }
    // All held at once here, each at its most (capacities do not shrink).
    let _held = crate::work::hold(|| {
        crate::work::size::<usize>(
            order.capacity()
                + low.capacity()
                + component.capacity()
                + popped.capacity()
                + stack.capacity(),
        ) + crate::work::size::<(usize, usize)>(path.capacity())
    });
    (component, popped)
}

/// The state of [`cycle_times`]' halving: a union-find over the nodes
/// (by size, paths halved), each representative's number in the graph of
/// one halving step, and the first `k` found for each node.
struct Loops {
    parent: Vec<usize>,
    size: Vec<usize>,
    /// The step that numbered it last, and its number then.
    local: Vec<(usize, usize)>,
    step: usize,
    first: Vec<usize>,
}

impl Loops {
    /// The representative of `node`, paths halved.
    fn find(&mut self, mut node: usize) -> usize {
        while self.parent[node] != node {
            crate::work::count(1);
            self.parent[node] = self.parent[self.parent[node]];
            node = self.parent[node];
        }
        node
    }

    /// Unites the sets of `a` and `b`, the smaller under the larger.
    fn unite(&mut self, a: usize, b: usize) {
        let (mut a, mut b) = (self.find(a), self.find(b));
        if a == b {
            return;
        }
        if self.size[a] < self.size[b] {
            std::mem::swap(&mut a, &mut b);
        }
        self.parent[b] = a;
        self.size[a] += self.size[b];
    }

    /// The number of `node`'s representative in this step, numbered now if
    /// it has none yet (`nodes` counting them).
    fn number(&mut self, node: usize, nodes: &mut usize) -> usize {
        let node = self.find(node);
        if self.local[node].0 != self.step {
            self.local[node] = (self.step, *nodes);
            *nodes += 1;
        }
        self.local[node].1
    }

    /// The edges of `edges` in the graph by `middle` (both ends added),
    /// over representatives: those whose ends are strongly connected
    /// there are moved to the front, and their count returned. The step's
    /// buffers are freed before it returns.
    fn split(&mut self, middle: usize, edges: &mut [(usize, usize)]) -> usize {
        let within = |(a, b): (usize, usize)| a.max(b) < middle;
        self.step += 1;
        // Numbered, and each row's length counted; then each row's end,
        // moved back to its start as it is filled. The numbers are looked
        // up again each time rather than kept, so that a step holds a word
        // per edge in its graph, not three.
        let (mut nodes, mut start) = (0usize, vec![0usize]);
        for &(a, b) in &*edges {
            crate::work::count(1);
            if within((a, b)) {
                let a = self.number(a, &mut nodes);
                self.number(b, &mut nodes);
                start.resize(nodes + 1, 0);
                start[a] += 1;
            }
        }
        let mut total = 0usize;
        for row in &mut start {
            total += *row;
            *row = total;
        }
        let mut to = vec![0usize; total];
        for &(a, b) in &*edges {
            crate::work::count(1);
            if within((a, b)) {
                let (a, b) = (self.number(a, &mut nodes), self.number(b, &mut nodes));
                start[a] -= 1;
                to[start[a]] = b;
            }
        }
        let graph = Graph { start, to };
        let _graph = crate::work::hold(|| graph.bytes());
        let (component, _) = components(&graph);
        let mut joined = 0usize;
        for at in 0..edges.len() {
            crate::work::count(1);
            if within(edges[at]) {
                let (a, b) = edges[at];
                let (a, b) = (self.number(a, &mut nodes), self.number(b, &mut nodes));
                if component[a] == component[b] {
                    edges.swap(joined, at);
                    joined += 1;
                }
            }
        }
        joined
    }
}

/// For each node of `graph`, whose nodes come in order (node `i` added
/// `i`th), the first `k` at which it lies on a loop of the graph of nodes
/// before `k`, `usize::MAX` if never: offline incremental strongly
/// connected components, by halving the times ([`halve`]). Each edge is
/// looked at a few times in each of about log2 n halvings, so the work is
/// n log n; the memory is linear: the edges, once, a few words per node,
/// and one step's buffers, freed before the next.
fn cycle_times(graph: &Graph) -> Vec<usize> {
    let n = graph.nodes();
    let mut edges: Vec<(usize, usize)> = (0..n)
        .flat_map(|from| graph.next(from).iter().map(move |&to| (from, to)))
        .collect();
    let mut loops = Loops {
        parent: (0..n).collect(),
        size: vec![1; n],
        local: vec![(0, 0); n],
        step: 0,
        first: vec![usize::MAX; n],
    };
    let _held = crate::work::hold(|| {
        crate::work::size::<(usize, usize)>(edges.capacity() + loops.local.capacity())
            + crate::work::size::<usize>(
                loops.parent.capacity() + loops.size.capacity() + loops.first.capacity(),
            )
    });
    halve(1, n + 1, &mut edges, &mut loops);
    loops.first
}

/// [`cycle_times`]' halving: `edges`, whose ends join (become strongly
/// connected) at some `k` in `from..=to` (`to` being never), get it as
/// their first ends' time if earlier, and their ends are united. Nothing
/// is held across the two halves but `edges`, split in place.
fn halve(from: usize, to: usize, edges: &mut [(usize, usize)], loops: &mut Loops) {
    if edges.is_empty() {
        return;
    }
    if from == to {
        if from <= loops.parent.len() {
            for &(a, b) in &*edges {
                crate::work::count(1);
                loops.first[a] = loops.first[a].min(from);
                loops.unite(a, b);
            }
        }
        return;
    }
    let middle = from + (to - from) / 2;
    let joined = loops.split(middle, edges);
    let (early, late) = edges.split_at_mut(joined);
    halve(from, middle, early, loops);
    halve(middle + 1, to, late, loops);
}

/// For each node of `graph` (added in order, as in [`cycle_times`]), the
/// first `k` at which, in the graph of the nodes before `k`, it reaches one
/// unusable by itself (`alone`) or a loop; `usize::MAX` if never. Nodes
/// are added in order, and each marked once, when it reaches a marked one
/// or becomes one, its referrers then marked in turn: each node and
/// reference is looked at a few times.
fn reaches_bad_from(graph: &Graph, alone: &[bool]) -> Vec<usize> {
    let n = graph.nodes();
    // What becomes unusable at each `k`, by itself or on a loop.
    let due = {
        let cycles = cycle_times(graph);
        let _cycles = crate::work::hold(|| crate::work::size::<usize>(cycles.capacity()));
        let due_at =
            |node: usize| cycles[node].min(if alone[node] { node + 1 } else { usize::MAX });
        let due = (0..n)
            .filter(|&node| due_at(node) <= n)
            .map(|node| (due_at(node), node));
        Graph::of(n + 1, || due.clone())
    };
    let _due = crate::work::hold(|| due.bytes());
    // Each node's referrers.
    let edges = (0..n).flat_map(|from| graph.next(from).iter().map(move |&to| (to, from)));
    let referrers = Graph::of(n, || edges.clone());
    let _referrers = crate::work::hold(|| referrers.bytes());
    let mut marked = vec![usize::MAX; n];
    let mut pending = Vec::new();
    for k in 1..=n {
        let added = k - 1;
        // Its references to nodes already in, and what is due.
        crate::work::count(1 + graph.next(added).len());
        if graph
            .next(added)
            .iter()
            .any(|&to| to <= added && marked[to] <= k)
        {
            pending.push(added);
        }
        pending.extend_from_slice(due.next(k));
        while let Some(node) = pending.pop() {
            crate::work::count(1);
            if marked[node] != usize::MAX {
                continue;
            }
            marked[node] = k;
            for &from in referrers.next(node) {
                crate::work::count(1);
                if from < k && marked[from] == usize::MAX {
                    pending.push(from);
                }
            }
        }
    }
    let _held =
        crate::work::hold(|| crate::work::size::<usize>(marked.capacity() + pending.capacity()));
    marked
}

/// The general entities `doctype`, a document type declaration's text
/// after `<!DOCTYPE` and white space, declares in its internal subset, in
/// a document of XML `version`, `standalone="yes"` or not (XML 1.0, 2.8,
/// 4.1, 4.2, 4.4.8 and 5.1).
///
/// Each `<!ENTITY name` followed by white space declares `name`, if it has
/// no `:` (Namespaces in XML 1.0, 3rd ed., 7: such a name is a namespace
/// error, so it declares nothing); the first declaration binds. One whose
/// definition is a literal keeps its replacement text
/// ([`replacement_text`]); an external or unparsed one (`SYSTEM`,
/// `PUBLIC`) is never usable (4.4.3, 4.4.4). What each is in a value is
/// decided once all are read ([`Entities::walk`], by the caller). Text in
/// a comment, a processing instruction or a literal declares nothing.
///
/// Parameter entities (`<!ENTITY % name`) are noted ([`Parameter`]), the
/// first declaration binding. A reference between declarations to a declared
/// internal one includes its replacement text there (4.4.8): its declarations
/// are read as the subset's, where they stand, and must be whole (WFC: PE
/// Between Declarations), and it may not include itself, directly or through
/// others (WFC: No Recursion). The bytes included, together, may not pass
/// `budget` (by default `doctype`'s length and 64 KiB, [`Scopes::budget`]). A
/// reference to an external one, or to one not declared there, is not read:
/// without standalone, nothing after it is processed (5.1), though still read
/// for the checks below; with standalone, an undeclared one is an error
/// (4.1), and after an external one processing goes on (5.1). Any reference
/// makes a document that is not standalone open ([`Entities::open`]), as an
/// external ID does (4.1: Entity Declared is then a validity constraint).
/// With standalone, a reference that is not itself in parameter-entity text
/// (in the document, in the literal of an entity whose binding declaration is
/// not in such text, or in an `ATTLIST` default that is not) must name an
/// entity with a declaration outside such text, anywhere; a reference in such
/// text is exempt from that (WFC: Entity Declared, [`Entities::declared`]).
/// Otherwise it is an error, as an undeclared one is. Every default, in such
/// text or not, needs the entities it reaches declared before it, by a
/// declaration of any kind (4.1); and in such text too, a reference to an
/// entity declared nowhere is an error with standalone, as libxml2 has it.
///
/// As libxml2 refuses them, these declare nothing at all (`None`, which the
/// per-type reader reports): a document type whose name is not an XML name or
/// is followed by other than ASCII white space, `[` or its end, with an
/// external ID not written as XML writes it (its public literal holding other
/// than `PubidChar`s too) or other text after the name or external ID than a
/// subset, or with more than white space after the subset's `]`; a markup
/// declaration not written whole as XML's grammar says ([`grammatical_end`]):
/// another or a lower-case keyword, a conditional section (libxml2 takes none
/// in an internal subset) or a nested document type; a name that is not an
/// XML name or a token that is not a name token; white space missing where
/// the grammar needs it, or more than white space before `>`; an entity's
/// system literal with `#`; a public literal with other than `PubidChar`s; a
/// content model mixing `,` and `|` in a group, mixed content with names not
/// ended by `)*`, or groups nested deeper than [`MAX_CONTENT_DEPTH`]; an
/// attribute type or default XML does not have; a comment with `--` inside or
/// `-` before its end; a character the version does not allow written as it
/// is ([`may_be_written`]) anywhere in the subset's own text, a literal, a
/// comment or a processing instruction, or in the head's system literal,
/// whether or not what holds it is used; parameter-entity text is judged as
/// its literal was, so in XML 1.1 a reference there to a restricted character
/// gives one in the text (libxml2, which reads XML 1.1 as 1.0, refuses such
/// a reference); an internal entity's declaration with more than white space
/// after its literal (`NDATA`); a parameter entity's with no white space
/// after `%` or its name, a name that is not an XML name, or `NDATA`; a
/// reference with no name; with standalone, a reference to an undeclared
/// parameter entity; a reference to an internal one whose literal cannot be
/// read, that includes itself, past the budget, or whose text is not whole
/// declarations, comments, processing instructions and references (a part of
/// one, text, a conditional section, `]`); and, wherever it stands, processed
/// or not (after a cut-off, under a colon-named entity, in a parameter entity
/// or an `ATTLIST` default), a literal with a reference that cannot be one,
/// an entity's literal with a `%` (WFC: PEs in Internal Subset), or an
/// `ATTLIST` default with `<` ([`has_bad_literal`]), and a markup declaration
/// with a `%` outside its literals but the one declaring a parameter entity
/// ([`has_parameter_reference`]), and a processing instruction named `xml` in
/// any casing, an XML declaration not at the start ([`named_xml`]), or whose
/// target is missing or not followed by white space
/// ([`instruction_problem`]); and a subset holding what its grammar does not
/// allow, where reading stops. Reading goes on after the cut-off for those
/// checks. The entities a processed `ATTLIST` default refers to are noted
/// ([`Entities::defaults`]); one an attribute value could not use where the
/// default stands, reached directly or through other entities (undeclared
/// there in a closed document, which includes one declared only after the
/// default, 4.1; external, unparsed, or not usable over what is declared
/// then), refuses the document type, as the caller checks once all are read
/// ([`Entities::a_default_is_an_error`]).
///
/// Two forms are refused, as the spec has them, that xmllint takes: a content
/// model split across parameter-entity text, as
/// `<!ENTITY % p '<!ELEMENT e (a'>%p;)>` (WFC: PE Between Declarations), and
/// `NDATA` with white space but no name after it. libxml2's limit on a name's
/// length is not copied: names are read in linear time, bounded by the file's
/// size cap; only its content-model depth is ([`MAX_CONTENT_DEPTH`]), as that
/// bounds a stack.
///
/// Reading is linear in `doctype` and the text included, which is at most
/// `budget` (by default `doctype`'s length and 64 KiB; per byte times the
/// length, and the extra, [`crate::Limits::entity_expansion_per_byte`]).
/// Memory is linear in them: the texts read at once, a few words each, each
/// parameter entity's replacement text, no longer than its literal, and the
/// references of `ATTLIST` defaults in included text, 24 bytes each, in a
/// buffer that grows by doubling: up to 16 bytes per byte included (a
/// 3-byte `&e;`), so it grows with the budget's per byte too. Judging the
/// defaults is linear in the entities and references but for the loops in
/// an open document: about log2 n passes over the n of them
/// ([`cycle_times`]), so n log n, in memory linear in them.
fn declared_entities(
    doctype: &str,
    version: XmlVersion,
    standalone: bool,
    budget: usize,
) -> Option<Entities> {
    let blank = [' ', '\t', '\r', '\n'];
    let refused = || None;
    let mut entities = Entities::default();
    // The name, an XML name followed by white space, `[` or the end; then
    // an external ID if any; then only a subset, if any, and white space;
    // libxml2 refuses anything else ("DOCTYPE improperly terminated"). In
    // an XML 1.1 file, NEL and LS here are not read as line ends, so not as
    // white space (a known limit, as in a tag); libxml2, which reads XML 1.1
    // as 1.0, refuses them too.
    let name_end = doctype.find(|c: char| !is_name_char(c));
    let name_end = name_end.unwrap_or(doctype.len());
    crate::work::count(1 + name_end);
    let after_name = &doctype[name_end..];
    if !doctype.starts_with(is_name_start)
        || !(after_name.is_empty() || after_name.starts_with(blank) || after_name.starts_with('['))
    {
        return refused();
    }
    let after_name = after_name.trim_start_matches(blank);
    let rest = match external_id(after_name) {
        Some(Ok(after)) => {
            entities.open = !standalone;
            after
        }
        Some(Err(())) => return refused(),
        None => after_name,
    };
    // A system literal holds only characters the version allows written as
    // they are; the rest of the head is names, white space and a public
    // literal, checked above.
    let head = &doctype[..doctype.len() - rest.len()];
    crate::work::count(1 + head.len());
    if !head.chars().all(|c| may_be_written(c, version)) {
        return refused();
    }
    let rest = rest.trim_start_matches(blank);
    crate::work::count(1);
    if rest.is_empty() {
        return Some(entities);
    }
    let Some(subset) = rest.strip_prefix('[') else {
        return refused();
    };
    let mut reader = Reader {
        entities,
        parameters: Parameters::default(),
        texts: vec![(Text::Subset(subset), 0)],
        included: 0,
        budget,
        processing: true,
        version,
        standalone,
    };
    let read = reader.read();
    let Reader {
        mut entities,
        parameters,
        texts,
        ..
    } = reader;
    let _held = crate::work::hold(|| {
        crate::work::size::<(Text<'_>, usize)>(texts.capacity())
            + crate::work::size::<bool>(parameters.reading.capacity())
            + parameters.list.iter().map(Parameter::bytes).sum::<usize>()
    });
    // Reading stopped at the subset's `]`, after which only white space may
    // come, or at what its grammar does not allow, which refuses it.
    let stopped = texts.first().map_or("", |&(_, at)| &subset[at..]);
    let after = stopped.trim_start_matches(blank).strip_prefix(']');
    // Each byte looked at.
    crate::work::count(1 + stopped.len());
    if !read || !after.is_some_and(|after| after.trim_start_matches(blank).is_empty()) {
        return refused();
    }
    // With standalone, an entity declared only in parameter-entity text is
    // not declared for a reference outside such text (WFC: Entity
    // Declared, [`Entities::declared`]).
    entities.standalone = standalone;
    Some(entities)
}

/// The reading of an internal subset ([`declared_entities`]).
struct Reader<'a> {
    entities: Entities,
    parameters: Parameters,
    /// The texts being read: the subset, then the replacement texts of the
    /// parameter entities included, innermost last; each with where
    /// reading is in it.
    texts: Vec<(Text<'a>, usize)>,
    /// The bytes included so far, and the most allowed (by default the
    /// document type's length, and 64 KiB; [`Scopes::budget`]).
    included: usize,
    budget: usize,
    /// Whether declarations are still processed (5.1); after that the
    /// subset is still read, for the declarations' well-formedness.
    processing: bool,
    version: XmlVersion,
    standalone: bool,
}

impl<'a> Reader<'a> {
    /// Reads the texts to the subset's end; false if the document type is
    /// refused.
    fn read(&mut self) -> bool {
        let blank = [' ', '\t', '\r', '\n'];
        // Past the first `delimiter` in `text`, if any: each byte looked at.
        let past = |text: &str, delimiter: &str| -> Option<usize> {
            let end = text.find(delimiter).map(|at| at + delimiter.len());
            crate::work::count(1 + end.unwrap_or(text.len()));
            end
        };
        while let Some((text, at)) = self.texts.last().cloned() {
            let whole = text.as_str();
            let rest = &whole[at..];
            let trimmed = rest.trim_start_matches(blank);
            crate::work::count(rest.len() - trimmed.len());
            let skipped = rest.len() - trimmed.len();
            let rest = trimmed;
            let inner = match text {
                Text::Subset(_) => None,
                Text::Included(_, parameter) => Some(parameter),
            };
            if rest.is_empty()
                && let Some(parameter) = inner
            {
                // Read to its end, after whole declarations only.
                self.parameters.reading[parameter] = false;
                self.texts.pop();
                continue;
            }
            let mut push = None;
            let end = if let Some(after) = rest.strip_prefix("<!--") {
                // No `--` inside but the end's (2.5).
                let end = after.find("--");
                crate::work::count(1 + end.unwrap_or(after.len()));
                match end {
                    Some(end) if after[end + 2..].starts_with('>') => Some(4 + end + 3),
                    _ => return false,
                }
            } else if let Some(after) = rest.strip_prefix("<?") {
                // One named `xml` in any casing (an XML declaration not at
                // the start), or whose target is missing or not followed by
                // white space, is refused, as libxml2 refuses it here (2.6,
                // `PITarget`).
                if named_xml(after).is_some() || instruction_problem(after).is_some() {
                    return false;
                }
                past(after, "?>").map(|end| 2 + end)
            } else if let Some(after) = rest.strip_prefix('%') {
                let Ok((end, included)) = self.reference(after) else {
                    return false;
                };
                push = included;
                Some(1 + end)
            } else if let Some(after) = rest.strip_prefix("<!") {
                let Ok(end) = self.declaration(after, inner.is_some()) else {
                    return false;
                };
                end.map(|end| 2 + end)
            } else {
                None
            };
            let Some(end) = end else {
                // Included text holds whole declarations, comments,
                // processing instructions and references only (WFC: PE
                // Between Declarations): not a part of one, text, a
                // conditional section or `]`. The subset ends at `]`; what
                // its grammar does not allow ends it too, and refuses it
                // ([`declared_entities`]).
                return inner.is_none();
            };
            // What the subset itself holds is written as it is, so holds
            // only characters the version allows written so: in a literal,
            // a comment or a processing instruction. Included text was
            // checked as its entity's literal.
            if inner.is_none() {
                let written = &rest[..end];
                // Each byte looked at.
                crate::work::count(1 + written.len());
                if !written.chars().all(|c| may_be_written(c, self.version)) {
                    return false;
                }
            }
            if let Some(last) = self.texts.last_mut() {
                last.1 = at + skipped + end;
            }
            if let Some((text, parameter)) = push {
                self.parameters.reading[parameter] = true;
                self.texts.push((text, 0));
            }
        }
        true
    }

    /// Reads a parameter-entity reference between declarations, `after`
    /// following its `%`: its length after the `%`, and the text it
    /// includes, if any. `Err` if it refuses the document type.
    fn reference(&mut self, after: &str) -> Result<(usize, Option<(Text<'a>, usize)>), ()> {
        let end = after.find(|c| !is_name_char(c)).unwrap_or(after.len());
        // The name scanned, hashed and compared.
        crate::work::count(1 + 3 * end);
        let (name, tail) = after.split_at(end);
        // One with no name is refused in both modes.
        if !name.starts_with(is_name_start) || !tail.starts_with(';') {
            return Err(());
        }
        // Any reference makes a document that is not standalone open (4.1:
        // Entity Declared is then a validity constraint).
        if !self.standalone {
            self.entities.open = true;
        }
        let parameters = &self.parameters;
        let found = parameters
            .index
            .get(name)
            .map(|&at| (at, &parameters.list[at]));
        let included = match found {
            // Read (4.4.8), never within itself (WFC: No
            // Recursion), and within the budget.
            Some((at, Parameter::Text(replaced))) => {
                self.included = self.included.saturating_add(replaced.len());
                if parameters.reading[at] || self.included > self.budget {
                    return Err(());
                }
                Some((Text::Included(Rc::clone(replaced), at), at))
            }
            // Not read: nothing after it is processed (5.1), unless
            // standalone; then an undeclared one is an error (4.1).
            Some((_, Parameter::External)) | None if !self.standalone => {
                self.processing = false;
                None
            }
            Some((_, Parameter::External)) => None,
            Some((_, Parameter::Unreadable)) | None => return Err(()),
        };
        Ok((1 + end, included))
    }

    /// Reads a markup declaration, `after` following its `<!`, `included`
    /// in a parameter entity's text or not: its length after the `<!`, if
    /// it ends. `Err` if it refuses the document type.
    fn declaration(&mut self, after: &str, included: bool) -> Result<Option<usize>, ()> {
        // Read whole by XML's grammar, processed or not.
        let Some(end) = grammatical_end(after) else {
            return Err(());
        };
        // Every literal is checked, processed or not, and so is the rest of
        // the declaration.
        if has_parameter_reference(&after[..end]) || has_bad_literal(after, self.version) {
            return Err(());
        }
        if self.processing
            && let Some(declared) = after.strip_prefix("ENTITY")
            && !declare(
                &mut self.entities,
                &mut self.parameters,
                declared,
                self.version,
                included,
            )
        {
            return Err(());
        }
        if self.processing
            && let Some(list) = after.strip_prefix("ATTLIST")
        {
            // With standalone, an entity a default outside parameter-entity
            // text refers to needs a declaration outside such text, anywhere.
            let outside = self.standalone && !included;
            let entities = &mut self.entities;
            let noted = default_references(
                list,
                self.version,
                &entities.index,
                &mut entities.defaults,
                outside.then_some(&mut entities.list),
            );
            if !noted {
                return Err(());
            }
        }
        Ok(Some(end))
    }
}

/// A text the internal subset is read from ([`declared_entities`]).
#[derive(Clone)]
enum Text<'a> {
    /// The subset itself.
    Subset(&'a str),
    /// A parameter entity's replacement text, and the entity's place.
    Included(Rc<str>, usize),
}

impl Text<'_> {
    fn as_str(&self) -> &str {
        match self {
            Text::Subset(text) => text,
            Text::Included(text, _) => text,
        }
    }
}

/// What reading an internal subset keeps besides the general entities
/// ([`declared_entities`]).
#[derive(Default)]
struct Parameters {
    /// Each declared parameter entity's place in `list`, by name, the
    /// first declaration binding.
    index: HashMap<String, usize>,
    list: Vec<Parameter>,
    /// Whether each is being read (included, not read to its end): a
    /// reference to it then is a loop.
    reading: Vec<bool>,
}

/// A declared parameter entity.
enum Parameter {
    /// Internal: its replacement text, read where it is referenced.
    Text(Rc<str>),
    /// Internal, its literal unreadable ([`replaced`]): a reference to it
    /// refuses the document type.
    Unreadable,
    /// External: not read.
    External,
}

impl Parameter {
    /// The bytes it holds ([`crate::work::hold`]).
    fn bytes(&self) -> usize {
        match self {
            Parameter::Text(text) => text.len(),
            Parameter::Unreadable | Parameter::External => 0,
        }
    }
}

/// Notes in `entities` the general entity `declared`, an entity
/// declaration after `<!ENTITY`, declares in a document of XML `version`,
/// if any, or in `parameters` the parameter entity it declares
/// ([`declared_entities`]); `included`: in a parameter entity's text.
/// False if the declaration is one libxml2 refuses with the document type:
/// an internal entity's literal followed by more than white space before
/// `>` (`NDATA` on an internal entity). Its literal's references were
/// checked before ([`has_bad_literal`]).
fn declare(
    entities: &mut Entities,
    parameters: &mut Parameters,
    declared: &str,
    version: XmlVersion,
    included: bool,
) -> bool {
    let blank = [' ', '\t', '\r', '\n'];
    let rest = declared.trim_start_matches(blank);
    if rest.len() == declared.len() {
        return true;
    }
    if let Some(parameter) = rest.strip_prefix('%') {
        return declare_parameter(parameters, parameter, version, included);
    }
    let end = rest.find(|c| !is_name_char(c)).unwrap_or(rest.len());
    // The name scanned.
    crate::work::count(1 + end);
    let (name, definition) = rest.split_at(end);
    let after_name = definition.trim_start_matches(blank);
    if !name.starts_with(is_name_start)
        || name.contains(':')
        || after_name.len() == definition.len()
    {
        return true;
    }
    // An internal entity's definition is a literal; an external or
    // unparsed one's starts `SYSTEM` or `PUBLIC`.
    let literal = after_name
        .chars()
        .next()
        .filter(|&c| c == '"' || c == '\'')
        .and_then(|quote| after_name[1..].split_once(quote));
    if let Some((_, after)) = literal {
        let trimmed = after.trim_start_matches(blank);
        // Each byte looked at.
        crate::work::count(1 + after.len() - trimmed.len());
        if !trimmed.starts_with('>') {
            return false;
        }
    }
    // Hashed, compared and copied.
    crate::work::count(1 + 3 * name.len());
    if let Some(&at) = entities.index.get(name) {
        entities.list[at].outside |= !included;
        return true;
    }
    let text = literal.and_then(|(literal, _)| replacement_text(literal, version, !included));
    entities.index.insert(name.to_owned(), entities.list.len());
    entities.list.push(Entity {
        text,
        outside: !included,
        included,
        ..Entity::default()
    });
    true
}

/// Whether `declaration`, a markup declaration after its `<!`, holds a
/// literal that is not well formed in a document of XML `version`, which
/// libxml2 refuses with the document type: an entity's value (general or
/// parameter, whatever its name) with a reference that cannot be one
/// ([`has_bad_reference`]), or an attribute default in an `ATTLIST` with one
/// or with `<` (3.1, WFC: No `<` in Attribute Values). System and public
/// literals hold no references and are not looked at.
fn has_bad_literal(declaration: &str, version: XmlVersion) -> bool {
    let blank = [' ', '\t', '\r', '\n'];
    if let Some(declared) = declaration.strip_prefix("ENTITY") {
        // `% `, the name and white space, then the value, if a literal.
        let mut rest = declared.trim_start_matches(blank);
        if let Some(parameter) = rest.strip_prefix('%') {
            rest = parameter.trim_start_matches(blank);
        }
        let end = rest.find(|c: char| blank.contains(&c) || c == '"' || c == '\'' || c == '>');
        let rest = rest[end.unwrap_or(rest.len())..].trim_start_matches(blank);
        // Each byte up to the value looked at.
        crate::work::count(1 + declared.len() - rest.len());
        let Some(quote) = rest.chars().next().filter(|&c| c == '"' || c == '\'') else {
            return false;
        };
        let Some((literal, _)) = rest[1..].split_once(quote) else {
            // Each byte looked at, to the end.
            crate::work::count(rest.len());
            return false;
        };
        // Each byte looked at for `%` (a parameter-entity reference, or a
        // bare `%`: neither may stand in a literal of the internal subset,
        // WFC: PEs in Internal Subset).
        crate::work::count(1 + 2 * literal.len());
        return literal.contains('%') || has_bad_reference(literal, version);
    }
    let Some(list) = declaration.strip_prefix("ATTLIST") else {
        return false;
    };
    // Each quoted default, to the declaration's end.
    let end = declaration_end(list).unwrap_or(list.len());
    let mut rest = &list[..end];
    while let Some(at) = rest.find(['"', '\'']) {
        crate::work::count(1 + at);
        let quote = &rest[at..=at];
        let Some((default, after)) = rest[at + 1..].split_once(quote) else {
            // Each byte looked at, to the end.
            crate::work::count(rest.len() - at);
            return false;
        };
        // Each byte looked at for the quote, and for `<`.
        crate::work::count(1 + 2 * default.len());
        if default.contains('<') || has_bad_reference(default, version) {
            return true;
        }
        rest = after;
    }
    false
}

/// Whether `declaration`, a markup declaration after its `<!` and to its
/// end, has `%` outside its literals other than the one that declares a
/// parameter entity (`<!ENTITY % `): a parameter-entity reference, which
/// the internal subset allows only between declarations (WFC: PEs in
/// Internal Subset), or a bare `%`; libxml2 refuses both.
fn has_parameter_reference(declaration: &str) -> bool {
    let blank = [' ', '\t', '\r', '\n'];
    let mut rest = declaration;
    // The `%` that declares a parameter entity, between white space.
    if let Some(declared) = rest.strip_prefix("ENTITY") {
        let named = declared.trim_start_matches(blank);
        if named.len() < declared.len()
            && let Some(name) = named.strip_prefix('%')
            && name.starts_with(blank)
        {
            rest = name;
        }
    }
    // Each byte looked at, literals skipped.
    crate::work::count(1 + rest.len());
    let mut quote = None;
    for c in rest.chars() {
        match (quote, c) {
            (None, '%') => return true,
            (None, '"' | '\'') => quote = Some(c),
            (Some(open), _) if open == c => quote = None,
            _ => {}
        }
    }
    false
}

/// Notes in `found` the entities the defaults of `list`, an `ATTLIST`
/// declaration after `ATTLIST`, refer to in a document of XML `version`:
/// references other than character references and XML's own (their
/// well-formedness was checked before, [`has_bad_literal`]). One not in
/// `declared` yet, in parameter-entity text or not, is noted as `None`: an
/// entity must be declared before a default refers to it (XML 1.0, 4.1). Each
/// is noted with the number of entities declared so far, where the default
/// stands; and, if `outside` is given (a default outside parameter-entity
/// text, with standalone), each declared one is marked in it as needing a
/// declaration outside such text, anywhere ([`Entity::outside_needed`]).
/// False if memory for one more is refused: they grow with what parameter
/// entities include, which no budget may bound.
fn default_references(
    list: &str,
    version: XmlVersion,
    declared: &HashMap<String, usize>,
    found: &mut Vec<(Option<usize>, usize)>,
    mut outside: Option<&mut Vec<Entity>>,
) -> bool {
    let end = declaration_end(list).unwrap_or(list.len());
    let mut rest = &list[..end];
    while let Some(at) = rest.find(['"', '\'']) {
        crate::work::count(1 + at);
        let quote = &rest[at..=at];
        let Some((default, after)) = rest[at + 1..].split_once(quote) else {
            // Each byte looked at, to the end.
            crate::work::count(rest.len() - at);
            return true;
        };
        for part in default.split('&').skip(1) {
            if let Some((name, _)) = part.split_once(';')
                && replacement(&BytesRef::new(name), version).is_none()
            {
                // Hashed and compared.
                crate::work::count(1 + 2 * name.len());
                let at = declared.get(name).copied();
                if found.try_reserve(1).is_err() {
                    return false;
                }
                found.push((at, declared.len()));
                if let (Some(at), Some(list)) = (at, outside.as_deref_mut()) {
                    list[at].outside_needed = true;
                }
            }
        }
        // Each byte of the default looked at.
        crate::work::count(1 + default.len());
        rest = after;
    }
    true
}

/// Whether `literal`, in a document of XML `version`, holds a reference
/// that cannot be one, read as in an attribute value ([`attribute_value`]):
/// a `&` with no `;` after it, a character reference to no legal
/// character, or an entity reference whose name is not an XML name.
fn has_bad_reference(literal: &str, version: XmlVersion) -> bool {
    // Each byte looked at.
    crate::work::count(1 + literal.len());
    let mut rest = literal;
    while let Some((_, after)) = rest.split_once('&') {
        let Some((name, after)) = after.split_once(';') else {
            return true;
        };
        let known = if name.starts_with('#') {
            replacement(&BytesRef::new(name), version).is_some()
        } else {
            is_xml_name(name)
        };
        if !known {
            return true;
        }
        rest = after;
    }
    false
}

/// Whether `name` is an XML `Name` (XML 1.0, 5th ed., and 1.1, 2.3).
fn is_xml_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(is_name_start) && chars.all(is_name_char)
}

/// The replacement text of an internal entity whose literal is `literal`, in
/// a document of XML `version` (XML 1.0, 4.5): line ends normalised to a line
/// feed (2.11; in XML 1.1 also U+0085 and U+2028), character references
/// replaced, other references kept. `None` if the literal has `<`, a
/// character reference to a character it does not allow, or `&` not ending in
/// `;`. A literal with `%` refuses the document type before
/// ([`has_bad_literal`]), and one written with a character the version does
/// not allow written as it is ([`may_be_written`]) right after its
/// declaration is read ([`Reader::read`]), so the text is then not used.
/// `written`: the literal is written in the document; in a parameter entity's
/// text it is not, its characters already read and their line ends normalised
/// (2.11 is on the entities read, not on replacement texts), so that is not
/// done again.
fn replacement_text(literal: &str, version: XmlVersion, written: bool) -> Option<String> {
    if literal.contains('<') {
        return None;
    }
    replaced(literal, version, written)
}

/// The replacement text of an internal entity, general or parameter, whose
/// literal is `literal`, in a document of XML `version`, as
/// [`replacement_text`] says, `<` allowed (a parameter entity's text may
/// hold markup).
fn replaced(literal: &str, version: XmlVersion, written: bool) -> Option<String> {
    // Each byte looked at (for `<` too), and copied.
    crate::work::count(1 + 2 * literal.len());
    let lines = if written {
        line_ends(literal, version)
    } else {
        Cow::Borrowed(literal)
    };
    let mut parts = lines.split('&');
    let mut text = parts.next().unwrap_or_default().to_owned();
    for part in parts {
        let (name, after) = part.split_once(';')?;
        if name.starts_with('#') {
            text.push(replacement(&BytesRef::new(name), version)?);
        } else {
            text.push('&');
            text.push_str(name);
            text.push(';');
        }
        text.push_str(after);
    }
    Some(text)
}

/// The length `text`, a replacement text or a value whose line ends are
/// normalised, has in a document of XML `version` with its references to
/// declared entities left out and its character references and XML's own
/// entities replaced: what it adds to an expansion besides the entities it
/// refers to.
fn own_length(text: &str, version: XmlVersion) -> usize {
    // Each byte looked at.
    crate::work::count(1 + text.len());
    let mut length = text.len();
    for part in text.split('&').skip(1) {
        if let Some((name, _)) = part.split_once(';') {
            length -= name.len() + 2;
            if let Some(c) = replacement(&BytesRef::new(name), version) {
                length += c.len_utf8();
            }
        }
    }
    length
}

/// `text` with its line ends normalised to a line feed, in a document of
/// XML `version` (2.11): CR LF and CR, and in XML 1.1 also CR NEL, NEL
/// and LS.
fn line_ends(text: &str, version: XmlVersion) -> Cow<'_, str> {
    let v11 = version == XmlVersion::Explicit1_1;
    let is_end = |c: char| c == '\r' || v11 && matches!(c, '\u{85}' | '\u{2028}');
    // Each byte looked at.
    crate::work::count(1 + text.len());
    if !text.contains(is_end) {
        return Cow::Borrowed(text);
    }
    // Copied.
    crate::work::count(text.len());
    let mut lines = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                // CR LF (and in XML 1.1 CR NEL) is one line end.
                let _ = chars.next_if(|&c| c == '\n' || v11 && c == '\u{85}');
                lines.push('\n');
            }
            '\u{85}' | '\u{2028}' if v11 => lines.push('\n'),
            c => lines.push(c),
        }
    }
    Cow::Owned(lines)
}

/// Notes in `parameters` the parameter entity `declared`, an entity
/// declaration after `<!ENTITY` and `%`, declares in a document of XML
/// `version` ([`Parameter`]), the first declaration binding; `included`: in a
/// parameter entity's text ([`replaced`]). False if libxml2 refuses it with
/// the document type: no white space after `%` or after the name, a name that
/// is not an XML name, or more than white space after its definition before
/// `>` (`NDATA`, which no parameter entity may have).
fn declare_parameter(
    parameters: &mut Parameters,
    declared: &str,
    version: XmlVersion,
    included: bool,
) -> bool {
    let blank = [' ', '\t', '\r', '\n'];
    let named = declared.trim_start_matches(blank);
    let end = named.find(|c| !is_name_char(c)).unwrap_or(named.len());
    // The name scanned.
    crate::work::count(1 + end);
    let (name, definition) = named.split_at(end);
    let defined = definition.trim_start_matches(blank);
    if named.len() == declared.len()
        || !name.starts_with(is_name_start)
        || defined.len() == definition.len()
    {
        return false;
    }
    let (literal, after) =
        if let Some(quote) = defined.chars().next().filter(|&c| c == '"' || c == '\'') {
            // Unterminated: the declaration's end is not found either.
            let Some((literal, after)) = defined[1..].split_once(quote) else {
                return true;
            };
            (Some(literal), after)
        } else {
            match external_id(defined) {
                Some(Ok(after)) => (None, after),
                _ => return false,
            }
        };
    let rest = after.trim_start_matches(blank);
    // Each byte looked at, and the name hashed, compared and copied.
    crate::work::count(1 + after.len() - rest.len() + 3 * name.len());
    if !rest.starts_with('>') {
        return false;
    }
    if !parameters.index.contains_key(name) {
        let parameter = match literal.map(|literal| replaced(literal, version, !included)) {
            None => Parameter::External,
            Some(None) => Parameter::Unreadable,
            Some(Some(text)) => {
                // Copied.
                crate::work::count(text.len());
                Parameter::Text(Rc::from(text))
            }
        };
        parameters
            .index
            .insert(name.to_owned(), parameters.list.len());
        parameters.list.push(parameter);
        parameters.reading.push(false);
    }
    true
}

/// The rest of `text` after an external ID at its start (XML 1.0, 4.2.2):
/// `SYSTEM` and a literal, or `PUBLIC` and two, each after white space.
/// `Some(Err(()))` if it starts with `SYSTEM` or `PUBLIC` but is not
/// written so (libxml2 refuses it); `None` if it starts with neither.
fn external_id(text: &str) -> Option<Result<&str, ()>> {
    let blank = [' ', '\t', '\r', '\n'];
    let (mut rest, literals) = if let Some(after) = text.strip_prefix("SYSTEM") {
        (after, 1)
    } else {
        (text.strip_prefix("PUBLIC")?, 2)
    };
    for literal in (0..literals).rev() {
        let trimmed = rest.trim_start_matches(blank);
        let Some(quote) = trimmed.chars().next().filter(|&c| c == '"' || c == '\'') else {
            return Some(Err(()));
        };
        let end = trimmed[1..].find(quote);
        // Each byte looked at, twice in a public literal.
        crate::work::count(1 + rest.len() - trimmed.len() + 2 * end.unwrap_or(trimmed.len()));
        let Some(end) = end.filter(|_| trimmed.len() < rest.len()) else {
            return Some(Err(()));
        };
        // A public literal holds only `PubidChar`s.
        if literal == 1 && !trimmed[1..=end].chars().all(is_pubid_char) {
            return Some(Err(()));
        }
        rest = &trimmed[2 + end..];
    }
    Some(Ok(rest))
}

/// The most groups a content model may nest, as libxml2 has it by default
/// (without `XML_PARSE_HUGE`): one more refuses the declaration. It bounds
/// the stack the groups are read with; libxml2's limit on a name's length
/// is not copied, as names are read in linear time.
const MAX_CONTENT_DEPTH: usize = 256;

/// Whether `c` is a `PubidChar` (XML 1.0, 2.3): what a public literal
/// may hold.
fn is_pubid_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || " \r\n-'()+,./:=?;!*#@$_%".contains(c)
}

/// Where the markup declaration `declaration` (after its `<!`) ends, past its
/// `>`, if it is written as XML's grammar says (XML 1.0, 2.8, 3.2, 3.3, 4.2,
/// 4.7) and libxml2 takes it in an internal subset: `ENTITY`, `ELEMENT`,
/// `ATTLIST` or `NOTATION` and white space, whole, its names XML names, an
/// entity's system literal with no `#` (libxml2: "Fragment not allowed"), a
/// public literal of `PubidChar`s, a content model nested at most
/// [`MAX_CONTENT_DEPTH`] deep. `None` otherwise: another or a lower-case
/// keyword, a conditional section, a document type, or a declaration not
/// ended. White space is ASCII; in an XML 1.1 file NEL and LS here are not
/// read as line ends (a known limit, as in a tag), and libxml2, which reads
/// XML 1.1 as 1.0, does not read them so either. References in literals are
/// checked apart ([`has_bad_literal`]), as is a `%` outside them
/// ([`has_parameter_reference`]). Linear in the bytes read.
fn grammatical_end(declaration: &str) -> Option<usize> {
    let mut grammar = Grammar {
        text: declaration,
        at: 0,
    };
    let read = if grammar.keyword("ENTITY") {
        grammar.entity()
    } else if grammar.keyword("ELEMENT") {
        grammar.element()
    } else if grammar.keyword("ATTLIST") {
        grammar.attlist()
    } else if grammar.keyword("NOTATION") {
        grammar.notation()
    } else {
        false
    };
    let ended = read && {
        grammar.blank();
        grammar.eat(">")
    };
    // Each byte looked at, about once.
    crate::work::count(1 + grammar.at);
    ended.then_some(grammar.at)
}

/// A markup declaration being read by XML's grammar ([`grammatical_end`]):
/// its text after `<!`, and how far it was read.
struct Grammar<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Grammar<'a> {
    fn rest(&self) -> &'a str {
        &self.text[self.at..]
    }

    /// Reads `token` if the text goes on with it.
    fn eat(&mut self, token: &str) -> bool {
        let found = self.rest().starts_with(token);
        if found {
            self.at += token.len();
        }
        found
    }

    /// Reads `keyword` and the white space that must follow it.
    fn keyword(&mut self, keyword: &str) -> bool {
        let from = self.at;
        let read = self.eat(keyword) && self.blank();
        if !read {
            self.at = from;
        }
        read
    }

    /// Reads white space (`S`, ASCII), if any: whether there was some.
    fn blank(&mut self) -> bool {
        let rest = self.rest();
        let length = rest.len() - rest.trim_start_matches([' ', '\t', '\r', '\n']).len();
        self.at += length;
        length > 0
    }

    /// Reads name characters, the first a name start if `name`: whether
    /// there was one (a `Name`, or a `Nmtoken`).
    fn token(&mut self, name: bool) -> bool {
        let rest = self.rest();
        let length = rest.find(|c: char| !is_name_char(c)).unwrap_or(rest.len());
        let read = length > 0 && (!name || rest.starts_with(is_name_start));
        if read {
            self.at += length;
        } else {
            // Each byte looked at, past where reading stops.
            crate::work::count(1 + length);
        }
        read
    }

    fn name(&mut self) -> bool {
        self.token(true)
    }

    /// Reads a quoted literal: what is between its quotes.
    fn literal(&mut self) -> Option<&'a str> {
        let rest = self.rest();
        let quote = rest.chars().next().filter(|&c| c == '"' || c == '\'')?;
        let Some((literal, _)) = rest[1..].split_once(quote) else {
            // Each byte looked at, to the end.
            crate::work::count(rest.len());
            return None;
        };
        self.at += literal.len() + 2;
        Some(literal)
    }

    /// Reads an `ExternalID`, or in a notation also a `PublicID` (4.2.2,
    /// 4.7); in an entity's, a system literal with `#` is refused.
    fn external_id(&mut self, notation: bool) -> bool {
        // A literal's bytes are counted as read ([`grammatical_end`]), and
        // again as checked, as [`external_id`] counts them.
        let system = |grammar: &mut Self| {
            grammar.literal().is_some_and(|literal| {
                crate::work::count(literal.len());
                notation || !literal.contains('#')
            })
        };
        if self.keyword("SYSTEM") {
            return system(self);
        }
        let public = self.keyword("PUBLIC").then(|| self.literal()).flatten();
        if !public.is_some_and(|id| {
            crate::work::count(id.len());
            id.chars().all(is_pubid_char)
        }) {
            return false;
        }
        let from = self.at;
        if self.blank() && self.rest().starts_with(['"', '\'']) {
            return system(self);
        }
        self.at = from;
        notation
    }

    /// Reads an entity declaration after `ENTITY` and white space, to
    /// before its end (4.2).
    fn entity(&mut self) -> bool {
        let parameter = self.eat("%");
        if (parameter && !self.blank()) || !self.name() || !self.blank() {
            return false;
        }
        if self.rest().starts_with(['"', '\'']) {
            return self.literal().is_some();
        }
        if !self.external_id(false) {
            return false;
        }
        // `NDATA` and a name, for a general entity only.
        let from = self.at;
        if !parameter && self.blank() && self.keyword("NDATA") {
            return self.name();
        }
        self.at = from;
        true
    }

    /// Reads an element type declaration after `ELEMENT` and white space,
    /// to before its end (3.2).
    fn element(&mut self) -> bool {
        if !self.name() || !self.blank() {
            return false;
        }
        if self.eat("EMPTY") || self.eat("ANY") {
            return true;
        }
        self.eat("(") && self.content()
    }

    /// Reads a content model after its `(` (3.2.1, 3.2.2): mixed content,
    /// or children, groups nested as deep as [`MAX_CONTENT_DEPTH`], read
    /// without recursion.
    fn content(&mut self) -> bool {
        let occurrence = |grammar: &mut Self| {
            let _ = grammar.eat("?") || grammar.eat("*") || grammar.eat("+");
        };
        self.blank();
        if self.eat("#PCDATA") {
            let mut names = false;
            loop {
                self.blank();
                if self.eat(")") {
                    // `*` after a list of names, optional without.
                    return self.eat("*") || !names;
                }
                if !self.eat("|") {
                    return false;
                }
                self.blank();
                if !self.name() {
                    return false;
                }
                names = true;
            }
        }
        // The separator each open group uses, once it has two parts.
        let mut groups: Vec<Option<char>> = vec![None];
        loop {
            // A content particle.
            self.blank();
            if self.eat("(") {
                if groups.len() == MAX_CONTENT_DEPTH {
                    return false;
                }
                groups.push(None);
                continue;
            }
            if !self.name() {
                return false;
            }
            occurrence(self);
            // Then the ends of groups, and a separator.
            loop {
                self.blank();
                if self.eat(")") {
                    groups.pop();
                    occurrence(self);
                    if groups.is_empty() {
                        return true;
                    }
                    continue;
                }
                let separator = if self.eat(",") {
                    ','
                } else if self.eat("|") {
                    '|'
                } else {
                    return false;
                };
                // One kind in a group: `,` (a sequence) or `|` (a choice).
                let Some(used) = groups.last_mut() else {
                    return false;
                };
                if *used.get_or_insert(separator) != separator {
                    return false;
                }
                break;
            }
        }
    }

    /// Reads an attribute-list declaration after `ATTLIST` and white
    /// space, to before its end (3.3).
    fn attlist(&mut self) -> bool {
        if !self.name() {
            return false;
        }
        loop {
            let blank = self.blank();
            if self.rest().starts_with('>') {
                return true;
            }
            if !blank || !self.name() || !self.blank() || !self.attribute_type() || !self.blank() {
                return false;
            }
            let default = if self.eat("#REQUIRED") || self.eat("#IMPLIED") {
                true
            } else {
                (!self.eat("#FIXED") || self.blank()) && self.literal().is_some()
            };
            if !default {
                return false;
            }
        }
    }

    /// Reads an attribute type (3.3.1).
    fn attribute_type(&mut self) -> bool {
        // An enumeration of names or name tokens.
        let enumeration = |grammar: &mut Self, name: bool| {
            if !grammar.eat("(") {
                return false;
            }
            loop {
                grammar.blank();
                if !grammar.token(name) {
                    return false;
                }
                grammar.blank();
                if grammar.eat(")") {
                    return true;
                }
                if !grammar.eat("|") {
                    return false;
                }
            }
        };
        if self.rest().starts_with('(') {
            return enumeration(self, false);
        }
        let from = self.at;
        if !self.name() {
            return false;
        }
        match &self.text[from..self.at] {
            "CDATA" | "ID" | "IDREF" | "IDREFS" | "ENTITY" | "ENTITIES" | "NMTOKEN"
            | "NMTOKENS" => true,
            "NOTATION" => self.blank() && enumeration(self, true),
            _ => false,
        }
    }

    /// Reads a notation declaration after `NOTATION` and white space, to
    /// before its end (4.7).
    fn notation(&mut self) -> bool {
        self.name() && self.blank() && self.external_id(true)
    }
}

/// Where the markup declaration `declaration` (after its `<!`) ends: past
/// its `>`, its literals skipped.
fn declaration_end(declaration: &str) -> Option<usize> {
    let mut quote = None;
    for (at, c) in declaration.char_indices() {
        match (quote, c) {
            (None, '>') => {
                // Each byte looked at.
                crate::work::count(1 + at);
                return Some(at + 1);
            }
            (None, '"' | '\'') => quote = Some(c),
            (Some(open), _) if open == c => quote = None,
            _ => {}
        }
    }
    crate::work::count(1 + declaration.len());
    None
}

/// Returns possible paths for `mime` in `dir`, in order, without duplicates:
/// `<dir>/<media>/<subtype>.xml` for each `registered` spelling (already
/// unique), then the lowercase path used by `update-mime-database` if not
/// already included.
pub(crate) fn type_file_paths(
    dir: &Path,
    registered: &[MimeType],
    mime: &MimeType,
) -> Vec<PathBuf> {
    let path = |media: &str, subtype: &str| {
        let path = dir.join(media).join(format!("{subtype}.xml"));
        // Copied twice: by each join.
        crate::work::count(2 * path.as_os_str().len());
        path
    };
    // Copied.
    crate::work::count(mime.as_str().len());
    let lowercase = mime.as_str().to_ascii_lowercase();
    let mut listed = false;
    let mut paths: Vec<PathBuf> = registered
        .iter()
        .map(|spelling| {
            // Compared.
            crate::work::count(1 + spelling.as_str().len());
            listed |= spelling.as_str() == lowercase;
            path(spelling.media(), spelling.subtype())
        })
        .collect();
    if !listed && let Some((media, subtype)) = lowercase.split_once('/') {
        paths.push(path(media, subtype));
    }
    paths
}

/// The types with an XML file in `dir`, for a folder without a `types` file.
/// Listing errors go to `diagnostics` ([`entries`]), and so does an entry
/// that cannot be checked.
pub(crate) fn types_in(dir: &Path, diagnostics: &mut Vec<Diagnostic>) -> Vec<MimeType> {
    types_under(dir, |_| true, |_| true, diagnostics, None)
}

/// Lists types as [`types_in`] does, but only in folders whose names match
/// `mime`'s media part, ignoring ASCII case. A lookup skips unrelated media
/// folders and their errors; of the entries that cannot be checked, only
/// one that would name `mime` is reported, as the others concern other
/// types. Adds each listed folder to `stamps` before listing it, so later
/// file additions are detected.
pub(crate) fn types_in_media(
    dir: &Path,
    mime: &MimeType,
    diagnostics: &mut Vec<Diagnostic>,
    stamps: &mut Vec<(PathBuf, FileStamp)>,
) -> Vec<MimeType> {
    let media = mime.media();
    types_under(
        dir,
        |name| {
            // Compared.
            crate::work::count(1 + name.len().min(media.len()));
            name.eq_ignore_ascii_case(media)
        },
        |entry| {
            // Compared.
            crate::work::count(1 + entry.as_str().len());
            entry == mime
        },
        diagnostics,
        Some(stamps),
    )
}

/// Lists types as [`types_in`] does, but only in media folders accepted by
/// `listed`, reporting an entry that cannot be checked only when `reported`
/// accepts the type it would name. If `stamps` is supplied, stamps each
/// folder before listing it.
fn types_under(
    dir: &Path,
    listed: impl Fn(&str) -> bool,
    reported: impl Fn(&MimeType) -> bool,
    diagnostics: &mut Vec<Diagnostic>,
    mut stamps: Option<&mut Vec<(PathBuf, FileStamp)>>,
) -> Vec<MimeType> {
    use crate::work::count;
    let mut types = Vec::new();
    for media_dir in entries(dir, diagnostics) {
        let media = media_dir.file_name();
        // Copied, then checked as UTF-8.
        count(1 + 2 * media.len());
        let Some(media) = media.to_str() else {
            continue;
        };
        if media == "packages" || !listed(media) {
            continue;
        }
        let media_path = media_dir.path();
        count(media_path.as_os_str().len());
        match probe(&media_path, std::fs::Metadata::is_dir) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(err) => {
                diagnostics.push(Diagnostic::new(
                    &media_path,
                    None,
                    format!("cannot be read: {err}"),
                ));
                continue;
            }
        }
        let stamp = stamps.is_some().then(|| FileStamp::of(&media_path));
        for file in entries(&media_path, diagnostics) {
            let name = file.file_name();
            // Copied, checked as UTF-8, its end compared.
            count(1 + 2 * name.len());
            let Some(subtype) = name.to_str().and_then(|name| name.strip_suffix(".xml")) else {
                continue;
            };
            // Copied, then checked as a type (counted there).
            count(media.len() + subtype.len());
            let Ok(mime) = MimeType::new(&format!("{media}/{subtype}")) else {
                continue;
            };
            // Regular files only (links followed), as `TypeInfo::read`
            // finds them; only a link needs another look.
            let regular = match file.file_type() {
                Ok(kind) if kind.is_symlink() => probe(&file.path(), std::fs::Metadata::is_file),
                Ok(kind) => Ok(kind.is_file()),
                Err(err) if absent(&err) => Ok(false),
                Err(err) => Err(err),
            };
            match regular {
                Ok(true) => types.push(mime),
                Ok(false) => {}
                // Reported only to a lookup of the type it would name.
                Err(err) => {
                    if reported(&mime) {
                        diagnostics.push(Diagnostic::new(
                            file.path(),
                            None,
                            format!("cannot be read: {err}"),
                        ));
                    }
                }
            }
        }
        if let (Some(stamps), Some(stamp)) = (stamps.as_deref_mut(), stamp) {
            stamps.push((media_path, stamp));
        }
    }
    types
}

/// Whether `path` (links followed) is what `is` accepts. A path that is
/// absent ([`absent`]) is not; another error is returned.
fn probe(path: &Path, is: fn(&std::fs::Metadata) -> bool) -> std::io::Result<bool> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(is(&metadata)),
        Err(err) if absent(&err) => Ok(false),
        Err(err) => Err(err),
    }
}

/// Whether `err` says a path is absent: it, or a folder on the way, is
/// missing (`NotFound`), or a step on the way is not a folder
/// (`NotADirectory`).
fn absent(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// Lists `dir`, discarding the partial listing and restarting after `EINTR`.
/// Other errors while opening the folder or reading an entry go to
/// `diagnostics`; entries read before the error are kept. A missing `dir`
/// returns no entries and no diagnostic.
fn entries(dir: &Path, diagnostics: &mut Vec<Diagnostic>) -> Vec<std::fs::DirEntry> {
    use std::io::ErrorKind;
    'list: loop {
        let mut entries = Vec::new();
        let listing = match listing_fault(dir, None).map_or_else(|| std::fs::read_dir(dir), Err) {
            Ok(listing) => listing,
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == ErrorKind::NotFound => return entries,
            Err(err) => {
                diagnostics.push(Diagnostic::new(
                    dir,
                    None,
                    format!("cannot be listed: {err}"),
                ));
                return entries;
            }
        };
        for entry in listing {
            match listing_fault(dir, Some(entries.len())).map_or(entry, Err) {
                Ok(entry) => entries.push(entry),
                Err(err) if err.kind() == ErrorKind::Interrupted => continue 'list,
                Err(err) => {
                    diagnostics.push(Diagnostic::new(
                        dir,
                        None,
                        format!("cannot be listed in full: {err}"),
                    ));
                    break;
                }
            }
        }
        return entries;
    }
}

#[cfg(test)]
thread_local! {
    /// Makes listing a folder fail once ([`listing_fault`]).
    #[allow(
        clippy::missing_const_for_thread_local,
        reason = "already const; clippy flags it on targets without native thread locals (Android, OpenBSD, illumos)"
    )]
    pub(crate) static LISTING_FAULT:
        std::cell::RefCell<Option<(PathBuf, Option<usize>, std::io::ErrorKind)>> =
        const { std::cell::RefCell::new(None) };
}

/// The error the test hook gives for listing `dir`: opening it (`None`)
/// or reading its entry after `read` others.
#[cfg(test)]
fn listing_fault(dir: &Path, read: Option<usize>) -> Option<std::io::Error> {
    LISTING_FAULT.with_borrow_mut(|fault| {
        let (faulty, at, _) = fault.as_ref()?;
        if faulty != dir || *at != read {
            return None;
        }
        // One call fails, not the next, as a signal interrupts one.
        fault.take().map(|(_, _, kind)| kind.into())
    })
}

#[cfg(not(test))]
const fn listing_fault(_: &Path, _: Option<usize>) -> Option<std::io::Error> {
    None
}

/// [`root_element_within`] the default limits.
#[cfg(any(test, fuzzing))]
pub(crate) fn root_element(bytes: &[u8]) -> Option<(String, String)> {
    root_element_within(bytes, &crate::Limits::default())
}

/// The namespace URI (empty when there is none) and local name of the root
/// element of the XML document `bytes` starts, if it gets that far.
///
/// Reads UTF-8, or UTF-16 starting with a byte order mark (`FF FE` for
/// little endian, `FE FF` for big endian), as every XML processor must
/// support (XML 1.0, 4.3.3). Transcodes UTF-16 first ([`utf16_as_utf8`]).
/// A declared encoding must agree with the mark ([`Mark::agrees`]); a
/// conflicting encoding, or UTF-16 declared without a mark, returns no
/// root element. Other encodings are not decoded. Without a mark, such a
/// document is read as UTF-8, which handles only its ASCII text correctly.
///
/// Ignores the whole declaration if it violates XML's grammar
/// ([`declaration_grammar_problem`]). In that case, the mark alone decides
/// the encoding and the document is read as XML 1.0. The same applies to
/// a processing instruction named `xml` in any casing ([`named_xml`]),
/// which is skipped, as is one libxml2 refuses ([`instruction_problem`]),
/// and as a declaration not at the start is. Nothing is reported: root
/// detection has nowhere to report, as it gives only the root's namespace
/// and name, or none.
///
/// The namespace is the declaring attribute's value, normalised using the
/// XML version declared at the start, or 1.0 if there is no declaration or
/// its version cannot be read.
/// References to the five predefined XML entities and character references
/// are replaced (`urn:a&amp;b` becomes `urn:a&b`), matching the value stored
/// by `update-mime-database`.
///
/// No root element is returned if the value fails the same checks as a
/// field's attribute value ([`attribute_value`]): an unresolved or
/// unterminated reference, a character the version does not allow written
/// literally, or `<`. A usable internal entity the document's internal subset
/// declares (internal parameter entities' text included where they are
/// referenced) is replaced in a namespace declaration, so Illustrator's
/// `xmlns="&ns_svg;"` declares the SVG namespace, within the budget `limits`
/// set ([`crate::Limits::entity_expansion_per_byte`]; [`Entities::expand`]).
/// Elsewhere it, or in a document with an external subset or a
/// parameter-entity reference (not `standalone="yes"`) an entity not declared
/// there, is not replaced ([`declared_entities`]): a value that uses one is
/// unknown, not an error, and a namespace declared with an entity not
/// declared there is unknown, so a root named in it returns no root element,
/// while one with only attributes in it keeps its root. A root with an
/// unbound prefix or a name that is not a `QName` also returns no root
/// element (`<1p:doc xmlns:1p="urn:x">`, `<x:y:doc xmlns:x="urn:x">`;
/// Namespaces in XML 1.0, 3rd ed., 5 and 7), and so does a namespace
/// declaration Namespaces in XML forbids ([`forbidden`]), a namespace error
/// or repeated attribute in the root's tag ([`tag_problem`]), any attribute a
/// per-type root's tag would not let its language be read with ([`language`]:
/// one not well formed, or with a value that cannot be read: `<r 1a="x">`,
/// `<r a="&bogus;">`, and in every document `&#0;` or `&1bad;`), or more
/// declarations than `limits` allow
/// ([`crate::Limits::max_namespace_bindings`]). Reserved namespaces are
/// compared after references are replaced.
pub(crate) fn root_element_within(
    bytes: &[u8],
    limits: &crate::Limits,
) -> Option<(String, String)> {
    let transcoded;
    let (mark, bytes) = match bytes {
        [0xFF, 0xFE, rest @ ..] => {
            transcoded = utf16_as_utf8(rest, u16::from_le_bytes)?;
            (Mark::Utf16Le, transcoded.as_bytes())
        }
        [0xFE, 0xFF, rest @ ..] => {
            transcoded = utf16_as_utf8(rest, u16::from_be_bytes)?;
            (Mark::Utf16Be, transcoded.as_bytes())
        }
        [0xEF, 0xBB, 0xBF, ..] => (Mark::Utf8, bytes),
        _ => (Mark::None, bytes),
    };
    let mut reader = quick_xml::Reader::from_reader(bytes);
    let mut version = XmlVersion::Implicit1_0;
    let mut scopes = Scopes::new(limits);
    loop {
        let at_start = reader.buffer_position() == 0;
        let exact = written_doctype(reader.get_ref());
        let event = reader.read_event().ok()?;
        match event {
            Event::DocType(doctype) => {
                scopes.declare_entities(&doctype, exact, version, bytes.len());
            }
            // Only a declaration at the start is one (only a byte order
            // mark, which `quick-xml` does not count, may come before).
            Event::Decl(declaration) if at_start => {
                // One outside XML's grammar is ignored as a whole.
                let Ok(declared) = declared_version(&declaration) else {
                    continue;
                };
                version = declared;
                scopes.declare_standalone(&declaration);
                // Its encoding looked for.
                crate::work::count(declaration.len());
                // `quick-xml` notes the encoding with its `encoding`
                // feature, but reads `&[u8]` as UTF-8 with or without it.
                if let Some(Ok(encoding)) = declaration.encoding()
                    && !mark.agrees(&encoding)
                {
                    return None;
                }
            }
            Event::Start(element) | Event::Empty(element) => {
                scopes.push(&element, version).ok()?;
                let namespace = scopes.resolve(element.name());
                if tag_problem(&element, &namespace, version, &scopes).is_some() {
                    return None;
                }
                let namespace = match namespace {
                    // Asks for its memory first.
                    Resolved::Bound(namespace) => try_copy(namespace.ok()?)?,
                    Resolved::Unbound => String::new(),
                    Resolved::Unknown => return None,
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

/// Returns the XML version from `declaration`: 1.0 or 1.1, with unknown
/// versions treated as 1.0. If the declaration is outside XML's grammar
/// ([`declaration_grammar_problem`]), returns the problem explaining why it is
/// ignored.
fn declared_version(declaration: &BytesDecl<'_>) -> Result<XmlVersion, String> {
    if let Some(problem) = declaration_grammar_problem(declaration) {
        return Err(format!(
            "bad XML (the XML declaration {problem}); it is ignored"
        ));
    }
    // Its version looked for.
    crate::work::count(declaration.len());
    Ok(declaration.xml_version().unwrap_or(XmlVersion::Implicit1_0))
}

/// Applies [`declared_version`] to `event` if it is a declaration at the start
/// of a per-type file. A processing instruction named `xml` in any casing
/// ([`named_xml`]) is treated as a declaration outside XML's grammar.
fn declared_at_start(event: &Event<'_>) -> Option<Result<XmlVersion, String>> {
    let problem = match event {
        Event::Decl(declaration) => return Some(declared_version(declaration)),
        Event::PI(instruction) => match named_xml(instruction)? {
            // Not followed by a `NameChar`, so by no name.
            "xml" => STRAY.to_owned(),
            name => format!("is named `{name}`, not `xml`"),
        },
        _ => return None,
    };
    Some(Err(format!(
        "bad XML (the XML declaration {problem}); it is ignored"
    )))
}

/// Checks `declaration`, starting at its name `xml`, against XML's grammar (XML
/// 1.0, 5th ed., and 1.1, 2.8, `XMLDecl`). Requires `version`, then optional
/// `encoding`, then optional `standalone`, each preceded by white space and
/// holding a valid value in single or double quotes. A version follows XML
/// 1.0's `VersionNum`: `1.` followed by digits. Returns the problem, if any.
fn declaration_grammar_problem(declaration: &str) -> Option<&'static str> {
    const NAMES: [&str; 3] = ["version", "encoding", "standalone"];
    const START: &str = "does not start with `version`";
    let is_blank = |c: char| matches!(c, ' ' | '\t' | '\r' | '\n');
    // Each byte scanned at most twice: found, then checked.
    crate::work::count(1 + 2 * declaration.len());
    let mut rest = declaration.strip_prefix("xml").unwrap_or(declaration);
    // Where in `NAMES` the next attribute may be, at the earliest.
    let mut next = 0;
    loop {
        let after_blank = rest.trim_start_matches(is_blank);
        let blank = after_blank.len() < rest.len();
        rest = after_blank;
        if rest.is_empty() {
            return (next == 0).then_some(START);
        }
        let end = rest.find(|c| c == '=' || is_blank(c)).unwrap_or(rest.len());
        let (name, after) = rest.split_at(end);
        let mut chars = name.chars();
        if !(chars.next().is_some_and(is_name_start) && chars.all(is_name_char)) {
            return Some(STRAY);
        }
        if !blank {
            return Some(NO_BLANK);
        }
        let Some(at) = NAMES.iter().position(|&known| known == name) else {
            return Some(if next == 0 {
                START
            } else {
                "has a name other than `version`, `encoding` and `standalone`"
            });
        };
        if next == 0 && at != 0 {
            return Some(START);
        }
        if at < next {
            return Some("has `version`, `encoding` or `standalone` repeated or out of order");
        }
        next = at + 1;
        let Some(after) = after.trim_start_matches(is_blank).strip_prefix('=') else {
            return Some("has a name not followed by `=`");
        };
        let after = after.trim_start_matches(is_blank);
        let Some(quote) = after.chars().next().filter(|&c| c == '"' || c == '\'') else {
            return Some("has a value that is not quoted");
        };
        let Some((value, after)) = after[1..].split_once(quote) else {
            return Some("has a value that is not closed");
        };
        match at {
            0 if !value.strip_prefix("1.").is_some_and(|digits| {
                !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
            }) =>
            {
                return Some("has a version other than `1.` and digits");
            }
            1 if !is_encoding_name(value) => {
                return Some("has an encoding that is not an encoding name");
            }
            2 if value != "yes" && value != "no" => {
                return Some("has a `standalone` other than `yes` or `no`");
            }
            _ => {}
        }
        rest = after;
    }
}

/// A declaration's problem: no white space before a name.
const NO_BLANK: &str = "has no white space before a name";

/// A declaration's problem: where a name or white space must come, text
/// up to `=` or white space that is not an XML name (`<?xml\u{85}?>`).
const STRAY: &str = "has a character that is not allowed there";

/// The name of `instruction`, a processing instruction's text from its
/// name on, if that is `xml` in any casing (`XML`, `Xml`), which no
/// processing instruction may have (XML 1.0, 5th ed., and 1.1, 2.6,
/// `PITarget`). The name ends before the first character that is not a
/// `NameChar`, so `<?xml-stylesheet?>` is not one, and `<?xml\u{85}?>` is:
/// `quick-xml` reads `<?xml` as a declaration only if ASCII white space
/// or `?>` follows. Such an instruction is a declaration outside XML's
/// grammar.
fn named_xml(instruction: &str) -> Option<&str> {
    let name = instruction.get(..3)?;
    (name.eq_ignore_ascii_case("xml") && !instruction[3..].starts_with(is_name_char))
        .then_some(name)
}

/// What is wrong with `instruction`, a processing instruction's text from its
/// target on, ending at its end or at `?>`, as libxml2 refuses it anywhere:
/// its target is missing or not an XML name (`<??>`, `<? x?>`, `<?1a?>`), or
/// is followed by something other than its end or white space
/// (`<?foo"bar"?>`, `<?foo?bar?>`). White space is ASCII only: U+0085 and
/// U+2028 written there are not white space, even in XML 1.1, as inside a tag
/// (a known limit; libxml2, which reads XML 1.1 as 1.0, refuses them too). In
/// XML 1.1 parameter-entity text, though, one written in the entity's literal
/// is a line end already normalised to a line feed ([`replaced`]), so
/// `<!ENTITY % p '<?foo\u{85}bar?>'>%p;` is read as `<?foo\nbar?>`. One named
/// `xml` ([`named_xml`]) is not looked at here.
fn instruction_problem(instruction: &str) -> Option<&'static str> {
    if !instruction.starts_with(is_name_start) {
        crate::work::count(1);
        return Some("is missing or not an XML name");
    }
    let end = instruction
        .find(|c: char| !is_name_char(c))
        .unwrap_or(instruction.len());
    crate::work::count(1 + end);
    let rest = &instruction[end..];
    let ended = rest.is_empty() || rest.starts_with("?>");
    (!ended && !rest.starts_with([' ', '\t', '\r', '\n']))
        .then_some("is not followed by white space")
}

/// Whether `name` is an `EncName` (XML 1.0, 5th ed., and 1.1, 4.3.3): an
/// ASCII letter, then ASCII letters, digits, `.`, `_` and `-`.
fn is_encoding_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphabetic())
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The byte order mark an XML document starts with, if any.
#[derive(Clone, Copy)]
enum Mark {
    None,
    Utf8,
    Utf16Le,
    Utf16Be,
}

impl Mark {
    /// Whether the declared `encoding` agrees with the byte order mark,
    /// ignoring ASCII case. With a mark, accepts its encoding (for UTF-16, the
    /// matching byte-order name or plain UTF-16). Without a mark, accepts any
    /// encoding except UTF-16; other names are read as UTF-8.
    fn agrees(self, encoding: &str) -> bool {
        let is = |name: &str| encoding.eq_ignore_ascii_case(name);
        match self {
            Self::None => !(is("UTF-16") || is("UTF-16LE") || is("UTF-16BE")),
            Self::Utf8 => is("UTF-8"),
            Self::Utf16Le => is("UTF-16") || is("UTF-16LE"),
            Self::Utf16Be => is("UTF-16") || is("UTF-16BE"),
        }
    }
}

/// Converts `bytes` from UTF-16 after its byte order mark to UTF-8, reading
/// units with `unit`. The bytes may be only a prefix of the document. Omits an
/// incomplete final character (an odd last byte or a trailing high surrogate),
/// as the rest of the document is omitted. An unpaired surrogate earlier in the
/// input is invalid UTF-16, a fatal XML error (XML 1.0, 4.3.3), so returns
/// `None`.
fn utf16_as_utf8(bytes: &[u8], unit: fn([u8; 2]) -> u16) -> Option<String> {
    // Each byte read.
    crate::work::count(bytes.len());
    let units = bytes.as_chunks::<2>().0.iter().map(|&pair| unit(pair));
    let mut text = String::with_capacity(bytes.len());
    let mut chars = char::decode_utf16(units).peekable();
    while let Some(decoded) = chars.next() {
        match decoded {
            Ok(c) => text.push(c),
            // Cut off: a high surrogate, last.
            Err(err)
                if chars.peek().is_none()
                    && (0xD800..0xDC00).contains(&err.unpaired_surrogate()) => {}
            Err(_) => return None,
        }
    }
    // Each byte written.
    crate::work::count(text.len());
    Some(text)
}

impl MimeDatabase {
    /// A human-readable description of `mime` ("PNG image") in the first of
    /// `locales` that has one, otherwise untranslated. Reads the type's XML
    /// files on first use and caches the result in this snapshot. If a folder
    /// listing failed while looking for filenames, the next call retries
    /// instead. An empty `<comment/>` supplies an empty description, replacing
    /// a less important folder's value.
    ///
    /// Tries the spellings each folder registers for the type in its `types`
    /// file, in file order. Without a `types` file, uses filenames in byte
    /// order from media folders matching the type's media part, ignoring ASCII
    /// case. Falls back to the lowercase filename used by
    /// `update-mime-database`. The spelling of `mime` does not affect the
    /// result.
    ///
    /// A known limit: in an XML 1.1 file, U+0085 (NEL) and U+2028 (LS)
    /// inside a tag are not read as white space, so such a tag is misread,
    /// with a diagnostic. A name that takes one in is not an XML name, and
    /// nothing after one in a start tag can be read: the element is skipped,
    /// the field left out, or, at the root, the file ignored or its language
    /// unknown. An end tag that then does not match its start tag skips the
    /// rest of the file. The same happens in XML 1.0, where such a tag is
    /// not well-formed.
    #[must_use]
    pub fn description(&self, mime: &MimeType, locales: &Locales) -> Option<String> {
        self.type_info(mime).comment.pick(locales)
    }

    /// The type's acronym ("PNG"), from its XML files like
    /// [`MimeDatabase::description`]. Not translated: spec 0.22 dropped
    /// translated acronyms, and any an older database has are ignored.
    /// One without its own `xml:lang` takes the root's, so under a root
    /// `xml:lang` it is translated and ignored too. Has the same XML 1.1
    /// limit as [`MimeDatabase::description`].
    #[must_use]
    pub fn acronym(&self, mime: &MimeType) -> Option<String> {
        self.type_info(mime).acronym.clone()
    }

    /// The expanded acronym ("Portable Network Graphics"); not translated,
    /// and with the same XML 1.1 limit, as [`MimeDatabase::acronym`].
    #[must_use]
    pub fn expanded_acronym(&self, mime: &MimeType) -> Option<String> {
        self.type_info(mime).expanded_acronym.clone()
    }

    /// Problems met reading `mime`'s XML files (reading them if not done
    /// yet), including a folder that could not be listed to find them, and
    /// a path that could not be checked (an error other than its absence).
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

    /// [`TypeInfo::read`] of `mime` from `dirs`, which register no
    /// spelling for it.
    fn read_in<'a>(mime: &MimeType, dirs: impl Iterator<Item = &'a Path>) -> TypeInfo {
        TypeInfo::read(
            dirs.map(|dir| type_file_paths(dir, &[], mime)),
            Vec::new(),
            &crate::Limits::default(),
        )
    }

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
        let info = read_in(&png, [low.path(), high.path()].into_iter());
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
        let info = read_in(&plain, [dir.path()].into_iter());
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
            let info = read_in(&t, [low.path(), high.path()].into_iter());
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
        let info = read_in(&t, [high.path()].into_iter());
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
        // (which is reported) or with another 1.x version.
        for v10 in [
            "",
            "<?xml version=\"1.0\"?>",
            "\u{feff}",
            "\u{feff}<?xml version=\"1.0\"?>",
            "<?xml version=\"1.2\"?>",
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
        // It is not read as untranslated, so the untranslated value stays.
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

    /// `text` in UTF-16, little endian or not, after a byte order mark.
    fn utf16(text: &str, little: bool) -> Vec<u8> {
        let mut bytes = if little {
            vec![0xFF, 0xFE]
        } else {
            vec![0xFE, 0xFF]
        };
        for unit in text.encode_utf16() {
            if little {
                bytes.extend_from_slice(&unit.to_le_bytes());
            } else {
                bytes.extend_from_slice(&unit.to_be_bytes());
            }
        }
        bytes
    }

    #[test]
    fn a_utf16_root_element_is_read() {
        let doc = ("urn:test".to_owned(), "doc".to_owned());
        for little in [true, false] {
            for text in [
                r#"<doc xmlns="urn:test"/>"#,
                r#"<?xml version="1.0" encoding="UTF-16"?><doc xmlns="urn:test"/>"#,
                "<?xml version=\"1.1\"?><doc xmlns=\"urn:\u{e9}&#x85;test\"/>",
            ] {
                let bytes = utf16(text, little);
                let expected = if text.contains("1.1") {
                    ("urn:\u{e9}\u{85}test".to_owned(), "doc".to_owned())
                } else {
                    doc.clone()
                };
                assert_eq!(root_element(&bytes), Some(expected.clone()), "{text}");
                // A prefix cut in a character: what is whole is read.
                let mut cut = utf16(&format!("{text}<!--\u{1F600}"), little);
                cut.pop();
                assert_eq!(root_element(&cut), Some(expected.clone()), "{text}");
                cut.truncate(cut.len() - 1);
                assert_eq!(root_element(&cut), Some(expected.clone()), "{text}");
                cut.truncate(cut.len() - 1);
                assert_eq!(root_element(&cut), Some(expected), "{text}");
            }
            // A surrogate unpaired inside: not UTF-16, so no document.
            let mut bad = utf16("<!--x--><doc xmlns=\"urn:test\"/>", little);
            let at = 6;
            bad[at..at + 2].copy_from_slice(&if little {
                0xD800u16.to_le_bytes()
            } else {
                0xD800u16.to_be_bytes()
            });
            assert_eq!(root_element(&bad), None);
        }
    }

    #[test]
    fn a_declared_encoding_must_agree_with_the_byte_order_mark() {
        let doc = Some(("urn:test".to_owned(), "doc".to_owned()));
        let text = |encoding: &str| {
            format!(r#"<?xml version="1.0" encoding="{encoding}"?><doc xmlns="urn:test"/>"#)
        };
        let utf8 = |text: &str, marked: bool| {
            let mut bytes = if marked {
                b"\xEF\xBB\xBF".to_vec()
            } else {
                Vec::new()
            };
            bytes.extend_from_slice(text.as_bytes());
            bytes
        };
        for little in [true, false] {
            let (own, other) = if little {
                ("utf-16le", "UTF-16BE")
            } else {
                ("UTF-16be", "utf-16LE")
            };
            for (encoding, agrees) in [
                ("UTF-16", true),
                ("utf-16", true),
                (own, true),
                (other, false),
                ("UTF-8", false),
                ("ISO-8859-1", false),
            ] {
                let found = root_element(&utf16(&text(encoding), little));
                assert_eq!(found, agrees.then(|| doc.clone().unwrap()), "{encoding}");
            }
        }
        for (encoding, marked, agrees) in [
            ("UTF-8", true, true),
            ("utf-8", true, true),
            ("UTF-16", true, false),
            ("ISO-8859-1", true, false),
            // No mark: UTF-16 named is wrong; any other name is read as
            // UTF-8.
            ("UTF-16", false, false),
            ("utf-16", false, false),
            ("UTF-16LE", false, false),
            ("Utf-16Be", false, false),
            ("UTF-8", false, true),
            ("ISO-8859-1", false, true),
            ("US-ASCII", false, true),
            ("UTF-16X", false, true),
        ] {
            let found = root_element(&utf8(&text(encoding), marked));
            assert_eq!(
                found,
                agrees.then(|| doc.clone().unwrap()),
                "{encoding} {marked}"
            );
        }
    }

    /// Declarations outside XML's grammar, each with what is wrong, after
    /// "the XML declaration "; `{e}` stands for an encoding name.
    const MALFORMED: [(&str, &str); 39] = [
        (r#"encoding="{e}""#, START),
        (r#"encoding="{e}" version="1.1""#, START),
        (r#"Version="1.1" encoding="{e}""#, START),
        (r#"junk version="1.1" encoding="{e}""#, START),
        (r#"version="1.1" version="1.1" encoding="{e}""#, ORDER),
        (r#"version="1.1" encoding="{e}" encoding="{e}""#, ORDER),
        (r#"version="1.1" standalone="yes" encoding="{e}""#, ORDER),
        (
            r#"version="1.1" encoding="{e}" standalone="no" standalone="no""#,
            ORDER,
        ),
        (r#"version="1.1" encoding="{e}" version="1.1""#, ORDER),
        (r#"version="1.1" foo="x" encoding="{e}""#, OTHER),
        (r#"version="1.1" encoding="{e}" foo="x""#, OTHER),
        (r#"version="1.1" encoding="{e}" junk"#, OTHER),
        (r#"version="1.1" a=b encoding="{e}""#, OTHER),
        (r#"version="1.1" encoding="{e}" a="#, OTHER),
        (r#"version="1.1"encoding="{e}""#, BLANK),
        (r#"version="1.1" encoding="{e}"standalone="no""#, BLANK),
        // XML 1.1's line ends are not white space in a declaration.
        ("version=\"1.1\"\u{85}encoding=\"{e}\"", STRAY),
        ("version=\"1.1\" encoding=\"{e}\"\u{85}", STRAY),
        ("version=\"1.1\"\u{2028}encoding=\"{e}\"", STRAY),
        ("version=\"1.1\" encoding=\"{e}\"\u{2028}", STRAY),
        ("version=\"1.1\" \u{85}", STRAY),
        ("version=\"1.1\" \u{2028}encoding=\"{e}\"", STRAY),
        ("\u{85}version=\"1.1\" encoding=\"{e}\"", STRAY),
        (r#"version="1.1" ="x" encoding="{e}""#, STRAY),
        (r#"version "1.1" encoding="{e}""#, EQ),
        (r#"version="1.1" encoding "{e}""#, EQ),
        (r#"version=1.1 encoding="{e}""#, QUOTE),
        (r#"version='1.1" encoding="{e}""#, CLOSE),
        (r#"version="1.x" encoding="{e}""#, VERSION),
        (r#"version="2.0" encoding="{e}""#, VERSION),
        (r#"version="1." encoding="{e}""#, VERSION),
        (r#"version="" encoding="{e}""#, VERSION),
        (
            r#"version="1.1" encoding="{e}" standalone="maybe""#,
            STANDALONE,
        ),
        (
            r#"version="1.1" encoding="{e}" standalone="Yes""#,
            STANDALONE,
        ),
        (r#"version="1.1" encoding="{e}" standalone="""#, STANDALONE),
        (r#"version="1.1" encoding="8bit""#, ENCODING),
        (r#"version="1.1" encoding="""#, ENCODING),
        (r#"version="1.1" encoding="UTF 8""#, ENCODING),
        (r#"version="1.1" encoding="-utf8""#, ENCODING),
    ];
    const START: &str = "does not start with `version`";
    const ORDER: &str = "has `version`, `encoding` or `standalone` repeated or out of order";
    const OTHER: &str = "has a name other than `version`, `encoding` and `standalone`";
    const BLANK: &str = "has no white space before a name";
    const STRAY: &str = "has a character that is not allowed there";
    const EQ: &str = "has a name not followed by `=`";
    const QUOTE: &str = "has a value that is not quoted";
    const CLOSE: &str = "has a value that is not closed";
    const VERSION: &str = "has a version other than `1.` and digits";
    const ENCODING: &str = "has an encoding that is not an encoding name";
    const STANDALONE: &str = "has a `standalone` other than `yes` or `no`";

    /// Declarations in XML's grammar, read as XML 1.1; `{e}` stands for an
    /// encoding name.
    const WELL_FORMED: [&str; 6] = [
        r#"version="1.1""#,
        r#"version = "1.1" encoding = "{e}" standalone = "yes""#,
        "version=\"1.1\"\t\r\n",
        "\t\r\nversion=\"1.1\"",
        r#"version="1.1" encoding="{e}""#,
        r#"version="1.1" standalone="no""#,
    ];

    /// `attributes` with `"` and `'` swapped.
    fn swap_quotes(attributes: &str) -> String {
        attributes
            .chars()
            .map(|c| match c {
                '"' => '\'',
                '\'' => '"',
                c => c,
            })
            .collect()
    }

    #[test]
    fn a_declaration_outside_xmls_grammar_is_ignored_by_root_detection() {
        let doc = Some(("urn:test".to_owned(), "doc".to_owned()));
        // Each mark, and an encoding that contradicts it.
        let marks: [(&[u8], &str, bool); 4] = [
            (b"", "UTF-16", false),
            (b"\xEF\xBB\xBF", "UTF-16", false),
            (b"\xFF\xFE", "UTF-16BE", true),
            (b"\xFE\xFF", "UTF-16LE", false),
        ];
        let bytes = |mark: &[u8], little: bool, text: &str| {
            if mark.len() == 2 {
                utf16(text, little)
            } else {
                [mark, text.as_bytes()].concat()
            }
        };
        for (mark, other, little) in marks {
            let text = |attributes: &str, root: &str| format!("<?xml {attributes}?>{root}");
            // An encoding that agrees with the mark.
            let own = match mark.len() {
                0 => "a-b.c_D9",
                3 => "UTF-8",
                _ => "UTF-16",
            };
            for (attributes, _) in MALFORMED {
                for attributes in [attributes.to_owned(), swap_quotes(attributes)] {
                    if attributes.contains("{e}") {
                        // The encoding is not checked: the mark alone
                        // decides.
                        let text = text(
                            &attributes.replace("{e}", other),
                            r#"<doc xmlns="urn:test"/>"#,
                        );
                        let found = root_element(&bytes(mark, little, &text));
                        assert_eq!(found, doc, "{mark:?} {text}");
                    }
                    // Nor the version: the document is XML 1.0.
                    let text = text(&attributes.replace("{e}", own), r#"<r xmlns="urn:&#1;"/>"#);
                    let found = root_element(&bytes(mark, little, &text));
                    assert_eq!(found, None, "{mark:?} {text}");
                }
            }
            for attributes in WELL_FORMED {
                for attributes in [attributes.to_owned(), swap_quotes(attributes)] {
                    let text = text(&attributes.replace("{e}", own), r#"<r xmlns="urn:&#1;"/>"#);
                    let found = root_element(&bytes(mark, little, &text));
                    assert_eq!(
                        found,
                        Some(("urn:\u{1}".into(), "r".into())),
                        "{mark:?} {text}"
                    );
                }
            }
            // An unknown version in the grammar is read as 1.0, and the
            // declaration's encoding is checked.
            let text =
                format!(r#"<?xml version="1.2" encoding="{other}"?><doc xmlns="urn:test"/>"#);
            assert_eq!(root_element(&bytes(mark, little, &text)), None, "{text}");
            let text = r#"<?xml version="1.2"?><r xmlns="urn:&#1;"/>"#;
            assert_eq!(root_element(&bytes(mark, little, text)), None, "{text}");
        }
    }

    #[test]
    fn a_declaration_outside_xmls_grammar_is_reported_and_ignored() {
        let field = "<comment>D&#1;E</comment><acronym>A</acronym></mime-type>";
        for bom in ["", "\u{feff}"] {
            for (attributes, problem) in MALFORMED {
                for attributes in [attributes.to_owned(), swap_quotes(attributes)] {
                    let text = format!(
                        "{bom}<?xml {}?>{OPEN}{field}",
                        attributes.replace("{e}", "UTF-8")
                    );
                    let info = parse(&text);
                    // Ignored, so read as XML 1.0: the reference cannot be
                    // replaced.
                    assert_eq!(
                        info.diagnostics.first(),
                        Some(&Diagnostic::new(
                            "t.xml",
                            Some(crate::Location::Offset(bom.len())),
                            format!("bad XML (the XML declaration {problem}); it is ignored")
                        )),
                        "{text:?}"
                    );
                    assert_eq!(info.diagnostics.len(), 2, "{text:?}");
                    assert_eq!(comments(&info), [], "{text:?}");
                    assert_eq!(info.acronym.as_deref(), Some("A"), "{text:?}");
                }
            }
            for attributes in WELL_FORMED {
                for attributes in [attributes.to_owned(), swap_quotes(attributes)] {
                    let text = format!(
                        "{bom}<?xml {}?>{OPEN}{field}",
                        attributes.replace("{e}", "a-b.c_D9")
                    );
                    let info = parse(&text);
                    assert!(
                        info.diagnostics.is_empty(),
                        "{text:?}: {:?}",
                        info.diagnostics
                    );
                    assert_eq!(comments(&info), [(None, "D\u{1}E")], "{text:?}");
                }
            }
            // An unknown version in the grammar is read as 1.0, silently.
            let text = format!("{bom}<?xml version=\"1.2\"?>{OPEN}{field}");
            let info = parse(&text);
            assert_eq!(info.diagnostics.len(), 1, "{:?}", info.diagnostics);
            assert!(
                info.diagnostics[0].message.contains("`&#1;`"),
                "{:?}",
                info.diagnostics
            );
        }
    }

    /// Processing instructions named `xml` in some casing, which are not
    /// the declaration, each with what is wrong, after "the XML
    /// declaration ". `quick-xml` reads `<?xml` followed by a character
    /// other than ASCII white space as a processing instruction.
    const NAMED_XML: [(&str, &str); 7] = [
        (r#"<?XML version="1.1"?>"#, "is named `XML`, not `xml`"),
        ("<?Xml?>", "is named `Xml`, not `xml`"),
        (
            r#"<?xmL version="1.1" encoding="UTF-8"?>"#,
            "is named `xmL`, not `xml`",
        ),
        ("<?XML\u{85}version=\"1.1\"?>", "is named `XML`, not `xml`"),
        ("<?xml\u{85}version=\"1.1\"?>", STRAY),
        ("<?xml\u{2028}version=\"1.1\"?>", STRAY),
        ("<?xml\"?>", STRAY),
    ];

    /// Processing instructions whose name only starts with `xml`.
    const NOT_NAMED_XML: [&str; 4] = [
        r#"<?xml-stylesheet href="a"?>"#,
        "<?XML-stylesheet?>",
        r#"<?xmlns version="1.1"?>"#,
        "<?xml\u{b7}?>",
    ];

    #[test]
    fn a_processing_instruction_named_xml_is_reported_and_ignored() {
        let field = "<comment>D&#1;E</comment><acronym>A</acronym></mime-type>";
        for bom in ["", "\u{feff}"] {
            for (instruction, problem) in NAMED_XML {
                let text = format!("{bom}{instruction}{OPEN}{field}");
                let info = parse(&text);
                // Ignored, so read as XML 1.0: the reference cannot be
                // replaced.
                assert_eq!(
                    info.diagnostics.first(),
                    Some(&Diagnostic::new(
                        "t.xml",
                        Some(crate::Location::Offset(bom.len())),
                        format!("bad XML (the XML declaration {problem}); it is ignored")
                    )),
                    "{text:?}"
                );
                assert_eq!(info.diagnostics.len(), 2, "{text:?}");
                assert_eq!(comments(&info), [], "{text:?}");
                assert_eq!(info.acronym.as_deref(), Some("A"), "{text:?}");
            }
            for instruction in NOT_NAMED_XML {
                let text = format!("{bom}{instruction}{OPEN}{field}");
                let info = parse(&text);
                assert_eq!(info.diagnostics.len(), 1, "{text:?}");
                assert!(
                    info.diagnostics[0].message.contains("`&#1;`"),
                    "{text:?}: {:?}",
                    info.diagnostics
                );
            }
        }
        // Not at the start, one is reported as a late declaration is.
        let late = "bad XML (an XML declaration not at the start of the file)";
        let late_after =
            "bad XML (an XML declaration not at the start of the file); the rest is skipped";
        let root = format!("{OPEN}<comment>a</comment></mime-type>");
        let mut cases = Vec::new();
        for (instruction, _) in NAMED_XML {
            cases.extend([
                (format!("<!-- c -->{instruction}{root}"), vec![(10, late)]),
                (
                    format!("<?xml version=\"1.0\"?>{instruction}{root}"),
                    vec![(21, late)],
                ),
                (
                    format!("{OPEN}{instruction}<comment>a</comment></mime-type>"),
                    vec![(OPEN.len(), late)],
                ),
                (
                    format!("{root}{instruction}<!-- c -->"),
                    vec![(root.len(), late_after)],
                ),
            ]);
        }
        for instruction in NOT_NAMED_XML {
            cases.extend([
                (format!("<!-- c -->{instruction}{root}"), vec![]),
                (
                    format!("{OPEN}{instruction}<comment>a</comment></mime-type>"),
                    vec![],
                ),
                (format!("{root}{instruction}<!-- c -->"), vec![]),
            ]);
        }
        check_misplaced(&cases);
    }

    #[test]
    fn a_processing_instruction_named_xml_is_ignored_by_root_detection() {
        let doc = Some(("urn:test".to_owned(), "doc".to_owned()));
        for (instruction, _) in NAMED_XML {
            let text = format!(r#"{instruction}<doc xmlns="urn:test"/>"#);
            assert_eq!(root_element(text.as_bytes()), doc, "{text:?}");
            // The document is XML 1.0.
            let text = format!(r#"{instruction}<r xmlns="urn:&#1;"/>"#);
            assert_eq!(root_element(text.as_bytes()), None, "{text:?}");
        }
    }

    #[test]
    fn a_declaration_is_checked_in_linear_time() {
        // For a declaration of D bytes (from `xml` to before `?>`), the
        // grammar check takes 1 + 2D steps, then looking for the version
        // D. Both readers look for `standalone` too (D more), and root
        // detection for the encoding (D more): 1 + 5D in the grammar, 1 +
        // 2D outside it (ignored); the per-type reader 1 + 4D and 1 + 2D.
        // Each bound is twice that, with the file's bytes for D.
        for (lead, in_grammar, root_cost, type_cost) in [("", true, 5, 4), ("8", false, 2, 2)] {
            let declaration = |n: usize| {
                let encoding = format!("{lead}a{}", "-".repeat(n));
                format!(r#"<?xml version="1.0" encoding="{encoding}"?>"#)
            };
            crate::work::assert_linear(
                10_000,
                |n| {
                    let text = format!(r#"{}<doc xmlns="u"/>"#, declaration(n));
                    let (found, steps) = crate::work::steps(|| root_element(text.as_bytes()));
                    assert!(found.is_some());
                    (steps, text.len())
                },
                |_, bytes| 2 * (root_cost * bytes + 1),
            );
            crate::work::assert_linear(
                10_000,
                |n| {
                    let text = format!("{}{OPEN}</mime-type>", declaration(n));
                    let (info, steps) = crate::work::steps(|| parse(&text));
                    assert_eq!(info.diagnostics.is_empty(), in_grammar);
                    (steps, text.len())
                },
                |_, bytes| 2 * (type_cost * bytes + 1),
            );
        }
    }

    #[test]
    fn a_utf16_root_element_is_transcoded_in_linear_time() {
        crate::work::assert_linear(
            10_000,
            |n| {
                let text = format!("<!--{}--><doc xmlns=\"urn:test\"/>", "\u{e9}".repeat(n));
                let bytes = utf16(&text, true);
                let (found, steps) = crate::work::steps(|| root_element(&bytes));
                assert!(found.is_some());
                // Each byte read and written.
                assert!(steps >= bytes.len(), "{steps} steps, {} bytes", bytes.len());
                (steps, bytes.len())
            },
            // Twice the analysed cost: per byte read one, and per byte
            // written at most one.
            |_, bytes| 2 * (2 * bytes + 100),
        );
    }

    /// Magic says XML for UTF-8, with or without a byte order mark, and for
    /// UTF-16 of either order; the root element refines it alike.
    #[test]
    fn utf16_xml_is_refined_by_its_root_element() {
        let db = MimeDatabase::from_files(&[vec![
            (
                "magic",
                b"MIME-Magic\0\n[50:application/xml]\n>0=\x00\x05<?xml\n>0=\x00\x04<doc\n\
                  >0=\x00\x04\xff\xfe<\x00\n>0=\x00\x04\xfe\xff\x00<\n>0=\x00\x04\xef\xbb\xbf<\n"
                    .as_slice(),
            ),
            ("subclasses", b"application/x-test+xml application/xml\n"),
            ("XMLnamespaces", b"urn:test doc application/x-test+xml\n"),
        ]]);
        let text = r#"<doc xmlns="urn:test"/>"#;
        let mut bom8 = b"\xEF\xBB\xBF".to_vec();
        bom8.extend_from_slice(text.as_bytes());
        for data in [
            text.as_bytes().to_vec(),
            bom8,
            utf16(text, true),
            utf16(text, false),
        ] {
            let found = db.type_for_data(&data);
            assert_eq!(found.mime.as_str(), "application/x-test+xml", "{data:?}");
            assert_eq!(found.decided_by, crate::DecidedBy::XmlRoot);
        }
    }

    /// How a problem with the root's tag other than its `xml:lang` starts,
    /// after "the root element's ".
    const TAG: &str = "tag cannot be read, so its language is unknown: ";

    /// The root's start tag with `attributes` after the namespace.
    fn root(attributes: &str) -> String {
        format!(r#"<mime-type xmlns="{NAMESPACE}" {attributes}>"#)
    }

    #[test]
    fn a_field_without_a_language_takes_the_roots() {
        let info = parse(&format!(
            "{}<comment>Deutsch</comment><acronym>DE</acronym></mime-type>",
            root(r#"xml:lang="de""#)
        ));
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(comments(&info), [(Some("de"), "Deutsch")]);
        assert_eq!(info.comment.pick(&Locales::untranslated()), None);
        assert_eq!(
            info.comment.pick(&Locales::new(["de"])).as_deref(),
            Some("Deutsch")
        );
        // A translated acronym is left out.
        assert_eq!(info.acronym, None);
        // Its own language, an empty one too, comes first.
        let info = parse(&format!(
            r#"{}<comment xml:lang="">English</comment><comment xml:lang="fr">F</comment><acronym xml:lang="">A</acronym></mime-type>"#,
            root(r#"xml:lang="de""#)
        ));
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(comments(&info), [(None, "English"), (Some("fr"), "F")]);
        assert_eq!(info.acronym.as_deref(), Some("A"));
        // An empty one at the root is untranslated.
        let info = parse(&format!(
            "{}<comment>English</comment></mime-type>",
            root(r#"xml:lang="""#)
        ));
        assert_eq!(comments(&info), [(None, "English")]);
        // Normalised by the version.
        for (declaration, lang) in [("", "de\u{85}"), ("<?xml version=\"1.1\"?>", "de ")] {
            let info = parse(&format!(
                "{declaration}{}<comment>D</comment></mime-type>",
                root("xml:lang=\"de\u{85}\"")
            ));
            assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
            assert_eq!(comments(&info), [(Some(lang), "D")], "{declaration}");
        }
    }

    #[test]
    fn a_field_whose_inherited_language_cannot_be_read_is_dropped() {
        let v11 = "<?xml version=\"1.1\"?>";
        for (declaration, attributes, problem) in [
            (
                "",
                r#"xml:lang="&e;""#,
                "`xml:lang` cannot be read: the reference `&e;` cannot be replaced".to_owned(),
            ),
            (
                "",
                r#"xml:lang="d&#1;e""#,
                "`xml:lang` cannot be read: the reference `&#1;` cannot be replaced".to_owned(),
            ),
            (
                v11,
                "xml:lang=\"d\u{1}e\"",
                "`xml:lang` cannot be read: a character XML does not allow, U+0001".to_owned(),
            ),
            (
                "",
                r#"xml:lang="de" xml:lang="fr""#,
                format!("`xml:lang` cannot be read: {}", twice("xml:lang")),
            ),
            // `quick-xml` finds these at the value or past `=`.
            (
                "",
                "xml:lang=de",
                format!("`xml:lang` cannot be read: {UNREADABLE}a value is not quoted"),
            ),
            (
                "",
                "xml:lang = ",
                format!("`xml:lang` cannot be read: {UNREADABLE}`=` is not followed by a value"),
            ),
            (
                "",
                r#"type="a/b" x xml:lang="de""#,
                format!("{TAG}{UNREADABLE}a name is not followed by `=`"),
            ),
            (
                "",
                r#"type=a xml:lang="de""#,
                format!("{TAG}{UNREADABLE}a value is not quoted"),
            ),
            (
                "",
                r#"type="a/b" x="#,
                format!("{TAG}{UNREADABLE}`=` is not followed by a value"),
            ),
        ] {
            let head = format!(
                r#"{declaration}{}<comment xml:lang="fr">F</comment>"#,
                root(attributes)
            );
            let text = format!(
                r#"{head}<comment>D</comment><comment xml:lang="">E</comment></mime-type>"#
            );
            let info = parse(&text);
            assert_eq!(
                info.diagnostics,
                [Diagnostic::new(
                    "t.xml",
                    Some(crate::Location::Offset(head.len())),
                    format!("bad XML (the root element's {problem}); the value is left out")
                )],
                "{text:?}"
            );
            // Only the field that takes it is dropped.
            assert_eq!(
                comments(&info),
                [(None, "E"), (Some("fr"), "F")],
                "{text:?}"
            );
        }
        // XML 1.1 allows a reference to a control.
        let info = parse(&format!(
            "{v11}{}<comment>D</comment></mime-type>",
            root(r#"xml:lang="d&#1;e""#)
        ));
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(comments(&info), [(Some("d\u{1}e"), "D")]);
    }

    #[test]
    fn any_attribute_error_in_the_root_tag_makes_its_language_unreadable() {
        let v11 = "<?xml version=\"1.1\"?>";
        for (declaration, attributes, problem) in [
            (
                "",
                r#"type="&e;""#,
                format!("{TAG}the reference `&e;` cannot be replaced"),
            ),
            ("", r#"1x="a""#, format!("{TAG}{}", not_a_name())),
            ("", r#"x:y:z="a""#, format!("{TAG}{}", not_a_name())),
            (
                "",
                r#"type="a<b""#,
                format!("{TAG}{UNREADABLE}`<` in a value"),
            ),
            (
                "",
                r#"xmlns:p="""#,
                format!("{TAG}{UNREADABLE}a prefix's namespace declaration is empty in XML 1.0"),
            ),
            (
                "<?xml version=\"1.0\"?>",
                r#"type="a" xmlns:p="" xml:lang="de""#,
                format!("{TAG}{UNREADABLE}a prefix's namespace declaration is empty in XML 1.0"),
            ),
        ] {
            let head = format!(
                r#"{declaration}{}<comment xml:lang="fr">F</comment>"#,
                root(attributes)
            );
            let text = format!(
                r#"{head}<comment>D</comment><comment xml:lang="">E</comment></mime-type>"#
            );
            let info = parse(&text);
            assert_eq!(
                info.diagnostics,
                [Diagnostic::new(
                    "t.xml",
                    Some(crate::Location::Offset(head.len())),
                    format!("bad XML (the root element's {problem}); the value is left out")
                )],
                "{text:?}"
            );
            // Fields with their own language are kept.
            assert_eq!(
                comments(&info),
                [(None, "E"), (Some("fr"), "F")],
                "{text:?}"
            );
        }
        // A clean root, with no `xml:lang`, keeps every field; so does
        // `xmlns:p=""` in XML 1.1.
        for (declaration, attributes) in [
            ("", r#"type="text/plain""#),
            (v11, r#"type="text/plain" xmlns:p="""#),
        ] {
            let info = parse(&format!(
                r#"{declaration}{}<comment>D</comment><comment xml:lang="fr">F</comment></mime-type>"#,
                root(attributes)
            ));
            assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
            assert_eq!(comments(&info), [(None, "D"), (Some("fr"), "F")]);
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
        assert!(info.comment.values.is_empty(), "{:?}", info.comment);
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
        // Other elements are reported and skipped.
        let text = format!(
            "{OPEN}<generic-icon name=\"a\" name=\"b\"/><x:comment xmlns:x=\"urn:x\" a=\"1\" a=\"2\">X</x:comment><comment>a</comment></mime-type>"
        );
        let info = parse(&text);
        let skipped = |name| format!("bad XML ({}); the element is skipped", twice(name));
        assert_eq!(
            info.diagnostics,
            [
                Diagnostic::new(
                    "t.xml",
                    text.find("<generic").map(crate::Location::Offset),
                    skipped("name")
                ),
                Diagnostic::new(
                    "t.xml",
                    text.find("<x:comment").map(crate::Location::Offset),
                    skipped("a")
                ),
            ]
        );
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
        assert!(info.comment.values.is_empty(), "{:?}", info.comment);
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
        let info = read_in(
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
            let alone = read_in(&t, [high.path()].into_iter());
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
            let both = read_in(&t, [low.path(), high.path()].into_iter());
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
        // Namespaces in XML 1.0, 3rd ed., 5 and 7: `<1p:mime-type>`,
        // `<:mime-type xmlns=…>` and `<x:y:mime-type>` are not per-type
        // files, whatever namespace their prefix is bound to.
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
        let info = read_in(&plain, [dir.path()].into_iter());
        assert_eq!(info.diagnostics.len(), 1);
        let mut diagnostics = Vec::new();
        assert_eq!(types_in(dir.path(), &mut diagnostics), [plain]);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
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
        let mut diagnostics = Vec::new();
        let mut types = types_in(dir.path(), &mut diagnostics);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        types.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(
            types,
            [
                MimeType::new("text/linked").unwrap(),
                MimeType::new("text/real").unwrap()
            ]
        );
    }

    #[test]
    fn an_interrupted_listing_is_listed_again() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["text/a.xml", "text/b.xml", "text/c.xml", "image/d.xml"] {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let all: Vec<MimeType> = ["image/d", "text/a", "text/b", "text/c"]
            .into_iter()
            .map(|name| MimeType::new(name).unwrap())
            .collect();
        // Opening, then mid-listing, of the top and of a media folder.
        for (folder, at) in [
            (dir.path().to_owned(), None),
            (dir.path().to_owned(), Some(1)),
            (dir.path().join("text"), None),
            (dir.path().join("text"), Some(1)),
        ] {
            let fault = (folder, at, std::io::ErrorKind::Interrupted);
            LISTING_FAULT.set(Some(fault.clone()));
            let mut diagnostics = Vec::new();
            let mut types = types_in(dir.path(), &mut diagnostics);
            types.sort();
            assert!(LISTING_FAULT.with_borrow(Option::is_none), "{fault:?}");
            assert_eq!(types, all, "{fault:?}");
            assert!(diagnostics.is_empty(), "{fault:?}: {diagnostics:?}");
        }
    }

    /// The first entry of `dir` as listed, by name.
    fn first_listed(dir: &Path) -> std::ffi::OsString {
        std::fs::read_dir(dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
    }

    #[test]
    fn a_listing_error_is_reported_and_keeps_what_was_read() {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path().to_owned();
        let text = top.join("text");
        for name in ["text/a.xml", "text/b.xml", "text/c.xml", "image/d.xml"] {
            let path = top.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let named = |names: &[&str]| -> Vec<MimeType> {
            let mut types: Vec<MimeType> = names
                .iter()
                .map(|name| MimeType::new(name).unwrap())
                .collect();
            types.sort();
            types
        };
        // The first media folder listed, and the first `text` file.
        let media = first_listed(&top);
        let kept_media = if media == "text" {
            named(&["text/a", "text/b", "text/c"])
        } else {
            named(&["image/d"])
        };
        let file = first_listed(&text);
        let subtype = file.to_str().unwrap().strip_suffix(".xml").unwrap();
        let kept_text = named(&["image/d", &format!("text/{subtype}")]);
        let err = || std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let unopened = format!("cannot be listed: {}", err());
        let cut = format!("cannot be listed in full: {}", err());
        for (folder, at, message, kept) in [
            (&top, None, &unopened, Vec::new()),
            (&text, None, &unopened, named(&["image/d"])),
            (&top, Some(1), &cut, kept_media),
            (&text, Some(1), &cut, kept_text),
        ] {
            let fault = (folder.clone(), at, std::io::ErrorKind::PermissionDenied);
            LISTING_FAULT.set(Some(fault.clone()));
            let mut diagnostics = Vec::new();
            let mut types = types_in(&top, &mut diagnostics);
            types.sort();
            assert!(LISTING_FAULT.with_borrow(Option::is_none), "{fault:?}");
            assert_eq!(types, kept, "{fault:?}");
            assert_eq!(
                diagnostics,
                [Diagnostic::new(folder, None, message.as_str())],
                "{fault:?}"
            );
        }
    }

    #[test]
    fn a_missing_folder_is_listed_silently() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("text")).unwrap();
        std::fs::write(dir.path().join("text/a.xml"), "").unwrap();
        let mut diagnostics = Vec::new();
        assert!(types_in(&dir.path().join("missing"), &mut diagnostics).is_empty());
        // A media folder gone once the top was listed.
        let fault = (dir.path().join("text"), None, std::io::ErrorKind::NotFound);
        LISTING_FAULT.set(Some(fault));
        assert!(types_in(dir.path(), &mut diagnostics).is_empty());
        assert!(LISTING_FAULT.with_borrow(Option::is_none));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    /// The comments `info` holds, by language, sorted.
    fn comments(info: &TypeInfo) -> Vec<(Option<&str>, &str)> {
        let names: HashMap<usize, &str> = info
            .comment
            .ids
            .iter()
            .map(|(name, &id)| (id, name.as_str()))
            .collect();
        let mut comments = info
            .comment
            .values
            .iter()
            .map(|(lang, text)| (lang.map(|id| names[&id]), text.as_str()))
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
        // attribute's name; one that is not an XML name drops the field,
        // so the German text does not replace the untranslated one.
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
            "{OPEN}<comment _a=\"1\" a.b-c\u{B7}9=\"2\" \u{C0}\u{300}=\"3\" \u{10000}=\"4\" xmlns:x=\"urn:x\" x:y=\"5\" xml:lang=\"de\">D</comment></mime-type>"
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
        let cases = [
            ("", "a=\"&bogus;\"", reference("&bogus;")),
            ("", "a='x&#0;'", reference("&#0;")),
            ("", "a=\"&#1;\"", reference("&#1;")),
            (v11, "a=\"&#xFFFE;\"", reference("&#xFFFE;")),
            ("", "a=\"x\u{1}\"", not_allowed('\u{1}')),
            ("", "a=\"\u{FFFF}\"", not_allowed('\u{FFFF}')),
            (v11, "a=\"\u{80}\"", not_allowed('\u{80}')),
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
        // problem is at the field's tag. The root's tag then cannot be
        // read either, so the untranslated values have their own language.
        let text = format!(
            r#"<mime-type xmlns="{NAMESPACE}" xmlns:f="{NAMESPACE}&bogus;"><comment xml:lang="">en</comment><f:comment xml:lang="de">F</f:comment><acronym xml:lang="">A</acronym></mime-type>"#
        );
        check_dropped(&text, &[(None, "en")], "<f:comment", &reference("&bogus;"));
        // What the version allows is kept; an element of another namespace
        // is reported and skipped.
        let text = format!(
            "{v11}{OPEN}<comment a=\"&#1;&lt;&#x41;\" b='\"' xmlns:p=\"urn:p\">C</comment><x:comment xmlns:x=\"urn:x\" a=\"&bogus;\">X</x:comment></mime-type>"
        );
        let info = parse(&text);
        assert_eq!(
            info.diagnostics,
            [Diagnostic::new(
                "t.xml",
                text.find("<x:comment").map(crate::Location::Offset),
                "bad XML (the reference `&bogus;` cannot be replaced); the element is skipped"
            )]
        );
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
        for declaration in ["", "<?xml version=\"1.0\"?>", "<?xml version=\"1.2\"?>"] {
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
        // In XML 1.1 it undeclares: the prefix is then not bound.
        let text = format!(
            "<?xml version=\"1.1\"?>{OPEN}<p:comment xmlns:p=\"\">X</p:comment></mime-type>"
        );
        assert_eq!(
            parse(&text).diagnostics,
            [Diagnostic::new(
                "t.xml",
                text.find("<p:comment").map(crate::Location::Offset),
                "bad XML (an element's prefix is not bound: p); the element is skipped"
            )]
        );
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
        // An element of another namespace is checked as any other: reported
        // and skipped.
        let text = format!(
            "{OPEN}<x:comment xmlns:x=\"urn:x\" a=\"1\" xmlns:y=\"&bogus;\">X</x:comment><comment>a</comment></mime-type>"
        );
        let info = parse(&text);
        assert_eq!(
            messages(&info),
            ["bad XML (the reference `&bogus;` cannot be replaced); the element is skipped"]
        );
        assert_eq!(comments(&info), [(None, "a")]);
    }

    #[test]
    fn a_namespace_error_is_at_its_tag() {
        // Reading stops at these; the problem is at the tag's `<`.
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
        // A tag's namespace declarations are read only up to an attribute
        // that cannot be read, so which namespace the field is in is not
        // known: the field is left out, and the attribute reported.
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
        // `xmlns`. Each drops the field, and is reported.
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
        // field: the field is dropped and reported, whether the root binds
        // the prefix or no declaration does.
        let name = "bad XML (an element's name is not an XML name); the value is left out";
        // A root that binds a prefix that is not a name cannot be read, so
        // the untranslated values have their own language.
        let tail = "<acronym xml:lang=\"\">A</acronym>";
        let bound = |prefix: &str| {
            format!(
                "<mime-type xmlns=\"{NAMESPACE}\" xmlns:{prefix}=\"{NAMESPACE}\"><comment xml:lang=\"\">en</comment>"
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
                    "<mime-type xmlns=\"{NAMESPACE}\" xmlns:1p=\"urn:q\"><comment xml:lang=\"\">en</comment><comment xml:lang=\"de\">a<1p:b/>d</comment>{tail}</mime-type>"
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
    fn nel_or_ls_inside_a_tag_is_always_reported() {
        // `quick-xml` takes only ASCII white space inside a tag, so U+0085
        // and U+2028 there misread it, in XML 1.1 as in 1.0 (where such a
        // tag is not well-formed); every such misreading is reported.
        let skipped = "bad XML (an element's name is not an XML name); the element is skipped";
        let attribute = "bad XML (an attribute cannot be read: a name is not an XML name); \
                         the value is left out";
        let messages = |info: &TypeInfo| -> Vec<String> {
            info.diagnostics.iter().map(|d| d.message.clone()).collect()
        };
        for declaration in ["", "<?xml version=\"1.1\"?>"] {
            for c in ['\u{85}', '\u{2028}'] {
                let field = |inner: &str| {
                    parse(&format!(
                        "{declaration}{OPEN}{inner}<acronym>A</acronym></mime-type>"
                    ))
                };
                // In the start and end tags of an element that is not a
                // field: its name is not an XML name, so it is skipped, and
                // the end tag matches.
                let info = field(&format!("<foo{c}>x</foo{c}><comment>D</comment>"));
                assert_eq!(messages(&info), [skipped], "{c:?} {declaration}");
                assert_eq!(comments(&info), [(None, "D")], "{c:?} {declaration}");
                assert_eq!(info.acronym.as_deref(), Some("A"), "{c:?} {declaration}");
                // The same on a field: the element is not a field.
                let info = field(&format!("<comment{c}>D</comment{c}>"));
                assert_eq!(messages(&info), [skipped], "{c:?} {declaration}");
                assert_eq!(comments(&info), [], "{c:?} {declaration}");
                assert_eq!(info.acronym.as_deref(), Some("A"), "{c:?} {declaration}");
                // An attribute after one cannot be read: the field is left
                // out.
                let info = field(&format!(
                    "<comment xml:lang=\"de\"{c}a=\"1\">D</comment><comment>E</comment>"
                ));
                assert_eq!(messages(&info), [attribute], "{c:?} {declaration}");
                assert_eq!(comments(&info), [(None, "E")], "{c:?} {declaration}");
                assert_eq!(info.acronym.as_deref(), Some("A"), "{c:?} {declaration}");
                // Nothing after one in a start tag can be read, even just
                // before `>` with no attribute after it.
                let info = field(&format!(
                    "<comment xml:lang=\"de\"{c}>D</comment><comment>E</comment>"
                ));
                let found = messages(&info);
                assert_eq!(found.len(), 1, "{c:?} {declaration}: {found:?}");
                assert!(
                    found[0].starts_with("bad XML (an attribute cannot be read")
                        && found[0].ends_with("; the value is left out"),
                    "{c:?} {declaration}: {found:?}"
                );
                assert_eq!(comments(&info), [(None, "E")], "{c:?} {declaration}");
                assert_eq!(info.acronym.as_deref(), Some("A"), "{c:?} {declaration}");
                // An end tag that no longer matches its start tag skips the
                // rest of the file.
                let info = field(&format!("<comment{c}>D</comment>"));
                let found = messages(&info);
                assert_eq!(found.len(), 2, "{c:?} {declaration}: {found:?}");
                assert_eq!(found[0], skipped, "{c:?} {declaration}");
                assert!(
                    found[1].starts_with("bad XML (ill-formed document: expected `</comment")
                        && found[1].ends_with("; the rest is skipped"),
                    "{c:?} {declaration}: {found:?}"
                );
                assert_eq!(info.acronym, None, "{c:?} {declaration}");
                // At the root, an attribute after one makes its language
                // unknown, and its end tag no longer matches.
                let info = parse(&format!(
                    "{declaration}<mime-type xmlns=\"{NAMESPACE}\"{c}><comment>D</comment></mime-type{c}>"
                ));
                let found = messages(&info);
                assert_eq!(found.len(), 2, "{c:?} {declaration}: {found:?}");
                assert!(
                    found[0].starts_with(
                        "bad XML (the root element's tag cannot be read, so its language is unknown"
                    ),
                    "{c:?} {declaration}: {found:?}"
                );
                assert!(
                    found[1].ends_with("; the rest is skipped"),
                    "{c:?} {declaration}: {found:?}"
                );
                assert_eq!(comments(&info), [], "{c:?} {declaration}");
                // A namespace declaration after one on the root is lost: the
                // file is ignored.
                let info = parse(&format!(
                    "{declaration}<mime-type xml:lang=\"de\"{c} xmlns=\"{NAMESPACE}\"><comment>D</comment></mime-type>"
                ));
                let found = messages(&info);
                assert!(
                    found.iter().any(|m| m.starts_with("not a per-type file: ")
                        && m.ends_with("; it is ignored")),
                    "{c:?} {declaration}: {found:?}"
                );
                assert_eq!(comments(&info), [], "{c:?} {declaration}");
                // A root whose name takes one in: the file is ignored.
                let info = parse(&format!(
                    "{declaration}<mime-type{c}xmlns=\"{NAMESPACE}\"><comment>D</comment></mime-type>"
                ));
                let found = messages(&info);
                assert_eq!(found.len(), 1, "{c:?} {declaration}: {found:?}");
                assert!(
                    found[0].starts_with("not a per-type file: the root element's name")
                        && found[0].ends_with("is not an XML name; it is ignored"),
                    "{c:?} {declaration}: {found:?}"
                );
                assert_eq!(comments(&info), [], "{c:?} {declaration}");
            }
        }
    }

    #[test]
    fn a_declaration_with_no_version_is_reported_and_ignored() {
        // Read as XML 1.0, so the reference cannot be replaced.
        for declaration in [
            "<?xml?>",
            "<?xml ?>",
            "<?xml encoding=\"UTF-8\"?>",
            "<?xml standalone=\"yes\"?>",
        ] {
            let info = parse(&format!(
                "{declaration}{OPEN}<comment>D&#1;E</comment></mime-type>"
            ));
            assert_eq!(
                info.diagnostics.first().map(|d| d.message.as_str()),
                Some("bad XML (the XML declaration does not start with `version`); it is ignored"),
                "{declaration}"
            );
            assert_eq!(comments(&info), [], "{declaration}");
        }
    }

    #[test]
    fn a_field_is_named_by_what_follows_the_last_colon() {
        // Not by `quick-xml`'s local name (after the first `:`): so
        // `<x:y:comment>` is a field whose name is not an XML name, reported.
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
        // An element of another namespace is not a field: nothing is said
        // of it.
        let text = format!("{head}<o:comment>X</o:comment>{tail}");
        let info = parse(&text);
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(comments(&info), [(None, "en")]);
        assert_eq!(info.acronym.as_deref(), Some("A"));
        // A prefix no declaration binds, or a name that is not a `QName`,
        // is a namespace error.
        let not_qname = "bad XML (an element's name is not an XML name); the element is skipped";
        for (other, message) in [
            (
                "<comment:y>X</comment:y>",
                "bad XML (an element's prefix is not bound: comment); the element is skipped",
            ),
            ("<comment: xml:lang=\"de\">X</comment:>", not_qname),
            ("<x:y:z>X</x:y:z>", not_qname),
            ("<x:comment:y>X</x:comment:y>", not_qname),
        ] {
            let text = format!("{head}{other}{tail}");
            let info = parse(&text);
            assert_eq!(messages(&info), [message], "{text:?}");
            assert_eq!(comments(&info), [(None, "en")], "{text:?}");
            assert_eq!(info.acronym.as_deref(), Some("A"), "{text:?}");
        }
    }

    #[test]
    #[ignore = "timing: run with `--release -- --ignored`"]
    fn many_languages_take_linear_time() {
        use std::fmt::Write as _;
        // Looking each value up among all the languages seen would be
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
                    assert_eq!(info.comment.values.len(), n);
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

    /// The `xml` namespace, with its last letter written as a reference.
    const XML_REFERENCED: &str = "http://www.w3.org/XML/1998/namespac&#101;";
    /// The `xmlns` namespace, two of its characters written as references.
    const XMLNS_REFERENCED: &str = "&#104;ttp://www.w3.org/2000/xmlns&#x2F;";

    /// Namespace declarations Namespaces in XML forbids, each with the
    /// problem reported, however their values are written.
    fn forbidden_bindings() -> [(String, String); 5] {
        let bound = |prefix: &str, namespace: &str| {
            format!("the namespace prefix '{prefix}' cannot be bound to '{namespace}'")
        };
        let xml = "http://www.w3.org/XML/1998/namespace";
        let xmlns = "http://www.w3.org/2000/xmlns/";
        [
            (format!("xmlns:p=\"{XML_REFERENCED}\""), bound("p", xml)),
            (format!("xmlns:p=\"{XMLNS_REFERENCED}\""), bound("p", xmlns)),
            (
                format!("xmlns:xmlns=\"{XMLNS_REFERENCED}\""),
                // Shown as written, cut.
                bound("xmlns", &quoted(XMLNS_REFERENCED)),
            ),
            (
                "xmlns:xml=\"urn:&#120;\"".to_owned(),
                bound("xml", "urn:&#120;"),
            ),
            ("xmlns:xml=\"\"".to_owned(), bound("xml", "")),
        ]
    }

    #[test]
    fn reserved_namespaces_are_compared_after_references_are_replaced() {
        // Namespaces in XML 1.0, 3rd ed., 3: the reserved names are
        // compared after references are replaced, as any namespace name.
        // So the `xml` namespace spelled with a reference is accepted for
        // the `xml` prefix, and either one so spelled is refused for
        // another prefix.
        for text in [
            format!(
                "<mime-type xmlns=\"{NAMESPACE}\" xmlns:xml=\"{XML_REFERENCED}\"><comment xml:lang=\"de\">a</comment></mime-type>"
            ),
            format!(
                "{OPEN}<comment xmlns:xml=\"{XML_REFERENCED}\" xml:lang=\"de\">a</comment></mime-type>"
            ),
            format!(
                "{OPEN}<x xmlns:xml=\"{XML_REFERENCED}\"><xml:y/></x><comment xml:lang=\"de\">a</comment></mime-type>"
            ),
        ] {
            let info = parse(&text);
            assert!(
                info.diagnostics.is_empty(),
                "{text:?}: {:?}",
                info.diagnostics
            );
            assert_eq!(comments(&info), [(Some("de"), "a")], "{text:?}");
        }
        // Refused on the root, a field's tag or another element: the rest
        // is skipped.
        let head = format!("{OPEN}<comment>en</comment>");
        for (attribute, problem) in forbidden_bindings() {
            for (text, at, kept) in [
                (
                    format!(
                        "<mime-type xmlns=\"{NAMESPACE}\" {attribute}><comment>en</comment></mime-type>"
                    ),
                    0,
                    &[][..],
                ),
                (
                    format!("{head}<comment {attribute}>X</comment></mime-type>"),
                    head.len(),
                    &[(None, "en")][..],
                ),
                (
                    format!(
                        "{head}<x {attribute}/><comment xml:lang=\"de\">X</comment></mime-type>"
                    ),
                    head.len(),
                    &[(None, "en")][..],
                ),
            ] {
                let info = parse(&text);
                assert_eq!(
                    info.diagnostics,
                    [Diagnostic::new(
                        "t.xml",
                        Some(crate::Location::Offset(at)),
                        format!("bad XML ({problem}); the rest is skipped")
                    )],
                    "{text:?}"
                );
                assert_eq!(comments(&info), kept, "{text:?}");
            }
        }
    }

    #[test]
    fn root_detection_compares_reserved_namespaces_after_references_are_replaced() {
        let xml = "http://www.w3.org/XML/1998/namespace";
        let found = |text: &str| root_element(text.as_bytes());
        assert_eq!(
            found(&format!("<r xmlns:xml=\"{XML_REFERENCED}\"/>")),
            Some((String::new(), "r".into()))
        );
        assert_eq!(
            found(&format!("<xml:r xmlns:xml=\"{XML_REFERENCED}\"/>")),
            Some((xml.into(), "r".into()))
        );
        for (attribute, _) in forbidden_bindings() {
            let text = format!("<r {attribute}/>");
            assert_eq!(found(&text), None, "{text:?}");
        }
    }

    #[test]
    fn a_reserved_default_namespace_gives_no_root_element() {
        // Namespaces in XML 1.0, 3rd ed., 3: neither reserved namespace may
        // be the default one, however written.
        let xml = "http://www.w3.org/XML/1998/namespace";
        let xmlns = "http://www.w3.org/2000/xmlns/";
        for namespace in [xml, xmlns, XML_REFERENCED, XMLNS_REFERENCED] {
            for text in [
                format!("<r xmlns=\"{namespace}\"/>"),
                format!("<r xmlns=\"{namespace}\">"),
                format!("<p:r xmlns:p=\"urn:p\" xmlns=\"{namespace}\"/>"),
            ] {
                assert_eq!(root_element(text.as_bytes()), None, "{text:?}");
            }
        }
        // So content detection keeps the type magic gives, as for any root
        // element that cannot be read.
        let db = MimeDatabase::from_files(&[vec![
            (
                "magic",
                b"MIME-Magic\0\n[50:application/xml]\n>0=\x00\x02<r\n".as_slice(),
            ),
            ("subclasses", b"application/x-r+xml application/xml\n"),
            (
                "XMLnamespaces",
                b"http://www.w3.org/2000/xmlns/ r application/x-r+xml\nurn:r r application/x-r+xml\n",
            ),
        ]]);
        let found = db.type_for_data(b"<r xmlns=\"urn:r\"/>");
        assert_eq!(found.mime.as_str(), "application/x-r+xml");
        let found = db.type_for_data(format!("<r xmlns=\"{xmlns}\"/>").as_bytes());
        assert_eq!(found.mime.as_str(), "application/xml");
        assert_eq!(found.decided_by, crate::DecidedBy::Content);
    }

    /// Parses `text` as a per-type file named `t.xml`, within `limits`.
    fn parse_with(text: &str, limits: &crate::Limits) -> TypeInfo {
        let mut info = TypeInfo::default();
        info.parse_within(Path::new("t.xml"), text.as_bytes(), limits);
        info
    }

    /// The default limits with no limit on namespace declarations.
    fn unbound() -> crate::Limits {
        crate::Limits {
            max_namespace_bindings: None,
            ..crate::Limits::default()
        }
    }

    /// ` xmlns:{prefix}0="urn:0"` and on, `count` declarations.
    fn bindings(prefix: &str, count: usize) -> String {
        use std::fmt::Write as _;
        let mut text = String::new();
        for at in 0..count {
            let _ = write!(text, " xmlns:{prefix}{at}=\"urn:{at}\"");
        }
        text
    }

    #[test]
    fn namespace_bindings_in_scope_are_limited() {
        let limits = crate::Limits::default();
        assert_eq!(limits.max_namespace_bindings, Some(128));
        let too_many = "bad XML (more than 128 namespace declarations on the open elements); the rest is skipped";
        // The root's default and 127 more are 128; one more is too many.
        for (extra, refused) in [(127, false), (128, true)] {
            let text = format!(
                "<mime-type xmlns=\"{NAMESPACE}\"{}><comment>a</comment></mime-type>",
                bindings("p", extra)
            );
            let info = parse_with(&text, &limits);
            if refused {
                assert_eq!(
                    info.diagnostics,
                    [Diagnostic::new(
                        "t.xml",
                        Some(crate::Location::Offset(0)),
                        too_many
                    )]
                );
                assert_eq!(comments(&info), []);
            } else {
                assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
                assert_eq!(comments(&info), [(None, "a")]);
            }
            let text = format!("<r xmlns=\"urn:r\"{}/>", bindings("p", extra));
            let found = root_element_within(text.as_bytes(), &limits);
            assert_eq!(found.is_none(), refused, "{extra}");
        }
        // Counted while in scope: those of an element that has ended are
        // not.
        let head = format!("<mime-type xmlns=\"{NAMESPACE}\"{}>", bindings("p", 63));
        let inner = format!("<x{}><y/></x>", bindings("q", 64));
        let text = format!("{head}{inner}{inner}<comment>a</comment></mime-type>");
        let info = parse_with(&text, &limits);
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(comments(&info), [(None, "a")]);
        let text = format!(
            "{head}{inner}<x{}><y xmlns=\"urn:y\"/></x><comment>a</comment></mime-type>",
            bindings("q", 64)
        );
        assert_eq!(
            parse_with(&text, &limits).diagnostics,
            [Diagnostic::new(
                "t.xml",
                text.find("<y xmlns").map(crate::Location::Offset),
                too_many
            )]
        );
        // Another limit, and none.
        let mut two = limits;
        two.max_namespace_bindings = Some(2);
        let text = format!(
            "<mime-type xmlns=\"{NAMESPACE}\"{}><comment>a</comment></mime-type>",
            bindings("p", 2)
        );
        assert_eq!(
            messages(&parse_with(&text, &two)),
            [
                "bad XML (more than 2 namespace declarations on the open elements); the rest is skipped"
            ]
        );
        let unlimited = crate::Limits::unlimited();
        assert_eq!(unlimited.max_namespace_bindings, None);
        let text = format!(
            "<mime-type xmlns=\"{NAMESPACE}\"{}><comment>a</comment></mime-type>",
            bindings("p", 10_000)
        );
        let info = parse_with(&text, &unlimited);
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(comments(&info), [(None, "a")]);
        let text = format!("<p9999:r{}/>", bindings("p", 10_000));
        assert_eq!(
            root_element_within(text.as_bytes(), &unbound()),
            Some(("urn:9999".into(), "r".into()))
        );
        assert_eq!(root_element_within(text.as_bytes(), &limits), None);
    }

    #[test]
    fn with_standalone_a_default_needs_its_entities_declared_before_and_outside() {
        // With standalone, an entity an `ATTLIST` default outside
        // parameter-entity text reaches, directly or through another, needs
        // a declaration of any kind before the default and one outside such
        // text anywhere; one in such text is exempt from the second only.
        const SA: &str = "<?xml version=\"1.0\" standalone=\"yes\"?>";
        const NS: &str = "<!ENTITY ns \"urn:x\">";
        let x = Some("urn:x");
        let cases: Vec<(String, Option<&str>)> = vec![
            // A default outside the text needs a declaration outside it,
            // anywhere, and one of any kind before the default, directly or
            // through another entity.
            (
                format!(
                    "{SA}<!DOCTYPE r [{NS}<!ENTITY % p \"<!ENTITY a 'v'>\"> %p;<!ATTLIST r b CDATA \"&a;\">]>"
                ),
                None,
            ),
            (
                format!(
                    "{SA}<!DOCTYPE r [<!ENTITY % p \"<!ENTITY a 'v'>\"> %p;<!ATTLIST r b CDATA \
                     \"&a;\"><!ENTITY a \"w\">{NS}]>"
                ),
                x,
            ),
            (
                format!(
                    "{SA}<!DOCTYPE r [<!ENTITY % p \"<!ENTITY a 'v'>\"> %p;<!ENTITY b \"&a;\">\
                     <!ATTLIST r c CDATA \"&b;\"><!ENTITY a \"w\">{NS}]>"
                ),
                x,
            ),
            (
                format!(
                    "{SA}<!DOCTYPE r [<!ENTITY % p \"<!ENTITY a 'v'>\"> %p;<!ENTITY b \"&a;\">\
                     <!ATTLIST r c CDATA \"&b;\">{NS}]>"
                ),
                None,
            ),
            (
                format!(
                    "{SA}<!DOCTYPE r [<!ENTITY b \"&a;\"><!ATTLIST r c CDATA \"&b;\">\
                     <!ENTITY a \"w\">{NS}]>"
                ),
                None,
            ),
            (
                format!("{SA}<!DOCTYPE r [<!ATTLIST r c CDATA \"&a;\"><!ENTITY a \"w\">{NS}]>"),
                None,
            ),
            // Not standalone: a default may refer to an entity declared only
            // in parameter-entity text.
            (
                format!(
                    "<!DOCTYPE r [{NS}<!ENTITY % p \"<!ENTITY a 'v'>\"> %p;<!ATTLIST r b CDATA \"&a;\">]>"
                ),
                x,
            ),
            // In the text, an entity declared nowhere stays an error, and so
            // does one declared only after the default.
            (
                format!(
                    "{SA}<!DOCTYPE r [{NS}<!ENTITY % p \"<!ATTLIST r b CDATA '&#38;u;'>\"> %p;]>"
                ),
                None,
            ),
            (
                format!(
                    "{SA}<!DOCTYPE r [{NS}<!ENTITY % p \"<!ATTLIST r b CDATA '&#38;a;'>\"> \
                     %p;<!ENTITY a \"v\">]>"
                ),
                None,
            ),
            (
                format!("{SA}<!DOCTYPE r [<!ENTITY % p \"<!ENTITY ns 'urn:&#38;u;'>\"> %p;{NS}]>"),
                None,
            ),
            (
                format!(
                    "{SA}<!DOCTYPE r [<!ENTITY % p \"<!ENTITY a 'v'>\"> %p;<!ENTITY a \
                     \"w\"><!ATTLIST r b CDATA \"&a;\">{NS}]>"
                ),
                x,
            ),
        ];
        check_parameter_cases(cases);
    }

    #[test]
    fn the_standalone_rule_binds_references_outside_parameter_entity_text() {
        // XML 1.0, 4.1 (WFC: Entity Declared): with standalone, a reference
        // that is not itself in parameter-entity text must name an entity
        // with a declaration outside such text; a reference in such text is
        // exempt from that. An entity an `ATTLIST` default reaches must be
        // declared before it, in such text or not, and one declared nowhere
        // is an error.
        const SA: &str = "<?xml version=\"1.0\" standalone=\"yes\"?>";
        const NS: &str = "<!ENTITY ns \"urn:x\">";
        let x = Some("urn:x");
        let cases: Vec<(String, Option<&str>)> = vec![
            // A default in the text, referring to one declared there (also
            // through another): exempt.
            (
                format!(
                    "{SA}<!DOCTYPE r [{NS}<!ENTITY % p \"<!ENTITY a 'v'><!ATTLIST r b CDATA \
                     '&#38;a;'>\"> %p;]>"
                ),
                x,
            ),
            (
                format!(
                    "{SA}<!DOCTYPE r [{NS}<!ENTITY % p \"<!ENTITY a 'v'><!ENTITY b \
                     '&#38;a;'><!ATTLIST r c CDATA '&#38;b;'>\"> %p;]>"
                ),
                x,
            ),
            (
                format!(
                    "{SA}<!DOCTYPE r [{NS}<!ENTITY a \"v\"><!ENTITY % p \"<!ATTLIST r b CDATA \
                     '&#38;a;'>\"> %p;]>"
                ),
                x,
            ),
            // An entity declared in the text whose own text refers to another
            // declared there, used through one declared outside: the
            // reference in its text is exempt, the outer one is not.
            (
                format!(
                    "{SA}<!DOCTYPE r [<!ENTITY % p \"<!ENTITY a 'urn:x'><!ENTITY ns \
                     '&#38;a;'>\"> %p;]>"
                ),
                None,
            ),
            (
                format!(
                    "{SA}<!DOCTYPE r [<!ENTITY % p \"<!ENTITY a 'urn:x'>\"> %p;<!ENTITY ns \"&a;\">]>"
                ),
                None,
            ),
            // Not standalone: unchanged.
            (
                format!(
                    "<!DOCTYPE r [<!ENTITY % p \"<!ENTITY a 'v'>\"> %p;<!ATTLIST r b CDATA \
                     \"&a;\"><!ENTITY a \"w\">{NS}]>"
                ),
                x,
            ),
            // Declared in the text and outside, the first binding.
            (
                format!("{SA}<!DOCTYPE r [<!ENTITY % p \"<!ENTITY ns 'urn:y'>\"> %p;{NS}]>"),
                Some("urn:y"),
            ),
            (
                format!("{SA}<!DOCTYPE r [{NS}<!ENTITY % p \"<!ENTITY ns 'urn:y'>\"> %p;]>"),
                x,
            ),
        ];
        check_parameter_cases(cases);
        // A per-type file the same way.
        let text = format!(
            "{SA}<!DOCTYPE mime-type [<!ENTITY ns \"{NAMESPACE}\"><!ENTITY % p \"<!ENTITY a \
             'v'><!ATTLIST mime-type b CDATA '&#38;a;'>\"> %p;]><mime-type \
             xmlns=\"&ns;\"><comment>c</comment></mime-type>"
        );
        let info = parse(&text);
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(comments(&info), [(None, "c")]);
    }

    #[test]
    fn parameter_entity_recursion_is_refused_by_its_own_check() {
        // A parameter entity that includes itself, directly or through
        // another, is refused at once (WFC: No Recursion), not by the budget:
        // with a large one, and with none, after a few steps.
        let ns = "<!ENTITY ns \"urn:x\">";
        for (per_byte, extra) in [(Some(1), 1 << 20), (None, 1 << 20)] {
            let limits = crate::Limits {
                entity_expansion_per_byte: per_byte,
                entity_expansion_extra: extra,
                ..crate::Limits::default()
            };
            for subset in [
                "<!ENTITY % p \"&#37;p;\"> %p;",
                "<!ENTITY % p \"&#37;q;\"><!ENTITY % q \"<!-- c -->&#37;p;\"> %p;",
            ] {
                let text = format!("<!DOCTYPE r [{ns}{subset}]><r xmlns=\"&ns;\"/>");
                let (found, steps) =
                    crate::work::steps(|| root_element_within(text.as_bytes(), &limits));
                assert_eq!(found, None, "{per_byte:?} {text}");
                assert!(steps < 2000, "{steps} steps: {per_byte:?} {text}");
            }
        }
    }

    #[test]
    fn included_text_holds_no_conditional_section_and_is_not_normalised_again() {
        // A conditional section, even one written whole, is not allowed in
        // parameter-entity text of the internal subset.
        let ns = "<!ENTITY ns \"urn:x\">";
        let text =
            format!("<!DOCTYPE r [{ns}<!ENTITY % p \"<![IGNORE[ x ]]>\"> %p;]><r xmlns=\"&ns;\"/>");
        assert_eq!(root_element(text.as_bytes()), None);
        // A parameter entity declared in included text keeps its literal
        // as it stands there: line ends from character references are not
        // normalised again (2.11 is on the entities read), so the entity it
        // declares in turn keeps CR LF, two spaces in the namespace.
        let text = "<!DOCTYPE r [<!ENTITY % p \"<!ENTITY &#37; q '<!ENTITY ns &#34;urn:a&#13;&#10;b&#34;>'>\"> %p; %q;]><r xmlns=\"&ns;\"/>";
        assert_eq!(
            root_element(text.as_bytes())
                .map(|(namespace, _)| namespace)
                .as_deref(),
            Some("urn:a  b")
        );
    }

    #[test]
    fn an_expansion_reserves_only_what_its_value_needs() {
        use std::fmt::Write as _;
        // The budget is charged with the bytes read, references included
        // (about three times the value for a doubling chain), but the memory
        // asked for is the value's length, exactly: here 4 * 2^10 bytes, with
        // a character reference and a predefined entity at each level adding
        // one byte each.
        let mut text = String::from("<!DOCTYPE r [<!ENTITY g0 \"urn:\">");
        for at in 1..=10 {
            let _ = write!(text, "<!ENTITY g{at} \"&g{0};&#x41;&amp;&g{0};\">", at - 1);
        }
        text.push_str("]><r xmlns=\"&g10;\"/>");
        let (namespace, _) = root_element(text.as_bytes()).unwrap();
        assert_eq!(namespace.len(), 4 * 1024 + 2 * 1023);
        assert_eq!(last_reserved(), namespace.len());
        // Character references to characters of two to four bytes, in an
        // entity's text (written escaped, so that the reference stays in its
        // replacement text, and as is) and in the value; and line ends in
        // the value, which count as normalised (2.11): CR LF one byte, and in
        // XML 1.1 NEL and LS too.
        let v11 = "<?xml version=\"1.1\"?>";
        for (lead, entity, value, expected) in [
            (
                "",
                "&#38;#x20AC;&#x20AC;",
                "urn:&#x1F600;&a;",
                "urn:\u{1F600}\u{20AC}\u{20AC}",
            ),
            (
                "",
                "&#38;#x1F600;&#x1F600;",
                "urn:&#x20AC;&a;",
                "urn:\u{20AC}\u{1F600}\u{1F600}",
            ),
            (
                v11,
                "&#38;#x85;&#x85;",
                "urn:&#x85;&a;",
                "urn:\u{85}\u{85}\u{85}",
            ),
            ("", "x", "urn:a\r\n&a;\r\nb", "urn:a x b"),
            ("", "x", "urn:a\r&a;\rb", "urn:a x b"),
            (v11, "x", "urn:a\u{85}&a;\u{2028}b\r\u{85}c", "urn:a x b c"),
        ] {
            let text =
                format!("{lead}<!DOCTYPE r [<!ENTITY a \"{entity}\">]><r xmlns=\"{value}\"/>");
            let (namespace, _) = root_element(text.as_bytes()).unwrap();
            assert_eq!(namespace, expected, "{text:?}");
            assert_eq!(last_reserved(), namespace.len(), "{text:?}");
        }
    }

    #[test]
    fn an_expanded_namespace_is_copied_only_where_memory_may_be_refused() {
        // What a namespace declaration expands to may be as large as memory
        // allows (no budget), so each copy of it asks for its memory first:
        // the tag's record of it, a prefix's namespace ID and the root's
        // namespace returned. A refused copy makes the binding one that
        // cannot be read, as a refused expansion does: an error only where a
        // name uses it. The test refuses the n-th copy alone (counted from
        // 0).
        let ns = "<!DOCTYPE r [<!ENTITY a \"urn:x\"><!ENTITY b \"urn:y\">]>";
        let root = |text: &str, refused: usize| {
            set_copy_refused(refused);
            let found = root_element(text.as_bytes()).map(|(namespace, _)| namespace);
            set_copy_refused(usize::MAX);
            found
        };
        // Used by the root: the tag's record, then the ID (prefixed), then
        // the namespace returned; refusing any one refuses the root.
        for (tag, copies) in [("<r xmlns=\"&a;\"/>", 2), ("<p:r xmlns:p=\"&a;\"/>", 3)] {
            let text = format!("{ns}{tag}");
            for refused in 0..copies {
                assert_eq!(root(&text, refused), None, "{refused} {text}");
            }
            assert_eq!(root(&text, copies).as_deref(), Some("urn:x"), "{text}");
        }
        // Not used: the default's record (0), then the prefix's record (1)
        // and ID (2), then the namespace returned (3). Refusing the prefix's
        // record or ID alone leaves the root as it is.
        let text = format!("{ns}<r xmlns=\"&a;\" xmlns:q=\"&b;\"/>");
        for refused in [1, 2, 4] {
            assert_eq!(root(&text, refused).as_deref(), Some("urn:x"), "{refused}");
        }
        for refused in [0, 3] {
            assert_eq!(root(&text, refused), None, "{refused}");
        }
        // In a per-type file, with the default's record (0), and with a
        // prefix's record (0) and ID (1), used by the root or not.
        let dtd = format!("<!DOCTYPE m:mime-type [<!ENTITY a \"{NAMESPACE}\">]>");
        let comment = |text: &str, refused: usize| {
            set_copy_refused(refused);
            let info = parse(text);
            set_copy_refused(usize::MAX);
            comments(&info)
                .into_iter()
                .map(|(_, c)| c.to_owned())
                .collect::<Vec<_>>()
        };
        let default = format!("{dtd}<mime-type xmlns=\"&a;\"><comment>c</comment></mime-type>");
        assert_eq!(comment(&default, 0), Vec::<String>::new());
        assert_eq!(comment(&default, 1), ["c"]);
        let used =
            format!("{dtd}<m:mime-type xmlns:m=\"&a;\"><m:comment>c</m:comment></m:mime-type>");
        for refused in [0, 1] {
            assert_eq!(comment(&used, refused), Vec::<String>::new(), "{refused}");
        }
        assert_eq!(comment(&used, 2), ["c"]);
        let unused = format!(
            "{dtd}<mime-type xmlns=\"{NAMESPACE}\" xmlns:q=\"&a;\"><comment>c</comment></mime-type>"
        );
        for refused in [0, 1, 2] {
            assert_eq!(comment(&unused, refused), ["c"], "{refused}");
        }
    }

    #[test]
    fn an_entity_bomb_without_a_budget_is_refused_not_aborted() {
        use std::fmt::Write as _;
        // With no expansion budget, a doubling chain of `levels` entities
        // (4 * 2^levels bytes) cannot be held: a namespace declaration that
        // expands to it cannot be read, as past a budget, instead of the
        // reader aborting (allocation failure, at 40 levels) or panicking
        // (capacity overflow, at 70).
        let bomb = |levels: usize, root: &str| {
            let mut text = String::from("<!DOCTYPE r [<!ENTITY g0 \"urn:\">");
            for at in 1..=levels {
                let _ = write!(text, "<!ENTITY g{at} \"&g{0};&g{0};\">", at - 1);
            }
            let _ = write!(text, "]>{root}");
            text
        };
        let unlimited = crate::Limits::unlimited();
        for levels in [40, 70] {
            let text = bomb(levels, &format!("<r xmlns=\"&g{levels};\"/>"));
            assert_eq!(
                root_element_within(text.as_bytes(), &unlimited),
                None,
                "{levels}"
            );
            let file = bomb(
                levels,
                &format!("<mime-type xmlns=\"&g{levels};\"><comment>c</comment></mime-type>"),
            )
            .replacen("<!DOCTYPE r", "<!DOCTYPE mime-type", 1);
            let info = parse_with(&file, &unlimited);
            assert_eq!(comments(&info), [], "{levels}");
            assert!(!info.diagnostics.is_empty(), "{levels}");
        }
    }

    #[test]
    fn the_expansion_budgets_follow_the_limits() {
        use std::fmt::Write as _;
        // The bytes parameter entities include and those namespace
        // declarations expand to may pass, per byte of the document type or
        // the document, `entity_expansion_per_byte` (default 1), plus
        // `entity_expansion_extra` (default 64 KiB); `None`: no budget.
        let with = |per_byte: Option<usize>, extra: usize| crate::Limits {
            entity_expansion_per_byte: per_byte,
            entity_expansion_extra: extra,
            ..crate::Limits::default()
        };
        let defaults = crate::Limits::default();
        assert_eq!(
            (
                defaults.entity_expansion_per_byte,
                defaults.entity_expansion_extra
            ),
            (Some(1), 64 * 1024)
        );
        let unlimited = crate::Limits::unlimited();
        assert_eq!(
            (
                unlimited.entity_expansion_per_byte,
                unlimited.entity_expansion_extra
            ),
            (None, 64 * 1024)
        );
        // Ten inclusions of one entity: more than the document type's length.
        let ten = format!(
            "<!DOCTYPE r [<!ENTITY % p \"<!ENTITY a 'urn:x'>\">{}]><r xmlns=\"&a;\"/>",
            "%p;".repeat(10)
        );
        // 2^13 inclusions of 19 bytes: 152 KiB, past the default budget.
        let mut laughs = String::from("<!DOCTYPE r [<!ENTITY % p0 \"<!ENTITY a 'urn:x'>\">");
        for at in 1..=13 {
            let _ = write!(laughs, "<!ENTITY % p{at} \"&#37;p{0};&#37;p{0};\">", at - 1);
        }
        laughs.push_str("%p13;]><r xmlns=\"&a;\"/>");
        // A namespace 100 times an entity of 104 bytes: about 10 KiB, past
        // the document's length.
        let long = format!(
            "<!DOCTYPE r [<!ENTITY a \"urn:{}\">]><r xmlns=\"{}\"/>",
            "x".repeat(100),
            "&a;".repeat(100)
        );
        let long_ns = format!("urn:{}", "x".repeat(100)).repeat(100);
        for (text, limits, found) in [
            (&ten, defaults, Some("urn:x")),
            (&ten, with(Some(1), 0), None),
            (&ten, with(Some(20), 0), Some("urn:x")),
            (&ten, with(Some(0), 2000), Some("urn:x")),
            (&ten, with(None, 0), Some("urn:x")),
            (&laughs, defaults, None),
            (&laughs, with(Some(1), 1024), None),
            (&laughs, with(None, 0), Some("urn:x")),
            (&laughs, unlimited, Some("urn:x")),
            (&long, defaults, Some(long_ns.as_str())),
            (&long, with(Some(1), 0), None),
            (&long, with(Some(30), 0), Some(long_ns.as_str())),
            (&long, with(None, 0), Some(long_ns.as_str())),
        ] {
            let root =
                root_element_within(text.as_bytes(), &limits).map(|(namespace, _)| namespace);
            assert_eq!(root.as_deref(), found, "{limits:?} {text}");
            // A per-type file reads its document type the same way.
            let file = text
                .replacen("<!DOCTYPE r", "<!DOCTYPE mime-type", 1)
                .replace("urn:x", NAMESPACE)
                .replace(
                    "<r xmlns=\"&a;\"/>",
                    "<mime-type xmlns=\"&a;\"><comment>c</comment></mime-type>",
                );
            if text != &long {
                let info = parse_with(&file, &limits);
                let read = if found.is_some() {
                    vec![(None, "c")]
                } else {
                    vec![]
                };
                assert_eq!(comments(&info), read, "{limits:?} {file}");
            }
        }
    }

    #[test]
    fn namespace_resolution_takes_linear_time() {
        // B bindings on the root, then E elements named with the first
        // prefix bound (the farthest back among them) that each bind and
        // use one more, B and E doubling together. Looking a prefix up
        // among all the bindings in scope would be quadratic.
        let text = |n: usize| {
            let mut text = format!("<mime-type xmlns=\"{NAMESPACE}\"{}>", bindings("p", n));
            for _ in 0..n {
                text.push_str("<p0:x xmlns:q=\"urn:q\"><q:y/></p0:x>");
            }
            text.push_str("<comment>a</comment></mime-type>");
            text
        };
        // A tag costs one step and one a byte to be scanned for its
        // bindings, and one and three a byte more where it is checked as
        // another element's (or the root's in root detection). A binding
        // costs two steps, three a byte of its value (checked, normalised,
        // copied) and three more for its ID (one more), six a byte of its
        // prefix (hashed and copied, copied again, hashed out of scope),
        // 22 to look `xmlns` up twice when its tag is checked (bound, and
        // its namespace), and six more to leave scope. A name costs one and
        // two a byte of its prefix. Here a prefix has at most 5 bytes and a
        // value 8 (n at most 2,000): at most 103 steps a binding on the
        // root, and 148 an element with its own (its binding 67, its two
        // names 8, its two tags checked 73), besides the bytes; in root
        // detection, four a byte. The bounds are twice that.
        crate::work::assert_linear(
            1000,
            |n| {
                let text = text(n);
                let (info, steps) =
                    crate::work::steps(|| parse_with(&text, &crate::Limits::unlimited()));
                assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
                assert_eq!(comments(&info), [(None, "a")]);
                (steps, text.len())
            },
            |items, bytes| 2 * (bytes + (103 + 148) * items),
        );
        crate::work::assert_linear(
            1000,
            |n| {
                let text = format!("<p0:r{}/>", bindings("p", n));
                let (found, steps) =
                    crate::work::steps(|| root_element_within(text.as_bytes(), &unbound()));
                assert_eq!(found, Some(("urn:0".into(), "r".into())));
                (steps, text.len())
            },
            |items, bytes| 2 * (4 * bytes + 103 * items),
        );
    }

    #[test]
    fn root_detection_applies_namespaces_in_xml_to_the_root_tag() {
        // As libxml2 does: each gives no root, so the magic type stays.
        for text in [
            "<r xmlns:p=\"\"/>",
            "<?xml version=\"1.0\"?><r xmlns:p=\"\"/>",
            "<xmlns:r/>",
            "<r q:a=\"1\"/>",
            "<r xmlns:p=\"urn:p\" p:a=\"1\" q:b=\"2\"/>",
        ] {
            assert_eq!(root_element(text.as_bytes()), None, "{text:?}");
        }
        // XML 1.1 allows undeclaring; `xml` and a bound prefix are bound.
        for (text, namespace) in [
            ("<?xml version=\"1.1\"?><r xmlns:p=\"\"/>", ""),
            ("<r xml:lang=\"de\"/>", ""),
            ("<p:r xmlns:p=\"urn:p\" p:a=\"1\"/>", "urn:p"),
        ] {
            assert_eq!(
                root_element(text.as_bytes()),
                Some((namespace.into(), "r".into())),
                "{text:?}"
            );
        }
    }

    #[test]
    fn a_namespace_error_on_another_element_skips_that_element() {
        // Namespaces in XML, applied to every element as libxml2 does: the
        // error is reported at the element's tag, the element and what is
        // in it are skipped, and the fields after it are read.
        let head = format!("{OPEN}<comment>en</comment>");
        let tail = "<comment xml:lang=\"de\">X</comment><acronym>A</acronym></mime-type>";
        let skipped = |problem: &str| format!("bad XML ({problem}); the element is skipped");
        let unbound = format!("{UNREADABLE}its prefix is not bound: q:a");
        for (element, at, problem) in [
            (
                "<x xmlns:p=\"\">c<q:y r:z=\"1\"/></x>",
                "<x",
                format!("{UNREADABLE}a prefix's namespace declaration is empty in XML 1.0"),
            ),
            (
                &format!("<x xmlns=\"{XML_REFERENCED}\"/>"),
                "<x",
                format!(
                    "{UNREADABLE}the default namespace cannot be `http://www.w3.org/XML/1998/namespace`"
                ),
            ),
            (
                "<xmlns:x/>",
                "<xmlns",
                "an element's name has the prefix `xmlns`".to_owned(),
            ),
            (
                "<q:x/>",
                "<q:x",
                "an element's prefix is not bound: q".to_owned(),
            ),
            ("<x q:a=\"1\"/>", "<x", unbound.clone()),
            // Nested in another element, at any depth.
            ("<x><y><z q:a=\"1\"/></y></x>", "<z", unbound.clone()),
            // Named as a field, but in no namespace a declaration binds.
            (
                "<q:comment>Y</q:comment>",
                "<q:comment",
                "an element's prefix is not bound: q".to_owned(),
            ),
            // Named as a field, in another namespace.
            (
                "<o:comment xmlns:o=\"urn:o\" q:a=\"1\">Y</o:comment>",
                "<o:comment",
                unbound.clone(),
            ),
        ] {
            let text = format!("{head}{element}{tail}");
            let info = parse(&text);
            assert_eq!(
                info.diagnostics,
                [Diagnostic::new(
                    "t.xml",
                    text.find(at).map(crate::Location::Offset),
                    skipped(&problem)
                )],
                "{text:?}"
            );
            assert_eq!(
                comments(&info),
                [(None, "en"), (Some("de"), "X")],
                "{text:?}"
            );
            assert_eq!(info.acronym.as_deref(), Some("A"), "{text:?}");
        }
        // Allowed: undeclaring in XML 1.1, `xml`, and bound prefixes.
        for (declaration, element) in [
            ("<?xml version=\"1.1\"?>", "<x xmlns:p=\"\"/>"),
            ("", "<x xml:lang=\"de\"><xml:y/></x>"),
            ("", "<p:x xmlns:p=\"urn:p\" p:a=\"1\"><y p:b=\"2\"/></p:x>"),
        ] {
            let text = format!("{declaration}{head}{element}{tail}");
            let info = parse(&text);
            assert!(
                info.diagnostics.is_empty(),
                "{text:?}: {:?}",
                info.diagnostics
            );
            assert_eq!(comments(&info), [(None, "en"), (Some("de"), "X")]);
        }
    }

    #[test]
    fn an_unbound_prefix_in_a_field_or_the_root_tag_drops_fields() {
        // As other namespace errors there.
        let head = format!("{OPEN}<comment>en</comment>");
        let tail = "<acronym>A</acronym></mime-type>";
        let left_out = |problem: &str| format!("bad XML ({problem}); the value is left out");
        let attribute = left_out(&format!("{UNREADABLE}its prefix is not bound: q:a"));
        let element = left_out("an element's prefix is not bound: q");
        for (field, mark, message) in [
            (
                "<comment xml:lang=\"de\" q:a=\"1\">X</comment>",
                "q:a",
                &attribute,
            ),
            (
                "<comment xml:lang=\"de\">a<b q:a=\"1\"/>d</comment>",
                "q:a",
                &attribute,
            ),
            (
                "<comment xml:lang=\"de\">a<b><q:i>c</q:i></b>d</comment>",
                "q:i>c",
                &element,
            ),
        ] {
            check_dropped(
                &format!("{head}{field}{tail}"),
                &[(None, "en")],
                mark,
                message,
            );
        }
        let head = format!(r#"{}<comment xml:lang="fr">F</comment>"#, root("q:a=\"1\""));
        let text = format!("{head}<comment>D</comment></mime-type>");
        let info = parse(&text);
        assert_eq!(
            info.diagnostics,
            [Diagnostic::new(
                "t.xml",
                Some(crate::Location::Offset(head.len())),
                format!(
                    "bad XML (the root element's {TAG}{UNREADABLE}its prefix is not bound: q:a); the value is left out"
                )
            )]
        );
        assert_eq!(comments(&info), [(Some("fr"), "F")]);
    }

    #[test]
    fn a_name_not_a_qname_or_an_attribute_repeated_in_a_namespace_is_an_error() {
        // As libxml2 applies Namespaces in XML (1.0, 3rd ed., 6.3: no two
        // attributes with the same expanded name; 7: names are `QName`s).
        let head = format!("{OPEN}<comment>en</comment>");
        let tail = "<comment xml:lang=\"de\">X</comment><acronym>A</acronym></mime-type>";
        let not_qname = "an element's name is not an XML name";
        let twice = format!("{UNREADABLE}an attribute appears twice in its namespace: q:a");
        let pair = "xmlns:p=\"urn:a\" xmlns:q=\"urn:&#97;\" p:a=\"1\" q:a=\"2\"";
        for (element, at, problem) in [
            ("<x:y:z/>", "<x:y", not_qname.to_owned()),
            (
                "<comment: xml:lang=\"de\">Y</comment:>",
                "<comment:",
                not_qname.to_owned(),
            ),
            ("<x a:b:c=\"1\"/>", "<x", not_a_name()),
            ("<x :a=\"1\"/>", "<x", not_a_name()),
            (&format!("<x {pair}/>"), "<x", twice.clone()),
            (&format!("<x><y {pair}/></x>"), "<y", twice.clone()),
        ] {
            let text = format!("{head}{element}{tail}");
            let info = parse(&text);
            assert_eq!(
                info.diagnostics,
                [Diagnostic::new(
                    "t.xml",
                    text.find(at).map(crate::Location::Offset),
                    format!("bad XML ({problem}); the element is skipped")
                )],
                "{text:?}"
            );
            assert_eq!(
                comments(&info),
                [(None, "en"), (Some("de"), "X")],
                "{text:?}"
            );
        }
        // Allowed: one prefixed and one not, or two namespaces.
        for element in [
            "<x xmlns:p=\"urn:a\" p:a=\"1\" a=\"2\"/>",
            "<x xmlns:p=\"urn:a\" xmlns:q=\"urn:b\" p:a=\"1\" q:a=\"2\"/>",
        ] {
            let info = parse(&format!("{head}{element}{tail}"));
            assert!(
                info.diagnostics.is_empty(),
                "{element}: {:?}",
                info.diagnostics
            );
        }
        // On a field's tag or one inside it, the field is dropped.
        let left_out = format!("bad XML ({twice}); the value is left out");
        for field in [
            format!("<comment {pair} xml:lang=\"de\">X</comment>"),
            format!("<comment xml:lang=\"de\">a<b {pair}/>d</comment>"),
        ] {
            check_dropped(
                &format!("{head}{field}<acronym>A</acronym></mime-type>"),
                &[(None, "en")],
                "q:a",
                &left_out,
            );
        }
        // On the root's, its language is not known.
        let start = format!(r#"{}<comment xml:lang="fr">F</comment>"#, root(pair));
        let text = format!("{start}<comment>D</comment></mime-type>");
        assert_eq!(
            parse(&text).diagnostics,
            [Diagnostic::new(
                "t.xml",
                Some(crate::Location::Offset(start.len())),
                format!("bad XML (the root element's {TAG}{twice}); the value is left out")
            )]
        );
        // Root detection gives no root.
        for text in [format!("<r {pair}/>"), "<r a:b:c=\"1\"/>".to_owned()] {
            assert_eq!(root_element(text.as_bytes()), None, "{text:?}");
        }
    }

    #[test]
    fn a_repeated_attribute_skips_another_element_and_gives_no_root() {
        // As on a field's tag, and as libxml2 reports it.
        let head = format!("{OPEN}<comment>en</comment>");
        let tail = "<comment xml:lang=\"de\">X</comment><acronym>A</acronym></mime-type>";
        for (element, at, name) in [
            ("<x a=\"1\" a=\"2\"/>", "<x", "a"),
            (
                "<x><y p:a=\"1\" xmlns:p=\"urn:p\" p:a=\"2\"/></x>",
                "<y",
                "p:a",
            ),
            ("<x xmlns:p=\"urn:p\" xmlns:p=\"urn:q\"/>", "<x", "xmlns:p"),
        ] {
            let text = format!("{head}{element}{tail}");
            let info = parse(&text);
            assert_eq!(
                info.diagnostics,
                [Diagnostic::new(
                    "t.xml",
                    text.find(at).map(crate::Location::Offset),
                    format!("bad XML ({}); the element is skipped", twice(name))
                )],
                "{text:?}"
            );
            assert_eq!(
                comments(&info),
                [(None, "en"), (Some("de"), "X")],
                "{text:?}"
            );
        }
        for text in [
            "<r a=\"1\" a=\"2\"/>",
            "<r xmlns=\"urn:r\" xmlns=\"urn:r\"/>",
        ] {
            assert_eq!(root_element(text.as_bytes()), None, "{text:?}");
        }
    }

    #[test]
    fn a_root_tag_with_any_attribute_error_gives_no_root() {
        // The checks a per-type root's tag has, not only those of its
        // namespace declarations, so the two agree.
        for text in [
            "<r a=\"&bogus;\"/>",
            "<r xml:lang=\"&bogus;\"/>",
            "<r a/>",
            "<r a=b/>",
            "<r a=\"1/>",
            "<r a=\"<\"/>",
            "<r a=\"&#1;\"/>",
            "<r 1a=\"x\"/>",
            "<r xmlns=\"urn:r\" a=\"&bogus;\"/>",
        ] {
            assert_eq!(root_element(text.as_bytes()), None, "{text:?}");
        }
        for (text, namespace) in [
            ("<r a=\"&amp;&#x41;\" xml:lang=\"de\"/>", ""),
            ("<?xml version=\"1.1\"?><r a=\"&#1;\"/>", ""),
            ("<r xmlns=\"urn:r\" b=\"1\"/>", "urn:r"),
        ] {
            assert_eq!(
                root_element(text.as_bytes()),
                Some((namespace.into(), "r".into())),
                "{text:?}"
            );
        }
        // So content detection keeps the type magic gives.
        let db = MimeDatabase::from_files(&[vec![
            (
                "magic",
                b"MIME-Magic\0\n[50:application/xml]\n>0=\x00\x02<r\n".as_slice(),
            ),
            ("subclasses", b"application/x-r+xml application/xml\n"),
            ("XMLnamespaces", b"urn:r r application/x-r+xml\n"),
        ]]);
        let found = db.type_for_data(b"<r xmlns=\"urn:r\" b=\"1\"/>");
        assert_eq!(found.mime.as_str(), "application/x-r+xml");
        let found = db.type_for_data(b"<r xmlns=\"urn:r\" a=\"&bogus;\"/>");
        assert_eq!(found.mime.as_str(), "application/xml");
        assert_eq!(found.decided_by, crate::DecidedBy::Content);
    }

    #[test]
    fn any_attribute_error_on_another_element_skips_it() {
        // The checks field and root tags have, not only the namespace and
        // repeat checks, so all three agree, as libxml2 reports these.
        let head = format!("{OPEN}<comment>en</comment>");
        let tail = "<comment xml:lang=\"de\">X</comment><acronym>A</acronym></mime-type>";
        for (declaration, element, at, problem) in [
            (
                "",
                "<x a=\"&bogus;\"/>",
                "<x",
                "the reference `&bogus;` cannot be replaced",
            ),
            (
                "",
                "<x a=\"&#1;\"/>",
                "<x",
                "the reference `&#1;` cannot be replaced",
            ),
            (
                "",
                "<x a=\"\u{1}\"/>",
                "<x",
                "a character XML does not allow, U+0001",
            ),
            (
                "",
                "<x a=\"<\"/>",
                "<x",
                "an attribute cannot be read: `<` in a value",
            ),
            (
                "",
                "<x a/>",
                "<x",
                "an attribute cannot be read: a name is not followed by `=`",
            ),
            (
                "",
                "<x a=b/>",
                "<x",
                "an attribute cannot be read: a value is not quoted",
            ),
            (
                "",
                "<x><y b=\"1\" a=\"&bogus;\"/></x>",
                "<y",
                "the reference `&bogus;` cannot be replaced",
            ),
            (
                "<?xml version=\"1.1\"?>",
                "<x a=\"\u{1}\"/>",
                "<x",
                "a character XML does not allow, U+0001",
            ),
        ] {
            let text = format!("{declaration}{head}{element}{tail}");
            let info = parse(&text);
            assert_eq!(
                info.diagnostics,
                [Diagnostic::new(
                    "t.xml",
                    text.find(at).map(crate::Location::Offset),
                    format!("bad XML ({problem}); the element is skipped")
                )],
                "{text:?}"
            );
            assert_eq!(
                comments(&info),
                [(None, "en"), (Some("de"), "X")],
                "{text:?}"
            );
        }
        // Allowed: references XML predefines, and in XML 1.1 `&#1;`.
        for (declaration, element) in [
            ("", "<x a=\"&amp;&#x41;\" b='\"'/>"),
            ("<?xml version=\"1.1\"?>", "<x a=\"&#1;\"/>"),
        ] {
            let text = format!("{declaration}{head}{element}{tail}");
            let info = parse(&text);
            assert!(
                info.diagnostics.is_empty(),
                "{text:?}: {:?}",
                info.diagnostics
            );
        }
    }

    #[test]
    fn repeated_attributes_are_found_in_linear_time_with_long_namespaces() {
        // A namespace of 100n bytes and n prefixed attributes, on n other
        // elements or on the root. An attribute that hashed its whole
        // namespace would make it quadratic. A byte of the namespace costs at
        // most seven steps (scanned with its tag, checked and normalised,
        // copied, and hashed and copied for its ID), in root detection four
        // more (the root's tag checked as a per-type root's and for namespace
        // errors); a byte elsewhere fewer, and an attribute at most 20
        // besides its bytes. The bounds are twice that.
        let long = |n: usize| format!("urn:{}", "n".repeat(100 * n));
        crate::work::assert_linear(
            100,
            |n| {
                let mut text = format!("<mime-type xmlns=\"{NAMESPACE}\" xmlns:p=\"{}\">", long(n));
                for _ in 0..n {
                    text.push_str("<x p:a=\"1\" p:b=\"2\"/>");
                }
                text.push_str("<comment>a</comment></mime-type>");
                let (info, steps) =
                    crate::work::steps(|| parse_with(&text, &crate::Limits::unlimited()));
                assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
                (steps, text.len())
            },
            |items, bytes| 2 * (7 * bytes + 20 * items),
        );
        crate::work::assert_linear(
            100,
            |n| {
                let mut text = format!("<r xmlns:p=\"{}\"", long(n));
                for at in 0..n {
                    let _ = std::fmt::Write::write_fmt(&mut text, format_args!(" p:a{at}=\"1\""));
                }
                text.push_str("/>");
                let (found, steps) =
                    crate::work::steps(|| root_element_within(text.as_bytes(), &unbound()));
                assert_eq!(found.map(|(_, local)| local).as_deref(), Some("r"));
                (steps, text.len())
            },
            |items, bytes| 2 * (11 * bytes + 20 * items),
        );
    }

    #[test]
    fn an_attribute_with_no_white_space_before_it_is_an_error() {
        // XML 1.0, 3.1 (`S` before each `Attribute`); libxml2: "attributes
        // construct error"; `quick-xml` alone would read it as an attribute.
        let problem = format!("{UNREADABLE}a name is not preceded by white space");
        let head = format!("{OPEN}<comment>en</comment>");
        let tail = "<acronym>A</acronym></mime-type>";
        check_dropped(
            &format!("{head}<comment a=\"1\"xml:lang=\"de\">X</comment>{tail}"),
            &[(None, "en")],
            "xml:lang=\"de\">X",
            &format!("bad XML ({problem}); the value is left out"),
        );
        let text = format!("{head}<x a='1'b=\"2\"/><comment xml:lang=\"de\">X</comment>{tail}");
        assert_eq!(
            parse(&text).diagnostics,
            [Diagnostic::new(
                "t.xml",
                text.find("<x").map(crate::Location::Offset),
                format!("bad XML ({problem}); the element is skipped")
            )]
        );
        let start = format!(
            r#"{}<comment xml:lang="fr">F</comment>"#,
            root("a=\"1\"b=\"2\"")
        );
        let text = format!("{start}<comment>D</comment></mime-type>");
        assert_eq!(
            parse(&text).diagnostics,
            [Diagnostic::new(
                "t.xml",
                Some(crate::Location::Offset(start.len())),
                format!("bad XML (the root element's {TAG}{problem}); the value is left out")
            )]
        );
        assert_eq!(root_element(b"<r a=\"1\"b=\"2\"/>"), None);
        assert!(root_element(b"<r a=\"1\"\tb=\"2\"/>").is_some());
    }

    #[test]
    fn what_is_inside_a_skipped_element_or_a_field_is_not_checked_again() {
        // Only the element skipped, or the field, is reported: what is in
        // it is not checked as another element is, also after an element
        // inside it has ended.
        let head = format!("{OPEN}<comment>en</comment>");
        let tail = "<acronym>A</acronym></mime-type>";
        for (inside, at, message) in [
            (
                "<x a=\"&bogus;\"><y/><q:z/></x>",
                "<x",
                "bad XML (the reference `&bogus;` cannot be replaced); the element is skipped",
            ),
            (
                "<comment xml:lang=\"de\">a<b x=\"&bogus;\"/><q:c/>d</comment>",
                "x=\"&",
                "bad XML (the reference `&bogus;` cannot be replaced); the value is left out",
            ),
            (
                "<comment xml:lang=\"de\">a<x:y:z/><b/><q:c/>d</comment>",
                "x:y:z",
                "bad XML (an element's name is not an XML name); the value is left out",
            ),
        ] {
            let text = format!("{head}{inside}{tail}");
            let info = parse(&text);
            assert_eq!(
                info.diagnostics,
                [Diagnostic::new(
                    "t.xml",
                    text.find(at).map(crate::Location::Offset),
                    message
                )],
                "{text:?}"
            );
            assert_eq!(comments(&info), [(None, "en")], "{text:?}");
        }
        // After it ends, elements are checked again.
        let text = format!("{head}<x a=\"&bogus;\"><y/></x><q:z/>{tail}");
        assert_eq!(parse(&text).diagnostics.len(), 2, "{text:?}");
    }

    #[test]
    fn another_elements_tag_is_checked_as_a_fields_is() {
        // In attribute order, as on a field's tag, and its own declarations
        // before its prefix (an empty one in XML 1.0 is why it is unbound).
        let head = format!("{OPEN}<comment>en</comment>");
        let tail = "<acronym>A</acronym></mime-type>";
        for (element, problem) in [
            (
                "<x a=\"&bogus;\" q:b=\"1\"/>",
                "the reference `&bogus;` cannot be replaced".to_owned(),
            ),
            (
                "<x q:b=\"1\" a=\"&bogus;\"/>",
                format!("{UNREADABLE}its prefix is not bound: q:b"),
            ),
            (
                "<p:x xmlns:p=\"\"/>",
                format!("{UNREADABLE}a prefix's namespace declaration is empty in XML 1.0"),
            ),
        ] {
            let text = format!("{head}{element}{tail}");
            assert_eq!(
                messages(&parse(&text)),
                [format!("bad XML ({problem}); the element is skipped")],
                "{text:?}"
            );
        }
    }

    /// A document type declaring `ns_extend` and `ns_ai` as Illustrator
    /// does, `w`, `lit` (not usable: it holds `<`), and a parameter entity
    /// `pe`, not referred to (a reference would make undeclared entities
    /// unknown).
    const DTD: &str = "<!DOCTYPE svg [\n<!ENTITY ns_extend \"http://ns.adobe.com/Extensibility/1.0/\">\n<!ENTITY ns_ai 'http://ns.adobe.com/AdobeIllustrator/10.0/'>\n<!-- <!ENTITY c \"x\"> --><?pi <!ENTITY p \"x\">?>\n<!ENTITY lit \"<!ENTITY l 'x'>\"><!ENTITY w \"5\"><!ENTITY % pe \"x\">\n<!ELEMENT svg ANY>\n]>";

    #[test]
    fn an_entity_the_internal_subset_declares_is_unknown_in_root_detection() {
        // Not an error; a namespace with an unknown value leaves its prefix
        // unknown, an error only for an element's name: each root below is
        // found.
        let svg = Some(("http://www.w3.org/2000/svg".to_owned(), "svg".to_owned()));
        for root in [
            "<svg xmlns:x=\"&ns_extend;\" xmlns=\"http://www.w3.org/2000/svg\">",
            "<svg xmlns:x=\"&ns_extend;\" xmlns:i=\"&ns_ai;\" xmlns=\"http://www.w3.org/2000/svg\" i:viewOrigin=\"1 2\" i:pageBounds=\"0\" x:a=\"1\" x:b=\"&w;\" width=\"&w;px\">",
            "<svg xmlns=\"http://www.w3.org/2000/svg\" xml:lang=\"&w;\">",
        ] {
            let text = format!("{DTD}{root}");
            assert_eq!(root_element(text.as_bytes()), svg, "{text:?}");
        }
        assert_eq!(
            root_element(b"<!DOCTYPE r [<!ENTITY w \"5\">]><r xmlns=\"urn:r\" width=\"&w;\"/>"),
            Some(("urn:r".into(), "r".into()))
        );
        for root in [
            // Not declared, or not by the internal subset's own markup.
            "<svg a=\"&c;\">",
            "<svg a=\"&p;\">",
            "<svg a=\"&l;\">",
            "<svg a=\"&pe;\">",
            "<svg a=\"&undeclared;\">",
            // Not the `xml` namespace.
            "<svg xmlns:xml=\"&ns_extend;\">",
            // One namespace, replaced, so a repeat.
            "<svg xmlns:x=\"&ns_extend;\" xmlns:i=\"&ns_extend;\" i:a=\"1\" x:a=\"2\">",
        ] {
            let text = format!("{DTD}{root}");
            assert_eq!(root_element(text.as_bytes()), None, "{text:?}");
        }
        // Namespaces that stay unknown: entities not declared in a
        // document with an external subset. Two are not equal; an
        // element's name in one is an error, and unknown is not `xml`'s.
        let open = "<!DOCTYPE svg SYSTEM \"x\">";
        let text = format!("{open}<svg xmlns:x=\"&u;\" xmlns:i=\"&u;\" i:a=\"1\" x:a=\"2\">");
        assert_eq!(
            root_element(text.as_bytes()),
            Some((String::new(), "svg".into()))
        );
        for root in [
            "<svg xmlns=\"&u;\">",
            "<x:svg xmlns:x=\"&u;\">",
            "<svg xmlns:xml=\"&u;\">",
        ] {
            let text = format!("{open}{root}");
            assert_eq!(root_element(text.as_bytes()), None, "{text:?}");
        }
        // Only the first document type declares.
        let text = "<!DOCTYPE r><!DOCTYPE r [<!ENTITY w \"5\">]><r a=\"&w;\"/>";
        assert_eq!(root_element(text.as_bytes()), None);
    }

    #[test]
    fn an_entity_the_internal_subset_declares_is_unknown_in_a_per_type_file() {
        // `u` is not declared, in a document with an external subset: so
        // the namespace `i` is bound to stays unknown.
        let dtd = "<!DOCTYPE mime-type SYSTEM \"x\" [<!ENTITY w \"5\"><!ENTITY l \"de\">]>";
        let open =
            format!("{dtd}<mime-type xmlns=\"{NAMESPACE}\" xmlns:i=\"&u;\" i:a=\"1\" b=\"&w;\">");
        // Attributes with unknown values, and prefixes bound to unknown
        // namespaces on attributes, are read.
        let text = format!(
            "{open}<comment w=\"&w;\" i:b=\"2\">a</comment><x i:c=\"&w;\"/><acronym>A</acronym></mime-type>"
        );
        let info = parse(&text);
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(comments(&info), [(None, "a")]);
        let reference = |name: &str| format!("the reference `&{name};` cannot be replaced");
        // A field whose language or text is unknown is dropped.
        for (field, mark, name) in [
            ("<comment xml:lang=\"&l;\">X</comment>", "xml:lang", "l"),
            ("<comment xml:lang=\"de\">&w;</comment>", "&w;</", "w"),
            // An element's name in an unknown namespace: a field's own,
            (
                "<i:comment xml:lang=\"de\">X</i:comment>",
                "<i:comment",
                "u",
            ),
            // or a tag's inside it.
            ("<comment xml:lang=\"de\">a<i:b/>d</comment>", "i:b/>", "u"),
        ] {
            let text =
                format!("{open}<comment>en</comment>{field}<acronym>A</acronym></mime-type>");
            let at = text.rfind(mark).unwrap();
            let info = parse(&text);
            assert_eq!(
                info.diagnostics,
                [Diagnostic::new(
                    "t.xml",
                    Some(crate::Location::Offset(at)),
                    format!("bad XML ({}); the value is left out", reference(name))
                )],
                "{text:?}"
            );
            assert_eq!(comments(&info), [(None, "en")], "{text:?}");
        }
        // Another element's: it is skipped.
        let text = format!("{open}<i:x/><comment>en</comment><acronym>A</acronym></mime-type>");
        assert_eq!(
            parse(&text).diagnostics,
            [Diagnostic::new(
                "t.xml",
                text.find("<i:x").map(crate::Location::Offset),
                format!("bad XML ({}); the element is skipped", reference("u"))
            )]
        );
        // The root's language unknown: fields without their own dropped.
        let text = format!(
            "{dtd}<mime-type xmlns=\"{NAMESPACE}\" xml:lang=\"&l;\"><comment xml:lang=\"fr\">F</comment><comment>D</comment></mime-type>"
        );
        let info = parse(&text);
        assert_eq!(
            messages(&info),
            [format!(
                "bad XML (the root element's `xml:lang` cannot be read: {}); the value is left out",
                reference("l")
            )]
        );
        assert_eq!(comments(&info), [(Some("fr"), "F")]);
        // The root's name in an unknown namespace: not a per-type file.
        let text =
            format!("{dtd}<m:mime-type xmlns:m=\"&u;\"><m:comment>a</m:comment></m:mime-type>");
        let info = parse(&text);
        assert_eq!(comments(&info), []);
        assert!(
            messages(&info)[0].starts_with("not a per-type file"),
            "{:?}",
            info.diagnostics
        );
        // Undeclared stays an error in a standalone document.
        let text = format!(
            "<!DOCTYPE mime-type>{OPEN}<x a=\"&undeclared;\"/><comment>en</comment></mime-type>"
        );
        assert_eq!(
            messages(&parse(&text)),
            [format!(
                "bad XML ({}); the element is skipped",
                reference("undeclared")
            )]
        );
    }

    #[test]
    fn the_internal_subset_is_read_in_linear_time() {
        // n declarations, each referring to the one before, and a literal of
        // n bytes, read by both readers. Counted from the code, an entity's
        // declaration of B bytes, its name k and its literal L, referring to
        // a name of r bytes, costs 3B + 5k + 8L + 2r + 15: each byte looked
        // at for the characters written, by the grammar and for `%`; a
        // literal's bytes again twice for `%` and its references, once for
        // bad references, twice for its replacement text, and once each for
        // line ends, its own length and its references; a name's once to scan
        // it, once to reach the literal and three times hashed and copied;
        // the name referred to hashed and compared; and 15 the passes' and
        // the entity's own. Here k <= 5, L <= 8 and r <= 4, so a declaration
        // costs at most 3B + 112, and the long literal 8 a byte more: 3 a
        // byte and 120 an item, besides the head and the root's tag (a few
        // hundred). The bounds are twice that.
        let dtd = |n: usize| {
            let mut dtd = format!(
                "<!DOCTYPE r [<!ENTITY long \"{}\"><!ENTITY e0 \"v\">",
                "x".repeat(n)
            );
            for at in 1..n {
                let _ = std::fmt::Write::write_fmt(
                    &mut dtd,
                    format_args!("<!ENTITY e{at} \"&e{};x\">", at - 1),
                );
            }
            dtd.push_str("]>");
            dtd
        };
        crate::work::assert_linear(
            1000,
            |n| {
                let text = format!("{}<r xmlns=\"urn:r\" a=\"&e{};\"/>", dtd(n), n - 1);
                let (found, steps) = crate::work::steps(|| root_element(text.as_bytes()));
                assert!(found.is_some());
                (steps, text.len())
            },
            |items, bytes| 2 * (3 * bytes + 120 * items + 200),
        );
        crate::work::assert_linear(
            1000,
            |n| {
                let text = format!(
                    "{}{OPEN}<comment a=\"&e0;\">a</comment></mime-type>",
                    dtd(n)
                );
                let (info, steps) = crate::work::steps(|| parse(&text));
                assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
                (steps, text.len())
            },
            |items, bytes| 2 * (3 * bytes + 120 * items + 400),
        );
    }

    #[test]
    fn a_reference_to_an_external_or_unparsed_entity_stays_an_error() {
        // XML 1.0, 4.4.4 (WFC: No External Entity References) and 4.4.3:
        // only internal entities give an unknown value; libxml2 agrees.
        let dtd = |kind: &str| {
            format!(
                "<!DOCTYPE {kind} [<!ENTITY e SYSTEM \"x.ent\"><!ENTITY u PUBLIC \"-//x//EN\" \"y.ent\">\n<!ENTITY n SYSTEM \"x.gif\" NDATA gif><!ENTITY i 'v'>]>"
            )
        };
        for (name, kept) in [("e", false), ("u", false), ("n", false), ("i", true)] {
            let text = format!("{}<r a=\"&{name};\"/>", dtd("r"));
            assert_eq!(root_element(text.as_bytes()).is_some(), kept, "{text:?}");
            let text = format!(
                "{}{OPEN}<comment>en</comment><comment xml:lang=\"de\" a=\"&{name};\">X</comment><acronym>A</acronym></mime-type>",
                dtd("mime-type")
            );
            if kept {
                assert_eq!(comments(&parse(&text)), [(None, "en"), (Some("de"), "X")]);
            } else {
                check_dropped(
                    &text,
                    &[(None, "en")],
                    &format!("a=\"&{name};"),
                    &format!(
                        "bad XML (the reference `&{name};` cannot be replaced); the value is left out"
                    ),
                );
            }
        }
    }

    /// `text`'s root element, if any, for `entities` declared before it.
    fn root_after(dtd: &str, root: &str) -> Option<(String, String)> {
        root_element(format!("{dtd}{root}").as_bytes())
    }

    #[test]
    fn only_usable_internal_entities_give_an_unknown_value() {
        // A literal with no `<` (written, or by a character reference) whose
        // references name usable entities; anything else is an error where
        // used, as libxml2 reports it ("not allowed in attributes values", a
        // loop).
        let dtd = "<!DOCTYPE r [<!ENTITY ok \"&lt;&#65;&amp;\"><!ENTITY lt1 \"x<y\"><!ENTITY lt2 \"&#60;\"><!ENTITY lt3 '&#x3c;'><!ENTITY b \"v\"><!ENTITY chained \"&b;&ok;\"><!ENTITY forward \"&later;\"><!ENTITY later \"v\"><!ENTITY self \"&self;\"><!ENTITY undeclared \"&nothing;\"><!ENTITY formed \"x&#38;y\"><!ENTITY via \"&lt1;\">]>";
        for (name, usable) in [
            ("ok", true),
            ("chained", true),
            ("later", true),
            ("lt1", false),
            ("lt2", false),
            ("lt3", false),
            // A reference to one declared later is allowed.
            ("forward", true),
            ("self", false),
            ("undeclared", false),
            // `%p;` in a literal refuses the whole document type (see
            // `a_parameter_entity_reference_in_a_literal_refuses_the_document_type`).
            // A bare `&` that only the replacement text forms: unusable,
            // while a literal that holds a bad reference refuses the whole
            // document type (see `every_literal_in_the_subset_is_well_formed`),
            // and so does one with a character not allowed written (see
            // `a_character_the_version_does_not_allow_refuses_the_subset`).
            ("formed", false),
            ("via", false),
        ] {
            let found = root_after(dtd, &format!("<r a=\"&{name};\"/>"));
            assert_eq!(found.is_some(), usable, "{name}");
            let text = format!(
                "{}{OPEN}<comment>en</comment><x a=\"&{name};\"/></mime-type>",
                dtd.replacen("DOCTYPE r", "DOCTYPE mime-type", 1)
            );
            let expected: &[&str] = if usable {
                &[]
            } else {
                &[&format!(
                    "bad XML (the reference `&{name};` cannot be replaced); the element is skipped"
                )]
            };
            assert_eq!(messages(&parse(&text)), expected, "{name}");
        }
    }

    #[test]
    fn an_undeclared_entity_is_unknown_only_with_an_external_subset_or_parameter_entities() {
        // Then it may be declared where it is not read (a validity problem
        // only, XML 1.0, 4.1); in a standalone document it is an error.
        for (dtd, kept) in [
            ("<!DOCTYPE r SYSTEM \"x.dtd\">", true),
            ("<!DOCTYPE r PUBLIC \"-//x//EN\" \"x.dtd\">", true),
            ("<!DOCTYPE r SYSTEM 'x.dtd' []>", true),
            ("<!DOCTYPE r [<!ENTITY % p \"\"> %p;]>", true),
            ("<!DOCTYPE r [<!ENTITY % p \"\">]>", false),
            ("<!DOCTYPE r [<!-- %p; --><!ENTITY a \"%p;\">]>", false),
            ("<!DOCTYPE r>", false),
            ("", false),
        ] {
            assert_eq!(root_after(dtd, "<r a=\"&u;\"/>").is_some(), kept, "{dtd:?}");
        }
        // Not an unusable one, nor an external one, nor an element's
        // namespace.
        for root in ["<r a=\"&lt1;\"/>", "<r a=\"&e;\"/>", "<r xmlns=\"&u;\"/>"] {
            let dtd = "<!DOCTYPE r SYSTEM \"x\" [<!ENTITY lt1 \"<\"><!ENTITY e SYSTEM \"y\">]>";
            assert_eq!(root_after(dtd, root), None, "{root}");
        }
        // In a per-type file too.
        let text = format!(
            "<!DOCTYPE mime-type SYSTEM \"x\">{OPEN}<comment a=\"&u;\">a</comment><x b=\"&u;\"/></mime-type>"
        );
        let info = parse(&text);
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(comments(&info), [(None, "a")]);
    }

    #[test]
    fn some_entity_declarations_declare_nothing() {
        // XML 1.0, 2.8, 4.2; Namespaces in XML 1.0, 3rd ed., 7: the first
        // declaration binds; `<!doctype` in another case is not a document
        // type declaration; a name needs white space after it; a name with
        // `:` is a namespace error and declares nothing.
        for (dtd, root, kept) in [
            (
                "<!DOCTYPE r [<!ENTITY e SYSTEM \"x\"><!ENTITY e \"v\">]>",
                "<r a=\"&e;\"/>",
                false,
            ),
            (
                "<!DOCTYPE r [<!ENTITY e \"v\"><!ENTITY e SYSTEM \"x\">]>",
                "<r a=\"&e;\"/>",
                true,
            ),
            (
                "<!DOCTYPE r [<!ENTITY e \"<\"><!ENTITY e \"v\">]>",
                "<r a=\"&e;\"/>",
                false,
            ),
            ("<!doctype r [<!ENTITY a \"v\">]>", "<r b=\"&a;\"/>", false),
            ("<!DocType r [<!ENTITY a \"v\">]>", "<r b=\"&a;\"/>", false),
            ("<!doctype r SYSTEM \"x\">", "<r b=\"&a;\"/>", false),
            ("<!DOCTYPE r [<!ENTITY a\"v\">]>", "<r b=\"&a;\"/>", false),
            (
                "<!DOCTYPE r [<!ENTITY a:b \"v\">]>",
                "<r c=\"&a:b;\"/>",
                false,
            ),
            // Not declared, so unknown where undeclared ones are.
            (
                "<!DOCTYPE r SYSTEM \"x\" [<!ENTITY a:b \"<\">]>",
                "<r c=\"&a:b;\"/>",
                true,
            ),
        ] {
            assert_eq!(root_after(dtd, root).is_some(), kept, "{dtd:?}");
        }
        // A document type after the root's start tag declares nothing.
        let text = format!(
            "{OPEN}<!DOCTYPE mime-type [<!ENTITY w \"5\">]><x a=\"&w;\"/><comment>a</comment></mime-type>"
        );
        let info = parse(&text);
        assert_eq!(
            messages(&info),
            [
                "bad XML (a document type declaration not before the root element)",
                "bad XML (the reference `&w;` cannot be replaced); the element is skipped",
            ]
        );
        assert_eq!(comments(&info), [(None, "a")]);
    }

    #[test]
    fn line_ends_between_attributes_and_the_xml_prefixs_id() {
        // CR, LF and CRLF are white space between attributes; `xml:a` and
        // `p:a`, `p` the first prefix bound (ID 0), are two names.
        for tag in [
            "<r a=\"1\"\rb=\"2\"\nc=\"3\"\r\nd=\"4\"/>",
            "<r xmlns:p=\"urn:p\" xml:a=\"1\" p:a=\"2\"/>",
        ] {
            assert!(root_element(tag.as_bytes()).is_some(), "{tag:?}");
            let text = format!(
                "{OPEN}{}<comment>a</comment></mime-type>",
                tag.replace("<r", "<x")
            );
            let info = parse(&text);
            assert!(
                info.diagnostics.is_empty(),
                "{tag:?}: {:?}",
                info.diagnostics
            );
            assert_eq!(comments(&info), [(None, "a")]);
        }
    }

    #[test]
    fn entity_references_are_walked_as_a_graph() {
        // Forward references are allowed; a loop of any length, and what
        // refers to one, is refused. A reference the replacement text forms
        // (`&#38;u;` stands for `&u;`) counts too.
        let dtd = "<!DOCTYPE r [<!ENTITY fwd \"&later;x\"><!ENTITY later \"&last;\"><!ENTITY last \"v\"><!ENTITY self \"&self;\"><!ENTITY a \"&b;\"><!ENTITY b \"&a;\"><!ENTITY c1 \"&c2;\"><!ENTITY c2 \"&c3;\"><!ENTITY c3 \"&c1;\"><!ENTITY to_loop \"&a;\"><!ENTITY formed \"&#38;u;\"><!ENTITY lt_text \"&#38;#60;\"><!ENTITY dup \"<\"><!ENTITY dup \"v\">]>";
        for (name, usable) in [
            ("fwd", true),
            ("later", true),
            ("self", false),
            ("a", false),
            ("b", false),
            ("c1", false),
            ("c3", false),
            ("to_loop", false),
            ("formed", false),
            ("lt_text", true),
            ("dup", false),
        ] {
            assert_eq!(
                root_after(dtd, &format!("<r x=\"&{name};\"/>")).is_some(),
                usable,
                "{name}"
            );
        }
    }

    #[test]
    fn a_namespace_declared_by_entities_is_their_replacement_text() {
        // In namespace declarations only, usable internal entities are
        // replaced (XML 1.0, 3.3.3: their white space becomes a space), then
        // checked as a value written out is.
        let svg = "http://www.w3.org/2000/svg";
        let illustrator = format!(
            "<!DOCTYPE svg [\n\t<!ENTITY ns_svg \"{svg}\">\n\t<!ENTITY ns_xlink \"http://www.w3.org/1999/xlink\">\n\t<!ENTITY ns_extend \"http://ns.adobe.com/Extensibility/1.0/\">\n]>\n<svg version=\"1.0\" xmlns:x=\"&ns_extend;\" xmlns=\"&ns_svg;\" xmlns:xlink=\"&ns_xlink;\" width=\"33\">"
        );
        assert_eq!(
            root_element(illustrator.as_bytes()),
            Some((svg.into(), "svg".into()))
        );
        let dtd = "<!DOCTYPE r [<!ENTITY ns \"urn:&part;x\"><!ENTITY part \"a&#10;b:\"><!ENTITY xml \"http://www.w3.org/XML/1998/namespace\"><!ENTITY xmlns \"http://www.w3.org/2000/xmlns/\"><!ENTITY empty \"\"><!ENTITY w \"5\">]>";
        for (root, found) in [
            ("<p:r xmlns:p=\"&ns;\"/>", Some("urn:a b:x")),
            ("<r xmlns=\"&ns;\" a=\"&w;\"/>", Some("urn:a b:x")),
            ("<r xmlns=\"x&ns;&amp;&#10;\"/>", Some("xurn:a b:x&\n")),
            // Reserved, or empty in XML 1.0.
            ("<r xmlns:p=\"&xml;\"/>", None),
            ("<r xmlns=\"&xmlns;\"/>", None),
            ("<r xmlns:p=\"&empty;\"/>", None),
            (
                "<xml:r xmlns:xml=\"&xml;\"/>",
                Some("http://www.w3.org/XML/1998/namespace"),
            ),
        ] {
            assert_eq!(
                root_after(dtd, root)
                    .map(|(namespace, _)| namespace)
                    .as_deref(),
                found,
                "{root}"
            );
        }
        // In a per-type file: the root, a field and another element.
        let dtd = format!(
            "<!DOCTYPE mime-type [<!ENTITY ns \"{NAMESPACE}\"><!ENTITY xml \"http://www.w3.org/XML/1998/namespace\">]>"
        );
        let text = format!(
            "{dtd}<m:mime-type xmlns:m=\"&ns;\"><m:comment>a</m:comment><f:comment xmlns:f=\"&ns;\" xml:lang=\"de\">b</f:comment></m:mime-type>"
        );
        let info = parse(&text);
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(comments(&info), [(None, "a"), (Some("de"), "b")]);
        let text = format!("{dtd}{OPEN}<comment>a</comment><x xmlns:p=\"&xml;\"/></mime-type>");
        assert_eq!(
            messages(&parse(&text)),
            [
                "bad XML (the namespace prefix 'p' cannot be bound to 'http://www.w3.org/XML/1998/namespace'); the rest is skipped"
            ]
        );
    }

    /// A document type nesting `levels` entities, each referring `refs`
    /// times to the one before (billion laughs).
    fn laughs(levels: usize, refs: usize) -> String {
        use std::fmt::Write as _;
        let mut dtd = String::from("<!DOCTYPE r [<!ENTITY l0 \"lol\">");
        for level in 1..=levels {
            let body = format!("&l{};", level - 1).repeat(refs);
            let _ = write!(dtd, "<!ENTITY l{level} \"{body}\">");
        }
        dtd.push_str("]>");
        dtd
    }

    #[test]
    fn an_expansion_longer_than_the_document_is_an_error() {
        use std::fmt::Write as _;
        // So that nested entities cannot make the work grow beyond
        // linear. 10 levels of 10 references: 10^10 bytes.
        let text = format!("{}<r xmlns=\"&l10;\"/>", laughs(10, 10));
        let (found, steps) = crate::work::steps(|| root_element(text.as_bytes()));
        assert_eq!(found, None);
        assert!(
            steps < 20 * text.len(),
            "{steps} steps for {} bytes",
            text.len()
        );
        // Within the document's length it is read.
        let text = format!("{}<r xmlns=\"&l2;\"/>", laughs(2, 3));
        assert_eq!(
            root_element(text.as_bytes()).map(|(namespace, _)| namespace.len()),
            Some(27)
        );
        // Expanding is linear: n levels doubling (refused past the length),
        // a chain of n (read), and n declarations of one long entity (each
        // within the length, together not). A byte of the document costs
        // at most four steps (scanned, its replacement text made and
        // walked, and written out once); a declaration on the root, at most
        // about 500 (checked, its message copied, stored and looked up).
        // The bounds are twice that. The budget here is the document's
        // length alone: the default's 64 KiB extra
        // (`entity_expansion_extra`) would add a constant to the work.
        let length_only = crate::Limits {
            entity_expansion_extra: 0,
            ..unbound()
        };
        let chain = |n: usize| {
            let mut dtd = String::from("<!DOCTYPE r [<!ENTITY c0 \"urn:\">");
            for at in 1..n {
                let _ = write!(dtd, "<!ENTITY c{at} \"&c{};x\">", at - 1);
            }
            dtd.push_str("]>");
            dtd
        };
        let shapes: [(&dyn Fn(usize) -> String, usize); 3] = [
            (&|n| format!("{}<r xmlns=\"&l{n};\"/>", laughs(n, 2)), 0),
            (&|n| format!("{}<r xmlns=\"&c{};\"/>", chain(n), n - 1), 0),
            (
                &|n| {
                    let mut root = String::from("<r");
                    for at in 0..n {
                        let _ = write!(root, " xmlns:p{at}=\"&long;\"");
                    }
                    format!(
                        "<!DOCTYPE r [<!ENTITY long \"urn:{}\">]>{root}/>",
                        "x".repeat(n)
                    )
                },
                500,
            ),
        ];
        for (shape, per_item) in shapes {
            crate::work::assert_linear(
                200,
                |n| {
                    let text = shape(n);
                    let (_, steps) =
                        crate::work::steps(|| root_element_within(text.as_bytes(), &length_only));
                    (steps, text.len())
                },
                |items, bytes| 2 * (4 * bytes + per_item * items + 400),
            );
        }
    }

    /// Runs `read` and checks it took at most 20 steps a byte of `text`.
    fn bounded<T>(text: &[u8], read: impl FnOnce() -> T) -> T {
        let (found, steps) = crate::work::steps(read);
        assert!(
            steps < 20 * text.len() + 1000,
            "{steps} steps for {} bytes",
            text.len()
        );
        found
    }

    #[test]
    fn a_reference_to_an_unusable_entity_makes_the_referrer_unusable() {
        // Forward or back. A forward reference to an unusable entity that
        // left the referrer usable would have its expansion follow the
        // unusable one past the budget: a 70-byte file allocating without
        // limit.
        let laughs = {
            let mut dtd = String::from("<!DOCTYPE r [<!ENTITY a \"&b;\"><!ENTITY b \"&#60;");
            dtd.push_str(&"&c8;".repeat(10));
            dtd.push_str("\"><!ENTITY c0 \"lollollol\">");
            for level in 1..=8 {
                let body = format!("&c{};", level - 1).repeat(10);
                let _ = std::fmt::Write::write_fmt(
                    &mut dtd,
                    format_args!("<!ENTITY c{level} \"{body}\">"),
                );
            }
            dtd.push_str("]><r xmlns=\"&a;\"/>");
            dtd
        };
        for text in [
            "<!DOCTYPE r [<!ENTITY b \"<\"><!ENTITY a \"&b;\">]><r c=\"&a;\"/>",
            "<!DOCTYPE r [<!ENTITY a \"&b;\"><!ENTITY b \"&#60;\">]><r c=\"&a;\"/>",
            "<!DOCTYPE r [<!ENTITY a \"urn:x&b;\"><!ENTITY b \"&#60;\">]><r xmlns=\"&a;\"/>",
            "<!DOCTYPE r [<!ENTITY a \"urn:x&b;\"><!ENTITY b SYSTEM \"y\">]><r xmlns=\"&a;\"/>",
            "<!DOCTYPE r [<!ENTITY a \"urn:&b;\"><!ENTITY b \"<\">]><r xmlns=\"&a;\"/>",
            "<!DOCTYPE r [<!ENTITY a \"&b;\"><!ENTITY b \"&#60;&b;\">]><r xmlns=\"&a;\"/>",
            &laughs,
        ] {
            assert_eq!(
                bounded(text.as_bytes(), || root_element(text.as_bytes())),
                None,
                "{text}"
            );
            // On another element of a per-type file: it is skipped.
            let text = format!(
                "{}{OPEN}<x xmlns:p=\"urn:&a;\"/><comment>a</comment></mime-type>",
                text[..text.find("<r ").unwrap()].replace("DOCTYPE r", "DOCTYPE mime-type"),
            );
            let info = bounded(text.as_bytes(), || parse(&text));
            assert_eq!(comments(&info), [(None, "a")], "{text}");
            assert_eq!(info.diagnostics.len(), 1, "{text}");
            assert!(
                messages(&info)[0].ends_with("the element is skipped"),
                "{text}"
            );
        }
        // The fuzzer's input that ran out of memory.
        let found: &[u8] = ARTIFACT;
        let _ = bounded(found, || root_element(found));
        let text = "<!DOCTYPE mime-type [<!ENTITY a \"&b;\"><!ENTITY b \"&#60;&b;\">]><mime-type xmlns=\"&a;\"><comment>a</comment></mime-type>";
        let info = bounded(text.as_bytes(), || parse(text));
        assert_eq!(comments(&info), []);
    }

    /// The input with which the `xml_root` fuzz target ran out of memory.
    const ARTIFACT: &[u8] = b"<!DOCTYPE r [<!ENTITY a \x22&b;$b;\x22><!ENTITY b \x22&c;c&;e&a;\x22\x1fxmY aurn:x\x22]a\x0d\x0ab\x22>}>>]><r xmlns=\x22&a;\x22\x1fxmY a \x22&b;&b;\x22>+*\xff=\x222\x22\x00\x00";

    /// What the brute-force model says a value referring to an entity is.
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Modelled {
        Usable,
        Unknown,
        Unusable,
    }

    /// An entity of the model: not declared, external, or internal with
    /// these parts of [`MODEL_PARTS`].
    enum Slot {
        Absent,
        External,
        Internal(Vec<usize>),
    }

    /// The parts an internal entity's literal is made of in the model.
    const MODEL_PARTS: [&str; 12] = [
        "x", "y", "&e0;", "&e1;", "&e2;", "&e3;", "&e4;", "&e5;", "&e6;", "&e7;", "&u;", "&#60;",
    ];

    /// What a value referring to entity `at` is, and what it expands to,
    /// following each reference with the `path` so far (exponential, but
    /// with eight entities cheap).
    fn modelled(
        slots: &[Slot],
        open: bool,
        at: usize,
        path: &mut Vec<usize>,
    ) -> (Modelled, String) {
        let undeclared = if open {
            Modelled::Unknown
        } else {
            Modelled::Unusable
        };
        let parts = match &slots[at] {
            Slot::Absent => return (undeclared, String::new()),
            Slot::External => return (Modelled::Unusable, String::new()),
            _ if path.contains(&at) => return (Modelled::Unusable, String::new()),
            Slot::Internal(parts) => parts,
        };
        path.push(at);
        let (mut state, mut text) = (Modelled::Usable, String::new());
        for &part in parts {
            let (got, value) = match MODEL_PARTS[part] {
                "&u;" => (undeclared, String::new()),
                "&#60;" => (Modelled::Unusable, String::new()),
                reference if reference.starts_with("&e") => {
                    let at = usize::from(reference.as_bytes()[2] - b'0');
                    modelled(slots, open, at, path)
                }
                written => (Modelled::Usable, written.to_owned()),
            };
            state = match (state, got) {
                (Modelled::Unusable, _) | (_, Modelled::Unusable) => Modelled::Unusable,
                (Modelled::Unknown, _) | (_, Modelled::Unknown) => Modelled::Unknown,
                _ => Modelled::Usable,
            };
            text.push_str(&value);
        }
        path.pop();
        (state, text)
    }

    #[test]
    fn usability_agrees_with_a_brute_force_model() {
        use std::fmt::Write as _;
        // Eight entities, each absent, external or internal with up to
        // three parts; a document open or not.
        let mut seed: u64 = 0x5eed_1234_abcd_0001;
        let mut next = |below: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            usize::try_from(seed % below).unwrap()
        };
        for _ in 0..8_000 {
            let open = next(4) == 0;
            let slots: Vec<Slot> = (0..8)
                .map(|_| match next(8) {
                    0 => Slot::Absent,
                    1 => Slot::External,
                    _ => Slot::Internal((0..next(4)).map(|_| next(12)).collect()),
                })
                .collect();
            let mut dtd = String::from(if open {
                "<!DOCTYPE r SYSTEM \"x\" ["
            } else {
                "<!DOCTYPE r ["
            });
            for (at, slot) in slots.iter().enumerate() {
                match slot {
                    Slot::Absent => {}
                    Slot::External => {
                        let _ = write!(dtd, "<!ENTITY e{at} SYSTEM \"e\">");
                    }
                    Slot::Internal(parts) => {
                        let literal: String = parts.iter().map(|&part| MODEL_PARTS[part]).collect();
                        let _ = write!(dtd, "<!ENTITY e{at} \"{literal}\">");
                    }
                }
            }
            dtd.push_str("]>");
            for at in 0..8 {
                let (state, value) = modelled(&slots, open, at, &mut Vec::new());
                let text = format!("{dtd}<r a=\"&e{at};\"/>");
                let found = root_element(text.as_bytes()).is_some();
                assert_eq!(found, state != Modelled::Unusable, "{text} {state:?}");
                let text = format!("{dtd}<p:r xmlns:p=\"urn:&e{at};\"/>");
                let expected =
                    (state == Modelled::Usable).then(|| (format!("urn:{value}"), "r".to_owned()));
                assert_eq!(root_element(text.as_bytes()), expected, "{text} {state:?}");
            }
        }
    }

    #[test]
    fn the_cache_of_expanded_values_is_per_tag() {
        // Replaced, not cleared, as clearing walks every slot of a map once
        // grown large. A child's own declaration is checked with its own
        // value, not its parent's.
        let text = "<!DOCTYPE mime-type [<!ENTITY ok \"urn:ok\"><!ENTITY bad \"http://www.w3.org/2000/xmlns/\">]><mime-type xmlns=\"http://www.freedesktop.org/standards/shared-mime-info\"><x xmlns=\"&ok;\"><y xmlns=\"&bad;\"/></x><comment>a</comment></mime-type>";
        let info = parse(text);
        assert_eq!(
            messages(&info),
            [
                "bad XML (an attribute cannot be read: the default namespace cannot be `http://www.w3.org/2000/xmlns/`); the element is skipped"
            ]
        );
        // That is what this test shows; the counted part below only bounds
        // the work with n declarations by entities on the root, then n
        // empty elements: a declaration costs about 180 steps besides its
        // bytes (expanded, kept for its tag, checked, bound and out of
        // scope), an empty element a few, dropping the cache once its
        // slots. The bounds are about twice that. A `clear` on every tag
        // would not be measurably superlinear here (`HashMap::clear` counts
        // nothing); only a clear that counted its slots fails it.
        crate::work::assert_linear(
            1000,
            |n| {
                let mut text = String::from("<!DOCTYPE r [<!ENTITY e \"urn:e\">]><r");
                for at in 0..n {
                    let _ =
                        std::fmt::Write::write_fmt(&mut text, format_args!(" xmlns:p{at}=\"&e;\""));
                }
                text.push('>');
                text.push_str(&"<x/>".repeat(n));
                text.push_str("</r>");
                let text = text
                    .replace("<r", &format!("<mime-type xmlns=\"{NAMESPACE}\""))
                    .replace("</r>", "</mime-type>");
                let (_, steps) =
                    crate::work::steps(|| parse_with(&text, &crate::Limits::unlimited()));
                (steps, text.len())
            },
            |items, bytes| 2 * (bytes + 180 * items),
        );
    }

    #[test]
    fn parameter_entities_and_standalone_decide_what_a_document_type_declares() {
        let root = |text: &str| root_element(text.as_bytes());
        let urn_x = Some(("urn:x".to_owned(), "r".to_owned()));
        // An internal parameter entity is read where it is referenced: the
        // first declaration, in the order read, binds.
        assert_eq!(
            root(
                "<!DOCTYPE r [<!ENTITY a \"urn:x\"> <!ENTITY % p \"<!ENTITY a 'urn:evil'>\"> %p;]><r xmlns=\"&a;\"/>"
            ),
            urn_x
        );
        assert_eq!(
            root(
                "<!DOCTYPE r [<!ENTITY % p \"<!ENTITY a 'urn:evil'>\"> %p; <!ENTITY a \"urn:x\">]><r xmlns=\"&a;\"/>"
            ),
            Some(("urn:evil".to_owned(), "r".to_owned()))
        );
        assert!(
            root("<!DOCTYPE r [<!ENTITY % p \"\"> %p; <!ENTITY a \"<\">]><r b=\"&a;\"/>").is_none()
        );
        // After an external one, declarations are not processed (5.1): the
        // entity is not declared (unknown here).
        assert!(
            root("<!DOCTYPE r [<!ENTITY % p SYSTEM \"p\"> %p; <!ENTITY a \"<\">]><r b=\"&a;\"/>")
                .is_some()
        );
        // 4.1: `standalone="yes"` makes it standalone whatever its subsets.
        assert!(root("<!DOCTYPE r SYSTEM \"x.dtd\"><r a=\"&u;\"/>").is_some());
        assert!(root("<?xml version=\"1.0\" standalone=\"no\"?><!DOCTYPE r SYSTEM \"x.dtd\"><r a=\"&u;\"/>").is_some());
        assert_eq!(
            root(
                "<?xml version=\"1.0\" standalone=\"yes\"?><!DOCTYPE r SYSTEM \"x.dtd\"><r a=\"&u;\"/>"
            ),
            None
        );
        assert_eq!(
            root(
                "<?xml version=\"1.0\" standalone='yes'?><!DOCTYPE r [<!ENTITY % p \"\"> %p;]><r a=\"&u;\"/>"
            ),
            None
        );
        // An undeclared entity in an entity's text: unknown when open.
        assert!(root("<!DOCTYPE r SYSTEM \"x\" [<!ENTITY a \"&u;\">]><r b=\"&a;\"/>").is_some());
        assert!(
            root("<!DOCTYPE r SYSTEM \"x\" [<!ENTITY a \"&u;\">]><r xmlns:p=\"&a;\"/>").is_some()
        );
        assert_eq!(
            root("<!DOCTYPE r SYSTEM \"x\" [<!ENTITY a \"urn:&u;\">]><r xmlns=\"&a;\"/>"),
            None
        );
        assert_eq!(
            root("<!DOCTYPE r [<!ENTITY a \"&u;\">]><r b=\"&a;\"/>"),
            None
        );
    }

    #[test]
    fn document_types_libxml2_rejects_declare_nothing_and_entity_text_is_normalised() {
        let root = |text: &str| root_element(text.as_bytes());
        let urn_x = Some(("urn:x".to_owned(), "r".to_owned()));
        // Document types libxml2 rejects declare nothing.
        for dtd in [
            "<!DOCTYPEr [<!ENTITY ns \"urn:x\">]>",
            "<!DOCTYPE [<!ENTITY ns \"urn:x\">]>",
            "<!DOCTYPE r SYSTEM\"y\" [<!ENTITY ns \"urn:x\">]>",
            "<!DOCTYPE r SYSTEMX [<!ENTITY ns \"urn:x\">]>",
            "<!DOCTYPE r PUBLIC\"p\" \"y\" [<!ENTITY ns \"urn:x\">]>",
            "<!DOCTYPE r PUBLIC \"p\"\"y\" [<!ENTITY ns \"urn:x\">]>",
            "<!DOCTYPE r [<!ENTITY ns \"urn:x\" NDATA x>]>",
            "<!DOCTYPE r [<!ENTITY ns \"urn:x\" junk>]>",
        ] {
            assert_eq!(root(&format!("{dtd}<r xmlns=\"&ns;\"/>")), None, "{dtd}");
        }
        for dtd in [
            "<!DOCTYPE r SYSTEM \"y\" [<!ENTITY ns \"urn:x\">]>",
            "<!DOCTYPE r PUBLIC \"p\" \"y\" [<!ENTITY ns \"urn:x\" >]>",
            "<!DOCTYPE\nr\t[<!ENTITY ns 'urn:x'>]>",
        ] {
            assert_eq!(root(&format!("{dtd}<r xmlns=\"&ns;\"/>")), urn_x, "{dtd}");
        }
        // A character the version does not allow makes an entity
        // unusable; line ends are normalised (2.11), then white space
        // becomes a space (3.3.3).
        for (text, found) in [
            (
                "<!DOCTYPE r [<!ENTITY a \"urn:\u{1}x\">]><r xmlns=\"&a;\"/>",
                None,
            ),
            (
                "<?xml version=\"1.1\"?><!DOCTYPE r [<!ENTITY a \"urn:\u{1}x\">]><r xmlns=\"&a;\"/>",
                None,
            ),
            (
                "<?xml version=\"1.1\"?><!DOCTYPE r [<!ENTITY a \"urn:&#1;x\">]><r xmlns=\"&a;\"/>",
                Some("urn:\u{1}x"),
            ),
            (
                "<!DOCTYPE r [<!ENTITY a \"urn:a\r\nb\">]><r xmlns=\"&a;\"/>",
                Some("urn:a b"),
            ),
            (
                "<!DOCTYPE r [<!ENTITY a \"urn:a\rb\tc\">]><r xmlns=\"&a;\"/>",
                Some("urn:a b c"),
            ),
            (
                "<?xml version=\"1.1\"?><!DOCTYPE r [<!ENTITY a \"urn:a\u{85}b\r\u{85}c\u{2028}d\">]><r xmlns=\"&a;\"/>",
                Some("urn:a b c d"),
            ),
            (
                "<!DOCTYPE r [<!ENTITY a \"urn:a\u{85}b\">]><r xmlns=\"&a;\"/>",
                Some("urn:a\u{85}b"),
            ),
        ] {
            assert_eq!(
                root(text).map(|(namespace, _)| namespace).as_deref(),
                found,
                "{text:?}"
            );
        }
    }

    #[test]
    fn declarations_after_an_internal_parameter_entity_are_processed_and_values_normalised() {
        let root = |text: &str| root_element(text.as_bytes()).map(|(namespace, _)| namespace);
        // Declarations after a reference to an internal parameter entity are
        // processed, with `standalone="yes"` or not.
        let pe_after =
            "<!DOCTYPE r [<!ENTITY % p \"\"> %p; <!ENTITY a \"urn:x\">]><r xmlns=\"&a;\"/>";
        let yes = format!("<?xml version=\"1.0\" standalone=\"yes\"?>{pe_after}");
        assert_eq!(root(&yes).as_deref(), Some("urn:x"));
        assert_eq!(
            root(&format!(
                "<?xml version=\"1.0\" standalone=\"no\"?>{pe_after}"
            ))
            .as_deref(),
            Some("urn:x")
        );
        let text = format!(
            "<?xml version=\"1.0\" standalone=\"yes\"?><!DOCTYPE mime-type [<!ENTITY % p \"\"> %p; <!ENTITY ns \"{NAMESPACE}\">]><mime-type xmlns=\"&ns;\"><comment>a</comment></mime-type>"
        );
        let info = parse(&text);
        assert!(info.diagnostics.is_empty(), "{:?}", info.diagnostics);
        assert_eq!(comments(&info), [(None, "a")]);
        // 2.11, then 3.3.3: a value written with line ends and entities is
        // normalised as one without entities.
        for (version, line_end) in [
            ("1.0", "\r\n"),
            ("1.0", "\r"),
            ("1.1", "\u{85}"),
            ("1.1", "\r\u{85}"),
            ("1.1", "\u{2028}"),
        ] {
            let declaration = format!("<?xml version=\"{version}\"?>");
            let plain = format!("{declaration}<r xmlns=\"urn:a{line_end}b\"/>");
            let with = format!(
                "{declaration}<!DOCTYPE r [<!ENTITY e \"\">]><r xmlns=\"urn:a{line_end}b&e;\"/>"
            );
            assert_eq!(root(&with), root(&plain), "{line_end:?}");
            assert_eq!(root(&with).as_deref(), Some("urn:a b"), "{line_end:?}");
            // In a per-type file: one namespace, so a repeat.
            let text = format!(
                "{declaration}<!DOCTYPE mime-type [<!ENTITY e \"\">]>{OPEN}<x xmlns:p=\"urn:a{line_end}b&e;\" xmlns:q=\"urn:a b\" p:a=\"1\" q:a=\"2\"/><comment>a</comment></mime-type>"
            );
            assert_eq!(
                messages(&parse(&text)),
                [
                    "bad XML (an attribute cannot be read: an attribute appears twice in its namespace: q:a); the element is skipped"
                ],
                "{line_end:?}"
            );
        }
        // NDATA on an internal parameter entity: rejected as on a general
        // one, so the document type declares nothing.
        assert_eq!(
            root(
                "<!DOCTYPE r [<!ENTITY % p \"x\" NDATA n><!ENTITY a \"urn:x\">]><r xmlns=\"&a;\"/>"
            ),
            None
        );
        assert_eq!(
            root("<!DOCTYPE r [<!ENTITY u SYSTEM \"x\" NDATA n><!ENTITY a \"urn:x\">]><r xmlns=\"&a;\"/>").as_deref(),
            Some("urn:x")
        );
    }

    #[test]
    fn an_expansion_that_fails_late_is_charged() {
        // If sizes were wrong, an expansion could fail only after writing;
        // what it wrote is charged, so n such declarations cost linear work
        // in all. An entity of n bytes whose size says 0.
        crate::work::assert_linear(
            1000,
            |n| {
                let mut entities = Entities::default();
                entities.index.insert("a".to_owned(), 0);
                entities.list.push(Entity {
                    text: Some("x".repeat(n)),
                    use_: Use::Usable,
                    size: 0,
                    ..Entity::default()
                });
                entities.budget = 4 * n;
                let ((), steps) = crate::work::steps(|| {
                    for _ in 0..n {
                        let _ = entities.expand("&a;", XmlVersion::Implicit1_0);
                    }
                });
                (steps, n)
            },
            // What is written is at most the budget, 4n bytes, and each call
            // costs a few steps besides (about 8): twice that.
            |items, _| 2 * (4 * items + 8 * items),
        );
    }

    #[test]
    fn empty_entities_are_charged_to_the_budget() {
        use std::fmt::Write as _;
        // The work done is charged, not the bytes written, so n
        // declarations each following n references to an empty entity do
        // not cost n * n steps: the expansions read at most the
        // document's length in all (two steps a byte with the scan of it),
        // and each declaration costs about 350 steps besides (its tag
        // checked, its value refused or bound): the bounds are twice that.
        let dtd = |kind: &str, n: usize| {
            format!(
                "<!DOCTYPE {kind} [<!ENTITY b \"\"><!ENTITY a \"{}\">]>",
                "&b;".repeat(n)
            )
        };
        crate::work::assert_linear(
            1000,
            |n| {
                let mut text = format!("{}{OPEN}", dtd("mime-type", n));
                for _ in 0..n {
                    text.push_str("<x xmlns:p=\"urn:0&a;\"/>");
                }
                text.push_str("<comment>c</comment></mime-type>");
                let (info, steps) = crate::work::steps(|| parse(&text));
                assert_eq!(comments(&info), [(None, "c")]);
                (steps, text.len())
            },
            |items, bytes| 2 * (2 * bytes + 350 * items),
        );
        crate::work::assert_linear(
            1000,
            |n| {
                let mut text = format!("{}<r", dtd("r", n));
                for at in 0..n {
                    let _ = write!(text, " xmlns:p{at}=\"urn:0&a;\"");
                }
                text.push_str("/>");
                let (_, steps) =
                    crate::work::steps(|| root_element_within(text.as_bytes(), &unbound()));
                (steps, text.len())
            },
            |items, bytes| 2 * (2 * bytes + 350 * items),
        );
    }

    #[test]
    fn a_parameter_entity_is_read_with_standalone_and_bad_ones_refuse_the_document_type() {
        let root = |text: &str| root_element(text.as_bytes()).map(|(namespace, _)| namespace);
        let yes = "<?xml version=\"1.0\" standalone=\"yes\"?>";
        // In standalone mode, an internal parameter entity is read, and one
        // with markup does not end declaration processing: `a` declared in its
        // text and after it, the first binds, the one outside meeting WFC:
        // Entity Declared.
        for (pe, found) in [
            ("<!ENTITY % p \"<!ENTITY a 'urn:y'>\">", Some("urn:y")),
            (
                "<!ENTITY % p \"&#60;!ENTITY a &#34;urn:y&#34;>\">",
                Some("urn:y"),
            ),
            ("<!ENTITY % p \"\">", Some("urn:x")),
            ("<!ENTITY % p SYSTEM \"p.ent\">", Some("urn:x")),
        ] {
            let text =
                format!("{yes}<!DOCTYPE r [{pe} %p; <!ENTITY a \"urn:x\">]><r xmlns=\"&a;\"/>");
            assert_eq!(root(&text).as_deref(), found, "{text}");
        }
        // Declarations and references libxml2 refuses: the document type
        // declares nothing.
        for (lead, subset) in [
            ("", "<!ENTITY % p SYSTEM \"x\" NDATA n>"),
            ("", "<!ENTITY % p PUBLIC \"i\" \"x\" NDATA n>"),
            ("", "<!ENTITY %p \"x\">"),
            ("", "<!ENTITY % p\"x\">"),
            ("", "<!ENTITY % 1p \"x\">"),
            (yes, "%q;"),
            (yes, "% junk ;"),
            (yes, "%;"),
        ] {
            let text =
                format!("{lead}<!DOCTYPE r [{subset} <!ENTITY a \"urn:x\">]><r xmlns=\"&a;\"/>");
            assert_eq!(root(&text), None, "{text}");
        }
        // Still declared: an external parameter entity without NDATA.
        let text =
            "<!DOCTYPE r [<!ENTITY % p SYSTEM \"x\"><!ENTITY a \"urn:x\">]><r xmlns=\"&a;\"/>";
        assert_eq!(root(text).as_deref(), Some("urn:x"));
    }

    #[test]
    fn more_parameter_entity_references_libxml2_refuses() {
        // A nameless reference refuses the document type in both modes; in
        // standalone mode so does one to an internal parameter entity whose
        // text is neither blank nor markup.
        let root = |text: &str| root_element(text.as_bytes()).map(|(namespace, _)| namespace);
        let yes = "<?xml version=\"1.0\" standalone=\"yes\"?>";
        for (lead, subset) in [
            ("", "% junk ;"),
            ("", "%;"),
            (yes, "<!ENTITY % p \"x\"> %p;"),
            (yes, "<!ENTITY % p \"&#65;\"> %p;"),
        ] {
            let text = format!(
                "{lead}<!DOCTYPE r SYSTEM \"d\" [{subset} <!ENTITY a \"urn:x\">]><r b=\"&u;\"/>"
            );
            assert_eq!(root(&text), None, "{text}");
        }
        // Blank text does not: later declarations are processed.
        for pe in ["\"\"", "\" \t&#32;\""] {
            let text = format!(
                "{yes}<!DOCTYPE r [<!ENTITY % p {pe}> %p; <!ENTITY a \"urn:x\">]><r xmlns=\"&a;\"/>"
            );
            assert_eq!(root(&text).as_deref(), Some("urn:x"), "{text}");
        }
    }

    #[test]
    fn a_long_language_is_stored_once() {
        use std::fmt::Write as _;
        // A root `xml:lang` of n bytes and n fields that take it, or n fields
        // that each have one of 1,000 bytes, are linear: no field copies the
        // inherited one, and no value hashes it again. The root's
        // language is hashed and copied once (three steps a byte), each field
        // costs about 12 steps; a field's own language is hashed, about two
        // steps a byte of the file. The bounds are twice that.
        let lang = |n: usize| "x".repeat(n);
        crate::work::assert_linear(
            1000,
            |n| {
                let mut text =
                    format!("<mime-type xmlns=\"{NAMESPACE}\" xml:lang=\"{}\">", lang(n));
                for at in 0..n {
                    let _ = write!(text, "<comment>{at}</comment>");
                }
                text.push_str("</mime-type>");
                let (info, steps) = crate::work::steps(|| parse(&text));
                assert_eq!(info.comment.values.len(), 1);
                (steps, text.len())
            },
            |items, _| 2 * (3 * items + 12 * items),
        );
        crate::work::assert_linear(
            100,
            |n| {
                let mut text = OPEN.to_owned();
                for at in 0..n {
                    let _ = write!(text, "<comment xml:lang=\"{}\">{at}</comment>", lang(1000));
                }
                text.push_str("</mime-type>");
                let (info, steps) = crate::work::steps(|| parse(&text));
                assert!(info.diagnostics.is_empty());
                (steps, text.len())
            },
            |items, bytes| 2 * (3 * bytes + 10 * items),
        );
    }

    #[test]
    fn a_reference_that_is_not_one_is_an_error_in_every_document() {
        // Only an undeclared entity whose name is an XML name may be unknown;
        // a character reference to no legal character, or a name that is not
        // one, is an error, also with an external subset; in an entity's
        // literal it refuses the document type.
        let root = |text: &str| root_element(text.as_bytes()).map(|(namespace, _)| namespace);
        let bad = ["&#0;", "&#x110000;", "&#xyz;", "&;", "&bad name;", "&1bad;"];
        assert_eq!(
            root("<!DOCTYPE r SYSTEM \"x\"><r a=\"&u;\"/>").as_deref(),
            Some("")
        );
        for reference in bad {
            for root_tag in [
                format!("<r a=\"{reference}\"/>"),
                format!("<r xml:lang=\"{reference}\"/>"),
                format!("<r xmlns=\"urn:{reference}\"/>"),
                format!("<p:r xmlns:p=\"urn:{reference}\"/>"),
            ] {
                let text = format!("<!DOCTYPE r SYSTEM \"x\">{root_tag}");
                assert_eq!(root(&text), None, "{text}");
            }
            let text = format!(
                "<!DOCTYPE r SYSTEM \"x\" [<!ENTITY e \"{reference}\"><!ENTITY a \"urn:x\">]><r xmlns=\"&a;\"/>"
            );
            assert_eq!(root(&text), None, "{text}");
            // A per-type file: a field's attribute or language, another
            // element's attribute or namespace.
            let open = "<!DOCTYPE mime-type SYSTEM \"x\">";
            for (inside, skipped) in [
                (
                    format!("<comment xml:lang=\"de\" a=\"{reference}\">X</comment>"),
                    false,
                ),
                (
                    format!("<comment xml:lang=\"{reference}\">X</comment>"),
                    false,
                ),
                (format!("<x a=\"{reference}\"/>"), true),
                (format!("<x xmlns:p=\"urn:{reference}\"/>"), true),
            ] {
                let text = format!("{open}{OPEN}<comment>en</comment>{inside}</mime-type>");
                let info = parse(&text);
                assert_eq!(comments(&info), [(None, "en")], "{text}");
                assert_eq!(info.diagnostics.len(), 1, "{text}");
                let tail = if skipped {
                    "the element is skipped"
                } else {
                    "the value is left out"
                };
                assert!(
                    messages(&info)[0].ends_with(tail),
                    "{text}: {:?}",
                    info.diagnostics
                );
            }
        }
        let text = "<!DOCTYPE r SYSTEM \"x\" [<!ENTITY e \"&u;\"><!ENTITY a \"urn:x\">]><r xmlns=\"&a;\"/>";
        assert_eq!(root(text).as_deref(), Some("urn:x"));
    }

    #[test]
    fn a_long_inherited_language_is_copied_once() {
        use std::fmt::Write as _;
        // The root's `xml:lang` is copied when read and when interned, not
        // per field (copies counted here, as the work counted elsewhere
        // leaves them out). The timing test for the same:
        // `a_long_inherited_language_takes_linear_time`.
        let mut text = format!(
            "<mime-type xmlns=\"{NAMESPACE}\" xml:lang=\"{}\">",
            "x".repeat(10_000)
        );
        for at in 0..1000 {
            let _ = write!(text, "<comment>{at}</comment>");
        }
        text.push_str("</mime-type>");
        let (copies, lookups) = (language_copies(), language_lookups());
        let info = parse(&text);
        assert_eq!(info.comment.values.len(), 1);
        assert_eq!(language_copies() - copies, 2);
        // Looked up once too: a field that copied the language and found it
        // interned would look it up again.
        assert_eq!(language_lookups() - lookups, 1);
    }

    #[test]
    #[ignore = "timing: run with `--release -- --ignored`"]
    fn a_long_inherited_language_takes_linear_time() {
        use std::fmt::Write as _;
        // A root `xml:lang` of n bytes and n fields: a field that copied and
        // hashed it would make this quadratic.
        let time = |n: usize| {
            let mut text = format!(
                "<mime-type xmlns=\"{NAMESPACE}\" xml:lang=\"{}\">",
                "x".repeat(n)
            );
            for at in 0..n {
                let _ = write!(text, "<comment>{at}</comment>");
            }
            text.push_str("</mime-type>");
            (0..3)
                .map(|_| {
                    let start = std::time::Instant::now();
                    let info = parse(&text);
                    assert_eq!(info.comment.values.len(), 1);
                    start.elapsed()
                })
                .min()
                .unwrap()
        };
        let (single, quadrupled) = (time(5_000), time(20_000));
        assert!(
            quadrupled < single * 8,
            "{quadrupled:?} for 20000 against {single:?} for 5000"
        );
    }

    #[test]
    fn a_reference_formed_by_a_replacement_text_must_name_one() {
        // `&#38;bad name;` stands for `&bad name;`, a reference whose name is
        // not an XML name: an error even in an open document.
        for formed in ["&#38;bad name;", "&#38;1bad;"] {
            let dtd = format!(
                "<?xml version=\"1.0\"?><!DOCTYPE r SYSTEM \"x\" [<!ENTITY e \"{formed}\">]>"
            );
            assert_eq!(
                root_element(format!("{dtd}<r a=\"&e;\"/>").as_bytes()),
                None,
                "{formed}"
            );
            let text = format!(
                "{}{OPEN}<comment>en</comment><comment xml:lang=\"de\" a=\"&e;\">X</comment></mime-type>",
                dtd.replacen("DOCTYPE r", "DOCTYPE mime-type", 1)
            );
            let info = parse(&text);
            assert_eq!(comments(&info), [(None, "en")], "{formed}");
        }
    }

    #[test]
    fn every_literal_in_the_subset_is_well_formed() {
        // Wherever a literal stands in the internal subset, a reference that
        // cannot be one refuses the document type, so `&u;` (unknown in this
        // open document) becomes an error.
        let root = |subset: &str, lead: &str| {
            let text = format!("{lead}<!DOCTYPE r SYSTEM \"x\" [{subset}]><r b=\"&u;\"/>");
            root_element(text.as_bytes())
        };
        for subset in [
            "<!ENTITY % p \"&#0;\">",
            "<!ENTITY % p \"x&y\">",
            "<!ENTITY % q \"\"> %q; <!ENTITY b \"&#0;\">",
            "<!ENTITY % q \"\"> %q; <!ENTITY % r \"&;\">",
            "<!ENTITY % q \"\"> %q; <!ATTLIST r a CDATA \"&#0;\">",
            "<!ENTITY a \"x&y\">",
            "<!ENTITY a \"x&\">",
            "<!ENTITY a \"&a&b;\">",
            "<!ENTITY a:b \"&#0;\">",
            "<!ATTLIST r a CDATA \"&#0;\">",
            "<!ATTLIST r a CDATA #FIXED \"&1bad;\">",
            "<!ATTLIST r a CDATA \"x&y\">",
            "<!ATTLIST r a CDATA \"x<y\">",
        ] {
            assert_eq!(root(subset, ""), None, "{subset}");
        }
        // After a standalone markup parameter entity too.
        let text = "<?xml version=\"1.0\" standalone=\"yes\"?><!DOCTYPE r [<!ENTITY a \"urn:x\"><!ENTITY % p \"<!ENTITY z 'v'>\"> %p; <!ENTITY b \"&#0;\">]><r xmlns=\"&a;\"/>";
        assert_eq!(root_element(text.as_bytes()), None);
        let text = text.replace("&#0;", "v");
        assert_eq!(
            root_element(text.as_bytes())
                .map(|(namespace, _)| namespace)
                .as_deref(),
            Some("urn:x")
        );
        // Well formed: kept.
        for subset in [
            "<!ATTLIST r a (x|y) \"x\">",
            "<!ATTLIST r a CDATA \"&u;&#65;&lt;\">",
            "<!ENTITY ok \"&#38;#60;\">",
            "<!ENTITY a:b \"v\">",
            "<!ENTITY % q \"\"> %q; <!ENTITY b \"v\">",
        ] {
            assert!(root(subset, "").is_some(), "{subset}");
        }
        // In a per-type file.
        let text = format!(
            "<!DOCTYPE mime-type SYSTEM \"x\" [<!ENTITY % p \"&#0;\">]>{OPEN}<x b=\"&u;\"/><comment>a</comment></mime-type>"
        );
        let info = parse(&text);
        // The document type refused is reported once, and so is the element
        // using an entity it would have declared.
        let found: Vec<_> = info
            .diagnostics
            .iter()
            .map(|d| d.message.as_str())
            .collect();
        assert_eq!(
            found,
            [
                REFUSED_DOCTYPE,
                "bad XML (the reference `&u;` cannot be replaced); the element is skipped"
            ],
            "{:?}",
            info.diagnostics
        );
        assert_eq!(comments(&info), [(None, "a")]);
    }

    #[test]
    fn an_attribute_default_refers_to_entities_as_a_value_does() {
        // A reference in a processed `ATTLIST` default follows the rules for
        // attribute values; one that would be an error there (undeclared in a
        // closed document, external, unparsed, not usable) refuses the
        // document type.
        let root = |text: &str| root_element(text.as_bytes()).map(|(namespace, _)| namespace);
        let ns = "<!ENTITY ns \"urn:x\">";
        for (lead, subset, kept) in [
            ("", "<!ATTLIST r a CDATA \"&u;\">", false),
            (
                "<?xml version=\"1.0\" standalone=\"yes\"?>",
                "<!ATTLIST r a CDATA \"&u;\">",
                false,
            ),
            ("", "<!ENTITY e \"v\"><!ATTLIST r a CDATA \"&e;\">", true),
            // Declared after: not declared where the default refers to it
            // (XML 1.0, 4.1, WFC: Entity Declared), unlike in a literal.
            ("", "<!ATTLIST r a CDATA \"&e;\"><!ENTITY e \"v\">", false),
            (
                "",
                "<!ENTITY e SYSTEM \"y\"><!ATTLIST r a CDATA \"&e;\">",
                false,
            ),
            (
                "",
                "<!ATTLIST r a CDATA \"&e;\"><!ENTITY e SYSTEM \"y\">",
                false,
            ),
            ("", "<!ENTITY e \"x<y\"><!ATTLIST r a CDATA \"&e;\">", false),
            ("", "<!ENTITY e \"&e;\"><!ATTLIST r a CDATA \"&e;\">", false),
            ("", "<!ENTITY e \"&u;\"><!ATTLIST r a CDATA \"&e;\">", false),
        ] {
            let system = if lead.is_empty() { "" } else { " SYSTEM \"x\"" };
            let text = format!("{lead}<!DOCTYPE r{system} [{ns}{subset}]><r xmlns=\"&ns;\"/>");
            assert_eq!(root(&text).is_some(), kept, "{text}");
        }
        // In an open document an undeclared one is unknown, as in a value;
        // after the cut-off a default is not processed.
        for subset in [
            "<!ATTLIST r a CDATA \"&u;\">",
            "<!ENTITY e \"&u;\"><!ATTLIST r a CDATA \"&e;\">",
            "<!ENTITY % q \"\"> %q; <!ATTLIST r a CDATA \"&e;\"><!ENTITY e SYSTEM \"y\">",
            // Not yet declared: undeclared there, so unknown here (a
            // validity matter, 4.1), whatever the later declaration.
            "<!ATTLIST r a CDATA \"&e;\"><!ENTITY e \"v\">",
            "<!ATTLIST r a CDATA \"&e;\"><!ENTITY e \"x<y\">",
        ] {
            let text = format!("<!DOCTYPE r SYSTEM \"x\" [{subset}]><r b=\"&u;\"/>");
            assert!(root(&text).is_some(), "{text}");
        }
    }

    /// The parts an entity's literal is made of in the defaults model.
    const DEFAULTS_PARTS: [&str; 11] = [
        "v", "<", "&e0;", "&e1;", "&e2;", "&e3;", "&e4;", "&e5;", "&e6;", "&e7;", "&u;",
    ];

    /// What the brute-force model says of an entity referred to from a
    /// default at a point: known there, pending (not declared yet there,
    /// or at all), or an error.
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum AtPoint {
        Fine,
        Pending,
        Bad,
    }

    /// An entity of the defaults model, in declaration order: external, or
    /// internal with these parts (`&eK;` references, `<`, text).
    enum Declared {
        External,
        Internal(Vec<&'static str>),
    }

    /// What a default at `point` (the declarations before it) that refers
    /// to `name` sees, following each reference with the path so far.
    fn at_point(
        decls: &[(usize, Declared)],
        point: usize,
        name: usize,
        path: &mut Vec<usize>,
    ) -> AtPoint {
        // The first declaration binds.
        let Some(at) = decls.iter().position(|(declared, _)| *declared == name) else {
            return AtPoint::Pending;
        };
        if at >= point {
            return AtPoint::Pending;
        }
        let Declared::Internal(parts) = &decls[at].1 else {
            return AtPoint::Bad;
        };
        if path.contains(&name) || parts.contains(&"<") {
            return AtPoint::Bad;
        }
        path.push(name);
        let mut seen = AtPoint::Fine;
        for part in parts {
            // `&u;` is never declared.
            let child = match part.strip_prefix("&e") {
                Some(reference) => Some(usize::from(reference.as_bytes()[0] - b'0')),
                None if *part == "&u;" => None,
                None => continue,
            };
            {
                let got = child.map_or(AtPoint::Pending, |child| {
                    at_point(decls, point, child, path)
                });
                match got {
                    AtPoint::Bad => seen = AtPoint::Bad,
                    AtPoint::Pending if seen == AtPoint::Fine => seen = AtPoint::Pending,
                    _ => {}
                }
            }
        }
        path.pop();
        seen
    }

    #[test]
    fn a_default_is_judged_where_it_stands() {
        use std::fmt::Write as _;
        // XML 1.0, 4.1: libxml2 expands a default where it stands, so each
        // entity it reaches must be declared before it (pending otherwise: an
        // error in a closed document, unknown in an open one), and usability
        // is judged over what is declared then.
        let root = |text: &str| root_element(text.as_bytes()).map(|(namespace, _)| namespace);
        let open = "<!DOCTYPE r SYSTEM \"x\" [<!ENTITY e \"&f;\"><!ATTLIST r a CDATA \"&e;\"><!ENTITY f \"x<y\">]><r b=\"&u;\"/>";
        assert_eq!(root(open).as_deref(), Some(""));
        for lead in [
            "",
            "<?xml version=\"1.0\" standalone=\"yes\"?>",
            "<?xml version=\"1.1\"?>",
        ] {
            for system in ["", " SYSTEM \"x\""] {
                // Closed only: an external subset opens it but with
                // standalone.
                if !system.is_empty() && !lead.contains("standalone") {
                    continue;
                }
                let text = format!(
                    "{lead}<!DOCTYPE r{system} [<!ENTITY ns \"urn:x\"><!ENTITY e \"&f;\"><!ATTLIST r a CDATA \"&e;\"><!ENTITY f \"v\">]><r xmlns=\"&ns;\"/>"
                );
                assert_eq!(root(&text), None, "{text}");
            }
        }
        // Against a model: eight entities in random order, external or
        // internal, defaults at random places; open or closed.
        let mut state: u64 = 0x0def_a017_5eed_0185;
        let mut next = |below: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            usize::try_from(state % below).unwrap()
        };
        for _ in 0..6_000 {
            let open = next(2) == 0;
            let decls: Vec<(usize, Declared)> = (0..next(9))
                .map(|_| {
                    let name = next(8);
                    if next(6) == 0 {
                        (name, Declared::External)
                    } else {
                        (
                            name,
                            Declared::Internal(
                                (0..next(3)).map(|_| DEFAULTS_PARTS[next(11)]).collect(),
                            ),
                        )
                    }
                })
                .collect();
            let defaults: Vec<(usize, usize)> = (0..next(3))
                .map(|_| (next(u64::try_from(decls.len()).unwrap() + 1), next(9)))
                .collect();
            let mut subset = String::from("<!ENTITY ns \"urn:x\">");
            let mut refused = false;
            for point in 0..=decls.len() {
                for &(at, name) in &defaults {
                    if at != point {
                        continue;
                    }
                    // 8 stands for `&u;`, never declared.
                    let seen = if name == 8 {
                        AtPoint::Pending
                    } else {
                        at_point(&decls, point, name, &mut Vec::new())
                    };
                    refused |= seen == AtPoint::Bad || seen == AtPoint::Pending && !open;
                    let reference = if name == 8 {
                        "u".to_owned()
                    } else {
                        format!("e{name}")
                    };
                    let _ = write!(subset, "<!ATTLIST r a CDATA \"&{reference};\">");
                }
                if let Some((name, declared)) = decls.get(point) {
                    match declared {
                        Declared::External => {
                            let _ = write!(subset, "<!ENTITY e{name} SYSTEM \"s\">");
                        }
                        Declared::Internal(parts) => {
                            let _ = write!(subset, "<!ENTITY e{name} \"{}\">", parts.concat());
                        }
                    }
                }
            }
            let (system, root_tag) = if open {
                (" SYSTEM \"x\"", "<r b=\"&u;\"/>")
            } else {
                ("", "<r xmlns=\"&ns;\"/>")
            };
            let text = format!("<!DOCTYPE r{system} [{subset}]>{root_tag}");
            assert_eq!(root(&text).is_none(), refused, "{text}");
        }
    }

    #[test]
    fn defaults_are_judged_in_n_log_n_time() {
        use std::fmt::Write as _;
        // n entities in a chain to one declared last, a loop of n, and n
        // defaults between the declarations, each referring to the chain's
        // start, its own link and the loop: no default walks the chain
        // again. Each byte is scanned about four times. The loops of an
        // open document are found by halving the declaration order
        // (`cycle_times`): each entity and reference is looked at once a
        // level, log2(n) + 1 levels, so the work is n log n, not linear:
        // about 11 steps per item a level (an item being three entities,
        // three references and three default references), and about 90
        // more per item for the rest (measured 264,720 steps at 500 items
        // and 542,993 at 1,000: 2.05 times, within the doubling check).
        // The bounds are about twice that.
        crate::work::assert_linear(
            500,
            |n| {
                let mut subset = String::new();
                for at in 0..n {
                    let _ = write!(subset, "<!ENTITY c{at} \"&c{};\">", at + 1);
                    // The loop closes only after the last default.
                    let _ = write!(subset, "<!ATTLIST r a{at} CDATA \"&c0;&c{at};&l0;\">");
                    let _ = write!(subset, "<!ENTITY l{at} \"&l{};\">", (at + 1) % n);
                }
                let _ = write!(subset, "<!ENTITY c{n} \"v\">");
                let text = format!("<!DOCTYPE r SYSTEM \"x\" [{subset}]><r b=\"&u;\"/>");
                let (found, steps) = crate::work::steps(|| root_element(text.as_bytes()));
                assert!(found.is_some());
                (steps, text.len())
            },
            |items, bytes| {
                let levels = usize::try_from(items.ilog2()).unwrap() + 1;
                2 * (4 * bytes + items * (100 + 12 * levels))
            },
        );
    }

    #[test]
    fn the_loop_analysis_holds_memory_linear_in_the_subset() {
        use std::fmt::Write as _;
        // The buffers the loop analysis holds at once, at most, grow with the
        // internal subset, whatever its shape: edges that never join a loop
        // (each entity refers to the 8 before it), many references to one
        // entity (a spine), and the loops of the counted test. Each distinct
        // reference holds about 4 words at once (its row in the graph, its
        // edge in the halving, and its row in one halving step's graph), each
        // entity about 15: 6.6 bytes per byte of the subset at most here
        // (back), 5.1 (loops), 2.9 (spine: the references of one entity read
        // at once, 16 bytes each, before repeats are dropped). The bound is
        // about twice that; before the halving freed its buffers it held
        // about 52 per byte (back).
        for shape in ["back", "spine", "loops"] {
            crate::work::assert_linear(
                400,
                |n| {
                    let mut subset = String::new();
                    match shape {
                        "back" => {
                            for at in 0..n {
                                let _ = write!(subset, "<!ENTITY e{at} \"");
                                for back in 1..=8.min(at) {
                                    let _ = write!(subset, "&e{};", at - back);
                                }
                                subset.push_str("\">");
                            }
                            subset.push_str("<!ATTLIST r a CDATA \"&e0;\">");
                        }
                        "spine" => {
                            let _ = write!(subset, "<!ENTITY a \"{}\">", "&a;".repeat(4 * n));
                            for at in 0..n {
                                let _ = write!(subset, "<!ENTITY b{at} \"\">");
                            }
                            subset.push_str("<!ATTLIST r a CDATA \"&b0;\">");
                        }
                        _ => {
                            for at in 0..n {
                                let _ = write!(subset, "<!ENTITY c{at} \"&c{};\">", at + 1);
                                let _ =
                                    write!(subset, "<!ATTLIST r a{at} CDATA \"&c0;&c{at};&l0;\">");
                                let _ = write!(subset, "<!ENTITY l{at} \"&l{};\">", (at + 1) % n);
                            }
                            let _ = write!(subset, "<!ENTITY c{n} \"v\">");
                        }
                    }
                    let text = format!("<!DOCTYPE r SYSTEM \"x\" [{subset}]><r b=\"&u;\"/>");
                    let (found, held) = crate::work::peak(|| root_element(text.as_bytes()));
                    assert!(found.is_some(), "{shape}");
                    (held, subset.len())
                },
                |_, bytes| 14 * bytes,
            );
        }
    }

    #[test]
    fn an_internal_parameter_entity_is_read_where_it_is_referenced() {
        // A reference between declarations to a declared internal parameter
        // entity includes its replacement text, whose declarations are
        // processed there (XML 1.0, 2.8, 4.4.8, 5.1). The root's namespace
        // shows what was declared: `None` when the document type declares
        // nothing, or `ns` is undeclared or not usable.
        const SA: &str = "<?xml version=\"1.0\" standalone=\"yes\"?>";
        const NS: &str = "<!ENTITY ns \"urn:x\">";
        let x = Some("urn:x");
        let cases: Vec<(String, Option<&str>)> = vec![
            // Included, nested (by a character reference), twice, adjacent.
            ("<!DOCTYPE r [<!ENTITY % p \"<!ENTITY ns 'urn:x'>\"> %p;]>".to_owned(), x),
            (
                "<!DOCTYPE r [<!ENTITY % q \"<!ENTITY ns 'urn:x'>\"><!ENTITY % p \"&#37;q;\"> %p;]>"
                    .to_owned(),
                x,
            ),
            ("<!DOCTYPE r [<!ENTITY % p \"<!ENTITY ns 'urn:x'>\"> %p; %p;]>".to_owned(), x),
            (
                "<!DOCTYPE r [<!ENTITY % p \"<!ENTITY a 'v'>\"><!ENTITY % q \"<!ENTITY ns \
                 'urn:x'>\">%p;%q;]>"
                    .to_owned(),
                x,
            ),
            (
                format!("<!DOCTYPE r [{NS}<!ENTITY % p \"<?pi x?><!-- c --> \"> %p;]>"),
                x,
            ),
            // Characters from character references in the parameter entity's
            // literal stand as written in its text: line ends there are not
            // normalised again (2.11 is on the entities read), as libxml2.
            (
                "<!DOCTYPE r [<!ENTITY % p \"<!ENTITY ns 'urn:a&#13;&#10;b'>\"> %p;]>".to_owned(),
                Some("urn:a  b"),
            ),
            // A parameter entity declared in included text.
            (
                "<!DOCTYPE r [<!ENTITY % p \"<!ENTITY &#37; q '<!ENTITY ns &#34;urn:x&#34;>'>\"> \
                 %p; %q;]>"
                    .to_owned(),
                x,
            ),
            // First declaration binds, in the order read.
            (
                "<!DOCTYPE r [<!ENTITY % p \"<!ENTITY ns 'urn:x'>\"> %p;<!ENTITY ns \"urn:y\">]>"
                    .to_owned(),
                x,
            ),
            (
                "<!DOCTYPE r [<!ENTITY ns \"urn:x\"><!ENTITY % p \"<!ENTITY ns 'urn:y'>\"> %p;]>"
                    .to_owned(),
                x,
            ),
            // Declarations after it are processed (no cut-off), standalone or
            // not.
            (format!("<!DOCTYPE r [<!ENTITY % p \"\"> %p;{NS}]>"), x),
            (
                format!("{SA}<!DOCTYPE r [<!ENTITY % p \"<!ENTITY a 'v'>\"> %p;{NS}]>"),
                x,
            ),
            // Standalone: an entity declared only in parameter-entity text
            // is not declared for a reference in the document (WFC: Entity
            // Declared); declared outside too, it is, the first declaration
            // binding.
            (format!("{SA}<!DOCTYPE r [<!ENTITY % p \"<!ENTITY ns 'urn:x'>\"> %p;]>"), None),
            (
                format!(
                    "{SA}<!DOCTYPE r [<!ENTITY % p \"<!ENTITY ns 'urn:x'>\"> %p;\
                     <!ENTITY ns \"urn:y\">]>"
                ),
                x,
            ),
        ];
        check_parameter_cases(cases);
    }

    #[test]
    fn included_text_is_checked_where_it_stands() {
        // Included text must be whole declarations (WFC: PE Between
        // Declarations), never include itself (WFC: No Recursion), and
        // passes the subset's checks.
        const NS: &str = "<!ENTITY ns \"urn:x\">";
        let cases: Vec<(String, Option<&str>)> = vec![
            // Not whole declarations, plain text, a conditional section.
            (
                format!("<!DOCTYPE r [{NS}<!ENTITY % p \"<!ENTITY a\"> %p; 'v'>]>"),
                None,
            ),
            (
                format!("<!DOCTYPE r [{NS}<!ENTITY % p \"<!ENTITY a 'v'\"> %p; >]>"),
                None,
            ),
            (
                format!("<!DOCTYPE r [{NS}<!ENTITY % p \"&#60;!-- x\"> %p; -->]>"),
                None,
            ),
            (
                format!("<!DOCTYPE r [{NS}<!ENTITY % p \"hello\"> %p;]>"),
                None,
            ),
            (
                format!("<!DOCTYPE r SYSTEM \"x\" [{NS}<!ENTITY % p \"hello\"> %p;]>"),
                None,
            ),
            (format!("<!DOCTYPE r [{NS}<!ENTITY % p \"]\"> %p;]>"), None),
            (
                format!("<!DOCTYPE r [{NS}<!ENTITY % p \"<![INCLUDE[<!ENTITY b 'v'>]]>\"> %p;]>"),
                None,
            ),
            // A loop.
            (
                format!("<!DOCTYPE r [{NS}<!ENTITY % p \"&#37;p;\"> %p;]>"),
                None,
            ),
            (
                format!(
                    "<!DOCTYPE r [{NS}<!ENTITY % p \"&#37;q;\"><!ENTITY % q \"&#37;p;\"> %p;]>"
                ),
                None,
            ),
            // Included text is checked as the subset is.
            (
                format!(
                    "<!DOCTYPE r [{NS}<!ENTITY % q \"x\"><!ENTITY % p \"<!ENTITY a \
                     '&#37;q;'>\"> %p;]>"
                ),
                None,
            ),
            (
                format!(
                    "<!DOCTYPE r [{NS}<!ENTITY % q \"ANY\"><!ENTITY % p \"<!ELEMENT r \
                     &#37;q;>\"> %p;]>"
                ),
                None,
            ),
            (
                format!("<!DOCTYPE r [{NS}<!ENTITY % p \"<!ENTITY a '&#38;#0;'>\"> %p;]>"),
                None,
            ),
        ];
        check_parameter_cases(cases);
    }

    #[test]
    fn a_parameter_entity_not_read_cuts_off_and_defaults_stand_where_included() {
        use std::fmt::Write as _;
        // A reference not read cuts off (5.1); after a cut-off, included text
        // is checked, not processed; a default in included text is judged where
        // it stands (4.1); the bytes included stay within the document type's
        // length.
        const SA: &str = "<?xml version=\"1.0\" standalone=\"yes\"?>";
        const NS: &str = "<!ENTITY ns \"urn:x\">";
        let x = Some("urn:x");
        let laughs = {
            let mut subset = String::from("<!ENTITY % p0 \"<!ENTITY a 'v'>\">");
            for at in 1..24 {
                let _ = write!(subset, "<!ENTITY % p{at} \"&#37;p{0};&#37;p{0};\">", at - 1);
            }
            subset + "%p23;"
        };
        let cases: Vec<(String, Option<&str>)> = vec![
            // Not read: undeclared (nested too) or external. Not standalone,
            // a cut-off; standalone, an undeclared one refuses and an
            // external one lets processing go on.
            (
                format!("<!DOCTYPE r [{NS}<!ENTITY % p \"&#37;q;\"> %p;]>"),
                x,
            ),
            (
                format!("{SA}<!DOCTYPE r [{NS}<!ENTITY % p \"&#37;q;\"> %p;]>"),
                None,
            ),
            (
                format!("<!DOCTYPE r [{NS} %p; <!ENTITY % p \"<!ENTITY a 'v'>\">]>"),
                x,
            ),
            (
                format!("{SA}<!DOCTYPE r [<!ENTITY % e SYSTEM \"e.ent\"> %e; {NS}]>"),
                x,
            ),
            (
                format!("<!DOCTYPE r [<!ENTITY % e SYSTEM \"e.ent\"> %e; {NS}]>"),
                None,
            ),
            // After a cut-off, included text is checked but not processed.
            (
                "<!DOCTYPE r [<!ENTITY % e SYSTEM \"e.ent\"><!ENTITY % p \"<!ENTITY ns \
                     'urn:x'>\"> %e; %p;]>"
                    .to_owned(),
                None,
            ),
            (
                "<!DOCTYPE r [<!ENTITY ns 'urn:x'><!ENTITY % p \"<!ENTITY a \
                     '&#38;#0;'>\"> %u; %p;]>"
                    .to_owned(),
                None,
            ),
            // ATTLIST defaults in included text, judged where they stand.
            (
                format!(
                    "{SA}<!DOCTYPE r [{NS}<!ENTITY e \"&f;\"><!ENTITY % p \"<!ATTLIST r a \
                     CDATA '&#38;e;'>\"> %p;<!ENTITY f \"v\">]>"
                ),
                None,
            ),
            (
                format!(
                    "{SA}<!DOCTYPE r [{NS}<!ENTITY f \"v\"><!ENTITY e \"&f;\"><!ENTITY % p \
                     \"<!ATTLIST r a CDATA '&#38;e;'>\"> %p;]>"
                ),
                x,
            ),
            (
                format!(
                    "<!DOCTYPE r [{NS}<!ENTITY bad \"x&#60;y\"><!ENTITY % p \"\">%p;\
                     <!ATTLIST r a CDATA \"&bad;\">]>"
                ),
                None,
            ),
            (
                format!(
                    "<!DOCTYPE r [{NS}<!ENTITY ext SYSTEM \"s\"><!ENTITY % p \"\">%p;\
                     <!ATTLIST r a CDATA \"&ext;\">]>"
                ),
                None,
            ),
            // Past the inclusion budget (the document type's length).
            (format!("<!DOCTYPE r [{NS}{laughs}]>"), None),
        ];
        check_parameter_cases(cases);
    }

    /// Each document type of `cases` with a root `xmlns="&ns;"`: its
    /// namespace as expected (`None`: none, or no root element).
    fn check_parameter_cases(cases: Vec<(String, Option<&str>)>) {
        for (doctype, expected) in cases {
            let text = format!("{doctype}<r xmlns=\"&ns;\"/>");
            let found = root_element(text.as_bytes()).map(|(namespace, _)| namespace);
            assert_eq!(found.as_deref(), expected, "{text}");
        }
    }

    #[test]
    fn parameter_entities_are_included_in_linear_time_and_memory() {
        use std::fmt::Write as _;
        // Shapes: "laughs", n entities each including the one before twice,
        // stopped by the budget (the document type's length); "deep", n
        // entities each including the one before, n texts read at once;
        // "many", n entities each included once, and one blank one included n
        // times. The bytes included are at most the document type's length,
        // each read about four times (blanks, a name or a declaration's end,
        // its literals); each item (a declaration and a reference) costs
        // about 60 steps besides (a name hashed and copied, a replacement
        // text built). The bounds are twice that. The budget here is the
        // document type's length alone: the default's 64 KiB extra
        // (`entity_expansion_extra`) would add a constant.
        let length_only = crate::Limits {
            entity_expansion_extra: 0,
            ..crate::Limits::default()
        };
        let subset = |shape: &str, n: usize| {
            let mut subset = String::from("<!ENTITY ns \"urn:x\">");
            match shape {
                "laughs" | "deep" => {
                    subset.push_str("<!ENTITY % p0 \"<!ENTITY a 'v'>\">");
                    let twice = if shape == "laughs" { 2 } else { 1 };
                    for at in 1..n {
                        let _ = write!(
                            subset,
                            "<!ENTITY % p{at} \"{}\">",
                            format!("&#37;p{};", at - 1).repeat(twice)
                        );
                    }
                    let _ = write!(subset, "%p{};", n - 1);
                }
                _ => {
                    subset.push_str("<!ENTITY % b \"  \">");
                    for at in 0..n {
                        let _ = write!(
                            subset,
                            "<!ENTITY % p{at} \"<!ENTITY a{at} 'v'>\">%p{at};%b;"
                        );
                    }
                }
            }
            subset
        };
        for shape in ["laughs", "deep", "many"] {
            crate::work::assert_linear(
                500,
                |n| {
                    let text = format!("<!DOCTYPE r [{}]><r xmlns=\"&ns;\"/>", subset(shape, n));
                    let (found, steps) =
                        crate::work::steps(|| root_element_within(text.as_bytes(), &length_only));
                    assert_eq!(found.is_some(), shape != "laughs", "{shape}");
                    (steps, text.len())
                },
                |items, bytes| 2 * (4 * 2 * bytes + 60 * items),
            );
            // The texts read at once, the entities' texts and their marks:
            // about 40 bytes a level (deep), 1.4 bytes a byte of the subset
            // (0.4 many; laughs about the same before the budget stops it);
            // the bound is about three times that.
            crate::work::assert_linear(
                500,
                |n| {
                    let text = format!("<!DOCTYPE r [{}]><r xmlns=\"&ns;\"/>", subset(shape, n));
                    let (_, held) =
                        crate::work::peak(|| root_element_within(text.as_bytes(), &length_only));
                    (held, text.len())
                },
                |_, bytes| 4 * bytes,
            );
        }
    }

    #[test]
    fn a_parameter_entity_reference_in_a_declaration_refuses_the_document_type() {
        // WFC: PEs in Internal Subset: a parameter-entity reference, or a
        // bare `%`, outside the literals of a markup declaration, in every
        // mode, processed or not (after a cut-off); the `%` that declares a
        // parameter entity is not one.
        let root = |text: &str| root_element(text.as_bytes()).map(|(namespace, _)| namespace);
        for (lead, system, cut) in [
            ("", "", ""),
            ("", " SYSTEM \"x\"", ""),
            ("<?xml version=\"1.0\" standalone=\"yes\"?>", "", ""),
            ("", "", "%q;"),
        ] {
            let text = |declarations: &str| {
                format!(
                    "{lead}<!DOCTYPE r{system} [<!ENTITY ns \"urn:x\">{cut}{declarations}]>\
                     <r xmlns=\"&ns;\"/>"
                )
            };
            for declarations in [
                "<!ENTITY % p \"CDATA\"><!ATTLIST r a %p; \"v\">",
                "<!ENTITY % p \"ANY\"><!ELEMENT r %p;>",
                "<!ENTITY % p \"r\"><!ATTLIST %p; a CDATA \"v\">",
                "<!ENTITY % p \"SYSTEM 'x'\"><!NOTATION n %p;>",
                "<!ENTITY % p \"'v'\"><!ENTITY a %p;>",
                "<!ENTITY % p \"x\"><!ENTITY % q %p;>",
                "<!ENTITY % p \"x\"><!ENTITY %p; \"v\">",
                "<!ELEMENT r (a%)>",
                "<!ATTLIST r a CDATA \"v\" %>",
            ] {
                let text = text(declarations);
                assert_eq!(root(&text), None, "{text}");
            }
            for declarations in [
                "<!ENTITY  %  p \"x\">",
                "<!ATTLIST r a CDATA \"100%\">",
                "<!ATTLIST r a CDATA '%p;'>",
                "<!NOTATION n SYSTEM \"a%b\">",
                "<!-- % --><?pi % ?>",
                "<!ELEMENT r ANY>",
            ] {
                let text = text(declarations);
                assert_eq!(root(&text).as_deref(), Some("urn:x"), "{text}");
            }
        }
    }

    #[test]
    fn a_parameter_entity_reference_in_a_literal_refuses_the_document_type() {
        // WFC: PEs in Internal Subset: in every mode, the parameter entity
        // declared or not; a bare `%` is not allowed in a literal either.
        let root = |text: &str| root_element(text.as_bytes()).map(|(namespace, _)| namespace);
        for (lead, system) in [
            ("", " SYSTEM \"x\""),
            ("<?xml version=\"1.0\" standalone=\"yes\"?>", ""),
        ] {
            for literal in [
                "<!ENTITY % p \"v\"><!ENTITY b \"%p;\">",
                "<!ENTITY b \"%p;\">",
                "<!ENTITY b \"100%\">",
                "<!ENTITY % q \"%p;\">",
            ] {
                let text = format!(
                    "{lead}<!DOCTYPE r{system} [<!ENTITY ns \"urn:x\">{literal}]><r xmlns=\"&ns;\"/>"
                );
                assert_eq!(root(&text), None, "{text}");
            }
        }
        let text = "<!DOCTYPE r [<!ENTITY ns \"urn:x\"><!ENTITY % p \"v\">]><r xmlns=\"&ns;\"/>";
        assert_eq!(root(text).as_deref(), Some("urn:x"));
    }

    #[test]
    fn a_processing_instruction_libxml2_refuses_in_the_internal_subset_refuses_the_document_type() {
        // There libxml2 refuses one named `xml` in any casing ("XML
        // declaration allowed only at the start of the document", "Invalid PI
        // name") and one with no target ("xmlParsePI : no target name"), in
        // the subset or in a parameter entity's text, before or after the
        // declarations, processed or not; one whose name only starts with
        // `xml` draws a warning, and is allowed, as any other is.
        let root = |text: &str| root_element(text.as_bytes()).map(|(namespace, _)| namespace);
        let refused = NAMED_XML
            .iter()
            .map(|&(instruction, _)| instruction)
            .chain([
                "<?xml?>",
                "<?xml ?>",
                "<?xml encoding=\"UTF-8\"?>",
                "<?xml version=\"1.0\"?>",
                "<?xml version=\"1.1\"?>",
                "<?xml\tx?>",
                "<?xml?x?>",
                "<??>",
                "<? x?>",
                "<?1abc x?>",
                "<?-x x?>",
                "<?\u{85}x?>",
            ]);
        let allowed = NOT_NAMED_XML.into_iter().chain([
            "<?foo bar?>",
            "<?x?>",
            "<?xm x?>",
            "<?xmlfoo x?>",
            "<?xml:foo x?>",
            "<?a:b x?>",
            "<?\u{e9} x?>",
        ]);
        let texts = |lead: &str, instruction: &str| {
            let ns = "<!ENTITY ns \"urn:x\">";
            let root = "<r xmlns=\"&ns;\"/>";
            [
                format!("{lead}<!DOCTYPE r [{instruction}{ns}]>{root}"),
                format!("{lead}<!DOCTYPE r [{ns}{instruction}]>{root}"),
                format!("{lead}<!DOCTYPE r SYSTEM \"y\" [{instruction}{ns}]>{root}"),
                format!("{lead}<!DOCTYPE r [<!ENTITY % p '{instruction}'>{ns}%p;]>{root}"),
                // After a reference to an undeclared parameter entity,
                // nothing is processed (5.1).
                format!("{lead}<!DOCTYPE r [{ns}%q;{instruction}]>{root}"),
            ]
        };
        for lead in ["", "<?xml version=\"1.1\"?>"] {
            for instruction in refused.clone() {
                for text in texts(lead, instruction) {
                    assert_eq!(root(&text), None, "{text:?}");
                }
            }
            for instruction in allowed.clone() {
                for text in texts(lead, instruction) {
                    assert_eq!(root(&text).as_deref(), Some("urn:x"), "{text:?}");
                }
            }
        }
        // In a per-type file, a namespace declared with an entity then
        // declared nowhere is handled as with any document type libxml2
        // refuses.
        let messages = |info: &TypeInfo| -> Vec<String> {
            info.diagnostics.iter().map(|d| d.message.clone()).collect()
        };
        let per_type = |subset: &str| {
            parse(&format!(
                "<!DOCTYPE mime-type [{subset}]><mime-type xmlns=\"&ns;\"><comment>a</comment></mime-type>"
            ))
        };
        let malformed = per_type(&format!("<!ENTITY ns \"{NAMESPACE}\" junk>"));
        assert!(!malformed.diagnostics.is_empty());
        for instruction in refused {
            let info = per_type(&format!("{instruction}<!ENTITY ns \"{NAMESPACE}\">"));
            assert_eq!(messages(&info), messages(&malformed), "{instruction:?}");
            assert_eq!(comments(&info), comments(&malformed), "{instruction:?}");
        }
        for instruction in allowed {
            let info = per_type(&format!("{instruction}<!ENTITY ns \"{NAMESPACE}\">"));
            assert!(
                info.diagnostics.is_empty(),
                "{instruction:?}: {:?}",
                info.diagnostics
            );
            assert_eq!(comments(&info), [(None, "a")], "{instruction:?}");
        }
    }

    /// The diagnostic for a document type libxml2 refuses.
    const REFUSED_DOCTYPE: &str =
        "bad XML (the document type declaration cannot be read); it declares nothing";

    /// Processing instructions libxml2 refuses that are not named `xml`,
    /// each with what is wrong, after "a processing instruction's target ".
    const REFUSED_INSTRUCTIONS: [(&str, &str); 10] = [
        ("<??>", "is missing or not an XML name"),
        ("<? x?>", "is missing or not an XML name"),
        ("<?1abc x?>", "is missing or not an XML name"),
        ("<?-x x?>", "is missing or not an XML name"),
        ("<?\u{85}x?>", "is missing or not an XML name"),
        ("<?foo\"bar\"?>", "is not followed by white space"),
        ("<?foo?bar?>", "is not followed by white space"),
        ("<?foo\u{a0}bar?>", "is not followed by white space"),
        // Not white space inside markup, in XML 1.1 too (a known limit);
        // libxml2, which reads XML 1.1 as 1.0, refuses them too.
        ("<?foo\u{85}bar?>", "is not followed by white space"),
        ("<?foo\u{2028}bar?>", "is not followed by white space"),
    ];

    /// Processing instructions libxml2 allows, with or without a warning.
    const ALLOWED_INSTRUCTIONS: [&str; 9] = [
        "<?foo bar?>",
        "<?foo\tbar?>",
        "<?foo\rbar?>",
        "<?foo\nbar?>",
        "<?foo?>",
        "<?xmlfoo x?>",
        "<?xml-stylesheet href=\"a\"?>",
        "<?xml:foo x?>",
        "<?\u{e9} x?>",
    ];

    #[test]
    fn a_document_type_libxml2_refuses_is_reported_once_and_declares_nothing() {
        // Whatever the form, one diagnostic of its own, at the document type,
        // besides those where its entities are used.
        let ns = format!("<!ENTITY ns \"{NAMESPACE}\">");
        let refused = [
            format!("<!DOCTYPE mime-type [{ns}<!ENTITY e \"x\" junk>]>"),
            format!("<!DOCTYPE mime-type [{ns}<!ENTITY e \"x\" NDATA n>]>"),
            format!("<!DOCTYPE mime-type [{ns}<!ENTITY % p \"ANY\"><!ELEMENT r %p;>]>"),
            format!("<!DOCTYPE mime-type [{ns}<!ENTITY b \"100%\">]>"),
            format!("<!DOCTYPE mime-type [{ns}<!ENTITY b \"a & b\">]>"),
            format!("<!DOCTYPE mime-type [{ns}<!ATTLIST mime-type a CDATA \"<\">]>"),
            format!("<!DOCTYPE mime-type [{ns}<!ATTLIST mime-type a CDATA \"&u;\">]>"),
            format!("<!DOCTYPE mime-type [{ns}<!ENTITY % p \"%p;\">%p;]>"),
            format!("<!DOCTYPE mime-type [{ns}<!ENTITY % p \"<!ENTITY\">%p;]>"),
            format!("<!DOCTYPE mime-type [{ns}<?xml?>]>"),
            format!("<!DOCTYPE mime-type [<?XML x?>{ns}]>"),
            format!("<!DOCTYPE mime-type [{ns}<? x?>]>"),
            format!("<!DOCTYPE mime-type [{ns}<?foo\"bar\"?>]>"),
            format!("<!DOCTYPE mime-type SYSTEMX [{ns}]>"),
            format!("<!DOCTYPE mime-type SYSTEM\"y\" [{ns}]>"),
            format!("<!doctype mime-type [{ns}]>"),
            format!("<!DOCTYPEmime-type [{ns}]>"),
        ];
        let messages = |info: &TypeInfo| -> Vec<String> {
            info.diagnostics.iter().map(|d| d.message.clone()).collect()
        };
        for (lead, doctype) in refused.iter().map(|doctype| ("", doctype.clone())).chain([(
            "<?xml version=\"1.0\" standalone=\"yes\"?>",
            format!("<!DOCTYPE mime-type [{ns}%q;]>"),
        )]) {
            let at = lead.len();
            // Its entities unused: the one diagnostic, at the document type.
            let text = format!("{lead}{doctype}{OPEN}<comment>a</comment></mime-type>");
            let info = parse(&text);
            assert_eq!(
                info.diagnostics,
                [Diagnostic::new(
                    "t.xml",
                    Some(crate::Location::Offset(at)),
                    REFUSED_DOCTYPE
                )],
                "{text:?}"
            );
            assert_eq!(comments(&info), [(None, "a")], "{text:?}");
            // Used, each use reported too, the document type once.
            let text = format!(
                "{lead}{doctype}{OPEN}<comment>a</comment><comment xml:lang=\"de\"><x:b xmlns:x=\"&ns;\"/>b</comment><comment xml:lang=\"fr\"><x:b xmlns:x=\"&ns;\"/>c</comment></mime-type>"
            );
            let info = parse(&text);
            let found = messages(&info);
            assert_eq!(found[0], REFUSED_DOCTYPE, "{text:?}");
            assert_eq!(
                found.iter().filter(|m| *m == REFUSED_DOCTYPE).count(),
                1,
                "{text:?}: {found:?}"
            );
            assert!(found.len() > 1, "{text:?}: {found:?}");
            assert_eq!(comments(&info), [(None, "a")], "{text:?}");
            // Root detection stays silent, as it has nowhere to report: it
            // gives only the root's namespace and name, or none. The
            // document type declares nothing.
            let text = format!("{lead}{doctype}<mime-type xmlns=\"&ns;\"/>");
            assert_eq!(root_element(text.as_bytes()), None, "{text:?}");
        }
        // One read, or not the first, is not reported.
        for (doctype, found) in [
            (format!("<!DOCTYPE mime-type [<?foo bar?>{ns}]>"), vec![]),
            ("<!DOCTYPE mime-type>".to_owned(), vec![]),
            ("<!DOCTYPE mime-type SYSTEM \"y\">".to_owned(), vec![]),
            (
                format!("<!DOCTYPE mime-type><!DOCTYPE mime-type [{ns}<?xml?>]>"),
                vec!["bad XML (a second document type declaration)".to_owned()],
            ),
        ] {
            let text = format!("{doctype}{OPEN}<comment>a</comment></mime-type>");
            let info = parse(&text);
            assert_eq!(messages(&info), found, "{text:?}");
            assert_eq!(comments(&info), [(None, "a")], "{text:?}");
        }
    }

    #[test]
    fn a_processing_instruction_libxml2_refuses_is_reported_in_the_body() {
        // As an XML declaration out of place is, also at the start; after
        // the root, the rest is skipped.
        let field = "<comment>a</comment>";
        for lead in ["", "<?xml version=\"1.1\"?>"] {
            for (instruction, problem) in REFUSED_INSTRUCTIONS {
                let message = format!("bad XML (a processing instruction's target {problem})");
                let skipped = format!("{message}; the rest is skipped");
                for (text, at, found) in [
                    (
                        format!("{lead}{instruction}{OPEN}{field}</mime-type>"),
                        lead.len(),
                        &message,
                    ),
                    (
                        format!("{lead}{OPEN}{instruction}{field}</mime-type>"),
                        lead.len() + OPEN.len(),
                        &message,
                    ),
                    (
                        format!("{lead}{OPEN}<comment>a{instruction}</comment></mime-type>"),
                        lead.len() + OPEN.len() + "<comment>a".len(),
                        &message,
                    ),
                    (
                        format!("{lead}{OPEN}{field}</mime-type>{instruction}<!-- -->x"),
                        lead.len() + OPEN.len() + field.len() + "</mime-type>".len(),
                        &skipped,
                    ),
                ] {
                    let info = parse(&text);
                    assert_eq!(
                        info.diagnostics,
                        [Diagnostic::new(
                            "t.xml",
                            Some(crate::Location::Offset(at)),
                            found.clone()
                        )],
                        "{text:?}"
                    );
                    assert_eq!(comments(&info), [(None, "a")], "{text:?}");
                }
            }
            for instruction in ALLOWED_INSTRUCTIONS {
                for text in [
                    format!("{lead}{instruction}{OPEN}{field}</mime-type>"),
                    format!("{lead}{OPEN}<comment>a{instruction}</comment></mime-type>"),
                    format!("{lead}{OPEN}{field}</mime-type>{instruction}"),
                ] {
                    let info = parse(&text);
                    assert!(
                        info.diagnostics.is_empty(),
                        "{text:?}: {:?}",
                        info.diagnostics
                    );
                    assert_eq!(comments(&info), [(None, "a")], "{text:?}");
                }
            }
        }
    }

    #[test]
    fn a_processing_instruction_libxml2_refuses_is_treated_as_a_late_declaration_by_root_detection()
    {
        // Skipped, as `<?xml version="1.0"?>` out of place is; in the
        // internal subset it refuses the document type.
        let doc = Some(("urn:test".to_owned(), "doc".to_owned()));
        let refused = REFUSED_INSTRUCTIONS
            .iter()
            .map(|&(instruction, _)| instruction)
            .chain(["<?xml version=\"1.0\"?>"]);
        for instruction in refused {
            for text in [
                format!(r#"{instruction}<doc xmlns="urn:test"/>"#),
                format!(r#"<?xml version="1.1"?>{instruction}<doc xmlns="urn:test"/>"#),
                format!(r#"<!-- -->{instruction}<doc xmlns="urn:test"/>"#),
            ] {
                assert_eq!(root_element(text.as_bytes()), doc, "{text:?}");
            }
            let text = format!(
                r#"<!DOCTYPE doc [{instruction}<!ENTITY ns "urn:test">]><doc xmlns="&ns;"/>"#
            );
            assert_eq!(root_element(text.as_bytes()), None, "{text:?}");
        }
        for instruction in ALLOWED_INSTRUCTIONS {
            let text = format!(
                r#"<!DOCTYPE doc [{instruction}<!ENTITY ns "urn:test">]><doc xmlns="&ns;"/>"#
            );
            assert_eq!(root_element(text.as_bytes()), doc, "{text:?}");
        }
    }

    #[test]
    fn a_document_type_head_libxml2_refuses_is_refused_and_reported() {
        // A name that is not an XML name, a name not followed by ASCII white
        // space, `[` or `>`, text after the name or external ID that is
        // neither, and anything but white space between the subset's `]` and
        // `>`: libxml2 refuses them
        // ("xmlParseDocTypeDecl : no DOCTYPE name !", "DOCTYPE improperly
        // terminated", "Content error in the internal subset"). In an
        // XML 1.1 file NEL and LS there are not read as line ends, so not
        // as white space (a known limit); libxml2, which reads XML 1.1 as
        // 1.0, refuses them too.
        let ns = "<!ENTITY ns \"urn:x\">";
        let with_subset = [
            "<!DOCTYPE {n} [{s}] junk>",
            "<!DOCTYPE {n} [{s}]junk>",
            "<!DOCTYPE {n} [{s}]]>",
            "<!DOCTYPE {n} [{s}] [ ]>",
            "<!DOCTYPE {n} [{s}]\u{85}>",
            "<!DOCTYPE {n} [{s}]\u{a0}>",
            "<!DOCTYPE {n} [{s} junk]>",
            "<!DOCTYPE 1{n} [{s}]>",
            "<!DOCTYPE -{n} [{s}]>",
            "<!DOCTYPE {n}\u{85}[{s}]>",
            "<!DOCTYPE {n}\u{2028}[{s}]>",
            "<!DOCTYPE {n}\u{a0}[{s}]>",
            "<!DOCTYPE {n} \u{85}[{s}]>",
            "<!DOCTYPE {n} junk [{s}]>",
            "<!DOCTYPE {n} system \"y\" [{s}]>",
            "<!DOCTYPE {n} SYSTEM \"y\" SYSTEM \"z\" [{s}]>",
            "<!DOCTYPE {n} SYSTEM \"y\"\u{85}[{s}]>",
            "<!DOCTYPE {n} SYSTEM \"y\" junk [{s}]>",
        ];
        let without_subset = [
            "<!DOCTYPE {n} junk>",
            "<!DOCTYPE {n} system \"y\">",
            "<!DOCTYPE {n} SYSTEM \"y\" SYSTEM \"z\">",
            "<!DOCTYPE {n} SYSTEM \"y\" junk>",
            "<!DOCTYPE 1{n}>",
            "<!DOCTYPE {n}\u{85}>",
            "<!DOCTYPE {n}\u{2028}>",
            "<!DOCTYPE {n}\u{a0}>",
        ];
        let accepted = [
            "<!DOCTYPE {n}>",
            "<!DOCTYPE {n} >",
            "<!DOCTYPE {n}\t\r\n>",
            "<!DOCTYPE {n}[{s}]>",
            "<!DOCTYPE {n} [{s}]>",
            "<!DOCTYPE {n} [{s}] >",
            "<!DOCTYPE {n} [{s}]\t\r\n>",
            "<!DOCTYPE {n} SYSTEM \"y\">",
            "<!DOCTYPE {n} SYSTEM \"y\" >",
            "<!DOCTYPE {n} SYSTEM \"y\"[{s}]>",
            "<!DOCTYPE {n} PUBLIC \"p\" \"s\" [{s}]>",
            "<!DOCTYPE {n} PUBLIC \"p\" \"s\"[{s}]>",
            "<!DOCTYPE {n}:x [{s}]>",
        ];
        let fill = |form: &str, name: &str| form.replace("{n}", name).replace("{s}", ns);
        for lead in ["", "<?xml version=\"1.1\"?>"] {
            for form in with_subset.iter().chain(&without_subset) {
                let doctype = fill(form, "mime-type");
                let text = format!("{lead}{doctype}{OPEN}<comment>a</comment></mime-type>");
                let info = parse(&text);
                assert_eq!(
                    info.diagnostics,
                    [Diagnostic::new(
                        "t.xml",
                        Some(crate::Location::Offset(lead.len())),
                        REFUSED_DOCTYPE
                    )],
                    "{text:?}"
                );
                assert_eq!(comments(&info), [(None, "a")], "{text:?}");
            }
            for form in with_subset {
                let text = format!("{lead}{}<r xmlns=\"&ns;\"/>", fill(form, "r"));
                assert_eq!(root_element(text.as_bytes()), None, "{text:?}");
            }
            for form in accepted {
                let doctype = fill(form, "mime-type");
                let text = format!("{lead}{doctype}{OPEN}<comment>a</comment></mime-type>");
                let info = parse(&text);
                assert!(
                    info.diagnostics.is_empty(),
                    "{text:?}: {:?}",
                    info.diagnostics
                );
                assert_eq!(comments(&info), [(None, "a")], "{text:?}");
                if form.contains("{s}") {
                    let text = format!("{lead}{}<r xmlns=\"&ns;\"/>", fill(form, "r"));
                    assert_eq!(
                        root_element(text.as_bytes()).map(|(namespace, _)| namespace),
                        Some("urn:x".to_owned()),
                        "{text:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn nel_or_ls_in_xml_1_1_parameter_entity_text_is_a_line_end_after_a_target() {
        // In XML 1.1 a NEL or LS written in a literal is a line end,
        // normalised before the text is included, so the instruction is read
        // as with LF; written in the subset itself, it is not white space
        // there (the known limit), and libxml2, which reads XML 1.1 as 1.0,
        // refuses it too; in XML 1.0 it is no line end.
        let root = |text: &str| root_element(text.as_bytes()).map(|(namespace, _)| namespace);
        for c in ['\u{85}', '\u{2028}'] {
            for (lead, included, direct) in [
                ("<?xml version=\"1.1\"?>", Some("urn:x"), None),
                ("", None, None),
            ] {
                let text = format!(
                    "{lead}<!DOCTYPE r [<!ENTITY % p '<?foo{c}bar?>'><!ENTITY ns \"urn:x\">%p;]><r xmlns=\"&ns;\"/>"
                );
                assert_eq!(root(&text).as_deref(), included, "{text:?}");
                let text = format!(
                    "{lead}<!DOCTYPE r [<?foo{c}bar?><!ENTITY ns \"urn:x\">]><r xmlns=\"&ns;\"/>"
                );
                assert_eq!(root(&text).as_deref(), direct, "{text:?}");
            }
        }
    }

    /// Declarations libxml2 (21504) refuses in an internal subset, which
    /// refuse the document type.
    const REFUSED_DECLARATIONS: [&str; 51] = [
        "<![IGNORE[ ]]>",
        "<![IGNORE[ x ]]>",
        "<![INCLUDE[ ]]>",
        "<![CDATA[ x ]]>",
        "<!DOCTYPE r>",
        "<!FOO bar>",
        "<!entity f \"x\">",
        "<!ENTITYf \"x\">",
        "<!-- c --->",
        "<!-- a -- b -->",
        "<!ENTITY 1f \"x\">",
        "<!ENTITY f>",
        "<!ENTITY>",
        "<!ENTITY f SYSTEM>",
        "<!ENTITY f SYSTEM\"s\">",
        "<!ENTITY f SYSTEM \"y#f\">",
        "<!ENTITY f PUBLIC \"p\" \"y#f\">",
        "<!ENTITY f PUBLIC \"p<\" \"s\">",
        "<!ENTITY f PUBLIC \"p\tq\" \"s\">",
        "<!ENTITY f PUBLIC \"p\">",
        "<!ENTITY f SYSTEM \"s\"NDATA n>",
        "<!ENTITY f SYSTEM \"s\" NDATA n junk>",
        "<!ENTITY f SYSTEM \"s\" NDATA>",
        "<!ENTITY f \"x\"junk>",
        "<!ENTITY f\"x\">",
        "<!ELEMENT r>",
        "<!ELEMENT>",
        "<!ELEMENT r (a,b|c)>",
        "<!ELEMENT r ANY junk>",
        "<!ELEMENT 1r ANY>",
        "<!ELEMENT r any>",
        "<!ELEMENT r(a)>",
        "<!ELEMENT r (#PCDATA|a)>",
        "<!ELEMENT r (#PCDATA)+>",
        "<!ELEMENT r ((#PCDATA))>",
        "<!ELEMENT r (a) *>",
        "<!ELEMENT r ()>",
        "<!ELEMENT r (a,)>",
        "<!ATTLIST>",
        "<!ATTLIST r a CDATA x>",
        "<!ATTLIST r a CDATA \"x\" junk>",
        "<!ATTLIST r a CDATA \"x\"b CDATA \"y\">",
        "<!ATTLIST r a cdata #IMPLIED>",
        "<!ATTLIST r a NOTATION(n) \"n\">",
        "<!ATTLIST r a () \"x\">",
        "<!ATTLIST r a CDATA #FIXED>",
        "<!ATTLIST r a>",
        "<!NOTATION n>",
        "<!NOTATION n SYSTEM>",
        "<!NOTATION n PUBLIC \"p<\">",
        "<!NOTATION n SYSTEM \"s\" junk>",
    ];

    /// Declarations libxml2 (21504) takes in an internal subset.
    const ACCEPTED_DECLARATIONS: [&str; 24] = [
        "<!ELEMENT r EMPTY>",
        "<!ELEMENT r ANY >",
        "<!ELEMENT r (#PCDATA)>",
        "<!ELEMENT r (#PCDATA)*>",
        "<!ELEMENT r ( #PCDATA | a | b )*>",
        "<!ELEMENT r (a,(b|c)*,d)+>",
        "<!ELEMENT r ((a,b)|c)>",
        "<!ELEMENT r ( a , b? )>",
        "<!ELEMENT r (a:b)>",
        "<!ATTLIST r>",
        "<!ATTLIST r a ID #IMPLIED b IDREF #REQUIRED c (x|y) \"x\" d NOTATION (n) #IMPLIED e CDATA #FIXED \"v\">",
        "<!ATTLIST r a IDREFS #IMPLIED b ENTITY #IMPLIED c ENTITIES #IMPLIED d NMTOKEN #IMPLIED e NMTOKENS #IMPLIED>",
        "<!ATTLIST r a ( x | 1y ) 'x'>",
        "<!ATTLIST r xmlns:a CDATA #IMPLIED>",
        "<!NOTATION n PUBLIC \"p\">",
        "<!NOTATION n PUBLIC \"p\" \"s\">",
        "<!NOTATION n SYSTEM \"y#f\">",
        "<!ENTITY f PUBLIC \"-//p (x)//EN\" \"s\">",
        "<!ENTITY f SYSTEM \"s\" NDATA n>",
        "<!ENTITY\tf\t'x'\t>",
        "<!---->",
        "<!--- c -->",
        "<!-- c- -->",
        "<?foo bar?>",
    ];

    #[test]
    fn the_internal_subset_is_read_by_xmls_grammar() {
        // Each declaration whole, as xmllint (libxml 21504) reads it, in the
        // subset and in parameter-entity text; one it refuses refuses the
        // document type, reported once.
        let root = |text: &str| root_element(text.as_bytes()).map(|(namespace, _)| namespace);
        let check = |lead: &str, subset: &str, found: Option<&str>| {
            let mut texts = vec![format!(
                "{lead}<!DOCTYPE r [<!ENTITY ns \"urn:x\">{subset}]><r xmlns=\"&ns;\"/>"
            )];
            if !subset.contains(['\'', '%', '&']) {
                texts.push(format!(
                    "{lead}<!DOCTYPE r [<!ENTITY ns \"urn:x\"><!ENTITY % p '{subset}'>%p;]><r xmlns=\"&ns;\"/>"
                ));
            }
            for text in texts {
                assert_eq!(root(&text).as_deref(), found, "{text:?}");
            }
            let text = format!(
                "{lead}<!DOCTYPE mime-type [{subset}]>{OPEN}<comment>a</comment></mime-type>"
            );
            let info = parse(&text);
            let refused = Diagnostic::new(
                "t.xml",
                Some(crate::Location::Offset(lead.len())),
                REFUSED_DOCTYPE,
            );
            assert_eq!(
                info.diagnostics,
                found.map_or(vec![refused], |_| vec![]),
                "{text:?}"
            );
            assert_eq!(comments(&info), [(None, "a")], "{text:?}");
        };
        // Content models nest as deep as libxml2 allows by default.
        let nested = |n: usize| format!("<!ELEMENT r {}a{}>", "(".repeat(n), ")".repeat(n));
        for lead in ["", "<?xml version=\"1.1\"?>"] {
            for subset in REFUSED_DECLARATIONS {
                check(lead, subset, None);
            }
            for subset in ACCEPTED_DECLARATIONS {
                check(lead, subset, Some("urn:x"));
            }
            check(lead, &nested(MAX_CONTENT_DEPTH), Some("urn:x"));
            check(lead, &nested(MAX_CONTENT_DEPTH + 1), None);
            // A public literal holds only `PubidChar`s, in the head too.
            for (id, found) in [
                ("\"p<\"", None),
                ("\"p{\"", None),
                ("\"p\u{e9}\"", None),
                ("'p\"q'", None),
                ("\"p\tq\"", None),
                ("\"p'q\"", Some("urn:x")),
                (
                    "\"-//p (x) a+b,c.d/e:f=g?h;i!j*k#l@m$n_o%p//EN\"",
                    Some("urn:x"),
                ),
            ] {
                let text = format!(
                    "{lead}<!DOCTYPE r PUBLIC {id} \"s\" [<!ENTITY ns \"urn:x\">]><r xmlns=\"&ns;\"/>"
                );
                assert_eq!(root(&text).as_deref(), found, "{text:?}");
            }
        }
        // The internal subset freedesktop.org.xml has, from the pinned
        // shared-mime-info source.
        let source = std::fs::read_to_string(
            crate::read::testing::reference_source().join("data/freedesktop.org.xml.in"),
        )
        .unwrap();
        let start = source.find("<!DOCTYPE mime-info [").unwrap();
        let end = start + source[start..].find("]>").unwrap();
        let subset = &source[start + "<!DOCTYPE mime-info [".len()..end];
        assert!(subset.contains("<!ATTLIST generic-icon name ("), "{subset}");
        check("", subset, Some("urn:x"));
    }

    #[test]
    fn the_internal_subsets_grammar_is_read_in_linear_time() {
        // A content model of n names, an attribute list of n definitions
        // and an enumeration of n tokens: each byte looked at a few times
        // (the grammar, `%` outside literals, the literals, the defaults).
        let dtd = |n: usize| {
            format!(
                "<!DOCTYPE r [<!ENTITY ns \"urn:x\"><!ELEMENT r ({})*><!ATTLIST r {}><!ATTLIST r e ({}) #IMPLIED>]>",
                vec!["a"; n].join("|"),
                (0..n)
                    .map(|at| format!("a{at} CDATA \"v\""))
                    .collect::<Vec<_>>()
                    .join(" "),
                vec!["t"; n].join("|"),
            )
        };
        crate::work::assert_linear(
            1000,
            |n| {
                let text = format!("{}<r xmlns=\"&ns;\"/>", dtd(n));
                let (found, steps) = crate::work::steps(|| root_element(text.as_bytes()));
                assert!(found.is_some());
                (steps, text.len())
            },
            |_, bytes| 2 * (6 * bytes + 200),
        );
    }

    #[test]
    fn a_character_the_version_does_not_allow_refuses_the_subset() {
        // Written as it is in a literal, a comment or a processing
        // instruction of the internal subset, or in the head's system
        // literal, a character that is not a `Char`, or in XML 1.1 a
        // restricted one, refuses the document type, as xmllint
        // (libxml 21504) has it; reported once, nothing declared. libxml2
        // reads XML 1.1 as 1.0, so takes U+007F and U+009F there.
        let positions = [
            "<!-- a{c}b -->",
            "<?pi a{c}b?>",
            "<!ENTITY e \"a{c}b\">",
            "<!ENTITY % p \"a{c}b\">",
            "<!ATTLIST e a CDATA \"a{c}b\">",
            "<!ATTLIST e a CDATA #FIXED 'a{c}b'>",
            "<!NOTATION n SYSTEM \"a{c}b\">",
            "<!NOTATION n PUBLIC \"p\" \"a{c}b\">",
            "<!ENTITY e SYSTEM \"a{c}b\">",
            "<!ENTITY e PUBLIC \"p\" 'a{c}b' NDATA n>",
        ];
        let root = |text: &str| root_element(text.as_bytes()).map(|(namespace, _)| namespace);
        for (lead, c, allowed) in [
            ("", '\u{0}', false),
            ("", '\u{1}', false),
            ("", '\u{1F}', false),
            ("", '\u{FFFE}', false),
            ("", '\u{7F}', true),
            ("", '\u{9F}', true),
            ("", '\u{85}', true),
            ("", '\u{2028}', true),
            ("", '\u{E9}', true),
            ("<?xml version=\"1.1\"?>", '\u{0}', false),
            ("<?xml version=\"1.1\"?>", '\u{1}', false),
            ("<?xml version=\"1.1\"?>", '\u{FFFE}', false),
            ("<?xml version=\"1.1\"?>", '\u{7F}', false),
            ("<?xml version=\"1.1\"?>", '\u{9F}', false),
            ("<?xml version=\"1.1\"?>", '\u{85}', true),
            ("<?xml version=\"1.1\"?>", '\u{2028}', true),
            ("<?xml version=\"1.1\"?>", '\u{E9}', true),
        ] {
            let found = allowed.then_some("urn:x");
            let subsets = positions.map(|position| position.replace("{c}", &c.to_string()));
            for subset in &subsets {
                let text = format!(
                    "{lead}<!DOCTYPE r [<!ENTITY ns \"urn:x\">{subset}]><r xmlns=\"&ns;\"/>"
                );
                assert_eq!(root(&text).as_deref(), found, "{text:?}");
                let text = format!(
                    "{lead}<!DOCTYPE mime-type [{subset}]>{OPEN}<comment>a</comment></mime-type>"
                );
                let info = parse(&text);
                let refused = Diagnostic::new(
                    "t.xml",
                    Some(crate::Location::Offset(lead.len())),
                    REFUSED_DOCTYPE,
                );
                let expected = if allowed { vec![] } else { vec![refused] };
                assert_eq!(info.diagnostics, expected, "{text:?}");
                assert_eq!(comments(&info), [(None, "a")], "{text:?}");
            }
            // The head's system literal.
            for head in [
                format!("SYSTEM \"a{c}b\""),
                format!("PUBLIC \"p\" \"a{c}b\""),
            ] {
                let text = format!(
                    "{lead}<!DOCTYPE r {head} [<!ENTITY ns \"urn:x\">]><r xmlns=\"&ns;\"/>"
                );
                assert_eq!(root(&text).as_deref(), found, "{text:?}");
                let text = format!(
                    "{lead}<!DOCTYPE mime-type {head}>{OPEN}<comment>a</comment></mime-type>"
                );
                assert_eq!(parse(&text).diagnostics.is_empty(), allowed, "{text:?}");
            }
        }
        // Parameter-entity text is judged as its literal was: a reference
        // to a restricted character, which XML 1.1 allows (and libxml2,
        // which reads XML 1.1 as 1.0, does not), gives one in the text, read
        // as there; XML 1.0 has no such reference.
        for (lead, found) in [("", None), ("<?xml version=\"1.1\"?>", Some("urn:x"))] {
            let text = format!(
                "{lead}<!DOCTYPE r [<!ENTITY ns \"urn:x\"><!ENTITY % p '<!-- a&#1;b -->'>%p;]><r xmlns=\"&ns;\"/>"
            );
            assert_eq!(root(&text).as_deref(), found, "{text:?}");
        }
    }

    #[test]
    fn a_comment_or_instruction_with_a_character_the_version_does_not_allow_is_reported() {
        // In the body, as a refused instruction is: before or inside the
        // root, the values still read; after it, the rest skipped. Root
        // detection stays silent, as it has nowhere to report: it gives only
        // the root's namespace and name, or none.
        let field = "<comment>a</comment>";
        for (lead, c, allowed) in [
            ("", '\u{1}', false),
            ("", '\u{FFFE}', false),
            ("", '\u{7F}', true),
            ("", '\u{85}', true),
            ("<?xml version=\"1.1\"?>", '\u{1}', false),
            ("<?xml version=\"1.1\"?>", '\u{7F}', false),
            ("<?xml version=\"1.1\"?>", '\u{85}', true),
            ("<?xml version=\"1.1\"?>", '\u{E9}', true),
        ] {
            for (markup, what) in [
                (format!("<!--a{c}b-->"), "comment"),
                (format!("<?pi a{c}b?>"), "processing instruction"),
            ] {
                let message = format!(
                    "bad XML (a {what} with a character XML does not allow, U+{:04X})",
                    u32::from(c)
                );
                let skipped = format!("{message}; the rest is skipped");
                for (text, at, found) in [
                    (
                        format!("{lead}{markup}{OPEN}{field}</mime-type>"),
                        lead.len(),
                        &message,
                    ),
                    (
                        format!("{lead}{OPEN}{markup}{field}</mime-type>"),
                        lead.len() + OPEN.len(),
                        &message,
                    ),
                    (
                        format!("{lead}{OPEN}<comment>a{markup}</comment></mime-type>"),
                        lead.len() + OPEN.len() + "<comment>a".len(),
                        &message,
                    ),
                    (
                        format!("{lead}{OPEN}{field}</mime-type>{markup}<!-- -->x"),
                        lead.len() + OPEN.len() + field.len() + "</mime-type>".len(),
                        &skipped,
                    ),
                ] {
                    let info = parse(&text);
                    let expected = if allowed && text.ends_with('x') {
                        // Read on: the text after the root is reported.
                        vec![Diagnostic::new(
                            "t.xml",
                            Some(crate::Location::Offset(text.len() - 1)),
                            "bad XML (content after the root element); the rest is skipped",
                        )]
                    } else if allowed {
                        vec![]
                    } else {
                        vec![Diagnostic::new(
                            "t.xml",
                            Some(crate::Location::Offset(at)),
                            found.clone(),
                        )]
                    };
                    assert_eq!(info.diagnostics, expected, "{text:?}");
                    assert_eq!(comments(&info), [(None, "a")], "{text:?}");
                }
                for text in [
                    format!("{lead}{markup}<doc xmlns=\"urn:test\"/>"),
                    format!("{lead}<doc xmlns=\"urn:test\">{markup}</doc>"),
                ] {
                    assert_eq!(
                        root_element(text.as_bytes()),
                        Some(("urn:test".to_owned(), "doc".to_owned())),
                        "{text:?}"
                    );
                }
            }
        }
    }
}
