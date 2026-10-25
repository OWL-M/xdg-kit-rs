//! The logical document end to end: mutations, their refusals, and the
//! serialized result's preservation guarantees.

use std::time::{Duration, SystemTime};

use xdg_kit_mime::MimeType;

use crate::document::tests::SAMPLE;
use crate::{BookmarkFile, EditError, Opened};

fn mime(text: &str) -> MimeType {
    MimeType::new(text).unwrap()
}

fn when(text: &str) -> SystemTime {
    crate::time::parse(text).unwrap()
}

fn opened() -> Opened {
    Opened {
        name: "viewer".to_owned(),
        exec: "'viewer %u'".to_owned(),
        mime: mime("image/png"),
        groups: Vec::new(),
        private: false,
        when: when("2024-01-02T03:04:05Z"),
    }
}

fn out(file: &BookmarkFile) -> String {
    String::from_utf8(file.to_bytes()).unwrap()
}

#[test]
fn no_edits_serialize_byte_identically() {
    let file = BookmarkFile::from_bytes(SAMPLE.as_bytes());
    assert_eq!(file.to_bytes(), SAMPLE.as_bytes());
}

#[test]
fn a_visit_changes_only_the_affected_bytes() {
    let mut file = BookmarkFile::from_bytes(SAMPLE.as_bytes());
    let uri = "file:///home/u/a%20b.txt";
    let mut record = opened();
    record.name = "editor".to_owned();
    record.exec = "'editor %u'".to_owned();
    file.open(uri, &record).unwrap();
    // Read-your-writes, before any serialization.
    let seen = file.get(uri).unwrap();
    assert_eq!(seen.visited(), Some(record.when));
    assert_eq!(seen.modified(), Some(record.when));
    assert_eq!(seen.mime_type(), Some("text/plain"), "mime is preserved");
    assert_eq!(
        seen.added(),
        crate::time::parse("2023-05-04T10:17:17.138078Z")
    );
    let apps = seen.applications();
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0].count(), Some(3), "incremented");
    // The output differs from the source only inside that bookmark's
    // tag and its application element: timestamps and the count.
    let written = out(&file);
    assert_eq!(
        written,
        SAMPLE
            .replace(
                "modified=\"2023-05-04T10:17:18Z\" visited=\"2023-05-04T10:17:19Z\"",
                "modified=\"2024-01-02T03:04:05Z\" visited=\"2024-01-02T03:04:05Z\""
            )
            .replace(
                "modified=\"2023-05-04T10:17:19Z\" count=\"2\"",
                "modified=\"2024-01-02T03:04:05Z\" count=\"3\""
            ),
        "comments, foreign metadata, unknown elements, whitespace: all kept"
    );
}

#[test]
fn set_mime_type_touches_only_that_element() {
    let mut file = BookmarkFile::from_bytes(SAMPLE.as_bytes());
    file.set_mime_type("file:///home/u/a%20b.txt", &mime("text/markdown"))
        .unwrap();
    assert_eq!(
        out(&file),
        SAMPLE.replace("type=\"text/plain\"", "type=\"text/markdown\""),
        "the foreign owner's mime-type is untouched"
    );
}

#[test]
fn an_insertion_appends_a_complete_escaped_entry() {
    let mut file = BookmarkFile::from_bytes(SAMPLE.as_bytes());
    let mut record = opened();
    record.name = "a<b&c\"d".to_owned();
    record.exec = "'tool %u' --flag=\"x\"".to_owned();
    record.groups = vec!["g&1".to_owned()];
    record.private = true;
    let uri = "file:///home/u/new<&>.png";
    file.open(uri, &record).unwrap();
    assert_eq!(file.get(uri).unwrap().mime_type(), Some("image/png"));
    assert_eq!(file.bookmarks().count(), 3);
    let written = out(&file);
    // The output parses back to the same logical content.
    let reread = BookmarkFile::from_bytes(written.as_bytes());
    assert!(reread.is_editable(), "{:?}", reread.diagnostics());
    let entry = reread.get(uri).expect("the new entry round-trips");
    assert_eq!(entry.mime_type(), Some("image/png"));
    assert_eq!(entry.groups(), ["g&1"]);
    assert!(entry.is_private());
    assert_eq!(entry.added(), Some(record.when));
    let apps = entry.applications();
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0].name(), Some("a<b&c\"d"));
    assert_eq!(apps[0].exec(), Some("'tool %u' --flag=\"x\""));
    assert_eq!(apps[0].count(), Some(1));
    // Everything before the insertion point is untouched.
    let end = SAMPLE.find("</xbel>").unwrap();
    assert_eq!(&written[..end], &SAMPLE[..end]);
}

