//! Entry points for the fuzz targets in `fuzz/`. Built only with
//! `--cfg fuzzing`, which `cargo fuzz` sets; not part of the API.

use std::path::Path;

use crate::Diagnostic;
use crate::model::Folder;
use crate::read::{cache, text};

/// Exercises a parsed folder as the database would: converts magic rules
/// to host byte order and matches them against `data`, calculates the
/// extent, and formats each diagnostic.
fn use_folder(folder: &Folder, diagnostics: &[Diagnostic], data: &[u8]) {
    for rule in &folder.magic {
        for matchlet in crate::database::to_host(&rule.matchlets) {
            let _ = crate::detect::matchlet_matches(&matchlet, data);
        }
    }
    let _ = folder.magic_extent();
    for diagnostic in diagnostics {
        let _ = diagnostic.to_string();
    }
}

/// Decodes `bytes` as a `mime.cache`, then runs its magic against the same
/// bytes.
pub fn mime_cache(bytes: &[u8]) {
    let mut folder = Folder::default();
    let mut diagnostics = Vec::new();
    let _ = cache::read(
        Path::new("mime.cache"),
        bytes,
        &mut folder,
        &mut diagnostics,
    );
    use_folder(&folder, &diagnostics, bytes);
}

/// A `magic` file and data to match: a two-byte big-endian length, that
/// many bytes of rules (the file after its `MIME-Magic` header, which is
/// added here so that mutations do not lose it), then the data.
pub fn magic(bytes: &[u8]) {
    let Some((len, rest)) = bytes.split_first_chunk::<2>() else {
        return;
    };
    let len = usize::from(u16::from_be_bytes(*len)).min(rest.len());
    let (rules, data) = rest.split_at(len);
    let mut file = b"MIME-Magic\0\n".to_vec();
    file.extend_from_slice(rules);
    parse("magic", &file, data);
}

/// Parses `bytes` as the text file `name`, then uses what it gives.
pub fn text_file(name: &str, bytes: &[u8]) {
    parse(name, bytes, bytes);
}

fn parse(name: &str, file: &[u8], data: &[u8]) {
    let mut folder = Folder::default();
    let mut diagnostics = Vec::new();
    text::parse(name, Path::new(name), file, &mut folder, &mut diagnostics);
    use_folder(&folder, &diagnostics, data);
}

/// Parses `bytes` as a per-type `MEDIA/SUBTYPE.xml` file and picks each
/// value for a few languages.
pub fn type_xml(bytes: &[u8]) {
    crate::xml::TypeInfo::parse_bytes(bytes).pick_all();
}

/// Looks for the root element of `bytes` as an XML document.
pub fn xml_root(bytes: &[u8]) {
    let _ = crate::xml::root_element(bytes);
}

/// Matches `name` against `pattern`.
pub fn fnmatch(pattern: &str, name: &[u8]) {
    let _ = crate::fnmatch::matches(pattern, name);
}

/// Separates the parts of a [`database`] input.
pub const SEPARATOR: &[u8] = b"\0--xkt--\0";

/// The files each folder of a [`database`] input has, in order. Keep
/// `fuzz/make-seeds.sh`, which writes the seeds, in step.
pub const FILES: [&str; 5] = ["globs2", "aliases", "subclasses", "magic", "XMLnamespaces"];

/// Builds a whole database and runs lookups in it. Splits `bytes` at
/// [`SEPARATOR`] into two folders' [`FILES`], most important folder first,
/// followed by a file name and data. Missing parts are empty. Adds a
/// `MIME-Magic` header to any `magic` part that lacks one, so mutations
/// cannot remove it. Checks that every detected type is canonical and a
/// kind of itself, but never its own ancestor, even when the files contain
/// parent loops.
pub fn database(bytes: &[u8]) {
    let mut parts: Vec<&[u8]> = Vec::new();
    let mut rest = bytes;
    while let Some(at) = rest
        .windows(SEPARATOR.len())
        .position(|window| window == SEPARATOR)
    {
        parts.push(&rest[..at]);
        rest = &rest[at + SEPARATOR.len()..];
    }
    parts.push(rest);
    let part = |at: usize| parts.get(at).copied().unwrap_or_default();
    // Each folder's files, then the name and the data.
    let magic_at = FILES
        .iter()
        .position(|name| *name == "magic")
        .expect("listed");
    let with_header = |bytes: &[u8]| {
        if bytes.starts_with(b"MIME-Magic\0\n") {
            bytes.to_vec()
        } else {
            [b"MIME-Magic\0\n".as_slice(), bytes].concat()
        }
    };
    let magics: Vec<Vec<u8>> = (0..2)
        .map(|folder| with_header(part(folder * FILES.len() + magic_at)))
        .collect();
    let folders: Vec<Vec<(&str, &[u8])>> = (0..2)
        .map(|folder| {
            FILES
                .iter()
                .enumerate()
                .map(|(at, name)| {
                    let bytes = if at == magic_at {
                        magics[folder].as_slice()
                    } else {
                        part(folder * FILES.len() + at)
                    };
                    (*name, bytes)
                })
                .collect()
        })
        .collect();
    let name = String::from_utf8_lossy(part(2 * FILES.len())).into_owned();
    let data = part(2 * FILES.len() + 1);
    let db = crate::MimeDatabase::from_files(&folders);
    let mut found: Vec<crate::MimeType> = db
        .type_for_name(&name)
        .matches()
        .iter()
        .map(|found| found.mime.clone())
        .collect();
    found.push(db.type_for_data(data).mime);
    found.push(db.type_for_name_and_data(&name, data).mime);
    for mime in &found {
        assert_eq!(&db.canonical(mime), mime, "detected types are canonical");
        assert!(db.is_a(mime, mime), "{mime} is a kind of itself");
        assert!(
            !db.ancestors(mime).contains(mime),
            "{mime} is not its own ancestor"
        );
        let _ = db.parents(mime);
        let _ = db.aliases(mime);
    }
}
