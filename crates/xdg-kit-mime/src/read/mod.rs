//! Readers for one database folder.

pub(crate) mod cache;
pub(crate) mod text;

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
        text::read(mime, text::Parts::All, &mut from_text, &mut diagnostics);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let mut from_cache = Folder::default();
        let bytes = std::fs::read(mime.join("mime.cache")).unwrap();
        cache::read(
            &mime.join("mime.cache"),
            &bytes,
            &mut from_cache,
            &mut diagnostics,
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
