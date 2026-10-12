//! Readers for one database folder.

pub(crate) mod cache;
pub(crate) mod text;

use std::path::Path;

use crate::{Diagnostic, Location};

/// The default problem cap, for tests.
#[cfg(test)]
pub(crate) use crate::limits::DEFAULT_DIAGNOSTICS as MAX_DIAGNOSTICS;

/// The problems found in one file: the first
/// [`Limits::max_diagnostics`](crate::Limits::max_diagnostics) are listed,
/// then one line says there are more.
pub(crate) struct Problems<'a> {
    path: &'a Path,
    diagnostics: &'a mut Vec<Diagnostic>,
    /// The most problems listed (`None`: all).
    max: Option<usize>,
    /// Problems reported so far.
    reported: usize,
}

impl<'a> Problems<'a> {
    pub(crate) fn new(
        path: &'a Path,
        diagnostics: &'a mut Vec<Diagnostic>,
        max: Option<usize>,
    ) -> Self {
        Self {
            path,
            diagnostics,
            max,
            reported: 0,
        }
    }

    /// The most problems listed (`None`: all).
    pub(crate) fn max(&self) -> Option<usize> {
        self.max
    }

    pub(crate) fn report(&mut self, location: Option<Location>, message: impl Into<String>) {
        self.reported += 1;
        let Some(max) = self.max else {
            self.diagnostics
                .push(Diagnostic::new(self.path, location, message));
            return;
        };
        match self.reported.cmp(&max.saturating_add(1)) {
            std::cmp::Ordering::Less => {
                self.diagnostics
                    .push(Diagnostic::new(self.path, location, message));
            }
            std::cmp::Ordering::Equal => self.diagnostics.push(Diagnostic::new(
                self.path,
                None,
                format!("more than {max} problems; the rest are not listed"),
            )),
            std::cmp::Ordering::Greater => {}
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Databases built by `update-mime-database` for tests.

    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// Builds `<dir>/mime` from the given package files (name, contents) with
    /// `update-mime-database`, which the dev shell provides.
    pub(crate) fn build(dir: &Path, packages: &[(&str, &str)]) -> PathBuf {
        let mime = dir.join("mime");
        std::fs::create_dir_all(mime.join("packages")).unwrap();
        for (name, contents) in packages {
            std::fs::write(mime.join("packages").join(name), contents).unwrap();
        }
        let status = Command::new("update-mime-database")
            .arg(&mime)
            // Keeps it from warning that the folder is not on the search path.
            .env("XDG_DATA_DIRS", dir)
            .env("XDG_DATA_HOME", dir)
            .status()
            .expect("update-mime-database is not on PATH: run the tests in the dev shell");
        assert!(status.success(), "update-mime-database failed");
        mime
    }

    /// The pinned shared-mime-info source the dev shell points to.
    pub(crate) fn reference_source() -> PathBuf {
        PathBuf::from(
            std::env::var_os("XDG_KIT_SMI_SRC")
                .expect("XDG_KIT_SMI_SRC is not set: run the tests in the dev shell"),
        )
    }

    /// The full reference database, built from the pinned source.
    pub(crate) fn reference(dir: &Path) -> PathBuf {
        let xml = std::fs::read_to_string(reference_source().join("data/freedesktop.org.xml.in"))
            .unwrap();
        build(dir, &[("freedesktop.org.xml", &xml)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Folder;

    fn both(mime: &std::path::Path) -> (Folder, Folder) {
        let mut from_text = Folder::default();
        let mut diagnostics = Vec::new();
        let limits = &crate::Limits::default();
        text::read(
            mime,
            text::Parts::All,
            &mut from_text,
            &mut diagnostics,
            limits,
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let mut from_cache = Folder::default();
        let bytes = std::fs::read(mime.join("mime.cache")).unwrap();
        cache::read(
            &mime.join("mime.cache"),
            &bytes,
            &mut from_cache,
            &mut diagnostics,
            limits,
        )
        .unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        (from_text, from_cache)
    }

    /// The entries of one part that only one reader found.
    fn only_in_one<T: PartialEq + std::fmt::Debug>(part: &str, text: &[T], cache: &[T]) {
        let only_text: Vec<_> = text.iter().filter(|item| !cache.contains(item)).collect();
        let only_cache: Vec<_> = cache.iter().filter(|item| !text.contains(item)).collect();
        assert!(
            only_text.is_empty() && only_cache.is_empty(),
            "{part}: text only: {only_text:#?}\ncache only: {only_cache:#?}"
        );
    }

    /// The two readings hold the same, part by part; a failure lists only
    /// the differing entries.
    fn agree(from_text: &Folder, from_cache: &Folder) {
        let (text, cache) = (from_text.comparable(), from_cache.comparable());
        only_in_one("globs", &text.globs, &cache.globs);
        only_in_one("glob deletions", &text.no_globs, &cache.no_globs);
        only_in_one("magic", &text.magic, &cache.magic);
        only_in_one("magic deletions", &text.no_magic, &cache.no_magic);
        only_in_one("aliases", &text.aliases, &cache.aliases);
        only_in_one("parents", &text.parents, &cache.parents);
        only_in_one("icons", &text.icons, &cache.icons);
        only_in_one("generic icons", &text.generic_icons, &cache.generic_icons);
        only_in_one("namespaces", &text.namespaces, &cache.namespaces);
        assert_eq!(text.max_extent, cache.max_extent);
        // Same entries, also in the same order and count.
        assert!(
            text == cache,
            "the same entries, but not in the same order or number"
        );
    }

    /// Every kind of line the generated files can hold, deletion markers
    /// included.
    const FIXTURE: &str = r#"<?xml version="1.0"?>
<mime-info xmlns="http://www.freedesktop.org/standards/shared-mime-info">
  <mime-type type="text/x-fixture">
    <sub-class-of type="text/plain"/>
    <alias type="text/x-fixture-old"/>
    <icon name="fixture-icon"/>
    <generic-icon name="fixture-generic"/>
    <glob pattern="*.fix" weight="70"/>
    <glob pattern="*.FIX" case-sensitive="true"/>
    <glob pattern="Fixfile"/>
    <glob pattern="fix-*.[ab]"/>
    <glob-deleteall/>
    <magic-deleteall/>
    <magic priority="60">
      <match type="string" offset="0:4" value="FIX" mask="0xffdfff">
        <match type="big16" offset="8" value="0x0102"/>
        <match type="little32" offset="10" value="0x01020304"/>
      </match>
      <match type="host16" offset="0" value="0x0a0b"/>
      <match type="byte" offset="2" value="7"/>
    </magic>
  </mime-type>
  <mime-type type="application/x-fixture+xml">
    <sub-class-of type="application/xml"/>
    <root-XML namespaceURI="urn:fixture" localName="fix"/>
    <root-XML namespaceURI="urn:fixture-any" localName=""/>
  </mime-type>
</mime-info>"#;

    #[test]
    fn the_cache_and_the_text_files_agree_on_a_fixture_with_every_kind_of_line() {
        let dir = tempfile::tempdir().unwrap();
        let mime = testing::build(dir.path(), &[("fixture.xml", FIXTURE)]);
        let (from_text, from_cache) = both(&mime);
        assert!(!from_text.no_globs.is_empty() && !from_text.no_magic.is_empty());
        assert_eq!(from_text.globs.len(), 4, "{:?}", from_text.globs);
        agree(&from_text, &from_cache);
    }

    /// One package of `globs` (pattern, type), built, then loaded with
    /// its `mime.cache` and from its text files alone.
    fn with_and_without_the_cache(globs: &[(&str, &str)]) -> [crate::MimeDatabase; 2] {
        use std::fmt::Write as _;
        let mut package = String::from(
            r#"<?xml version="1.0"?>
<mime-info xmlns="http://www.freedesktop.org/standards/shared-mime-info">"#,
        );
        for (pattern, mime) in globs {
            write!(
                package,
                r#"<mime-type type="{mime}"><glob pattern="{pattern}"/></mime-type>"#
            )
            .unwrap();
        }
        package.push_str("</mime-info>");
        let dir = tempfile::tempdir().unwrap();
        let mime = testing::build(dir.path(), &[("globs.xml", &package)]);
        let (from_text, from_cache) = both(&mime);
        agree(&from_text, &from_cache);
        let cached = crate::MimeDatabase::from_dirs([&mime]);
        std::fs::remove_file(mime.join("mime.cache")).unwrap();
        let text = crate::MimeDatabase::from_dirs([&mime]);
        for db in [&cached, &text] {
            assert!(db.diagnostics().is_empty(), "{:?}", db.diagnostics());
        }
        [cached, text]
    }

    /// The type `name` gets from each of `dbs`, if any.
    fn named(dbs: &[crate::MimeDatabase], name: &str) -> Vec<Option<String>> {
        dbs.iter()
            .map(|db| {
                db.type_for_name(name)
                    .best()
                    .map(|found| found.mime.to_string())
            })
            .collect()
    }

    #[test]
    fn a_set_with_a_colon_reads_the_same_with_and_without_the_cache() {
        let dbs = with_and_without_the_cache(&[("*[[:digit:]].foo", "application/x-digit")]);
        let digit = Some("application/x-digit".to_owned());
        assert_eq!(named(&dbs, "A1.foo"), [digit.clone(), digit]);
        assert_eq!(named(&dbs, "AB.foo"), [None, None]);
    }

    #[test]
    fn a_bracket_that_never_matches_matches_nothing_with_and_without_the_cache() {
        // `[.` with no `.]` after it: glibc's `fnmatch` gives up on the
        // bracket. The text files then hold the flags inside the pattern,
        // so the readers' globs differ, but neither matches anything.
        let package = r#"<?xml version="1.0"?>
<mime-info xmlns="http://www.freedesktop.org/standards/shared-mime-info">
<mime-type type="text/x-p"><glob pattern="*[a:b[.x" case-sensitive="true"/></mime-type>
</mime-info>"#;
        let dir = tempfile::tempdir().unwrap();
        let mime = testing::build(dir.path(), &[("p.xml", package)]);
        let globs2 = std::fs::read_to_string(mime.join("globs2")).unwrap();
        assert!(
            globs2.ends_with("50:text/x-p:*[a:b[.x:cs\n50:text/x-p:*[a:b[.x\n"),
            "{globs2}"
        );
        let cached = crate::MimeDatabase::from_dirs([&mime]);
        std::fs::remove_file(mime.join("mime.cache")).unwrap();
        let text = crate::MimeDatabase::from_dirs([&mime]);
        let dbs = [cached, text];
        for name in ["foo[a", "foo[a:b[.x", "foo[a:b[.x:cs", "a"] {
            assert_eq!(named(&dbs, name), [None, None], "{name}");
        }
        // The text files' flagless copy is another pattern: both listed.
        let p = crate::MimeType::new("text/x-p").unwrap();
        assert_eq!(dbs[0].patterns_for(&p), ["*[a:b[.x"]);
        assert_eq!(dbs[1].patterns_for(&p), ["*[a:b[.x:cs", "*[a:b[.x"]);
        for db in &dbs {
            assert!(db.diagnostics().is_empty(), "{:?}", db.diagnostics());
        }
    }

    #[test]
    fn case_is_folded_per_character_with_and_without_the_cache() {
        let dbs = with_and_without_the_cache(&[("*Σ", "application/x-sigma")]);
        let sigma = Some("application/x-sigma".to_owned());
        for name in ["AΣ", "aΣ", "aσ", "ΣΣ"] {
            assert_eq!(named(&dbs, name), [sigma.clone(), sigma.clone()], "{name}");
        }
        // `ς` is lowercase already: no context makes `Σ` into it.
        assert_eq!(named(&dbs, "aς"), [None, None]);
    }

    /// The class rows need the matcher's fold. `update-mime-database`
    /// writes `*[A-C].x` as `*[a-c].x`, so the range rows check only that
    /// both readers agree; `fnmatch`'s unit tests cover folded ranges.
    #[test]
    fn case_insensitive_sets_match_either_case_with_and_without_the_cache() {
        let dbs = with_and_without_the_cache(&[
            ("*[[:upper:]].up", "application/x-upper"),
            ("*[[:lower:]].low", "application/x-lower"),
            ("*[A-C].x", "application/x-range"),
        ]);
        for (name, mime) in [
            ("A.up", Some("application/x-upper")),
            ("a.up", Some("application/x-upper")),
            ("É.UP", Some("application/x-upper")),
            ("1.up", None),
            ("A.LOW", Some("application/x-lower")),
            ("b.x", Some("application/x-range")),
            ("B.x", Some("application/x-range")),
            ("d.x", None),
        ] {
            let mime = mime.map(str::to_owned);
            assert_eq!(named(&dbs, name), [mime.clone(), mime], "{name}");
        }
    }

    #[test]
    fn the_cache_and_the_text_files_agree_on_the_reference_database() {
        let dir = tempfile::tempdir().unwrap();
        let mime = testing::reference(dir.path());
        let (from_text, from_cache) = both(&mime);
        assert!(
            from_text.globs.len() > 1000,
            "{} globs",
            from_text.globs.len()
        );
        assert!(
            from_text.magic.len() > 500,
            "{} magic rules",
            from_text.magic.len()
        );
        agree(&from_text, &from_cache);
    }
}
