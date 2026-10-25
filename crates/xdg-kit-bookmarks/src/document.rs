//! The bookmark document: the original bytes, the span index over them,
//! and (in later stages) the pending edits and the write baseline.

use xdg_kit_basedir::__hashed::{Map, Str};

use crate::bookmark::Bookmark;
use crate::edit::{Edits, Entry};
use crate::work::{self, Counted};
use crate::xml::{self, Parsed};
use crate::{Diagnostic, Limits};

/// An XBEL bookmark document, read leniently and edited losslessly.
pub struct BookmarkFile {
    pub(crate) source: Vec<u8>,
    pub(crate) parsed: Parsed,
    pub(crate) limits: Limits,
    /// Each href's first entry, and how many entries share it.
    pub(crate) index: Map<Str<Counted>, (usize, usize)>,
    /// The pending edits: with the source, the logical document.
    pub(crate) edits: Edits,
    /// Each pending inserted entry, by href.
    pub(crate) inserted_index: Map<Str<Counted>, usize>,
    /// The file behind the document, when read from one.
    pub(crate) path: Option<std::path::PathBuf>,
    /// What the destination must still look like for a write to commit.
    pub(crate) baseline: crate::write::Baseline,
}

impl std::fmt::Debug for BookmarkFile {
    /// The entries in file order; the index into them is left out.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BookmarkFile")
            .field("parsed", &self.parsed)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl BookmarkFile {
    /// Parses a document from bytes, with the default [`Limits`]. There is
    /// no file behind it: it can be read, edited and serialized, but not
    /// written.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self::from_bytes_with(bytes, &Limits::default())
    }

    /// [`BookmarkFile::from_bytes`] within `limits`.
    #[must_use]
    pub fn from_bytes_with(bytes: &[u8], limits: &Limits) -> Self {
        let mut parsed = xml::parse(bytes, limits);
        let mut index = Map::<Str<Counted>, (usize, usize)>::new();
        for at in 0..parsed.bookmarks.len() {
            // The lookup; the map counts each byte of the href hashed, and
            // compared on a match.
            work::count(1);
            let href = &parsed.bookmarks[at].href;
            let lookup = index.lookup(&href.text);
            if let Some(entry) = index.get_mut_at(&lookup) {
                entry.1 += 1;
                let message = format!(
                    "the URI {} has more than one entry",
                    crate::error::quoted(&href.text)
                );
                parsed
                    .diagnostics
                    .push(Diagnostic::new(Some(href.value.start), message));
            } else {
                index.insert_at(&lookup, (at, 1));
            }
        }
        Self {
            source: bytes.to_vec(),
            parsed,
            limits: limits.clone(),
            index,
            edits: Edits::default(),
            inserted_index: Map::new(),
            path: None,
            baseline: crate::write::Baseline::Detached,
        }
    }

