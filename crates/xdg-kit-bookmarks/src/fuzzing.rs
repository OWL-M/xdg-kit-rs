//! Entry points for the fuzz targets in `fuzz/`. Built only with
//! `--cfg fuzzing`, which `cargo fuzz` sets; not part of the API.

use std::time::{Duration, SystemTime};

use xdg_kit_mime::MimeType;

use crate::{BookmarkFile, Opened};

/// Parses `bytes` as XBEL and holds the document to its own contracts:
/// the diagnostics and entries walk, an editable document serializes
/// byte-identically while unedited, and an edit sequence (picked by the
/// first byte) serializes to a document that loads back editable with
/// the edits in place.
pub fn xbel(bytes: &[u8]) {
    let mut file = BookmarkFile::from_bytes(bytes);
    for diagnostic in file.diagnostics() {
        let _ = (&diagnostic.path, diagnostic.offset, &diagnostic.message);
    }
    for bookmark in file.bookmarks() {
        let _ = (bookmark.visited(), bookmark.mime_type());
    }
    if !file.is_editable() {
        return;
    }
    assert_eq!(
        file.to_bytes(),
        bytes,
        "an unedited editable document is byte-identical"
    );
    let opened = Opened {
        name: "fuzz".to_owned(),
        exec: "'fuzz %u'".to_owned(),
        mime: MimeType::new("text/plain").expect("a fixed type"),
        groups: vec!["fuzzed".to_owned()],
        private: true,
        when: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000),
    };
    if file.open("file:///fuzzed", &opened).is_err() {
        // The input itself can make this fixed, valid edit refusable:
        // entries sharing the URI (ambiguous) or a full bookmark budget.
        return;
    }
    match bytes.first().copied().unwrap_or(0) % 3 {
        0 => {}
        1 => {
            assert!(file.remove("file:///fuzzed").expect("still editable"));
            file.open("file:///fuzzed", &opened)
                .expect("removal freed the budget");
        }
        _ => file.clear().expect("still editable"),
    }
    let out = file.to_bytes();
    let again = BookmarkFile::from_bytes(&out);
    assert!(again.is_editable(), "edited output loads back editable");
    assert_eq!(
        again.get("file:///fuzzed").is_some(),
        file.get("file:///fuzzed").is_some(),
        "the edits survive the round trip"
    );
}