#[test]
fn nul_and_other_unwritable_input_is_refused_unchanged() {
    let mut file = BookmarkFile::from_bytes(SAMPLE.as_bytes());
    let mut record = opened();
    record.name = "a\u{0}b".to_owned();
    let refused = file.open("file:///x", &record);
    assert!(
        matches!(refused, Err(EditError::InvalidText("application name"))),
        "{refused:?}"
    );
    let early = SystemTime::UNIX_EPOCH - Duration::from_secs(1 << 35);
    let refused = file.open(
        "file:///x",
        &Opened {
            when: early,
            ..opened()
        },
    );
    assert!(
        matches!(refused, Err(EditError::TimeOutOfRange)),
        "{refused:?}"
    );
    assert_eq!(file.to_bytes(), SAMPLE.as_bytes(), "nothing changed");
}

#[test]
fn registration_merges_groups_and_only_sets_private() {
    let mut file = BookmarkFile::from_bytes(SAMPLE.as_bytes());
    let uri = "file:///home/u/a%20b.txt";
    let mut record = opened();
    record.groups = vec!["notes & things".to_owned(), "pictures".to_owned()];
    record.private = false;
    file.open(uri, &record).unwrap();
    let seen = file.get(uri).unwrap();
    assert_eq!(seen.groups(), ["notes & things", "pictures"], "merged");
    assert!(seen.is_private(), "false never clears the mark");
    let written = out(&file);
    let reread = BookmarkFile::from_bytes(written.as_bytes());
    assert_eq!(
        reread.get(uri).unwrap().groups(),
        ["notes & things", "pictures"]
    );
    assert!(reread.get(uri).unwrap().is_private());
}

#[test]
fn a_new_application_and_missing_structure_are_created_in_place() {
    // The second sample bookmark has no info at all; a visit builds the
    // whole chain and the result parses back.
    let mut file = BookmarkFile::from_bytes(SAMPLE.as_bytes());
    let uri = "sftp://host/file";
    file.open(uri, &opened()).unwrap();
    let written = out(&file);
    let reread = BookmarkFile::from_bytes(written.as_bytes());
    assert!(reread.is_editable(), "{written}");
    let entry = reread.get(uri).unwrap();
    assert_eq!(
        entry.mime_type(),
        None,
        "a visit never writes a type the entry did not have"
    );
    assert_eq!(entry.visited(), Some(opened().when));
    let apps = entry.applications();
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0].name(), Some("viewer"));
    assert_eq!(apps[0].count(), Some(1));
}

#[test]
fn lifecycle_before_any_write_composes() {
    let mut file = BookmarkFile::from_bytes(SAMPLE.as_bytes());
    let uri = "file:///home/u/fresh.txt";
    let mut record = opened();
    record.mime = mime("text/plain");
    file.open(uri, &record).unwrap();
    record.when += Duration::from_secs(60);
    file.open(uri, &record).unwrap();
    let seen = file.get(uri).unwrap();
    assert_eq!(seen.applications()[0].count(), Some(2), "updates compose");
    assert_eq!(seen.visited(), Some(record.when));
    file.set_mime_type(uri, &mime("text/x-readme")).unwrap();
    assert_eq!(file.get(uri).unwrap().mime_type(), Some("text/x-readme"));
    assert!(file.remove(uri).unwrap());
    assert!(file.get(uri).is_none());
    assert_eq!(file.bookmarks().count(), 2);
    assert_eq!(file.to_bytes(), SAMPLE.as_bytes(), "all pending edits gone");
    assert!(!file.remove(uri).unwrap(), "nothing left to remove");
}

#[test]
fn removal_takes_the_element_and_its_indentation_not_the_comment() {
    let mut file = BookmarkFile::from_bytes(SAMPLE.as_bytes());
    assert!(file.remove("file:///home/u/a%20b.txt").unwrap());
    let written = out(&file);
    assert!(
        written.contains("<!-- written by hand -->"),
        "comments stay"
    );
    assert!(!written.contains("<bookmark href=\"file:"));
    assert!(
        written.contains("<bookmark href=\"sftp:"),
        "the other stays"
    );
    let reread = BookmarkFile::from_bytes(written.as_bytes());
    assert!(reread.is_editable(), "{written}");
    assert_eq!(reread.bookmarks().count(), 1);
    // No doubled blank line where the element was: its own indentation
    // went with it.
    assert!(!written.contains("\n\n\n"), "{written:?}");
}

