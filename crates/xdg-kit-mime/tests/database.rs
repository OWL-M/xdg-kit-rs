//! The database as a whole, on folders built by `update-mime-database` (from
//! the dev shell) out of small packages written here.

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use xdg_kit_locale::Locales;
use xdg_kit_mime::{Certainty, DecidedBy, MimeDatabase, MimeType, PathOptions};

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
fn the_data_home_listed_again_is_read_once() {
    let dir = tempfile::tempdir().unwrap();
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
    let fifo = path("fifo");
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &fifo,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        0,
    )
    .unwrap();
    assert_eq!(detect(&fifo, &defaults).mime.as_str(), "inode/fifo");
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

/// Needs a filesystem that holds user extended attributes, and fails where
/// there is none, unless `XDG_KIT_SKIP_XATTR_TEST=1` says to skip it (the
/// Nix build sandbox's folders cannot hold them; `checks.workspace` sets it).
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
