//! The database as a whole, on folders built by `update-mime-database` (from
//! the dev shell) out of small packages written here.

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use xdg_kit_locale::Locales;
use xdg_kit_mime::{
    Certainty, DecidedBy, Location, MimeDatabase, MimeType, PathOptions, TreeOptions,
};

mod common;

const HEAD: &str = r#"<?xml version="1.0"?>
<mime-info xmlns="http://www.freedesktop.org/standards/shared-mime-info">"#;
const TAIL: &str = "</mime-info>";

/// Builds `<dir>/mime` from one package.
fn build(dir: &Path, types: &str) -> PathBuf {
    let mime = dir.join("mime");
    fs::create_dir_all(mime.join("packages")).unwrap();
    fs::write(
        mime.join("packages/test.xml"),
        format!("{HEAD}{types}{TAIL}"),
    )
    .unwrap();
    common::update_mime_database(&mime, dir);
    mime
}

fn m(text: &str) -> MimeType {
    MimeType::new(text).unwrap()
}

const SYSTEM: &str = r#"
  <mime-type type="text/x-diff">
    <comment>Differences</comment>
    <comment xml:lang="de">Unterschiede</comment>
    <sub-class-of type="text/plain"/>
    <glob pattern="*.diff"/>
    <glob pattern="*.patch"/>
    <magic priority="50"><match type="string" offset="0" value="diff\t"/></magic>
  </mime-type>
  <mime-type type="application/x-old">
    <alias type="application/x-older"/>
    <glob pattern="*.old"/>
  </mime-type>
  <mime-type type="text/x-c++src">
    <sub-class-of type="text/plain"/>
    <glob pattern="*.C" case-sensitive="true"/>
    <glob pattern="*.cc"/>
  </mime-type>
  <mime-type type="text/x-csrc">
    <sub-class-of type="text/plain"/>
    <glob pattern="*.c"/>
  </mime-type>
  <mime-type type="application/x-archive-thing">
    <glob pattern="*.gz" weight="50"/>
    <glob pattern="Makefile"/>
  </mime-type>
  <mime-type type="application/x-compressed-tar">
    <glob pattern="*.tar.gz"/>
  </mime-type>
  <mime-type type="application/x-loop-a">
    <sub-class-of type="application/x-loop-b"/>
  </mime-type>
  <mime-type type="application/x-loop-b">
    <sub-class-of type="application/x-loop-a"/>
  </mime-type>
  <mime-type type="application/xml">
    <sub-class-of type="text/plain"/>
    <glob pattern="*.xml"/>
    <magic priority="40"><match type="string" offset="0" value="&lt;?xml"/></magic>
  </mime-type>
  <mime-type type="application/x-thing+xml">
    <sub-class-of type="application/xml"/>
    <root-XML namespaceURI="urn:thing" localName="thing"/>
  </mime-type>
  <mime-type type="text/x-bare"/>
  <mime-type type="application/x-text-based">
    <sub-class-of type="text/x-bare"/>
  </mime-type>
  <mime-type type="application/x-accented">
    <glob pattern="*é.x"/>
    <glob pattern="é*"/>
  </mime-type>
  <mime-type type="image/x-host-order">
    <magic priority="60"><match type="host16" offset="0" value="0x0102"/></magic>
  </mime-type>
"#;

/// A user folder that redefines `text/x-diff`'s globs and magic.
const USER: &str = r#"
  <mime-type type="text/x-diff">
    <glob-deleteall/>
    <magic-deleteall/>
    <glob pattern="*.dif"/>
    <comment xml:lang="de">Diff-Datei</comment>
  </mime-type>
"#;

fn system(dir: &Path) -> MimeDatabase {
    let mime = build(dir, SYSTEM);
    let db = MimeDatabase::from_dirs(&[mime]);
    assert!(db.diagnostics().is_empty(), "{:?}", db.diagnostics());
    db
}

#[test]
fn names_follow_the_spec_rules() {
    let dir = tempfile::tempdir().unwrap();
    let db = system(dir.path());
    let best = |name: &str| {
        db.type_for_name(name)
            .best()
            .map(|found| found.mime.to_string())
    };
    assert_eq!(best("a.diff").as_deref(), Some("text/x-diff"));
    assert_eq!(
        best("A.DIFF").as_deref(),
        Some("text/x-diff"),
        "case-insensitive"
    );
    assert_eq!(
        best("main.C").as_deref(),
        Some("text/x-c++src"),
        "case-sensitive *.C"
    );
    assert_eq!(best("main.c").as_deref(), Some("text/x-csrc"));
    assert_eq!(
        best("x.tar.gz").as_deref(),
        Some("application/x-compressed-tar"),
        "longest pattern"
    );
    assert_eq!(
        best("Makefile").as_deref(),
        Some("application/x-archive-thing"),
        "literal"
    );
    assert_eq!(
        best("dir/a.diff").as_deref(),
        Some("text/x-diff"),
        "only the last part"
    );
    assert_eq!(best("nothing.zzz"), None);
    assert_eq!(best("aé.x").as_deref(), Some("application/x-accented"));
    // A name that is not UTF-8 never matches a pattern with non-ASCII
    // characters, even where its bytes would.
    let bytes = |raw: &[u8]| std::ffi::OsStr::from_bytes(raw).to_owned();
    assert!(db.type_for_name(bytes(b"\xc3\xa9\xff")).is_empty());
    assert!(db.type_for_name(bytes(b"\xff\xc3\xa9.x")).is_empty());
    assert_eq!(
        db.type_for_name(bytes(b"\xff.diff"))
            .best()
            .map(|found| found.mime),
        Some(m("text/x-diff")),
        "ASCII patterns still match"
    );
    let all = db.type_for_name("x.tar.gz");
    assert_eq!(all.matches().len(), 2);
    assert_eq!(
        all.best().unwrap().certainty,
        Certainty::Guess,
        "two types claim the name"
    );
    assert_eq!(
        db.type_for_name("a.diff").best().unwrap().certainty,
        Certainty::Certain
    );
}