#[test]
fn clear_empties_the_list_but_keeps_the_frame() {
    let mut file = BookmarkFile::from_bytes(SAMPLE.as_bytes());
    file.open("file:///pending", &opened()).unwrap();
    file.clear().unwrap();
    assert_eq!(file.bookmarks().count(), 0);
    let written = out(&file);
    assert!(written.starts_with("<?xml"));
    assert!(written.contains("<xbel"));
    assert!(written.contains("</xbel>"));
    assert!(!written.contains("<bookmark"));
    assert!(written.contains("<!-- written by hand -->"));
}

#[test]
fn duplicates_block_updates_and_leave_with_remove() {
    let text = "<xbel><bookmark href=\"x\"/><bookmark href=\"x\"/>\
                <bookmark href=\"y\"/></xbel>";
    let mut file = BookmarkFile::from_bytes(text.as_bytes());
    assert!(matches!(
        file.open("x", &opened()),
        Err(EditError::AmbiguousUri(_))
    ));
    assert!(matches!(
        file.set_mime_type("x", &mime("text/plain")),
        Err(EditError::AmbiguousUri(_))
    ));
    assert!(matches!(
        file.set_mime_type("absent", &mime("text/plain")),
        Err(EditError::NoSuchUri(_))
    ));
    assert!(file.remove("x").unwrap(), "removes all duplicates");
    assert_eq!(file.bookmarks().count(), 1);
    assert_eq!(out(&file), "<xbel><bookmark href=\"y\"/></xbel>");
}

#[test]
fn uneditable_documents_refuse_every_mutation() {
    let mut file = BookmarkFile::from_bytes(b"<xbel><bookmark href=\"x\"");
    assert!(matches!(
        file.open("x", &opened()),
        Err(EditError::NotEditable)
    ));
    assert!(matches!(
        file.set_mime_type("x", &mime("text/plain")),
        Err(EditError::NotEditable)
    ));
    assert!(matches!(file.remove("x"), Err(EditError::NotEditable)));
    assert!(matches!(file.clear(), Err(EditError::NotEditable)));
    assert_eq!(
        file.to_bytes(),
        b"<xbel><bookmark href=\"x\"",
        "serialized as read"
    );
}

#[test]
fn inserting_resolves_prefixes_by_uri_or_self_declares() {
    // The bookmark namespace under another prefix: reused, not renamed.
    let other = "<xbel xmlns:bm=\"http://www.freedesktop.org/standards/desktop-bookmarks\">\n\
                 </xbel>\n";
    let mut file = BookmarkFile::from_bytes(other.as_bytes());
    let mut record = opened();
    record.groups = vec!["g".to_owned()];
    file.open("file:///x", &record).unwrap();
    let written = out(&file);
    assert!(written.contains("<bm:groups>"), "{written}");
    assert!(written.contains("<bm:application "), "{written}");
    assert!(
        written.contains("xmlns:mime="),
        "the undeclared namespace is self-declared: {written}"
    );
    let reread = BookmarkFile::from_bytes(written.as_bytes());
    assert!(reread.is_editable(), "{written}");
    assert_eq!(reread.get("file:///x").unwrap().groups(), ["g"]);
    // No declarations at all: everything self-declares and round-trips.
    let mut bare = BookmarkFile::from_bytes(b"<xbel></xbel>");
    bare.open("file:///y", &record).unwrap();
    let written = out(&bare);
    let reread = BookmarkFile::from_bytes(written.as_bytes());
    assert!(reread.is_editable(), "{written}");
    let entry = reread.get("file:///y").unwrap();
    assert_eq!(entry.groups(), ["g"]);
    assert_eq!(entry.applications()[0].name(), Some("viewer"));
    assert_eq!(entry.mime_type(), Some("image/png"));
}

/// `n` entries with distinct URIs, and the bytes of those URIs.
fn many(n: usize) -> (String, usize) {
    use std::fmt::Write as _;
    let mut text = String::from("<xbel>\n");
    let mut uris = 0;
    for at in 0..n {
        let _ = writeln!(
            text,
            "  <bookmark href=\"file:///n{at}\" added=\"2024-01-02T03:04:05Z\"/>"
        );
        uris += format!("file:///n{at}").len();
    }
    text.push_str("</xbel>\n");
    (text, uris)
}

