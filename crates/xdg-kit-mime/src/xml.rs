//! The per-type `MEDIA/SUBTYPE.xml` files (spec "The MEDIA/SUBTYPE.xml
//! files"), and the root element of XML documents for `XMLnamespaces`.

use std::path::{Path, PathBuf};

use quick_xml::NsReader;
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;

/// The namespace of the database's XML files.
const NAMESPACE: &str = "http://www.freedesktop.org/standards/shared-mime-info";
use xdg_kit_locale::Locales;

use crate::{Diagnostic, MimeDatabase, MimeType};

/// Values by language (`None` for the untranslated one), later entries
/// replacing earlier ones for the same language.
#[derive(Debug, Default)]
struct Localized(Vec<(Option<String>, String)>);

impl Localized {
    fn set(&mut self, lang: Option<String>, text: String) {
        match self.0.iter_mut().find(|(known, _)| *known == lang) {
            Some((_, value)) => *value = text,
            None => self.0.push((lang, text)),
        }
    }

    /// The value for the first of `locales`' keys present, else the
    /// untranslated one.
    fn pick(&self, locales: &Locales) -> Option<String> {
        let get = |lang: Option<&str>| {
            self.0
                .iter()
                .find(|(known, _)| known.as_deref() == lang)
                .map(|(_, value)| value.clone())
        };
        locales.select(|key| get(Some(key))).or_else(|| get(None))
    }
}

/// What a type's XML files say, merged over the folders.
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
            match std::fs::read(&path) {
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
        let mut reader = NsReader::from_reader(bytes);
        let mut depth = 0usize;
        // The element being collected and its language.
        let mut field: Option<(Field, Option<String>, String)> = None;
        loop {
            let (namespace, event) = match reader.read_resolved_event() {
                Ok(resolved) => resolved,
                Err(err) => {
                    let at = usize::try_from(reader.error_position()).unwrap_or(usize::MAX);
                    self.diagnostics.push(Diagnostic::new(
                        path,
                        Some(crate::Location::Offset(at)),
                        format!("bad XML ({err}); the rest is skipped"),
                    ));
                    return;
                }
            };
            match event {
                Event::Start(start) => {
                    depth += 1;
                    // Only the spec's own elements (or ones in no namespace);
                    // an extension's `<x:comment>` is not the comment.
                    let ours = match &namespace {
                        ResolveResult::Bound(bound) => bound.as_ref() == NAMESPACE,
                        ResolveResult::Unbound => true,
                        ResolveResult::Unknown(_) => false,
                    };
                    if depth == 2 && ours {
                        let kind = match start.local_name().as_ref() {
                            "comment" => Some(Field::Comment),
                            "acronym" => Some(Field::Acronym),
                            "expanded-acronym" => Some(Field::ExpandedAcronym),
                            _ => None,
                        };
                        if let Some(kind) = kind {
                            let lang = start
                                .try_get_attribute("xml:lang")
                                .ok()
                                .flatten()
                                .and_then(|attribute| {
                                    attribute
                                        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                                        .ok()
                                })
                                .map(std::borrow::Cow::into_owned)
                                .filter(|lang| !lang.is_empty());
                            field = Some((kind, lang, String::new()));
                        }
                    }
                }
                Event::Text(text) => {
                    if let Some((_, _, value)) = &mut field {
                        value.push_str(&text.xml10_content());
                    }
                }
                Event::CData(data) => {
                    if let Some((_, _, value)) = &mut field {
                        value.push_str(&data.xml10_content());
                    }
                }
                Event::GeneralRef(reference) => {
                    if let Some((_, _, value)) = &mut field {
                        let name = reference.xml10_content();
                        if let Ok(Some(c)) = reference.resolve_char_ref() {
                            value.push(c);
                        } else if let Some(resolved) =
                            quick_xml::escape::resolve_predefined_entity(&name)
                        {
                            value.push_str(resolved);
                        }
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
                }
                Event::Eof => return,
                _ => {}
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Field {
    Comment,
    Acronym,
    ExpandedAcronym,
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
            if let Ok(mime) = MimeType::new(&format!("{media}/{subtype}")) {
                types.push(mime);
            }
        }
    }
    types
}

/// The namespace URI (empty when there is none) and local name of the root
/// element of the XML document `bytes` starts, if it gets that far.
pub(crate) fn root_element(bytes: &[u8]) -> Option<(String, String)> {
    let mut reader = NsReader::from_reader(bytes);
    loop {
        let (namespace, event) = reader.read_resolved_event().ok()?;
        match event {
            Event::Start(element) | Event::Empty(element) => {
                let namespace = match namespace {
                    ResolveResult::Bound(namespace) => namespace.into_inner().to_owned(),
                    ResolveResult::Unbound => String::new(),
                    ResolveResult::Unknown(_) => return None,
                };
                let local = element.local_name().as_ref().to_owned();
                return Some((namespace, local));
            }
            Event::Eof => return None,
            _ => {}
        }
    }
}

impl MimeDatabase {
    /// A human-readable description of `mime` ("PNG image") in the first of
    /// `locales` that has one, else untranslated. Read from the type's XML
    /// files on first use and kept in this snapshot.
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
    fn broken_xml_is_reported_and_types_are_listed_from_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("text")).unwrap();
        std::fs::create_dir_all(dir.path().join("packages")).unwrap();
        std::fs::write(
            dir.path().join("text/plain.xml"),
            "<mime-type><comment>Text</commen>",
        )
        .unwrap();
        std::fs::write(dir.path().join("packages/x.xml"), "").unwrap();
        let plain = MimeType::new("text/plain").unwrap();
        let info = TypeInfo::read(&plain, [dir.path()].into_iter());
        assert_eq!(info.diagnostics.len(), 1);
        assert_eq!(types_in(dir.path()), [plain]);
    }
}