    /// The entries of the logical document, in file order, pending
    /// insertions last. Pending edits show through immediately.
    pub fn bookmarks(&self) -> impl Iterator<Item = Bookmark<'_>> {
        let source = (0..self.parsed.bookmarks.len())
            .filter(|at| !self.edits.removed.contains(at))
            .map(|at| Bookmark {
                file: self,
                entry: Entry::Source(at),
            });
        let inserted = self
            .edits
            .inserted
            .iter()
            .enumerate()
            .filter(|(_, owned)| owned.is_some())
            .map(|(at, _)| Bookmark {
                file: self,
                entry: Entry::Inserted(at),
            });
        source.chain(inserted)
    }

    /// The entry with exactly this URI (string match, as `GLib` matches).
    /// With several such entries — junk other writers left — the first;
    /// reading one of them never guesses more, and each duplicate is a
    /// diagnostic.
    #[must_use]
    pub fn get(&self, uri: &str) -> Option<Bookmark<'_>> {
        // The lookups; the maps count the bytes hashed and compared.
        work::count(2);
        if let Some(&(first, _)) = self.index.get(uri)
            && !self.edits.removed.contains(&first)
        {
            return Some(Bookmark {
                file: self,
                entry: Entry::Source(first),
            });
        }
        let &at = self.inserted_index.get(uri)?;
        self.edits.inserted[at].is_some().then_some(Bookmark {
            file: self,
            entry: Entry::Inserted(at),
        })
    }

    /// Problems found while reading: content that was skipped or read
    /// only in part, and anything that makes the document uneditable.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.parsed.diagnostics
    }

    /// Whether the document may be edited: well-formed XML end to end,
    /// one `xbel` root, no duplicate attributes, every prefix declared,
    /// no internal DTD subset, within the limits. Reading works either
    /// way; editing a document that fails this is refused.
    #[must_use]
    pub fn is_editable(&self) -> bool {
        self.parsed.not_editable.is_none() && self.parsed.root.is_some()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A GLib-shaped sample with content this crate does not understand:
    /// a comment, a processing instruction, another owner's metadata with
    /// our element names inside, an unknown attribute, odd whitespace.
    pub(crate) const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<xbel version="1.0"
      xmlns:bookmark="http://www.freedesktop.org/standards/desktop-bookmarks"
      xmlns:mime="http://www.freedesktop.org/standards/shared-mime-info">
  <!-- written by hand -->
  <bookmark href="file:///home/u/a%20b.txt" added="2023-05-04T10:17:17.138078Z" modified="2023-05-04T10:17:18Z" visited="2023-05-04T10:17:19Z">
    <info>
      <metadata owner="http://freedesktop.org">
        <mime:mime-type type="text/plain"/>
        <bookmark:groups>
          <bookmark:group>notes &amp; things</bookmark:group>
        </bookmark:groups>
        <bookmark:applications>
          <bookmark:application name="editor" exec="&apos;editor %u&apos;" modified="2023-05-04T10:17:19Z" count="2"/>
        </bookmark:applications>
        <bookmark:private/>
        <x-extra>kept</x-extra>
      </metadata>
      <metadata owner="http://example.org">
        <mime:mime-type type="application/x-foreign"/>
      </metadata>
    </info>
  </bookmark>
  <?pi data?>
  <bookmark href="sftp://host/file" x-unknown="yes">
  </bookmark>
</xbel>
"#;

    #[test]
    fn the_sample_is_indexed_with_its_spans() {
        let file = BookmarkFile::from_bytes(SAMPLE.as_bytes());
        assert!(file.is_editable(), "{:?}", file.diagnostics());
        assert_eq!(file.diagnostics(), []);
        let parsed = &file.parsed;
        assert_eq!(parsed.bookmarks.len(), 2);
        let first = &parsed.bookmarks[0];
        let src = |range: &std::ops::Range<usize>| &SAMPLE[range.clone()];
        assert_eq!(first.href.text, "file:///home/u/a%20b.txt");
        assert_eq!(src(&first.href.value), "file:///home/u/a%20b.txt");
        assert!(src(&first.span).starts_with("<bookmark "));
        assert!(src(&first.span).ends_with("</bookmark>"));
        assert_eq!(
            first.added.as_ref().unwrap().time,
            crate::time::parse("2023-05-04T10:17:17.138078Z")
        );
        let meta = first.info.as_ref().unwrap().metadata.as_ref().unwrap();
        assert_eq!(
            meta.mime.as_ref().unwrap().value.as_ref().unwrap().text,
            "text/plain"
        );
        let groups = meta.groups.as_ref().unwrap();
        assert_eq!(groups.list.len(), 1);
        assert_eq!(groups.list[0].name, "notes & things");
        let apps = meta.applications.as_ref().unwrap();
        assert_eq!(apps.list.len(), 1);
        let app = &apps.list[0];
        assert_eq!(app.name.as_ref().unwrap().text, "editor");
        assert_eq!(app.exec.as_ref().unwrap().text, "'editor %u'");
        assert_eq!(app.count.as_ref().unwrap().1, Some(2));
        assert!(meta.private.is_some());
        // The foreign owner's metadata is opaque: our element inside it
        // was not read.
        assert_eq!(
            meta.mime.as_ref().unwrap().value.as_ref().unwrap().text,
            "text/plain"
        );
        let second = &parsed.bookmarks[1];
        assert_eq!(second.href.text, "sftp://host/file");
        assert!(second.info.is_none());
        assert!(second.added.is_none());
        // The root insert point is just before its end tag.
        let root = parsed.root.as_ref().unwrap();
        assert!(SAMPLE[root.end_insert..].starts_with("</xbel>"));
        assert!(SAMPLE[first.end_insert..].starts_with("</bookmark>"));
    }

    #[test]
    fn reading_tolerates_junk_with_diagnostics() {
        let junk = r#"<xbel>
  <bookmark href="x" added="not a time"/>
  <bookmark/>
  <bookmark href="y"><info><metadata owner="http://freedesktop.org">
    <bookmark:application xmlns:bookmark="http://www.freedesktop.org/standards/desktop-bookmarks"/>
  </metadata></info></bookmark>
</xbel>"#;
        let file = BookmarkFile::from_bytes(junk.as_bytes());
        assert!(file.is_editable(), "{:?}", file.diagnostics());
        assert_eq!(file.parsed.bookmarks.len(), 3);
        assert!(
            file.diagnostics()
                .iter()
                .any(|d| d.message.contains("not a timestamp"))
        );
        assert!(
            file.diagnostics()
                .iter()
                .any(|d| d.message.contains("no href"))
        );
        // The self-closing bookmark is indexed.
        assert!(file.parsed.bookmarks[1].self_closing);
        // An application outside an applications list is unknown content.
        assert!(
            file.parsed.bookmarks[2]
                .info
                .as_ref()
                .unwrap()
                .metadata
                .as_ref()
                .unwrap()
                .applications
                .is_none()
        );
    }

    #[test]
    fn what_cannot_be_edited_is_said_and_still_read() {
        let cases: [(&str, &str); 7] = [
            ("<xbel><bookmark href=\"a\"/>", "bad XML"),
            ("<xbel><bookmark href=\"a\"></xbel>", "bad XML"),
            (
                "<xbel><bookmark href=\"a\" href=\"b\"/></xbel>",
                "bad attribute",
            ),
            (
                "<xbel><undeclared:x/></xbel>",
                "the prefix `undeclared` is not declared",
            ),
            (
                "<!DOCTYPE xbel [<!ENTITY e \"x\">]><xbel/>",
                "an internal DTD subset",
            ),
            ("<xbel>&nbsp;</xbel>", "not a predefined entity"),
            ("<notxbel/>", "the root element is not xbel"),
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
        // Bookmarks before the problem are still served.
        let truncated = BookmarkFile::from_bytes(
            b"<xbel><bookmark href=\"kept\"></bookmark><bookmark href=\"lost",
        );
        assert!(!truncated.is_editable());
        assert_eq!(truncated.parsed.bookmarks.len(), 1);
        assert_eq!(truncated.parsed.bookmarks[0].href.text, "kept");
    }

    #[test]
    fn limits_are_enforced() {
        let mut deep = String::from("<xbel>");
        for _ in 0..100 {
            deep.push_str("<a>");
        }
        let file = BookmarkFile::from_bytes(deep.as_bytes());
        assert!(!file.is_editable());
        assert!(
            file.diagnostics()
                .iter()
                .any(|d| d.message.contains("nested deeper"))
        );
        let mut many = String::from("<xbel>");
        for at in 0..5 {
            use std::fmt::Write as _;
            let _ = write!(many, "<bookmark href=\"{at}\"/>");
        }
        many.push_str("</xbel>");
        let limits = Limits {
            max_bookmarks: 3,
            ..Limits::default()
        };
        let file = BookmarkFile::from_bytes_with(many.as_bytes(), &limits);
        assert!(!file.is_editable());
        assert_eq!(file.parsed.bookmarks.len(), 3);
        assert!(
            file.diagnostics()
                .iter()
                .any(|d| d.message.contains("more than 3 bookmarks"))
        );
    }

    #[test]
    fn the_typed_view_reads_the_sample() {
        let file = BookmarkFile::from_bytes(SAMPLE.as_bytes());
        let listed: Vec<_> = file.bookmarks().map(|b| b.href().to_owned()).collect();
        assert_eq!(listed, ["file:///home/u/a%20b.txt", "sftp://host/file"]);
        let first = file.get("file:///home/u/a%20b.txt").unwrap();
        assert_eq!(
            first.added(),
            crate::time::parse("2023-05-04T10:17:17.138078Z")
        );
        assert_eq!(first.modified(), crate::time::parse("2023-05-04T10:17:18Z"));
        assert_eq!(first.visited(), crate::time::parse("2023-05-04T10:17:19Z"));
        assert_eq!(first.mime_type(), Some("text/plain"));
        assert_eq!(first.groups(), ["notes & things"]);
        assert!(first.is_private());
        let apps = first.applications();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name(), Some("editor"));
        assert_eq!(apps[0].exec(), Some("'editor %u'"));
        assert_eq!(apps[0].count(), Some(2));
        assert_eq!(
            apps[0].modified(),
            crate::time::parse("2023-05-04T10:17:19Z")
        );
        let second = file.get("sftp://host/file").unwrap();
        assert_eq!(second.mime_type(), None);
        assert_eq!(second.groups(), Vec::<&str>::new());
        assert!(!second.is_private());
        assert!(second.applications().is_empty());
        assert!(file.get("file:///other").is_none());
        // Exact string match only, as GLib matches.
        assert!(file.get("file:///home/u/a b.txt").is_none());
    }

    #[test]
    fn duplicate_uris_read_as_the_first_with_a_diagnostic() {
        let file = BookmarkFile::from_bytes(
            b"<xbel><bookmark href=\"x\" added=\"2023-05-04T10:17:17Z\"/>\
              <bookmark href=\"x\"/></xbel>",
        );
        assert!(file.is_editable());
        assert_eq!(file.bookmarks().count(), 2);
        assert!(file.get("x").unwrap().added().is_some(), "the first");
        assert!(
            file.diagnostics()
                .iter()
                .any(|d| d.message.contains("more than one entry"))
        );
    }

    #[test]
    fn a_second_root_or_outside_text_is_refused() {
        let file = BookmarkFile::from_bytes(b"<xbel/><xbel/>");
        assert!(!file.is_editable());
        let file = BookmarkFile::from_bytes(b"<xbel/>junk");
        assert!(!file.is_editable());
    }
}