#[test]
fn large_files_are_read_in_linear_time() {
    crate::work::assert_linear(
        2_000,
        |n| {
            let (text, _) = many(n);
            let (file, steps) = crate::work::steps(|| BookmarkFile::from_bytes(text.as_bytes()));
            assert_eq!(file.bookmarks().count(), n);
            (steps, text.len())
        },
        // Twice the analysed cost. Per byte: the event walk counts each
        // byte once with its event, the attributes' names and values are
        // examined again in the tag reader, each timestamp is parsed and
        // each href hashed and copied by the index (three more). Per
        // event (at most four per entry plus the frame): a step, the
        // lookup, and the copies around it.
        |n, bytes| 2 * (8 * (n + 4) + 6 * bytes),
    );
}

#[test]
fn lookups_take_constant_time() {
    crate::work::assert_linear(
        2_000,
        |n| {
            let (text, uris) = many(n);
            let file = BookmarkFile::from_bytes(text.as_bytes());
            let (found, steps) = crate::work::steps(|| {
                (0..n)
                    .filter(|at| file.get(&format!("file:///n{at}")).is_some())
                    .count()
            });
            assert_eq!(found, n);
            (steps, uris)
        },
        // Per lookup: two counted steps and the key formatted; per key
        // byte: hashed, and compared on the match.
        |n, bytes| 2 * (3 * n + 3 * bytes),
    );
}

#[test]
fn serializing_is_linear_in_the_source() {
    crate::work::assert_linear(
        2_000,
        |n| {
            let (text, _) = many(n);
            let mut file = BookmarkFile::from_bytes(text.as_bytes());
            file.set_mime_type("file:///n0", &mime("text/plain"))
                .unwrap();
            let (written, steps) = crate::work::steps(|| file.to_bytes());
            assert!(written.len() > text.len());
            (steps, text.len())
        },
        // The source copied around the patches, the patch text, and the
        // lookup that placed the edit.
        |_, bytes| 2 * (bytes + 200),
    );
}

#[test]
fn insertions_into_a_container_use_its_own_scope() {
    // The applications container redeclares `bookmark` to mean a foreign
    // namespace; an insertion resolved against the metadata's scope would
    // land under that foreign meaning and vanish on re-reading.
    let text = "<xbel xmlns:bookmark=\"http://www.freedesktop.org/standards/desktop-bookmarks\">\n\
          <bookmark href=\"x\">\n    <info>\n      <metadata owner=\"http://freedesktop.org\">\n\
        <b2:applications xmlns:b2=\"http://www.freedesktop.org/standards/desktop-bookmarks\" \
         xmlns:bookmark=\"http://example.org/foreign\">\n\
          <b2:application name=\"old\" exec=\"'o %u'\" modified=\"2024-01-02T03:04:05Z\" count=\"1\"/>\n\
        </b2:applications>\n      </metadata>\n    </info>\n  </bookmark>\n</xbel>\n";
    let mut file = BookmarkFile::from_bytes(text.as_bytes());
    assert!(file.is_editable(), "{:?}", file.diagnostics());
    file.open("x", &opened()).unwrap();
    let written = out(&file);
    let reread = BookmarkFile::from_bytes(written.as_bytes());
    assert!(reread.is_editable(), "{written}");
    let apps = reread.get("x").unwrap().applications();
    let names: Vec<_> = apps.iter().map(crate::Application::name).collect();
    assert_eq!(names, [Some("old"), Some("viewer")], "{written}");
}

#[test]
fn new_metadata_inside_an_info_uses_the_infos_scope() {
    // The <info> redeclares `b`; a metadata block created inside it must
    // not render children under the shadowed, foreign meaning.
    let text = "<xbel xmlns:b=\"http://www.freedesktop.org/standards/desktop-bookmarks\">\n\
          <bookmark href=\"x\">\n    <info xmlns:b=\"urn:foreign\">\n    </info>\n  </bookmark>\n</xbel>\n";
    let mut file = BookmarkFile::from_bytes(text.as_bytes());
    assert!(file.is_editable(), "{:?}", file.diagnostics());
    file.open("x", &opened()).unwrap();
    assert_eq!(file.get("x").unwrap().applications().len(), 1);
    let written = out(&file);
    let reread = BookmarkFile::from_bytes(written.as_bytes());
    assert!(reread.is_editable(), "{written}");
    let apps = reread.get("x").unwrap().applications();
    assert_eq!(apps.len(), 1, "{written}");
    assert_eq!(apps[0].name(), Some("viewer"));
}