#[test]
fn content_magic_host_order_xml_roots_and_fallbacks() {
    let dir = tempfile::tempdir().unwrap();
    let db = system(dir.path());
    let found = db.type_for_data(b"diff\ta b");
    assert_eq!(
        (found.mime.as_str(), found.decided_by),
        ("text/x-diff", DecidedBy::Content)
    );
    let host = if cfg!(target_endian = "little") {
        [2u8, 1]
    } else {
        [1, 2]
    };
    assert_eq!(db.type_for_data(&host).mime.as_str(), "image/x-host-order");
    let xml = db.type_for_data(br#"<?xml version="1.0"?><thing xmlns="urn:thing"/>"#);
    assert_eq!(
        (xml.mime.as_str(), xml.decided_by),
        ("application/x-thing+xml", DecidedBy::XmlRoot)
    );
    assert_eq!(
        db.type_for_data(b"<?xml version=\"1.0\"?><other/>")
            .mime
            .as_str(),
        "application/xml"
    );
    let text = db.type_for_data(b"just words\n");
    assert_eq!(
        (text.mime.as_str(), text.certainty),
        ("text/plain", Certainty::Guess)
    );
    assert_eq!(
        db.type_for_data(b"\x00\x01\x02").mime.as_str(),
        "application/octet-stream"
    );
    assert_eq!(
        db.type_for_data(b"").mime.as_str(),
        "application/x-zerosize"
    );
    assert_eq!(
        db.sniff_len(),
        128,
        "rules look 5 bytes in; the text check needs 128"
    );
}

/// Types whose names and content compete.
const RIVALS: &str = r#"
  <mime-type type="application/x-base">
    <icon name="Base-Icon"/>
    <generic-icon name="Base-Generic"/>
    <magic priority="50"><match type="string" offset="0" value="BASE"/></magic>
  </mime-type>
  <mime-type type="application/x-sub">
    <sub-class-of type="application/x-base"/>
    <glob pattern="*.amb" weight="40"/>
  </mime-type>
  <mime-type type="application/x-other">
    <glob pattern="*.amb" weight="60"/>
  </mime-type>
  <mime-type type="application/x-amb-binary">
    <glob pattern="*.txtamb"/>
  </mime-type>
  <mime-type type="text/x-amb-source">
    <glob pattern="*.txtamb"/>
  </mime-type>
  <mime-type type="application/x-third">
    <magic priority="50"><match type="string" offset="0" value="THIRD"/></magic>
  </mime-type>
  <mime-type type="application/x-literal">
    <glob pattern="special.amb"/>
  </mime-type>
  <mime-type type="application/x-suffix">
    <glob pattern="*.wild"/>
  </mime-type>
  <mime-type type="application/x-wildcard">
    <glob pattern="f*.wild"/>
  </mime-type>
  <mime-type type="application/x-long">
    <glob pattern="*.long.ext" weight="40"/>
  </mime-type>
  <mime-type type="application/x-short">
    <glob pattern="*.ext" weight="60"/>
  </mime-type>
  <mime-type type="application/xml">
    <magic priority="40"><match type="string" offset="0" value="&lt;?xml"/></magic>
  </mime-type>
  <mime-type type="application/x-xml-sub">
    <sub-class-of type="application/xml"/>
    <magic priority="80"><match type="string" offset="0" value="&lt;?xml version=&quot;1.0&quot;?&gt;&lt;!--sub--&gt;"/></magic>
  </mime-type>
  <mime-type type="application/x-any-element+xml">
    <sub-class-of type="application/xml"/>
    <root-XML namespaceURI="urn:any" localName=""/>
  </mime-type>
  <mime-type type="application/x-any-namespace+xml">
    <sub-class-of type="application/xml"/>
    <root-XML namespaceURI="" localName="lonely"/>
  </mime-type>
"#;

#[test]
fn name_rules_literal_then_suffix_then_wildcard_and_weight_before_length() {
    let dir = tempfile::tempdir().unwrap();
    let db = MimeDatabase::from_dirs([build(dir.path(), RIVALS)]);
    assert!(db.diagnostics().is_empty(), "{:?}", db.diagnostics());
    let types = |name: &str| -> Vec<String> {
        db.type_for_name(name)
            .matches()
            .iter()
            .map(|found| found.mime.to_string())
            .collect()
    };
    assert_eq!(
        types("special.amb"),
        ["application/x-literal"],
        "a literal match leaves out the *.amb patterns"
    );
    assert_eq!(
        types("f.wild"),
        ["application/x-suffix"],
        "a *.ext match leaves out other wildcards"
    );
    assert_eq!(types("fwild"), Vec::<String>::new());
    assert_eq!(types("f.x.wild"), ["application/x-suffix"]);
    assert_eq!(
        types("a.long.ext"),
        ["application/x-short", "application/x-long"],
        "weight before pattern length"
    );
}

#[test]
fn icons_from_the_database_are_kept_as_written() {
    let dir = tempfile::tempdir().unwrap();
    let db = MimeDatabase::from_dirs([build(dir.path(), RIVALS)]);
    assert_eq!(db.icon_name(&m("APPLICATION/X-BASE")), "Base-Icon");
    assert_eq!(
        db.generic_icon_name(&m("application/x-base")),
        "Base-Generic"
    );
}

#[test]
fn a_name_that_fits_the_content_wins_else_the_best_name_is_a_guess() {
    let dir = tempfile::tempdir().unwrap();
    let db = MimeDatabase::from_dirs([build(dir.path(), RIVALS)]);
    let detect = |name: &str, data: &[u8]| {
        let found = db.type_for_name_and_data(name, data);
        (found.mime.to_string(), found.certainty, found.decided_by)
    };
    assert_eq!(
        detect("f.amb", b"BASE data"),
        (
            "application/x-sub".into(),
            Certainty::Certain,
            DecidedBy::Name
        ),
        "the lower-weight name is a subclass of the content's type"
    );
    assert_eq!(
        detect("f.amb", b"THIRD data"),
        (
            "application/x-other".into(),
            Certainty::Guess,
            DecidedBy::Name
        ),
        "content names neither: the best name, a guess"
    );
    assert_eq!(
        detect("f.amb", b"plain words"),
        (
            "application/x-other".into(),
            Certainty::Guess,
            DecidedBy::Name
        ),
        "no magic, text, and no name is text: the best name, a guess"
    );
    assert_eq!(
        detect("f.txtamb", b"plain words"),
        (
            "text/x-amb-source".into(),
            Certainty::Guess,
            DecidedBy::Name
        ),
        "no magic: text content picks the name that is text"
    );
    assert_eq!(
        detect("f.txtamb", b"\x00\x01binary"),
        (
            "application/x-amb-binary".into(),
            Certainty::Guess,
            DecidedBy::Name
        ),
        "no magic, binary: every name fits, the first wins"
    );
    assert_eq!(
        detect("f.none", b"BASE data"),
        (
            "application/x-base".into(),
            Certainty::Certain,
            DecidedBy::Content
        )
    );

    let files = tempfile::tempdir().unwrap();
    fs::write(files.path().join("f.amb"), "BASE data").unwrap();
    let found = db
        .type_for_path(files.path().join("f.amb"), &PathOptions::default())
        .unwrap();
    assert_eq!(
        (found.mime.as_str(), found.certainty),
        ("application/x-sub", Certainty::Certain),
        "the same rule for a file"
    );
}

#[test]
fn xml_roots_by_namespace_alone_by_name_alone_and_below_a_subclass() {
    let dir = tempfile::tempdir().unwrap();
    let db = MimeDatabase::from_dirs([build(dir.path(), RIVALS)]);
    let detect = |data: &str| {
        let found = db.type_for_data(data.as_bytes());
        (found.mime.to_string(), found.decided_by)
    };
    assert_eq!(
        detect(r#"<?xml version="1.0"?><whatever xmlns="urn:any"/>"#),
        ("application/x-any-element+xml".into(), DecidedBy::XmlRoot),
        "the namespace with any element"
    );
    assert_eq!(
        detect(r#"<?xml version="1.0"?><lonely xmlns="urn:elsewhere"/>"#),
        ("application/x-any-namespace+xml".into(), DecidedBy::XmlRoot),
        "the local name in any namespace"
    );
    assert_eq!(
        detect(r#"<?xml version="1.0"?><!--sub--><whatever xmlns="urn:any"/>"#),
        ("application/x-any-element+xml".into(), DecidedBy::XmlRoot),
        "magic gave a subclass of application/xml"
    );
    assert_eq!(
        detect(r#"<?xml version="1.0"?><!--sub--><other/>"#),
        ("application/x-xml-sub".into(), DecidedBy::Content)
    );
}

#[test]
fn one_pattern_in_two_folders_ranks_the_more_important_folder_first() {
    let low_dir = tempfile::tempdir().unwrap();
    let high_dir = tempfile::tempdir().unwrap();
    let low = build(
        low_dir.path(),
        r#"<mime-type type="application/x-low"><glob pattern="*.same"/></mime-type>"#,
    );
    let high = build(
        high_dir.path(),
        r#"<mime-type type="application/x-high"><glob pattern="*.same"/></mime-type>"#,
    );
    let db = MimeDatabase::from_dirs([high, low]);
    let found = db.type_for_name("a.same");
    let types: Vec<_> = found
        .matches()
        .iter()
        .map(|found| found.mime.as_str())
        .collect();
    assert_eq!(types, ["application/x-high", "application/x-low"]);
    assert_eq!(found.best().unwrap().certainty, Certainty::Guess);
    assert_eq!(db.patterns_for(&m("application/x-low")), ["*.same"]);
}

#[test]
fn a_rule_that_looks_far_in_asks_for_at_most_64_kib() {
    let dir = tempfile::tempdir().unwrap();
    let mime = build(
        dir.path(),
        r#"<mime-type type="application/x-far">
             <magic><match type="string" offset="0:100000" value="FAR"/></magic>
           </mime-type>"#,
    );
    let db = MimeDatabase::from_dirs(&[mime]);
    assert_eq!(db.sniff_len(), 64 * 1024);
}

#[test]
fn names_and_data_together_without_io() {
    let dir = tempfile::tempdir().unwrap();
    let db = system(dir.path());
    let detect = |name: &str, data: &[u8]| {
        let found = db.type_for_name_and_data(name, data);
        (found.mime.to_string(), found.certainty, found.decided_by)
    };
    assert_eq!(
        detect("a.diff", b"\x00 not a diff"),
        ("text/x-diff".into(), Certainty::Certain, DecidedBy::Name),
        "a name all patterns agree on decides alone"
    );
    assert_eq!(
        detect("x.tar.gz", b"plain words").1,
        Certainty::Guess,
        "two types claim the name and the content names neither"
    );
    assert_eq!(
        detect("noext", b"diff\tdata"),
        ("text/x-diff".into(), Certainty::Certain, DecidedBy::Content)
    );
    assert_eq!(
        detect("noext", b""),
        (
            "application/x-zerosize".into(),
            Certainty::Certain,
            DecidedBy::Content
        )
    );
    assert_eq!(
        detect("noext", b"\x00\x01"),
        (
            "application/octet-stream".into(),
            Certainty::Guess,
            DecidedBy::Fallback
        )
    );
}

#[test]
fn relationships_icons_patterns_and_types() {
    let dir = tempfile::tempdir().unwrap();
    let db = system(dir.path());
    assert_eq!(
        db.canonical(&m("application/x-older")),
        m("application/x-old")
    );
    assert_eq!(
        db.aliases(&m("application/x-old")),
        [m("application/x-older")]
    );
    assert_eq!(
        db.parents(&m("application/x-thing+xml")),
        [m("application/xml")]
    );
    assert_eq!(
        db.ancestors(&m("application/x-thing+xml")),
        [
            m("application/xml"),
            m("text/plain"),
            m("application/octet-stream")
        ]
    );
    assert!(db.is_a(&m("text/x-diff"), &m("text/plain")));
    assert!(
        db.is_a(&m("text/x-whatever"), &m("text/plain")),
        "implicit rule"
    );
    assert!(db.is_a(&m("application/x-older"), &m("application/octet-stream")));
    assert!(!db.is_a(&m("inode/directory"), &m("application/octet-stream")));
    // Subclassing is transitive: a type below a text/* type is text/plain.
    assert!(db.is_a(&m("application/x-text-based"), &m("text/plain")));
    // The loop that crashed xdg-mime-rs: answered, not recursed forever.
    assert!(!db.is_a(&m("application/x-loop-a"), &m("text/plain")));
    assert_eq!(
        db.ancestors(&m("application/x-loop-a")),
        [m("application/x-loop-b"), m("application/octet-stream")]
    );
    assert_eq!(db.icon_name(&m("image/png")), "image-png");
    assert_eq!(db.generic_icon_name(&m("image/png")), "image-x-generic");
    assert_eq!(
        db.icon_name(&m("IMAGE/PNG")),
        "image-png",
        "derived icon names are lowercase"
    );
    assert_eq!(db.generic_icon_name(&m("IMAGE/PNG")), "image-x-generic");
    assert_eq!(db.patterns_for(&m("text/x-diff")), ["*.diff", "*.patch"]);
    let all = db.all_types();
    assert!(all.contains(&m("text/x-diff")) && all.contains(&m("application/x-loop-b")));
    assert!(
        !all.contains(&m("application/x-older")),
        "aliases are not types"
    );
}

#[test]
fn later_folders_override_and_delete() {
    let system_dir = tempfile::tempdir().unwrap();
    let user_dir = tempfile::tempdir().unwrap();
    let system = build(system_dir.path(), SYSTEM);
    let user = build(user_dir.path(), USER);
    // Most important first.
    let db = MimeDatabase::from_dirs(&[user.clone(), system.clone()]);
    assert!(db.diagnostics().is_empty(), "{:?}", db.diagnostics());
    assert_eq!(db.dirs(), [user.as_path(), system.as_path()]);
    assert_eq!(
        db.patterns_for(&m("text/x-diff")),
        ["*.dif"],
        "glob-deleteall"
    );
    assert_eq!(
        db.type_for_data(b"diff\tx").mime.as_str(),
        "text/plain",
        "magic-deleteall"
    );
    let german = Locales::new(["de"]);
    assert_eq!(
        db.description(&m("text/x-diff"), &german).as_deref(),
        Some("Diff-Datei")
    );
    assert_eq!(
        db.description(&m("text/x-diff"), &Locales::untranslated())
            .as_deref(),
        Some("Differences"),
        "the user folder has no untranslated comment"
    );

    // A folder read from text files only merges like one read from the cache.
    fs::remove_file(system.join("mime.cache")).unwrap();
    let db = MimeDatabase::from_dirs(&[user, system]);
    assert!(db.diagnostics().is_empty(), "{:?}", db.diagnostics());
    assert_eq!(db.patterns_for(&m("text/x-diff")), ["*.dif"]);
    assert_eq!(
        db.type_for_name("main.C").best().unwrap().mime.as_str(),
        "text/x-c++src"
    );
}

#[test]
fn an_empty_value_in_a_more_important_folder_replaces_a_value() {
    let system_dir = tempfile::tempdir().unwrap();
    let user_dir = tempfile::tempdir().unwrap();
    let system = build(system_dir.path(), SYSTEM);
    // `update-mime-database` writes these self-closed: `<comment/>`.
    let user = build(
        user_dir.path(),
        r#"<mime-type type="text/x-diff">
             <comment></comment><comment xml:lang="de"></comment>
             <acronym></acronym><expanded-acronym></expanded-acronym>
           </mime-type>"#,
    );
    let file = fs::read_to_string(user.join("text/x-diff.xml")).unwrap();
    assert!(file.contains("<comment/>"), "{file}");
    let db = MimeDatabase::from_dirs(&[user, system]);
    assert!(db.diagnostics().is_empty(), "{:?}", db.diagnostics());
    let diff = m("text/x-diff");
    assert!(
        db.type_diagnostics(&diff).is_empty(),
        "{:?}",
        db.type_diagnostics(&diff)
    );
    assert_eq!(
        db.description(&diff, &Locales::untranslated()).as_deref(),
        Some("")
    );
    assert_eq!(
        db.description(&diff, &Locales::new(["de"])).as_deref(),
        Some("")
    );
    assert_eq!(db.acronym(&diff).as_deref(), Some(""));
    assert_eq!(db.expanded_acronym(&diff).as_deref(), Some(""));
}

/// Fails at once when `path`, under the temporary folder, holds a `:`,
/// which would split it in an XDG path list.
fn assert_listable(path: &Path) {
    assert!(
        !path.as_os_str().as_bytes().contains(&b':'),
        "TMPDIR contains ':', which an XDG path list cannot hold; set TMPDIR to a path without ':'"
    );
}

#[test]
fn the_data_home_listed_again_is_read_once() {
    let dir = tempfile::tempdir().unwrap();
    assert_listable(dir.path());
    build(dir.path(), SYSTEM);
    let data = dir.path().as_os_str().to_owned();
    let base = xdg_kit_basedir::BaseDirs::from_vars(|name| match name {
        "HOME" => Some("/home/u".into()),
        "XDG_DATA_HOME" | "XDG_DATA_DIRS" => Some(data.clone()),
        _ => None,
    })
    .unwrap();
    let db = MimeDatabase::load(&base);
    assert_eq!(db.dirs(), [dir.path().join("mime").as_path()]);
    assert_eq!(db.type_for_name("a.diff").matches().len(), 1);
}

#[test]
fn a_folder_reached_through_a_link_is_read_once() {
    let dir = tempfile::tempdir().unwrap();
    let mime = build(dir.path(), SYSTEM);
    let link = dir.path().join("linked");
    symlink(&mime, &link).unwrap();
    let db = MimeDatabase::from_dirs(&[link.clone(), mime.clone()]);
    assert_eq!(db.dirs(), [link.as_path()], "kept at its first place");
    assert_eq!(db.type_for_name("a.diff").matches().len(), 1);
    assert_eq!(db.patterns_for(&m("text/x-diff")), ["*.diff", "*.patch"]);

    let db = MimeDatabase::from_dirs([mime.as_path()]);
    let shown = format!("{db:?}");
    assert!(shown.starts_with("MimeDatabase { dirs: ["), "{shown}");
    assert!(shown.len() < 500, "a summary, not every rule: {shown}");
}

/// A database that defines the built-in detection types as aliases of
/// other types.
const ALIASED: &str = r#"
  <mime-type type="x-test/plain"><alias type="text/plain"/></mime-type>
  <mime-type type="x-test/empty"><alias type="application/x-zerosize"/></mime-type>
  <mime-type type="x-test/folder"><alias type="inode/directory"/></mime-type>
  <mime-type type="text/x-note"><glob pattern="*.note"/></mime-type>
"#;

#[test]
fn results_are_canonical_even_for_aliased_built_in_types() {
    let dir = tempfile::tempdir().unwrap();
    let db = MimeDatabase::from_dirs([build(dir.path(), ALIASED)]);
    assert_eq!(db.type_for_data(b"hello").mime, m("x-test/plain"));
    assert_eq!(db.type_for_data(b"").mime, m("x-test/empty"));
    assert_eq!(
        db.type_for_name_and_data("a.unknown", b"").mime,
        m("x-test/empty")
    );
    assert_eq!(
        db.type_for_name_and_data("a.unknown", b"hello").mime,
        m("x-test/plain")
    );
    let options = PathOptions::default();
    assert_eq!(
        db.type_for_path(dir.path(), &options).unwrap().mime,
        m("x-test/folder")
    );
    let empty = dir.path().join("empty.unknown");
    fs::write(&empty, "").unwrap();
    assert_eq!(
        db.type_for_path(&empty, &options).unwrap().mime,
        m("x-test/empty")
    );
    let ancestors = db.ancestors(&m("text/x-note"));
    assert!(ancestors.contains(&m("x-test/plain")), "{ancestors:?}");
    assert!(!ancestors.contains(&m("text/plain")), "{ancestors:?}");
    assert!(db.is_a(&m("text/x-note"), &m("text/plain")));
    assert!(
        !db.ancestors(&m("x-test/plain"))
            .contains(&m("x-test/plain"))
    );
}

#[test]
fn refresh_notices_a_changed_mode() {
    let dir = tempfile::tempdir().unwrap();
    let mime = build(dir.path(), SYSTEM);
    let db = MimeDatabase::from_dirs([mime.clone()]);
    assert!(db.refresh().is_none(), "nothing changed");
    // Only the status change time moves. Flip a permission bit until `stat`
    // shows it moved: on a filesystem with coarse timestamps that takes
    // until the next tick.
    let cache = mime.join("mime.cache");
    let before = fs::metadata(&cache).unwrap();
    let mut mode = before.permissions().mode();
    for _ in 0..200 {
        mode ^= 0o004;
        fs::set_permissions(&cache, fs::Permissions::from_mode(mode)).unwrap();
        let now = fs::metadata(&cache).unwrap();
        if (now.ctime(), now.ctime_nsec()) != (before.ctime(), before.ctime_nsec()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        db.refresh().is_some(),
        "a chmod can make a file readable or not"
    );
}

#[test]
fn refresh_notices_changed_added_and_removed_files() {
    let dir = tempfile::tempdir().unwrap();
    let extra = tempfile::tempdir().unwrap();
    let mime = build(dir.path(), SYSTEM);
    let later = extra.path().join("mime");
    let db = MimeDatabase::from_dirs(&[later.clone(), mime.clone()]);
    assert!(db.refresh().is_none(), "nothing changed");

    // A folder that appears.
    build(extra.path(), USER);
    let db = db.refresh().expect("the new folder was noticed");
    assert_eq!(db.patterns_for(&m("text/x-diff")), ["*.dif"]);
    assert!(db.refresh().is_none());

    // A text file that changes while the cache is there: the folder is
    // read again, and the cache still wins.
    fs::write(
        later.join("globs2"),
        "0:text/x-diff:__NOGLOBS__\n50:text/x-diff:*.textonly\n",
    )
    .unwrap();
    let db = db.refresh().expect("the changed globs2 was noticed");
    assert_eq!(
        db.patterns_for(&m("text/x-diff")),
        ["*.dif"],
        "from the cache"
    );

    // The cache removed: the text files are read instead.
    fs::remove_file(later.join("mime.cache")).unwrap();
    let db = db.refresh().expect("the removed cache was noticed");
    assert_eq!(
        db.patterns_for(&m("text/x-diff")),
        ["*.textonly"],
        "read from the text files now"
    );

    // A file that changes size.
    fs::write(
        later.join("globs2"),
        "0:text/x-diff:__NOGLOBS__\n50:text/x-diff:*.different\n",
    )
    .unwrap();
    let db = db.refresh().expect("the changed globs2 was noticed");
    assert_eq!(db.patterns_for(&m("text/x-diff")), ["*.different"]);

    // A change that keeps the size: only the modification time tells.
    fs::write(
        later.join("globs2"),
        "0:text/x-diff:__NOGLOBS__\n50:text/x-diff:*.differenx\n",
    )
    .unwrap();
    fs::File::options()
        .write(true)
        .open(later.join("globs2"))
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(10))
        .unwrap();
    let db = db.refresh().expect("the rewritten globs2 was noticed");
    assert_eq!(db.patterns_for(&m("text/x-diff")), ["*.differenx"]);

    // A file added to a folder that was already read.
    fs::remove_file(later.join("icons")).unwrap();
    let db = db.refresh().expect("the removed icons file was noticed");
    assert_eq!(db.icon_name(&m("text/x-diff")), "text-x-diff");
    fs::write(later.join("icons"), "text/x-diff:my-diff-icon\n").unwrap();
    let db = db.refresh().expect("the added icons file was noticed");
    assert_eq!(db.icon_name(&m("text/x-diff")), "my-diff-icon");

    // A per-type XML file whose description was read, rewritten.
    let german = Locales::new(["de"]);
    assert_eq!(
        db.description(&m("text/x-diff"), &german).as_deref(),
        Some("Diff-Datei")
    );
    let xml = later.join("text/x-diff.xml");
    let text = fs::read_to_string(&xml).unwrap();
    fs::write(&xml, text.replace("Diff-Datei", "Neue Diff-Datei")).unwrap();
    let db = db.refresh().expect("the rewritten description was noticed");
    assert_eq!(
        db.description(&m("text/x-diff"), &german).as_deref(),
        Some("Neue Diff-Datei")
    );

    // A folder that disappears.
    fs::remove_dir_all(&later).unwrap();
    let db = db.refresh().expect("the removed folder was noticed");
    assert_eq!(db.patterns_for(&m("text/x-diff")), ["*.diff", "*.patch"]);
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

/// Runs `f` in a thread and returns its result. If a FIFO blocks the
/// read, the test fails after `timeout`.
#[cfg(not(target_vendor = "apple"))]
fn within<T: Send + 'static>(timeout: Duration, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || send.send(f()));
    receive.recv_timeout(timeout).expect("it returns in time")
}

#[cfg(not(target_vendor = "apple"))]
#[test]
fn a_fifo_mime_cache_is_reported_and_the_text_files_used() {
    let dir = tempfile::tempdir().unwrap();
    let mime = build(dir.path(), SYSTEM);
    fs::remove_file(mime.join("mime.cache")).unwrap();
    fifo(&mime.join("mime.cache"));
    let db = within(Duration::from_secs(10), move || {
        Box::new(MimeDatabase::from_dirs(vec![mime]))
    });
    let messages: Vec<String> = db.diagnostics().iter().map(ToString::to_string).collect();
    assert_eq!(messages.len(), 1, "{messages:#?}");
    assert!(messages[0].contains("mime.cache"), "{messages:#?}");
    assert_eq!(
        db.type_for_name("a.diff").best().unwrap().mime.as_str(),
        "text/x-diff"
    );
}

#[cfg(not(target_vendor = "apple"))]
#[test]
fn a_fifo_text_file_is_reported_and_the_rest_used() {
    let dir = tempfile::tempdir().unwrap();
    let mime = build(dir.path(), SYSTEM);
    fs::remove_file(mime.join("mime.cache")).unwrap();
    fs::remove_file(mime.join("magic")).unwrap();
    fifo(&mime.join("magic"));
    let db = within(Duration::from_secs(10), move || {
        Box::new(MimeDatabase::from_dirs(vec![mime]))
    });
    let messages: Vec<String> = db.diagnostics().iter().map(ToString::to_string).collect();
    assert_eq!(messages.len(), 1, "{messages:#?}");
    assert!(messages[0].contains("magic"), "{messages:#?}");
    assert_eq!(
        db.type_for_name("a.diff").best().unwrap().mime.as_str(),
        "text/x-diff"
    );
}

#[test]
fn damaged_files_are_reported_and_the_rest_used() {
    let dir = tempfile::tempdir().unwrap();
    let mime = build(dir.path(), SYSTEM);
    fs::write(mime.join("mime.cache"), b"\x00\x01\x00\x02 too short").unwrap();
    fs::write(
        mime.join("aliases"),
        "application/x-older application/x-old\nnot-a-type\n",
    )
    .unwrap();
    let db = MimeDatabase::from_dirs(&[mime]);
    let messages: Vec<String> = db.diagnostics().iter().map(ToString::to_string).collect();
    assert_eq!(messages.len(), 2, "{messages:#?}");
    assert!(messages[0].contains("mime.cache"), "{messages:#?}");
    assert!(messages[1].contains("aliases:2"), "{messages:#?}");
    assert_eq!(
        db.canonical(&m("application/x-older")),
        m("application/x-old")
    );
    assert_eq!(
        db.type_for_name("a.diff").best().unwrap().mime.as_str(),
        "text/x-diff"
    );
}

/// A folder holding the given files of `tests/old-writers`, which
/// `update-mime-database` 0.30 (`mime.cache` 1.0, `globs`) and 0.60
/// (`mime.cache` 1.1) wrote from `package.xml` there.
fn old_writer(dir: &Path, files: &[&str]) -> PathBuf {
    let from = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/old-writers");
    for file in files {
        let to = dir.join(file.rsplit('/').next().unwrap());
        fs::copy(from.join(file), to).unwrap();
    }
    dir.to_owned()
}

#[test]
fn a_version_1_0_cache_is_left_for_the_text_files() {
    let dir = tempfile::tempdir().unwrap();
    let mime = old_writer(dir.path(), &["0.30/mime.cache", "0.30/globs"]);
    let db = MimeDatabase::from_dirs(&[mime]);
    let messages: Vec<String> = db.diagnostics().iter().map(ToString::to_string).collect();
    assert_eq!(messages.len(), 1, "{messages:#?}");
    assert!(
        messages[0]
            .ends_with("mime.cache: unsupported version 1.0; the text files are read instead"),
        "{messages:#?}"
    );
    for (name, mime) in [
        ("Afile", "text/x-a"),
        ("x.aa", "text/x-a"),
        ("abcz", "text/x-a"),
        ("Bfile", "text/x-b"),
        ("x.bb", "text/x-b"),
    ] {
        assert_eq!(
            db.type_for_name(name).best().map(|found| found.mime),
            Some(m(mime)),
            "{name}"
        );
    }
}

#[test]
fn a_version_1_1_cache_is_read() {
    let dir = tempfile::tempdir().unwrap();
    // No text files: what is found comes from the cache.
    let mime = old_writer(dir.path(), &["0.60/mime.cache"]);
    let db = MimeDatabase::from_dirs(&[mime]);
    assert!(db.diagnostics().is_empty(), "{:?}", db.diagnostics());
    for (name, mime) in [
        ("Afile", "text/x-a"),
        ("x.aa", "text/x-a"),
        ("abcz", "text/x-a"),
        ("Bfile", "text/x-b"),
        ("x.bb", "text/x-b"),
    ] {
        assert_eq!(
            db.type_for_name(name).best().map(|found| found.mime),
            Some(m(mime)),
            "{name}"
        );
    }
    assert_eq!(
        db.patterns_for(&m("text/x-a")).len(),
        3,
        "{:?}",
        db.patterns_for(&m("text/x-a"))
    );
}

#[test]
fn folders_and_special_files_are_typed_by_kind() {
    let dir = tempfile::tempdir().unwrap();
    let db = system(dir.path());
    let files = tempfile::tempdir().unwrap();
    let path = |name: &str| files.path().join(name);
    let detect = |path: &Path, options: &PathOptions| db.type_for_path(path, options).unwrap();
    let defaults = PathOptions::default();

    fs::create_dir(path("folder")).unwrap();
    assert_eq!(
        detect(&path("folder"), &defaults).mime.as_str(),
        "inode/directory"
    );
    assert_eq!(
        detect(Path::new("/"), &defaults).mime.as_str(),
        "inode/mount-point"
    );
    // A folder on another filesystem than its parent, where there is one.
    let on_other_device = |dir: &str| {
        fs::metadata(dir)
            .is_ok_and(|inner| fs::metadata("/").is_ok_and(|root| root.dev() != inner.dev()))
    };
    match ["/proc", "/sys", "/dev"]
        .into_iter()
        .find(|dir| on_other_device(dir))
    {
        Some(mounted) => assert_eq!(
            detect(Path::new(mounted), &defaults).mime.as_str(),
            "inode/mount-point",
            "{mounted}"
        ),
        None => eprintln!("no separate filesystem below /: mount points by device not checked"),
    }
    let mut no_mounts = PathOptions::default();
    no_mounts.mount_points = false;
    assert_eq!(
        detect(Path::new("/"), &no_mounts).mime.as_str(),
        "inode/directory"
    );

    // A FIFO is typed by its kind, never opened (opening would block).
    #[cfg(not(target_vendor = "apple"))]
    {
        fifo(&path("fifo"));
        assert_eq!(detect(&path("fifo"), &defaults).mime.as_str(), "inode/fifo");
    }
    assert_eq!(
        detect(Path::new("/dev/null"), &defaults).mime.as_str(),
        "inode/chardevice"
    );
    let _listener = UnixListener::bind(path("socket")).unwrap();
    assert_eq!(
        detect(&path("socket"), &defaults).mime.as_str(),
        "inode/socket"
    );
    // Making a block device needs root: use one the system has, if any (the
    // Nix build sandbox has none).
    let block = fs::read_dir("/dev").ok().and_then(|entries| {
        entries.flatten().map(|entry| entry.path()).find(|path| {
            fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_block_device())
        })
    });
    match block {
        Some(block) => assert_eq!(
            detect(&block, &defaults).mime.as_str(),
            "inode/blockdevice",
            "{}",
            block.display()
        ),
        None => eprintln!("no block device under /dev: inode/blockdevice not checked"),
    }
}

#[test]
fn paths_by_name_content_and_link() {
    let dir = tempfile::tempdir().unwrap();
    let db = system(dir.path());
    let files = tempfile::tempdir().unwrap();
    let path = |name: &str| files.path().join(name);
    let detect = |path: &Path, options: &PathOptions| db.type_for_path(path, options).unwrap();
    let defaults = PathOptions::default();

    fs::write(path("a.diff"), "diff\tx").unwrap();
    let found = detect(&path("a.diff"), &defaults);
    assert_eq!(
        (found.mime.as_str(), found.decided_by),
        ("text/x-diff", DecidedBy::Name)
    );

    // Two types claim the name: the content decides.
    fs::write(path("x.tar.gz"), "plain words").unwrap();
    let found = detect(&path("x.tar.gz"), &defaults);
    assert_eq!(found.certainty, Certainty::Guess);

    fs::write(path("noext"), "diff\tdata").unwrap();
    assert_eq!(
        detect(&path("noext"), &defaults).mime.as_str(),
        "text/x-diff"
    );
    fs::write(path("empty"), "").unwrap();
    assert_eq!(
        detect(&path("empty"), &defaults).mime.as_str(),
        "application/x-zerosize"
    );
    fs::write(path("binary"), b"\x00\x01").unwrap();
    assert_eq!(
        detect(&path("binary"), &defaults).mime.as_str(),
        "application/octet-stream"
    );

    symlink("a.diff", path("link")).unwrap();
    symlink("missing", path("broken")).unwrap();
    assert_eq!(
        detect(&path("link"), &defaults).mime.as_str(),
        "text/x-diff"
    );
    assert_eq!(
        detect(&path("broken"), &defaults).mime.as_str(),
        "inode/symlink"
    );
    let mut no_follow = PathOptions::default();
    no_follow.follow_links = false;
    assert_eq!(
        detect(&path("link"), &no_follow).mime.as_str(),
        "inode/symlink"
    );

    // An unreadable file whose name decides is not read at all. (Root reads
    // it anyway, so there the check proves nothing and is skipped.)
    fs::write(path("secret.diff"), "x").unwrap();
    fs::set_permissions(path("secret.diff"), fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(path("secret.diff")).is_ok() {
        eprintln!("an unreadable file can be read (root?): not-opening not checked");
    } else {
        assert_eq!(
            detect(&path("secret.diff"), &defaults).mime.as_str(),
            "text/x-diff"
        );
    }
    assert!(db.type_for_path(path("missing"), &defaults).is_err());
}

#[test]
fn a_link_whose_target_cannot_be_reached_is_a_symlink_and_a_refused_one_an_error() {
    let db = MimeDatabase::from_dirs(Vec::<PathBuf>::new());
    let files = tempfile::tempdir().unwrap();
    let path = |name: &str| files.path().join(name);
    let detect = |name: &str| db.type_for_path(path(name), &PathOptions::default());
    fs::write(path("file"), "x").unwrap();
    symlink("missing", path("missing")).unwrap();
    symlink("loop", path("loop")).unwrap();
    symlink("file/child", path("through-a-file")).unwrap();
    symlink("n".repeat(300), path("too-long")).unwrap();
    for name in ["missing", "loop", "through-a-file", "too-long"] {
        assert_eq!(
            detect(name)
                .map(|found| found.mime.as_str().to_owned())
                .ok(),
            Some("inode/symlink".to_owned()),
            "{name}"
        );
    }

    // A target that exists but may not be looked at is an error. (Root
    // may look anyway, so there the check proves nothing and is skipped.)
    fs::create_dir(path("locked")).unwrap();
    fs::write(path("locked/inside"), "x").unwrap();
    symlink("locked/inside", path("refused")).unwrap();
    fs::set_permissions(path("locked"), fs::Permissions::from_mode(0o000)).unwrap();
    let readable = fs::metadata(path("refused")).is_ok();
    let refused = detect("refused");
    fs::set_permissions(path("locked"), fs::Permissions::from_mode(0o755)).unwrap();
    if readable {
        eprintln!("a locked folder can be looked into (root?): the error is not checked");
    } else {
        assert_eq!(
            refused.map_err(|err| err.kind()).err(),
            Some(std::io::ErrorKind::PermissionDenied)
        );
    }
}

/// Needs a filesystem that holds user extended attributes, and fails where
/// there is none, unless `XDG_KIT_SKIP_XATTR_TEST=1` says to skip it (the
/// Nix build sandbox's folders cannot hold them; `checks.workspace` sets it).
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "hurd",
    target_vendor = "apple"
))]
#[test]
fn the_extended_attribute_is_used_only_when_asked() {
    if std::env::var_os("XDG_KIT_SKIP_XATTR_TEST").is_some_and(|value| value == "1") {
        eprintln!("skipped: XDG_KIT_SKIP_XATTR_TEST=1");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let db = system(dir.path());
    let files = tempfile::tempdir().unwrap();
    let file = files.path().join("a.diff");
    fs::write(&file, "x").unwrap();
    if let Err(err) = rustix::fs::setxattr(
        &file,
        "user.mime_type",
        b"application/x-older",
        rustix::fs::XattrFlags::empty(),
    ) {
        panic!(
            "the test folder's filesystem cannot hold extended attributes ({err}); run the tests where it can, or set XDG_KIT_SKIP_XATTR_TEST=1 to skip this test"
        );
    }
    let mut options = PathOptions::default();
    assert_eq!(
        db.type_for_path(&file, &options).unwrap().mime.as_str(),
        "text/x-diff"
    );
    options.extended_attribute = true;
    let found = db.type_for_path(&file, &options).unwrap();
    assert_eq!(
        (found.mime.as_str(), found.decided_by),
        ("application/x-old", DecidedBy::Attribute)
    );
}

/// A folder with only the given text files (no `mime.cache`).
fn text_only(dir: &Path, files: &[(&str, &[u8])]) -> MimeDatabase {
    for (name, contents) in files {
        fs::write(dir.join(name), contents).unwrap();
    }
    MimeDatabase::from_dirs([dir])
}

/// `application/x-custom`'s magic, `[90:application/x-custom]` followed by
/// `lines`, as the only rule of a text-only folder.
fn custom_magic(dir: &Path, lines: &[u8]) -> MimeDatabase {
    let mut magic = b"MIME-Magic\0\n[90:application/x-custom]\n".to_vec();
    magic.extend_from_slice(lines);
    text_only(dir, &[("magic", &magic)])
}

fn is_custom(db: &MimeDatabase, data: &[u8]) -> bool {
    db.type_for_data(data).mime.as_str() == "application/x-custom"
}

#[test]
fn a_magic_matchlet_whose_only_child_is_damaged_is_dropped() {
    for (what, child, problem) in [
        (
            "malformed",
            &b"1>x=\x00\x01Q\n"[..],
            "unknown or malformed field, line skipped",
        ),
        (
            "unknown field",
            b"1>1=\x00\x01Q!future\n",
            "unknown or malformed field, line skipped",
        ),
        (
            "cut off at the end",
            b"1>1=\x00\x05Q",
            "truncated value, rest of file skipped",
        ),
        (
            "nesting skips a level",
            b"2>1=\x00\x01Q\n",
            "nesting skips a level or is too deep, line skipped",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let db = custom_magic(dir.path(), &[&b">0=\x00\x01A\n"[..], child].concat());
        assert!(!is_custom(&db, b"AQ"), "{what}");
        assert!(!is_custom(&db, b"A"), "{what}");
        let messages: Vec<(Option<Location>, &str)> = db
            .diagnostics()
            .iter()
            .map(|found| (found.location, found.message.as_str()))
            .collect();
        // The child's line, then the section's header.
        assert_eq!(
            messages,
            [
                (Some(Location::Offset(45)), problem),
                (
                    Some(Location::Offset(12)),
                    "no test left whole (one lost all its nested lines), section skipped"
                ),
            ],
            "{what}"
        );
    }
}

#[test]
fn a_magic_matchlet_that_keeps_one_child_still_matches_through_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = custom_magic(
        dir.path(),
        b">0=\x00\x01A\n1>1=\x00\x01Q\n1>1=\x00\x01Z!future\n",
    );
    assert!(is_custom(&db, b"AQ"));
    assert!(!is_custom(&db, b"AZ"));
    assert!(!is_custom(&db, b"A"));
}

#[test]
fn losing_every_child_passes_up_the_magic_tree() {
    let dir = tempfile::tempdir().unwrap();
    let db = custom_magic(
        dir.path(),
        b">0=\x00\x01A\n1>1=\x00\x01Q\n2>2=\x00\x01R!future\n>0=\x00\x01B\n",
    );
    assert!(!is_custom(&db, b"AQR"));
    assert!(!is_custom(&db, b"AQ"));
    assert!(is_custom(&db, b"B"), "a sibling rule is kept");
    // A line below a damaged line goes with it, not under an earlier
    // sibling.
    let dir = tempfile::tempdir().unwrap();
    let db = custom_magic(
        dir.path(),
        b">0=\x00\x01A\n1>1=\x00\x01B\n1>1=\x00\x01Q!future\n2>2=\x00\x01C\n",
    );
    assert!(is_custom(&db, b"AB"));
}

#[test]
fn a_damaged_top_level_magic_line_disappears_but_one_of_unknown_depth_drops_the_section() {
    let dir = tempfile::tempdir().unwrap();
    let db = custom_magic(dir.path(), b">0=\x00\x01A!future\n>0=\x00\x01B\n");
    assert!(is_custom(&db, b"B"));
    assert!(!is_custom(&db, b"A"));
    let dir = tempfile::tempdir().unwrap();
    let db = custom_magic(dir.path(), b">0=\x00\x01B\nx>0=\x00\x01C\n>0=\x00\x01D\n");
    assert!(!is_custom(&db, b"B"));
    assert!(!is_custom(&db, b"D"));
}

const NESTED_MAGIC: &str = r#"
  <mime-type type="application/x-custom">
    <magic priority="90">
      <match type="string" offset="0" value="A">
        <match type="string" offset="1" value="Q"/>
        <match type="string" offset="1" value="Z"/>
      </match>
    </magic>
  </mime-type>
"#;

/// Points the value of a matchlet of the first magic rule in
/// `mime.cache` outside the file, and returns its first byte. `route`
/// picks the matchlet: its index among the rule's matchlets, then among
/// each one's children.
fn wreck_cache_matchlet(mime: &Path, route: &[usize]) -> u8 {
    let path = mime.join("mime.cache");
    let mut bytes = fs::read(&path).unwrap();
    let u32_at = |bytes: &[u8], at: usize| {
        u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
    };
    let magic = u32_at(&bytes, 24);
    let first_match = u32_at(&bytes, magic + 8);
    let mut count = u32_at(&bytes, first_match + 8);
    let mut first = u32_at(&bytes, first_match + 12);
    let mut matchlet = 0;
    for &index in route {
        assert!(index < count, "{route:?}");
        matchlet = first + 32 * index;
        count = u32_at(&bytes, matchlet + 24);
        first = u32_at(&bytes, matchlet + 28);
    }
    let value = bytes[u32_at(&bytes, matchlet + 16)];
    bytes[matchlet + 16..matchlet + 20].copy_from_slice(&u32::MAX.to_be_bytes());
    fs::write(&path, bytes).unwrap();
    value
}

#[test]
fn a_cache_matchlet_whose_children_are_damaged_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let mime = build(dir.path(), NESTED_MAGIC);
    wreck_cache_matchlet(&mime, &[0, 0]);
    wreck_cache_matchlet(&mime, &[0, 1]);
    let db = MimeDatabase::from_dirs([&mime]);
    let messages: Vec<&str> = db
        .diagnostics()
        .iter()
        .map(|found| found.message.as_str())
        .collect();
    assert_eq!(
        messages,
        [
            "matchlet value or mask outside the file, skipped",
            "matchlet value or mask outside the file, skipped",
            "no matchlet left whole (one lost all its children), entry skipped",
        ]
    );
    for data in [&b"A"[..], b"AQ", b"AZ"] {
        assert!(!is_custom(&db, data), "{data:?}");
    }
}

#[test]
fn a_cache_entry_that_loses_its_matchlets_for_mixed_reasons_is_reported_once() {
    // One matchlet loses its only child, the other its own value.
    let dir = tempfile::tempdir().unwrap();
    let mime = build(
        dir.path(),
        r#"
  <mime-type type="application/x-custom">
    <magic priority="90">
      <match type="string" offset="0" value="A">
        <match type="string" offset="1" value="Q"/>
      </match>
      <match type="string" offset="0" value="B"/>
    </magic>
  </mime-type>
"#,
    );
    assert_eq!(wreck_cache_matchlet(&mime, &[0, 0]), b'Q');
    assert_eq!(wreck_cache_matchlet(&mime, &[1]), b'B');
    let db = MimeDatabase::from_dirs([&mime]);
    let messages: Vec<&str> = db
        .diagnostics()
        .iter()
        .map(|found| found.message.as_str())
        .collect();
    assert_eq!(
        messages,
        [
            "matchlet value or mask outside the file, skipped",
            "matchlet value or mask outside the file, skipped",
            "no matchlet left whole (one lost all its children), entry skipped",
        ]
    );
    for data in [&b"A"[..], b"AQ", b"B"] {
        assert!(!is_custom(&db, data), "{data:?}");
    }
}

#[test]
fn a_cache_matchlet_that_keeps_one_child_still_matches_through_it() {
    let dir = tempfile::tempdir().unwrap();
    let mime = build(dir.path(), NESTED_MAGIC);
    let lost = wreck_cache_matchlet(&mime, &[0, 1]);
    let kept = if lost == b'Q' { b'Z' } else { b'Q' };
    let db = MimeDatabase::from_dirs([&mime]);
    assert!(is_custom(&db, &[b'A', kept]));
    assert!(!is_custom(&db, &[b'A', lost]));
    assert!(!is_custom(&db, b"A"));
}

/// `x-content/x-custom`'s tree magic, `[50:x-content/x-custom]` followed by
/// `lines`, as the only rule of a text-only folder; and the types found for
/// a volume holding the folder `a/b`.
fn custom_tree_types(lines: &str) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let treemagic = format!("MIME-TreeMagic\0\n[50:x-content/x-custom]\n{lines}");
    let db = text_only(dir.path(), &[("treemagic", treemagic.as_bytes())]);
    let volume = tempfile::tempdir().unwrap();
    fs::create_dir_all(volume.path().join("a/b")).unwrap();
    db.content_types_for_root(volume.path(), &TreeOptions::default())
        .unwrap()
        .iter()
        .map(ToString::to_string)
        .collect()
}

#[test]
fn a_tree_matchlet_whose_only_child_is_damaged_is_dropped() {
    for child in [
        "1>\"a/c\"=socket\n",
        "1>\"../c\"=directory\n",
        "1>\"a/c\"=directory,not/a type!\n",
        "2>\"a/c\"=directory\n",
        "1>\"Foo",
    ] {
        assert!(
            custom_tree_types(&format!(">\"a\"=directory\n{child}")).is_empty(),
            "{child}"
        );
    }
    assert_eq!(
        custom_tree_types(">\"a\"=directory\nx>\"a\"=directory\n>\"a/b\"=directory\n"),
        Vec::<String>::new()
    );
}

#[test]
fn a_tree_matchlet_that_keeps_one_child_still_matches_through_it() {
    assert_eq!(
        custom_tree_types(">\"a\"=directory\n1>\"a/b\"=directory\n1>\"a/c\"=socket\n"),
        ["x-content/x-custom"]
    );
    assert_eq!(
        custom_tree_types(">\"a\"=directory\n1>\"a/c\"=directory\n1>\"a/b\"=socket\n"),
        Vec::<String>::new()
    );
}

#[test]
fn a_well_formed_last_tree_line_without_a_newline_is_read() {
    assert_eq!(
        custom_tree_types(">\"a\"=directory\n1>\"a/b\"=directory"),
        ["x-content/x-custom"]
    );
    assert_eq!(
        custom_tree_types(">\"a\"=directory\n1>\"a/c\"=directory"),
        Vec::<String>::new()
    );
}

#[test]
fn non_empty_asks_for_an_entry_besides_dot_and_dot_dot() {
    assert_eq!(
        custom_tree_types(">\"a/b\"=directory,non-empty\n"),
        Vec::<String>::new()
    );
    assert_eq!(
        custom_tree_types(">\"a\"=directory,non-empty\n"),
        ["x-content/x-custom"]
    );
}

/// Whether `x-content/x-custom`, with the tree rule `line`, is found for a
/// volume holding `xo/inner/f`, `xo/run` (executable), `xo/notes` (text)
/// and `xo/d/x`, where `xo` can be searched but not listed (mode 111).
/// `None` where the permission is not enforced (as for root).
#[cfg(any(target_os = "linux", target_os = "android"))]
fn found_through_unlistable_folder(line: &str) -> Option<bool> {
    let dir = tempfile::tempdir().unwrap();
    let treemagic = format!("MIME-TreeMagic\0\n[50:x-content/x-custom]\n{line}\n");
    let db = text_only(dir.path(), &[("treemagic", treemagic.as_bytes())]);
    assert!(db.diagnostics().is_empty(), "{:?}", db.diagnostics());
    let volume = tempfile::tempdir().unwrap();
    let xo = volume.path().join("xo");
    fs::create_dir_all(xo.join("inner")).unwrap();
    fs::create_dir_all(xo.join("d")).unwrap();
    fs::write(xo.join("inner/f"), "").unwrap();
    fs::write(xo.join("d/x"), "").unwrap();
    fs::write(xo.join("notes"), "some plain text\n").unwrap();
    fs::write(xo.join("run"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(xo.join("run"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&xo, fs::Permissions::from_mode(0o111)).unwrap();
    let enforced = fs::read_dir(&xo).is_err();
    let found = enforced.then(|| {
        !db.content_types_for_root(volume.path(), &TreeOptions::default())
            .unwrap()
            .is_empty()
    });
    fs::set_permissions(&xo, fs::Permissions::from_mode(0o755)).unwrap();
    if !enforced {
        eprintln!("folder permissions are not enforced here (root?): not checked");
    }
    found
}

#[test]
#[cfg(any(target_os = "linux", target_os = "android"))]
fn a_path_through_a_folder_that_cannot_be_listed_is_found() {
    let Some(found) = found_through_unlistable_folder(">\"xo/inner/f\"=file,match-case") else {
        return;
    };
    assert!(found);
    for (line, expected) in [
        // Case-insensitively, the spelling the rule writes is looked up.
        (">\"xo/inner/f\"=file", true),
        // The volume root can be listed.
        (">\"XO/inner/f\"=file", true),
        // Other spellings in `xo` cannot be seen.
        (">\"xo/INNER/f\"=file", false),
        (">\"xo/INNER/f\"=file,match-case", false),
        // The checks on an entry of `xo`.
        (">\"xo/run\"=file,executable", true),
        (">\"xo/notes\"=file,executable", false),
        (">\"xo/notes\"=file,text/plain", true),
        (">\"xo/d\"=directory,non-empty", true),
        (">\"xo/inner\"=directory,non-empty,inode/directory", true),
    ] {
        assert_eq!(
            found_through_unlistable_folder(line),
            Some(expected),
            "{line}"
        );
    }
}

#[test]
fn a_folder_that_can_be_listed_is_searched_in_every_spelling() {
    assert_eq!(
        custom_tree_types(">\"A/B\"=directory\n"),
        ["x-content/x-custom"]
    );
}

/// Whether `x-content/x-custom`, with the only tree rule `>"path"=file`,
/// is found for the volume `root`.
fn custom_file_found(root: &Path, path: &str) -> bool {
    let dir = tempfile::tempdir().unwrap();
    let treemagic = format!("MIME-TreeMagic\0\n[50:x-content/x-custom]\n>\"{path}\"=file\n");
    let db = text_only(dir.path(), &[("treemagic", treemagic.as_bytes())]);
    assert!(db.diagnostics().is_empty(), "{:?}", db.diagnostics());
    !db.content_types_for_root(root, &TreeOptions::default())
        .unwrap()
        .is_empty()
}

#[test]
fn a_path_is_found_whatever_the_route_to_its_folder() {
    // A lookup follows at most 40 links. `real/target` is a file through
    // two links; one name for `real` is a chain of 39 links, the other
    // one link. Looked up as one path, `<chain>/target` has 41 links.
    // Which name the volume lists first is the file system's order, so
    // both are swapped.
    let cases = [("a", "A"), ("A", "a")]
        .into_iter()
        .flat_map(|names| [(names, true), (names, false)]);
    for ((chain, short), chain_first) in cases {
        let root = tempfile::tempdir().unwrap();
        let path = |name: &str| root.path().join(name);
        fs::create_dir(path("real")).unwrap();
        fs::write(path("file"), "x").unwrap();
        symlink("t2", path("real/target")).unwrap();
        symlink("../file", path("real/t2")).unwrap();
        for link in 1..38 {
            symlink(format!("l{}", link + 1), path(&format!("l{link}"))).unwrap();
        }
        symlink("real", path("l38")).unwrap();
        let make_chain = || symlink("l1", path(chain)).unwrap();
        if chain_first {
            make_chain();
        }
        symlink("real", path(short)).unwrap();
        if !chain_first {
            make_chain();
        }
        assert!(
            custom_file_found(root.path(), "a/target"),
            "{chain} through 39 links, {short} through one"
        );
    }
}

#[test]
fn a_path_longer_than_the_system_allows_is_found_step_by_step() {
    use rustix::fs::{Mode, OFlags};
    // 21 folders of 200 bytes: the whole path is over 4,096 bytes, each
    // name well under 255.
    let name = "d".repeat(200);
    let root = tempfile::tempdir().unwrap();
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let mut folder = rustix::fs::open(root.path(), flags, Mode::empty()).unwrap();
    for _ in 0..21 {
        rustix::fs::mkdirat(&folder, name.as_str(), Mode::RWXU).unwrap();
        folder = rustix::fs::openat(&folder, name.as_str(), flags, Mode::empty()).unwrap();
    }
    rustix::fs::openat(
        &folder,
        "target",
        OFlags::WRONLY | OFlags::CREATE | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .unwrap();
    let rule_path = format!("{}target", format!("{name}/").repeat(21));
    assert!(root.path().join(&rule_path).as_os_str().len() > 4096);
    assert!(custom_file_found(root.path(), &rule_path));
}

const ESCAPED_NAMESPACE: &str = r#"
  <mime-type type="application/xml">
    <magic priority="40"><match type="string" offset="0" value="&lt;?xml"/></magic>
  </mime-type>
  <mime-type type="application/x-amp+xml">
    <sub-class-of type="application/xml"/>
    <root-XML namespaceURI="urn:a&amp;b" localName="doc"/>
  </mime-type>
  <mime-type type="application/x-plain+xml">
    <sub-class-of type="application/xml"/>
    <root-XML namespaceURI="urn:plain" localName="doc"/>
  </mime-type>
"#;

#[test]
fn xml_namespaces_are_compared_as_values() {
    let dir = tempfile::tempdir().unwrap();
    let mime = build(dir.path(), ESCAPED_NAMESPACE);
    assert_eq!(
        fs::read_to_string(mime.join("XMLnamespaces")).unwrap(),
        "urn:a&b doc application/x-amp+xml\nurn:plain doc application/x-plain+xml\n",
        "update-mime-database stores the value"
    );
    for cache in [true, false] {
        if !cache {
            fs::remove_file(mime.join("mime.cache")).unwrap();
        }
        let db = MimeDatabase::from_dirs([&mime]);
        assert!(db.diagnostics().is_empty(), "{:?}", db.diagnostics());
        let detect = |root: &str| {
            let found = db.type_for_data(format!("<?xml version=\"1.0\"?>{root}").as_bytes());
            (found.mime.to_string(), found.decided_by)
        };
        for (root, wanted) in [
            (r#"<doc xmlns="urn:a&amp;b"/>"#, "application/x-amp+xml"),
            (r#"<doc xmlns="urn:a&#38;b"/>"#, "application/x-amp+xml"),
            (
                r#"<p:doc xmlns:p="urn:a&#x26;b"/>"#,
                "application/x-amp+xml",
            ),
            (r#"<doc xmlns="urn:plain"/>"#, "application/x-plain+xml"),
            (r#"<p:doc xmlns:p="urn:plain"/>"#, "application/x-plain+xml"),
        ] {
            assert_eq!(
                detect(root),
                (wanted.to_owned(), DecidedBy::XmlRoot),
                "{root} (cache: {cache})"
            );
        }
        assert_eq!(
            detect(r#"<doc xmlns="urn:a&amp;b&bogus;"/>"#).0,
            "application/xml",
            "an unknown entity: no root match"
        );
    }
}

#[test]
fn white_space_in_xml_namespaces_is_normalised() {
    // update-mime-database refuses a namespace with a space or a newline,
    // but keeps a tab written as a character reference.
    let dir = tempfile::tempdir().unwrap();
    let mime = build(
        dir.path(),
        r#"
  <mime-type type="application/xml">
    <magic priority="40"><match type="string" offset="0" value="&lt;?xml"/></magic>
  </mime-type>
  <mime-type type="application/x-tab+xml">
    <sub-class-of type="application/xml"/>
    <root-XML namespaceURI="urn:a&#9;b" localName="doc"/>
  </mime-type>
"#,
    );
    assert_eq!(
        fs::read_to_string(mime.join("XMLnamespaces")).unwrap(),
        "urn:a\tb doc application/x-tab+xml\n"
    );
    for cache in [true, false] {
        if !cache {
            fs::remove_file(mime.join("mime.cache")).unwrap();
        }
        let db = MimeDatabase::from_dirs([&mime]);
        assert!(db.diagnostics().is_empty(), "{:?}", db.diagnostics());
        let detect = |root: &str| {
            db.type_for_data(format!("<?xml version=\"1.0\"?>{root}").as_bytes())
                .mime
                .to_string()
        };
        assert_eq!(
            detect("<doc xmlns=\"urn:a&#9;b\"/>"),
            "application/x-tab+xml",
            "cache: {cache}"
        );
        assert_eq!(
            detect("<doc xmlns=\"urn:a\tb\"/>"),
            "application/xml",
            "a tab as it is becomes a space (cache: {cache})"
        );
    }
}

#[test]
fn implicit_ancestors_are_followed_through_their_own_parents() {
    let dir = tempfile::tempdir().unwrap();
    let db = text_only(
        dir.path(),
        &[(
            "subclasses",
            b"text/x-child text/x-parent\n\
              text/plain application/x-parent\n\
              application/x-parent application/x-grandparent\n\
              application/octet-stream application/x-under-any\n",
        )],
    );
    assert!(db.is_a(&m("text/x-any"), &m("application/x-parent")));
    assert!(db.is_a(&m("text/x-any"), &m("application/x-grandparent")));
    assert!(db.is_a(&m("image/x-any"), &m("application/x-under-any")));
    assert_eq!(
        db.ancestors(&m("text/x-child")),
        [
            m("text/x-parent"),
            m("text/plain"),
            m("application/x-parent"),
            m("application/x-grandparent"),
            m("application/octet-stream"),
            m("application/x-under-any"),
        ]
    );
    assert!(!db.is_a(&m("inode/directory"), &m("application/x-under-any")));
}

#[test]
fn an_inode_type_under_a_data_type_is_any_data_too() {
    // Subclassing is transitive: `text/x-any` is `application/octet-stream`.
    let dir = tempfile::tempdir().unwrap();
    let db = text_only(dir.path(), &[("subclasses", b"inode/x-thing text/x-any\n")]);
    assert_eq!(
        db.ancestors(&m("inode/x-thing")),
        [
            m("text/x-any"),
            m("text/plain"),
            m("application/octet-stream"),
        ]
    );
    assert_eq!(db.ancestors(&m("inode/directory")), []);
}

#[test]
fn a_parent_loop_through_text_plain_ends_with_application_octet_stream_last() {
    let dir = tempfile::tempdir().unwrap();
    let db = text_only(
        dir.path(),
        &[(
            "subclasses",
            b"text/plain application/x-loop\n\
              application/x-loop application/octet-stream\n\
              application/x-loop text/x-looped\n\
              text/x-looped text/plain\n",
        )],
    );
    assert_eq!(
        db.ancestors(&m("text/x-any")),
        [
            m("text/plain"),
            m("application/x-loop"),
            m("text/x-looped"),
            m("application/octet-stream"),
        ]
    );
    // Named explicitly, it is last too.
    assert_eq!(
        db.ancestors(&m("text/plain")),
        [
            m("application/x-loop"),
            m("text/x-looped"),
            m("application/octet-stream"),
        ]
    );
}

#[test]
fn an_explicit_application_octet_stream_parent_comes_last() {
    let dir = tempfile::tempdir().unwrap();
    let db = text_only(
        dir.path(),
        &[(
            "subclasses",
            b"application/x-a application/octet-stream\n\
              application/x-a application/x-b\n\
              application/x-b application/x-c\n\
              application/x-c application/octet-stream\n\
              application/octet-stream application/x-under-any\n\
              application/x-under-any application/x-b\n",
        )],
    );
    assert_eq!(
        db.ancestors(&m("application/x-a")),
        [
            m("application/x-b"),
            m("application/x-c"),
            m("application/octet-stream"),
            m("application/x-under-any"),
        ]
    );
    // Its own ancestors are found as any type's.
    assert_eq!(
        db.ancestors(&m("application/octet-stream")),
        [
            m("application/x-under-any"),
            m("application/x-b"),
            m("application/x-c"),
        ]
    );
    // An `inode/*` type below it is data.
    let db = text_only(
        dir.path(),
        &[(
            "subclasses",
            b"inode/x-thing application/octet-stream\n\
              inode/x-thing inode/x-other\n",
        )],
    );
    assert_eq!(
        db.ancestors(&m("inode/x-thing")),
        [m("inode/x-other"), m("application/octet-stream")]
    );
}
