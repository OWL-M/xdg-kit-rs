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