#[test]
fn still_more_invalid_xml_is_uneditable() {
    let cases: [(&str, &str); 6] = [
        ("<xbel>&#1;</xbel>", "which XML cannot hold"),
        ("<xbel><:bad/></xbel>", "not an XML name"),
        (
            "<xbel xmlns:xml=\"urn:wrong\"/>",
            "only to the XML namespace",
        ),
        (
            "<xbel><?xml version=\"1.0\"?></xbel>",
            "declaration appears past the start",
        ),
        ("<xbel><!-- \u{1} --></xbel>", "cannot appear in XML"),
        (
            "<xbel xmlns:other=\"http://www.w3.org/XML/1998/namespace\"/>",
            "only to the prefix `xml`",
        ),
    ];
    for (text, expected) in cases {
        let file = BookmarkFile::from_bytes(text.as_bytes());
        assert!(!file.is_editable(), "{text}");
        assert!(
            file.diagnostics()
                .iter()
                .any(|d| d.message.contains(expected)),
            "{text}: {:?}",
            file.diagnostics()
        );
    }
}

#[test]
fn many_attributes_are_checked_in_linear_time() {
    use std::fmt::Write as _;
    let text = |n: usize| {
        let mut text = String::from("<xbel><bookmark href=\"x\"");
        for at in 0..n {
            let _ = write!(text, " a{at}=\"v\"");
        }
        text.push_str("/></xbel>");
        text
    };
    crate::work::assert_linear(
        2_000,
        |n| {
            let text = text(n);
            let (file, steps) = crate::work::steps(|| BookmarkFile::from_bytes(text.as_bytes()));
            assert!(file.is_editable());
            (steps, text.len())
        },
        // Per byte: the event walk, the attribute reader's examination,
        // the QName split, and the expanded-name key built, hashed and
        // copied by the set (a handful of passes in all).
        |n, bytes| 2 * (10 * (n + 8) + 8 * bytes),
    );
}

#[test]
fn group_text_normalizes_line_endings_but_references_stay() {
    let text = "<xbel xmlns:bookmark=\"http://www.freedesktop.org/standards/desktop-bookmarks\">\
                <bookmark href=\"x\"><info><metadata owner=\"http://freedesktop.org\">\
                <bookmark:groups><bookmark:group>a\r\nb\rc&#13;d</bookmark:group>\
                </bookmark:groups></metadata></info></bookmark></xbel>";
    let file = BookmarkFile::from_bytes(text.as_bytes());
    assert!(file.is_editable(), "{:?}", file.diagnostics());
    assert_eq!(
        file.get("x").unwrap().groups(),
        ["a\nb\nc\rd"],
        "literal line ends normalize; the reference keeps its return"
    );
}

#[test]
fn a_byte_order_mark_shifts_no_span() {
    // quick-xml counts positions past a BOM; a span not shifted back
    // would splice edits into the wrong bytes.
    for text in [
        "\u{FEFF}<xbel><bookmark href=\"x\" visited=\"2023-05-04T10:17:19Z\"/><bookmark href=\"y\"/></xbel>",
        "\u{FEFF}<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<xbel><bookmark href=\"x\" visited=\"2023-05-04T10:17:19Z\"/><bookmark href=\"y\"/></xbel>",
    ] {
        let mut file = BookmarkFile::from_bytes(text.as_bytes());
        assert!(file.is_editable(), "{:?}", file.diagnostics());
        assert_eq!(file.to_bytes(), text.as_bytes(), "byte-identical, BOM too");
        let mut record = opened();
        record.name = "editor".to_owned();
        file.open("x", &record).unwrap();
        file.open("file:///a", &opened()).unwrap();
        file.remove("y").unwrap();
        let written = out(&file);
        assert!(written.starts_with('\u{FEFF}'), "the mark survives");
        let reread = BookmarkFile::from_bytes(written.as_bytes());
        assert!(reread.is_editable(), "{written}");
        let x = reread.get("x").expect("the edited entry survives");
        assert_eq!(x.visited(), Some(record.when), "{written}");
        assert!(reread.get("file:///a").is_some(), "{written}");
        assert!(reread.get("y").is_none(), "{written}");
    }
}

#[test]
fn a_byte_order_marked_file_writes_and_reloads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recently-used.xbel");
    std::fs::write(&path, "\u{FEFF}<xbel><bookmark href=\"x\"/></xbel>").unwrap();
    let mut file = BookmarkFile::read(&path).unwrap();
    file.open("file:///a", &opened()).unwrap();
    let _synced = file.write().unwrap();
    let reread = BookmarkFile::read(&path).unwrap();
    assert!(reread.is_editable(), "{:?}", reread.diagnostics());
    assert!(reread.get("x").is_some());
    assert!(reread.get("file:///a").is_some());
}

#[test]
fn prologue_and_raw_attribute_errors_are_uneditable() {
    let cases: [(&str, &str); 4] = [
        ("<xbel><bookmark href='a<b'/></xbel>", "a literal `<`"),
        (
            "<?xml version='garbage'?><xbel/>",
            "is not a value version can have",
        ),
        (
            "<!DOCTYPE xbel><!DOCTYPE xbel><xbel/>",
            "more than one document type",
        ),
        ("<xbel><!DOCTYPE xbel></xbel>", "past the root element"),
    ];
    for (text, expected) in cases {
        let file = BookmarkFile::from_bytes(text.as_bytes());
        assert!(!file.is_editable(), "{text}");
        assert!(
            file.diagnostics()
                .iter()
                .any(|d| d.message.contains(expected)),
            "{text}: {:?}",
            file.diagnostics()
        );
    }
    // An escaped `&lt;` in a value is fine, and stays one.
    let fine = BookmarkFile::from_bytes(b"<xbel><bookmark href=\"a&lt;b\"/></xbel>");
    assert!(fine.is_editable(), "{:?}", fine.diagnostics());
    assert_eq!(fine.bookmarks().next().unwrap().href(), "a<b");
    assert_eq!(fine.to_bytes(), b"<xbel><bookmark href=\"a&lt;b\"/></xbel>");
}

#[test]
fn many_namespaces_are_resolved_in_linear_time() {
    use std::fmt::Write as _;
    // N declarations in scope and N lookups that find no default
    // namespace: a scan per lookup would be quadratic.
    let text = |n: usize| {
        let mut text = String::from("<xbel");
        for at in 0..n {
            let _ = write!(text, " xmlns:p{at}=\"urn:n{at}\"");
        }
        text.push('>');
        for _ in 0..n {
            text.push_str("<u/>");
        }
        text.push_str("</xbel>");
        text
    };
    crate::work::assert_linear(
        1_000,
        |n| {
            let text = text(n);
            let (file, steps) = crate::work::steps(|| BookmarkFile::from_bytes(text.as_bytes()));
            assert!(file.is_editable());
            (steps, text.len())
        },
        // Per byte: the event walk, the attribute reader, the snapshot
        // copy of the root's bindings (once — one element declares), and
        // the resolver's keys hashed and copied. Per element: the events,
        // the lookups, and the frame bookkeeping.
        |n, bytes| 2 * (12 * (n + 8) + 8 * bytes),
    );
}

#[test]
fn a_named_prefix_cannot_be_bound_to_nothing() {
    let file = BookmarkFile::from_bytes(b"<xbel xmlns:p=\"\"><p:bookmark href=\"x\"/></xbel>");
    assert!(!file.is_editable());
    assert!(
        file.diagnostics()
            .iter()
            .any(|d| d.message.contains("cannot be bound to nothing")),
        "{:?}",
        file.diagnostics()
    );
    // Resetting the default namespace stays legal.
    let reset = BookmarkFile::from_bytes(
        b"<xbel><bookmark href=\"x\" xmlns=\"urn:other\"/>\
          <bookmark href=\"y\"><info xmlns=\"\"/></bookmark></xbel>",
    );
    assert!(reset.is_editable(), "{:?}", reset.diagnostics());
}

#[test]
fn declaration_grammar_and_pi_targets_are_enforced() {
    let cases: [(&str, &str); 4] = [
        (
            "<?xml version='1.0' standalone='maybe'?><xbel/>",
            "is not a value standalone can have",
        ),
        (
            "<?xml version='1.0' bogus='x'?><xbel/>",
            "cannot stand here in an XML declaration",
        ),
        (
            "<?xml version='1.0' version='1.0'?><xbel/>",
            "malformed XML declaration",
        ),
        (
            "<xbel><?1bad x?></xbel>",
            "not a processing instruction target",
        ),
    ];
    for (text, expected) in cases {
        let file = BookmarkFile::from_bytes(text.as_bytes());
        assert!(!file.is_editable(), "{text}");
        assert!(
            file.diagnostics()
                .iter()
                .any(|d| d.message.contains(expected)),
            "{text}: {:?}",
            file.diagnostics()
        );
    }
    // The full well-formed shapes stay fine.
    for text in [
        "<?xml version=\"1.0\"?><xbel/>",
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><xbel/>",
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?><xbel/>",
        "<?xml version=\"1.0\" standalone=\"no\"?><xbel/>",
        "<xbel><?good-target some data?></xbel>",
    ] {
        let file = BookmarkFile::from_bytes(text.as_bytes());
        assert!(file.is_editable(), "{text}: {:?}", file.diagnostics());
    }
}

#[test]
fn declaring_children_parse_in_linear_time() {
    use std::fmt::Write as _;
    // N bindings inherited from the root and one more declared per child:
    // a snapshot that copied the inherited scope would be quadratic.
    let text = |n: usize| {
        let mut text = String::from("<xbel");
        for at in 0..n {
            let _ = write!(text, " xmlns:p{at}=\"urn:n{at}\"");
        }
        text.push('>');
        for at in 0..n {
            let _ = write!(text, "<u xmlns:q{at}=\"urn:m{at}\"/>");
        }
        text.push_str("</xbel>");
        text
    };
    crate::work::assert_linear(
        1_000,
        |n| {
            let text = text(n);
            let (file, steps) = crate::work::steps(|| BookmarkFile::from_bytes(text.as_bytes()));
            assert!(file.is_editable());
            (steps, text.len())
        },
        // Per byte: the event walk, the attribute reader, each element's
        // own bindings copied into its snapshot node (one node each,
        // sharing the rest), and the resolver's keys hashed and copied.
        |n, bytes| 2 * (14 * (n + 8) + 8 * bytes),
    );
}

#[test]
fn serializing_under_many_bindings_is_linear() {
    use std::fmt::Write as _;
    let text = |n: usize| {
        let mut text = String::from("<xbel");
        for at in 0..n {
            let _ = write!(text, " xmlns:p{at}=\"urn:n{at}\"");
        }
        text.push_str("></xbel>");
        text
    };
    crate::work::assert_linear(
        1_000,
        |n| {
            let text = text(n);
            let mut file = BookmarkFile::from_bytes(text.as_bytes());
            file.open("file:///a", &opened()).unwrap();
            let (written, steps) = crate::work::steps(|| file.to_bytes());
            assert!(written.len() > text.len());
            (steps, text.len())
        },
        // The source copied around the insertion, and a handful of prefix
        // searches, each walking the bindings once with hashed shadow
        // tracking (the prefixes' bytes hashed by the set).
        |n, bytes| 2 * (20 * (n + 8) + 10 * bytes + 600),
    );
}

#[test]
fn a_missing_type_attribute_is_added_inside_the_element() {
    let text = "<xbel xmlns:mime=\"http://www.freedesktop.org/standards/shared-mime-info\">\
                <bookmark href=\"x\"><info><metadata owner=\"http://freedesktop.org\">\
                <mime:mime-type/></metadata></info></bookmark></xbel>";
    let mut file = BookmarkFile::from_bytes(text.as_bytes());
    assert_eq!(file.get("x").unwrap().mime_type(), None);
    file.set_mime_type("x", &mime("text/markdown")).unwrap();
    let written = out(&file);
    assert!(
        written.contains("<mime:mime-type type=\"text/markdown\"/>"),
        "{written}"
    );
    let reread = BookmarkFile::from_bytes(written.as_bytes());
    assert!(reread.is_editable(), "{written}");
    assert_eq!(reread.get("x").unwrap().mime_type(), Some("text/markdown"));
}

#[test]
fn more_invalid_xml_is_uneditable() {
    let cases: [(&str, &str); 5] = [
        ("<xbel a:p=\"v\"/>", "not declared"),
        ("<xbel><1bad/></xbel>", "not an XML name"),
        ("<xbel>\u{1}</xbel>", "cannot appear in XML"),
        (
            "<xbel xmlns:a=\"urn:u\" xmlns:b=\"urn:u\"><x a:p=\"1\" b:p=\"2\"/></xbel>",
            "resolve to the same name",
        ),
        ("<xbel/>&amp;", "content outside the root"),
    ];
    for (text, expected) in cases {
        let file = BookmarkFile::from_bytes(text.as_bytes());
        assert!(!file.is_editable(), "{text}");
        assert!(
            file.diagnostics()
                .iter()
                .any(|d| d.message.contains(expected)),
            "{text}: {:?}",
            file.diagnostics()
        );
    }
}

#[test]
fn tabs_and_line_ends_in_values_survive_the_round_trip() {
    // A parser normalizes literal tabs and line ends in attribute values
    // to spaces; written as character references, the value comes back.
    let mut file = BookmarkFile::from_bytes(b"<xbel></xbel>");
    let mut record = opened();
    record.name = "line one\nline two\tend".to_owned();
    file.open("file:///x", &record).unwrap();
    let written = out(&file);
    let reread = BookmarkFile::from_bytes(written.as_bytes());
    assert!(reread.is_editable(), "{written}");
    assert_eq!(
        reread.get("file:///x").unwrap().applications()[0].name(),
        Some("line one\nline two\tend"),
        "{written}"
    );
}

#[test]
fn cdata_contributes_to_a_groups_name() {
    let text = "<xbel xmlns:bookmark=\"http://www.freedesktop.org/standards/desktop-bookmarks\">\
                <bookmark href=\"x\"><info><metadata owner=\"http://freedesktop.org\">\
                <bookmark:groups><bookmark:group>a <![CDATA[b & c]]> d</bookmark:group>\
                </bookmark:groups></metadata></info></bookmark></xbel>";
    let file = BookmarkFile::from_bytes(text.as_bytes());
    assert!(file.is_editable(), "{:?}", file.diagnostics());
    assert_eq!(file.get("x").unwrap().groups(), ["a b & c d"]);
}

#[test]
fn duplicate_application_records_update_only_the_first_everywhere() {
    let text = "<xbel xmlns:bookmark=\"http://www.freedesktop.org/standards/desktop-bookmarks\">\
                <bookmark href=\"x\"><info><metadata owner=\"http://freedesktop.org\">\
                <bookmark:applications>\
                <bookmark:application name=\"a\" exec=\"'a %u'\" count=\"2\"/>\
                <bookmark:application name=\"a\" exec=\"'a %u'\" count=\"7\"/>\
                </bookmark:applications></metadata></info></bookmark></xbel>";
    let mut file = BookmarkFile::from_bytes(text.as_bytes());
    let mut record = opened();
    record.name = "a".to_owned();
    record.exec = "'a %u'".to_owned();
    file.open("x", &record).unwrap();
    let counts = |apps: Vec<crate::Application<'_>>| {
        apps.iter()
            .map(crate::Application::count)
            .collect::<Vec<_>>()
    };
    // The logical view and the written file agree: the first record is
    // the application's, the duplicate stays as stored.
    assert_eq!(
        counts(file.get("x").unwrap().applications()),
        [Some(3), Some(7)]
    );
    let reread = BookmarkFile::from_bytes(out(&file).as_bytes());
    assert_eq!(
        counts(reread.get("x").unwrap().applications()),
        [Some(3), Some(7)]
    );
}

#[test]
fn an_absent_count_means_one_registration() {
    let text = "<xbel xmlns:bookmark=\"http://www.freedesktop.org/standards/desktop-bookmarks\">\
                <bookmark href=\"x\"><info><metadata owner=\"http://freedesktop.org\">\
                <bookmark:applications>\
                <bookmark:application name=\"a\" exec=\"'a %u'\"/>\
                </bookmark:applications></metadata></info></bookmark></xbel>";
    let mut file = BookmarkFile::from_bytes(text.as_bytes());
    assert_eq!(
        file.get("x").unwrap().applications()[0].count(),
        Some(1),
        "the spec's default for an absent count"
    );
    let mut record = opened();
    record.name = "a".to_owned();
    file.open("x", &record).unwrap();
    assert_eq!(file.get("x").unwrap().applications()[0].count(), Some(2));
    let reread = BookmarkFile::from_bytes(out(&file).as_bytes());
    assert_eq!(reread.get("x").unwrap().applications()[0].count(), Some(2));
}

#[test]
fn self_closing_containers_are_expanded() {
    let text = "<xbel/>";
    let mut file = BookmarkFile::from_bytes(text.as_bytes());
    file.open("file:///x", &opened()).unwrap();
    let written = out(&file);
    let reread = BookmarkFile::from_bytes(written.as_bytes());
    assert!(reread.is_editable(), "{written}");
    assert!(reread.get("file:///x").is_some());
    // A self-closing bookmark grows its info chain in place.
    let text = "<xbel><bookmark href=\"x\"/></xbel>";
    let mut file = BookmarkFile::from_bytes(text.as_bytes());
    file.open("x", &opened()).unwrap();
    let written = out(&file);
    let reread = BookmarkFile::from_bytes(written.as_bytes());
    assert!(reread.is_editable(), "{written}");
    let entry = reread.get("x").unwrap();
    assert_eq!(entry.applications().len(), 1);
    assert_eq!(
        entry.mime_type(),
        None,
        "a visit adds no type to an existing entry"
    );
}
