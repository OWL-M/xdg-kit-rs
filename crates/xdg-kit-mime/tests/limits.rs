//! The database's [`Limits`] and [`TreeOptions`], set through the `_with`
//! entry points.

use std::fs;
use std::path::{Path, PathBuf};

use xdg_kit_mime::{Limits, MAX_RULE_DEPTH, MimeDatabase, MimeType, TreeOptions};

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

fn u32_at(bytes: &[u8], at: usize) -> usize {
    u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
}

fn set_u32(bytes: &mut [u8], at: usize, value: usize) {
    bytes[at..at + 4].copy_from_slice(&u32::try_from(value).unwrap().to_be_bytes());
}

/// Two `mime` folders under `dir` whose rules nest `depth` deep below the
/// top level: `text/x-cached` (magic `B`, chained `depth` deep in a
/// `mime.cache`), and from text files `text/x-text` (magic `A`) and
/// `x-content/x-deep` (tree magic: a folder `a`, at every level).
fn deep_rules(dir: &Path, depth: usize) -> [PathBuf; 2] {
    let cached = build(
        &dir.join("cached"),
        r#"<mime-type type="text/x-cached"><magic><match type="string" offset="0" value="B"/></magic></mime-type>"#,
    );
    let path = cached.join("mime.cache");
    let mut bytes = fs::read(&path).unwrap();
    let value_at = bytes.len();
    bytes.extend_from_slice(b"B\0\0\0");
    let chain = bytes.len();
    for level in 0..=depth {
        let at = bytes.len();
        bytes.resize(at + 32, 0);
        // start 0, range 1, word size 1, one byte at `value_at`, no mask.
        for (field, value) in [1, 1, 1, value_at, 0].into_iter().enumerate() {
            set_u32(&mut bytes, at + 4 + 4 * field, value);
        }
        if level < depth {
            set_u32(&mut bytes, at + 24, 1);
            set_u32(&mut bytes, at + 28, at + 32);
        }
    }
    let magic = u32_at(&bytes, 24);
    let first_match = u32_at(&bytes, magic + 8);
    set_u32(&mut bytes, first_match + 8, 1);
    set_u32(&mut bytes, first_match + 12, chain);
    fs::write(&path, bytes).unwrap();

    let text = dir.join("text/mime");
    fs::create_dir_all(&text).unwrap();
    let indent = |level: usize| {
        if level == 0 {
            String::new()
        } else {
            level.to_string()
        }
    };
    let mut magic = b"MIME-Magic\0\n[50:text/x-text]\n".to_vec();
    let mut tree = String::from("MIME-TreeMagic\0\n[50:x-content/x-deep]\n");
    for level in 0..=depth {
        magic.extend_from_slice(format!("{}>0=\0\x01A\n", indent(level)).as_bytes());
        tree.push_str(&indent(level));
        tree.push_str(">\"a\"=directory\n");
    }
    fs::write(text.join("magic"), magic).unwrap();
    fs::write(text.join("treemagic"), tree).unwrap();
    [cached, text]
}

/// Runs `f` on a thread with a 2 MiB stack.
fn on_small_stack(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap();
}

/// Rules nested `depth` deep are read, merged, matched, read again by
/// `refresh` and dropped on a 2 MiB stack, with `depth` allowed.
fn deep_rules_fit(depth: usize) {
    let dir = tempfile::tempdir().unwrap();
    let folders = deep_rules(dir.path(), depth);
    let volume = dir.path().join("volume");
    fs::create_dir_all(volume.join("a")).unwrap();
    on_small_stack(move || {
        let mut limits = Limits::default();
        limits.max_rule_depth = depth;
        let db = MimeDatabase::from_dirs_with(folders.clone(), &limits);
        let check = |db: &MimeDatabase| {
            assert!(db.diagnostics().is_empty(), "{:?}", db.diagnostics());
            assert_eq!(db.type_for_data(b"A").mime, m("text/x-text"));
            assert_eq!(db.type_for_data(b"B").mime, m("text/x-cached"));
            let types = db
                .content_types_for_root(&volume, &TreeOptions::default())
                .unwrap();
            assert_eq!(types, [m("x-content/x-deep")]);
        };
        check(&db);
        fs::write(folders[1].join("aliases"), "").unwrap();
        let again = db.refresh().expect("a file was added");
        check(&again);
        drop(db);
        drop(again);
    });
}

/// How [`MAX_RULE_DEPTH`] was chosen: with `MAX_RULE_DEPTH` raised for the
/// run (depths above it are cut there, so they are not measured), run with
/// `RULE_DEPTH=n` and `--ignored`, in debug and release, bisecting for the
/// deepest `n` that does not overflow the stack: debug 1,493, release
/// 4,656 on x86-64 with rustc 1.98.1. The ceiling, 1,024, leaves about 30%
/// of the debug figure.
#[test]
#[ignore = "measurement"]
fn measure_rule_depth() {
    let depth = std::env::var("RULE_DEPTH").unwrap().parse().unwrap();
    deep_rules_fit(depth);
}

#[test]
fn rules_as_deep_as_the_ceiling_fit_a_small_stack() {
    deep_rules_fit(MAX_RULE_DEPTH);
}

#[test]
fn rules_deeper_than_the_ceiling_are_cut_there() {
    let dir = tempfile::tempdir().unwrap();
    let folders = deep_rules(dir.path(), MAX_RULE_DEPTH + 1);
    on_small_stack(move || {
        let mut limits = Limits::default();
        limits.max_rule_depth = usize::MAX;
        for limits in [limits, Limits::unlimited()] {
            let db = MimeDatabase::from_dirs_with(folders.clone(), &limits);
            // The deepest test is skipped, and each test above it then
            // has no nested test left: the rule is dropped.
            assert_eq!(db.type_for_data(b"A").mime, m("text/plain"));
            assert_eq!(db.type_for_data(b"B").mime, m("text/plain"));
            assert!(
                db.diagnostics()
                    .iter()
                    .any(|d| d.message == "nesting skips a level or is too deep, line skipped"),
                "{:?}",
                db.diagnostics()
            );
        }
    });
}

#[test]
fn the_rule_depth_can_be_lowered() {
    let dir = tempfile::tempdir().unwrap();
    let folders = deep_rules(dir.path(), 3);
    let volume = dir.path().join("volume");
    fs::create_dir_all(volume.join("a")).unwrap();
    let tree = |db: &MimeDatabase| {
        db.content_types_for_root(&volume, &TreeOptions::default())
            .unwrap()
    };
    let default = MimeDatabase::from_dirs(folders.clone());
    assert_eq!(default.type_for_data(b"A").mime, m("text/x-text"));
    assert_eq!(default.type_for_data(b"B").mime, m("text/x-cached"));
    assert_eq!(tree(&default), [m("x-content/x-deep")]);
    let mut limits = Limits::default();
    limits.max_rule_depth = 2;
    let low = MimeDatabase::from_dirs_with(folders, &limits);
    assert_eq!(low.type_for_data(b"A").mime, m("text/plain"));
    assert_eq!(low.type_for_data(b"B").mime, m("text/plain"));
    assert!(tree(&low).is_empty());
    let messages: Vec<&str> = low
        .diagnostics()
        .iter()
        .map(|d| d.message.as_str())
        .collect();
    assert!(
        messages.contains(&"magic nested too deep, branch skipped")
            && messages.contains(&"nesting skips a level or is too deep, line skipped"),
        "{messages:?}"
    );
}

#[test]
fn the_defaults_are_the_limits_without_with() {
    let limits = Limits::default();
    assert_eq!(limits.max_file_len, Some(xdg_kit_basedir::MAX_FILE_LEN));
    assert_eq!(limits.max_diagnostics, Some(100));
    assert_eq!(limits.max_cache_string, Some(4096));
    assert_eq!(limits.cache_copy_per_byte, Some(8));
    assert_eq!(limits.cache_copy_extra, 64 * 1024);
    assert_eq!(limits.max_rule_depth, 64);
    assert_eq!(limits.max_namespace_bindings, Some(128));
    assert_eq!(limits.entity_expansion_per_byte, Some(1));
    assert_eq!(limits.entity_expansion_extra, 64 * 1024);
    let unlimited = Limits::unlimited();
    assert_eq!(
        (
            unlimited.max_file_len,
            unlimited.max_diagnostics,
            unlimited.max_cache_string,
            unlimited.cache_copy_per_byte,
            unlimited.max_rule_depth,
            unlimited.max_namespace_bindings,
        ),
        (None, None, None, None, MAX_RULE_DEPTH, None)
    );
    // Kept, so that a budget set later has the default's shape.
    assert_eq!(unlimited.cache_copy_extra, 64 * 1024);
    assert_eq!(unlimited.entity_expansion_per_byte, None);
    assert_eq!(unlimited.entity_expansion_extra, 64 * 1024);
    let dir = tempfile::tempdir().unwrap();
    let mime = text_folder(dir.path(), &bad_lines(150));
    let db = MimeDatabase::from_dirs([&mime]);
    assert_eq!(db.limits(), &Limits::default());
    let with = MimeDatabase::from_dirs_with([&mime], &Limits::default());
    assert_eq!(db.diagnostics(), with.diagnostics());
    assert_eq!(db.diagnostics().len(), 101);
    let tree = TreeOptions::default();
    assert_eq!(
        (tree.max_lookups, tree.max_reopens),
        (Some(10_000), Some(60_000))
    );
    let tree = TreeOptions::unlimited();
    assert_eq!((tree.max_lookups, tree.max_reopens), (None, None));
}

/// A `globs2` of `count` lines with too few fields, and one good glob.
fn bad_lines(count: usize) -> String {
    let mut text = "x\n".repeat(count);
    text.push_str("50:text/x-good:*.good\n");
    text
}

/// `<dir>/mime` holding only `globs2`, with `globs2` as given.
fn text_folder(dir: &Path, globs2: &str) -> PathBuf {
    let mime = dir.join("mime");
    fs::create_dir_all(&mime).unwrap();
    fs::write(mime.join("globs2"), globs2).unwrap();
    mime
}

#[test]
fn the_problems_listed_per_file_can_be_set_or_unlimited() {
    let dir = tempfile::tempdir().unwrap();
    let mime = text_folder(dir.path(), &bad_lines(5));
    assert_eq!(MimeDatabase::from_dirs([&mime]).diagnostics().len(), 5);
    let mut limits = Limits::default();
    limits.max_diagnostics = Some(2);
    let db = MimeDatabase::from_dirs_with([&mime], &limits);
    let messages: Vec<&str> = db
        .diagnostics()
        .iter()
        .map(|d| d.message.as_str())
        .collect();
    assert_eq!(messages.len(), 3, "{messages:?}");
    assert_eq!(messages[2], "more than 2 problems; the rest are not listed");
    let mime = text_folder(&dir.path().join("many"), &bad_lines(150));
    limits.max_diagnostics = None;
    let db = MimeDatabase::from_dirs_with([&mime], &limits);
    assert_eq!(db.diagnostics().len(), 150);
    assert_eq!(
        db.type_for_name("a.good").best().unwrap().mime,
        m("text/x-good")
    );
}

#[test]
fn the_problems_listed_per_type_file_follow_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    let mime = text_folder(dir.path(), "50:text/x-a:*.a\n");
    fs::create_dir_all(mime.join("text")).unwrap();
    let comments = "<comment>\u{1}</comment>".repeat(5);
    fs::write(
        mime.join("text/x-a.xml"),
        format!(
            r#"<?xml version="1.0"?><mime-type xmlns="http://www.freedesktop.org/standards/shared-mime-info" type="text/x-a">{comments}</mime-type>"#
        ),
    )
    .unwrap();
    let a = m("text/x-a");
    let all = MimeDatabase::from_dirs([&mime]).type_diagnostics(&a).len();
    assert!(all >= 5, "{all}");
    let mut limits = Limits::default();
    limits.max_diagnostics = Some(2);
    let db = MimeDatabase::from_dirs_with([&mime], &limits);
    assert_eq!(db.type_diagnostics(&a).len(), 3);
}

#[test]
fn the_problems_listed_per_type_file_can_be_unlimited() {
    let dir = tempfile::tempdir().unwrap();
    let mime = text_folder(dir.path(), "50:text/x-a:*.a\n");
    fs::create_dir_all(mime.join("text")).unwrap();
    let comments = "<comment>\u{1}</comment>".repeat(150);
    fs::write(
        mime.join("text/x-a.xml"),
        format!(
            r#"<?xml version="1.0"?><mime-type xmlns="http://www.freedesktop.org/standards/shared-mime-info" type="text/x-a">{comments}</mime-type>"#
        ),
    )
    .unwrap();
    let a = m("text/x-a");
    assert_eq!(
        MimeDatabase::from_dirs([&mime]).type_diagnostics(&a).len(),
        101
    );
    let mut limits = Limits::default();
    limits.max_diagnostics = None;
    let db = MimeDatabase::from_dirs_with([&mime], &limits);
    assert_eq!(db.type_diagnostics(&a).len(), 150);
}

/// A `mime.cache` whose literal list is replaced by `count` entries whose
/// type is outside the file: a problem each.
fn bad_literals(dir: &Path, count: usize) -> PathBuf {
    let mime = build(
        dir,
        r#"<mime-type type="text/x-a"><glob pattern="literal"/></mime-type>"#,
    );
    let path = mime.join("mime.cache");
    let mut bytes = fs::read(&path).unwrap();
    let literals = u32_at(&bytes, 12);
    let pattern_at = u32_at(&bytes, literals + 4);
    let list = bytes.len();
    bytes.resize(list + 4 + 12 * count, 0);
    set_u32(&mut bytes, list, count);
    for entry in 0..count {
        let at = list + 4 + 12 * entry;
        set_u32(&mut bytes, at, pattern_at);
        set_u32(&mut bytes, at + 4, 0xffff_ffff);
        set_u32(&mut bytes, at + 8, 50);
    }
    set_u32(&mut bytes, 12, list);
    fs::write(&path, bytes).unwrap();
    mime
}

#[test]
fn the_problems_listed_per_cache_can_be_set_or_unlimited() {
    let dir = tempfile::tempdir().unwrap();
    let mime = bad_literals(dir.path(), 150);
    let listed = |limits: &Limits| {
        MimeDatabase::from_dirs_with([&mime], limits)
            .diagnostics()
            .iter()
            .map(|d| d.message.clone())
            .collect::<Vec<_>>()
    };
    let default = listed(&Limits::default());
    assert_eq!(default.len(), 101, "{default:?}");
    assert_eq!(default[0], "not a MIME type, entry skipped");
    let mut limits = Limits::default();
    limits.max_diagnostics = Some(2);
    assert_eq!(
        listed(&limits)[2..],
        ["more than 2 problems; the rest are not listed"]
    );
    limits.max_diagnostics = None;
    assert_eq!(listed(&limits).len(), 150);
}

#[test]
fn files_larger_than_the_limit_are_reported_and_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let globs2 = "50:text/x-a:*.a\n";
    let mime = text_folder(dir.path(), globs2);
    fs::create_dir_all(mime.join("text")).unwrap();
    let xml = r#"<?xml version="1.0"?><mime-type xmlns="http://www.freedesktop.org/standards/shared-mime-info" type="text/x-a"><comment>A</comment></mime-type>"#;
    fs::write(mime.join("text/x-a.xml"), xml).unwrap();
    let a = m("text/x-a");
    let untranslated = xdg_kit_mime::Locales::untranslated();
    let read = MimeDatabase::from_dirs([&mime]);
    assert_eq!(read.type_for_name("x.a").best().unwrap().mime, a);
    assert_eq!(read.description(&a, &untranslated).as_deref(), Some("A"));

    let mut limits = Limits::default();
    // `globs2` fits; the XML file does not.
    limits.max_file_len = Some(globs2.len() as u64);
    let small = MimeDatabase::from_dirs_with([&mime], &limits);
    assert_eq!(small.type_for_name("x.a").best().unwrap().mime, a);
    assert_eq!(small.description(&a, &untranslated), None);
    let problems = small.type_diagnostics(&a);
    assert!(
        problems.len() == 1 && problems[0].message.starts_with("cannot be read"),
        "{problems:?}"
    );
    // Kept by `refresh`: a text file now too large too.
    fs::write(mime.join("globs2"), format!("{globs2}50:text/x-b:*.b\n")).unwrap();
    let refreshed = small.refresh().expect("globs2 changed");
    assert_eq!(refreshed.limits(), &limits);
    assert!(refreshed.type_for_name("x.a").is_empty());
    assert!(
        refreshed
            .diagnostics()
            .iter()
            .any(|d| d.message.starts_with("cannot be read")),
        "{:?}",
        refreshed.diagnostics()
    );
    assert_eq!(refreshed.description(&a, &untranslated), None);
    let unlimited = MimeDatabase::from_dirs_with([&mime], &Limits::unlimited());
    assert_eq!(
        unlimited.description(&a, &untranslated).as_deref(),
        Some("A")
    );
}

#[test]
fn a_cache_larger_than_the_limit_gives_way_to_the_text_files() {
    let dir = tempfile::tempdir().unwrap();
    let mime = build(
        dir.path(),
        r#"<mime-type type="text/x-a"><glob pattern="*.a"/></mime-type>"#,
    );
    let cache = fs::metadata(mime.join("mime.cache")).unwrap().len();
    let mut limits = Limits::default();
    limits.max_file_len = Some(cache - 1);
    let db = MimeDatabase::from_dirs_with([&mime], &limits);
    assert_eq!(db.type_for_name("x.a").best().unwrap().mime, m("text/x-a"));
    assert!(
        db.diagnostics()[0]
            .message
            .ends_with("the text files are read instead"),
        "{:?}",
        db.diagnostics()
    );
    assert!(MimeDatabase::from_dirs([&mime]).diagnostics().is_empty());
}

/// A database whose `mime.cache` holds a literal pattern of `len` bytes.
fn long_literal(dir: &Path, len: usize) -> (PathBuf, String) {
    let pattern = format!("{}.long", "x".repeat(len - 5));
    let mime = build(
        dir,
        &format!(r#"<mime-type type="text/x-long"><glob pattern="{pattern}"/></mime-type>"#),
    );
    (mime, pattern)
}

#[test]
fn the_longest_cache_string_can_be_set_or_unlimited() {
    let dir = tempfile::tempdir().unwrap();
    let (mime, pattern) = long_literal(dir.path(), 5_000);
    let long = m("text/x-long");
    let default = MimeDatabase::from_dirs([&mime]);
    assert!(default.patterns_for(&long).is_empty());
    assert!(
        default
            .diagnostics()
            .iter()
            .any(|d| d.message == "bad pattern, entry skipped"),
        "{:?}",
        default.diagnostics()
    );
    let mut limits = Limits::default();
    limits.max_cache_string = Some(5_000);
    let db = MimeDatabase::from_dirs_with([&mime], &limits);
    assert_eq!(db.patterns_for(&long), std::slice::from_ref(&pattern));
    assert!(db.diagnostics().is_empty(), "{:?}", db.diagnostics());
    limits.max_cache_string = Some(4_999);
    let db = MimeDatabase::from_dirs_with([&mime], &limits);
    assert!(db.patterns_for(&long).is_empty());
    limits.max_cache_string = None;
    let db = MimeDatabase::from_dirs_with([&mime], &limits);
    assert_eq!(db.patterns_for(&long), [pattern]);
}

/// A `mime.cache` whose literal list is replaced by `count` entries that
/// all point at one pattern of 4,000 bytes: it copies far more than its
/// size.
fn shared_literal(dir: &Path, count: usize) -> PathBuf {
    let mime = build(
        dir,
        r#"<mime-type type="text/x-a"><glob pattern="literal"/></mime-type>"#,
    );
    let path = mime.join("mime.cache");
    let mut bytes = fs::read(&path).unwrap();
    let literals = u32_at(&bytes, 12);
    let mime_at = u32_at(&bytes, literals + 8);
    let pattern_at = bytes.len();
    bytes.extend_from_slice(&[b'y'; 4_000]);
    bytes.extend_from_slice(&[0; 4]);
    let list = bytes.len();
    bytes.resize(list + 4 + 12 * count, 0);
    set_u32(&mut bytes, list, count);
    for entry in 0..count {
        let at = list + 4 + 12 * entry;
        set_u32(&mut bytes, at, pattern_at);
        set_u32(&mut bytes, at + 4, mime_at);
        set_u32(&mut bytes, at + 8, 50);
    }
    set_u32(&mut bytes, 12, list);
    fs::write(&path, bytes).unwrap();
    mime
}

#[test]
fn the_cache_copy_budget_can_be_set_or_lifted() {
    let dir = tempfile::tempdir().unwrap();
    let mime = shared_literal(dir.path(), 100);
    let exhausted = |db: &MimeDatabase| {
        db.diagnostics()
            .iter()
            .any(|d| d.message == "more data than the file can hold, rest skipped")
    };
    let len = fs::metadata(mime.join("mime.cache")).unwrap().len();
    // 100 × 4,000 bytes is more than 8 bytes per byte and 64 KiB.
    assert!(100 * 4_000 > 8 * len + 64 * 1024);
    let default = MimeDatabase::from_dirs([&mime]);
    assert!(exhausted(&default));
    let mut limits = Limits::default();
    limits.cache_copy_per_byte = None;
    let unlimited = MimeDatabase::from_dirs_with([&mime], &limits);
    assert!(!exhausted(&unlimited), "{:?}", unlimited.diagnostics());
    // Enough for all 100 copies by the extra alone.
    limits.cache_copy_per_byte = Some(0);
    limits.cache_copy_extra = 100 * 4_000 + 64 * 1024;
    assert!(!exhausted(&MimeDatabase::from_dirs_with([&mime], &limits)));
    // Nothing to copy.
    limits.cache_copy_extra = 0;
    let none = MimeDatabase::from_dirs_with([&mime], &limits);
    assert!(exhausted(&none));
    assert!(none.patterns_for(&m("text/x-a")).is_empty());
    // Per byte: a budget of 8 per byte plus 64 KiB is the default's.
    limits.cache_copy_per_byte = Some(1_000);
    assert!(!exhausted(&MimeDatabase::from_dirs_with([&mime], &limits)));
}

/// A volume where the rule path `d/d/…/d/x` (`depth` folders `d`) is
/// found only after a dead end `depth` deep, each of whose folders also
/// holds the other spelling (`d` or `D`), listed after it: so every folder
/// on the way down still has a name left, and the search goes back to
/// each. Only the other spelling at the top leads to `x`. Also the tree
/// magic for it.
fn backtracking_volume(dir: &Path, depth: usize) -> (PathBuf, PathBuf) {
    // The order the file system lists the two spellings in, as the search
    // sees it: the same in every folder they are made in this order.
    let probe = dir.join("probe");
    for spelling in ["d", "D"] {
        fs::create_dir_all(probe.join(spelling)).unwrap();
    }
    let first = fs::read_dir(&probe)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name();
    let other = if first == "d" { "D" } else { "d" };
    let volume = dir.join("volume");
    let mut path = volume.clone();
    for _ in 0..depth {
        fs::create_dir_all(path.join(&first)).unwrap();
        fs::create_dir(path.join(other)).unwrap();
        path.push(&first);
    }
    let mut found = volume.join(other);
    for _ in 1..depth {
        found.push(&first);
    }
    fs::create_dir_all(found.join("x")).unwrap();
    let mime = dir.join("mime");
    fs::create_dir_all(&mime).unwrap();
    let rule = format!("{}x", "d/".repeat(depth));
    fs::write(
        mime.join("treemagic"),
        format!("MIME-TreeMagic\0\n[50:x-content/x-found]\n>\"{rule}\"=directory\n"),
    )
    .unwrap();
    (volume, mime)
}

#[test]
fn tree_magic_lookups_can_be_set_or_unlimited() {
    let dir = tempfile::tempdir().unwrap();
    let (volume, mime) = backtracking_volume(dir.path(), 30);
    let db = MimeDatabase::from_dirs([&mime]);
    let found = |options: &TreeOptions| {
        !db.content_types_for_root(&volume, options)
            .unwrap()
            .is_empty()
    };
    assert!(found(&TreeOptions::default()));
    let mut options = TreeOptions::default();
    options.max_lookups = Some(10);
    assert!(!found(&options));
    options.max_lookups = None;
    assert!(found(&options));
    assert!(found(&TreeOptions::unlimited()));
}

#[test]
fn tree_magic_reopens_can_be_set_or_unlimited() {
    let dir = tempfile::tempdir().unwrap();
    // Deeper than the 16 handles a search keeps open, so going back up
    // the dead end opens folders again.
    let (volume, mime) = backtracking_volume(dir.path(), 40);
    let db = MimeDatabase::from_dirs([&mime]);
    let found = |options: &TreeOptions| {
        !db.content_types_for_root(&volume, options)
            .unwrap()
            .is_empty()
    };
    let mut options = TreeOptions::default();
    assert!(found(&options));
    options.max_reopens = Some(0);
    assert!(!found(&options));
    options.max_reopens = None;
    assert!(found(&options));
}

#[test]
fn both_readers_keep_a_leaf_at_the_deepest_level_and_cut_one_below() {
    let mut custom = Limits::default();
    custom.max_rule_depth = 5;
    for (limits, max) in [(Limits::default(), 64), (custom, 5)] {
        let dir = tempfile::tempdir().unwrap();
        let db = MimeDatabase::from_dirs_with(deep_rules(&dir.path().join("at"), max), &limits);
        assert!(db.diagnostics().is_empty(), "{max}: {:?}", db.diagnostics());
        assert_eq!(db.type_for_data(b"A").mime, m("text/x-text"));
        assert_eq!(db.type_for_data(b"B").mime, m("text/x-cached"));
        let db =
            MimeDatabase::from_dirs_with(deep_rules(&dir.path().join("past"), max + 1), &limits);
        assert_eq!(db.type_for_data(b"A").mime, m("text/plain"));
        assert_eq!(db.type_for_data(b"B").mime, m("text/plain"));
        let messages: Vec<&str> = db
            .diagnostics()
            .iter()
            .map(|d| d.message.as_str())
            .collect();
        assert!(
            messages.contains(&"magic nested too deep, branch skipped")
                && messages.contains(&"nesting skips a level or is too deep, line skipped"),
            "{max}: {messages:?}"
        );
    }
}

#[test]
fn namespace_bindings_in_scope_can_be_limited_or_unlimited() {
    use std::fmt::Write as _;
    let dir = tempfile::tempdir().unwrap();
    let mime = text_folder(dir.path(), "50:text/x-a:*.a\n");
    fs::write(
        mime.join("magic"),
        b"MIME-Magic\0\n[50:application/xml]\n>0=\x00\x02<r\n",
    )
    .unwrap();
    fs::write(mime.join("subclasses"), "text/x-r application/xml\n").unwrap();
    fs::write(mime.join("XMLnamespaces"), "urn:r r text/x-r\n").unwrap();
    // 200 bindings in scope: the root's default and 199 more.
    let mut more = String::new();
    for at in 0..199 {
        let _ = write!(more, " xmlns:p{at}=\"urn:{at}\"");
    }
    fs::create_dir_all(mime.join("text")).unwrap();
    fs::write(
        mime.join("text/x-a.xml"),
        format!(
            r#"<mime-type xmlns="http://www.freedesktop.org/standards/shared-mime-info"{more}><comment>A</comment></mime-type>"#
        ),
    )
    .unwrap();
    let document = format!(r#"<r xmlns="urn:r"{more}/>"#);
    let a = m("text/x-a");
    let untranslated = xdg_kit_mime::Locales::untranslated();
    let mut limits = Limits::default();
    for (max, read) in [(Some(128), false), (Some(200), true), (None, true)] {
        limits.max_namespace_bindings = max;
        let db = MimeDatabase::from_dirs_with([&mime], &limits);
        let description = db.description(&a, &untranslated);
        assert_eq!(description.is_some(), read, "{max:?}");
        let problems: Vec<String> = db
            .type_diagnostics(&a)
            .into_iter()
            .map(|d| d.message)
            .collect();
        if read {
            assert!(problems.is_empty(), "{max:?}: {problems:?}");
        } else {
            assert_eq!(
                problems,
                [
                    "bad XML (more than 128 namespace declarations on the open elements); the rest is skipped"
                ]
            );
        }
        let expected = if read { "text/x-r" } else { "application/xml" };
        assert_eq!(
            db.type_for_data(document.as_bytes()).mime,
            m(expected),
            "{max:?}"
        );
    }
}

#[test]
fn the_entity_expansion_budget_can_be_set_or_lifted() {
    // Parameter entities included in a document type and entities
    // expanded in namespace declarations, within the bytes per byte of it
    // plus the extra, in a per-type file and in root detection.
    let dir = tempfile::tempdir().unwrap();
    let mime = text_folder(dir.path(), "50:text/x-a:*.a\n");
    fs::write(
        mime.join("magic"),
        b"MIME-Magic\0\n[50:application/xml]\n>0=\x00\x02<!\n",
    )
    .unwrap();
    fs::write(mime.join("subclasses"), "text/x-r application/xml\n").unwrap();
    fs::write(mime.join("XMLnamespaces"), "urn:r r text/x-r\n").unwrap();
    let ten = "%p;".repeat(10);
    fs::create_dir_all(mime.join("text")).unwrap();
    fs::write(
        mime.join("text/x-a.xml"),
        format!(
            r#"<!DOCTYPE mime-type [<!ENTITY % p "<!ENTITY ns 'http://www.freedesktop.org/standards/shared-mime-info'>">{ten}]><mime-type xmlns="&ns;"><comment>A</comment></mime-type>"#
        ),
    )
    .unwrap();
    let document =
        format!(r#"<!DOCTYPE r [<!ENTITY % p "<!ENTITY a 'urn:r'>">{ten}]><r xmlns="&a;"/>"#);
    let a = m("text/x-a");
    let untranslated = xdg_kit_mime::Locales::untranslated();
    let mut limits = Limits::default();
    for (per_byte, extra, read) in [
        (Some(1), 64 * 1024, true),
        (Some(1), 0, false),
        (Some(20), 0, true),
        (Some(0), 2000, true),
        (None, 0, true),
    ] {
        limits.entity_expansion_per_byte = per_byte;
        limits.entity_expansion_extra = extra;
        let db = MimeDatabase::from_dirs_with([&mime], &limits);
        let description = db.description(&a, &untranslated);
        assert_eq!(description.is_some(), read, "{per_byte:?} {extra}");
        let expected = if read { "text/x-r" } else { "application/xml" };
        assert_eq!(
            db.type_for_data(document.as_bytes()).mime,
            m(expected),
            "{per_byte:?} {extra}"
        );
    }
}

#[test]
fn an_entity_bomb_without_a_budget_is_refused_not_aborted() {
    use std::fmt::Write as _;
    // Reading with no limit returns an error, not an abort. A doubling
    // chain of 40 and of 70 entities, in a per-type file and in a document
    // whose root element is looked at.
    let bomb = |levels: usize, root: &str| {
        let mut text = String::from("<!DOCTYPE r [<!ENTITY g0 \"urn:\">");
        for at in 1..=levels {
            let _ = write!(text, "<!ENTITY g{at} \"&g{0};&g{0};\">", at - 1);
        }
        let _ = write!(text, "]>{root}");
        text
    };
    for levels in [40, 70] {
        let dir = tempfile::tempdir().unwrap();
        let mime = text_folder(dir.path(), "50:text/x-a:*.a\n");
        fs::write(
            mime.join("magic"),
            b"MIME-Magic\0\n[50:application/xml]\n>0=\x00\x02<!\n",
        )
        .unwrap();
        fs::create_dir_all(mime.join("text")).unwrap();
        let file = bomb(
            levels,
            &format!("<mime-type xmlns=\"&g{levels};\"><comment>A</comment></mime-type>"),
        )
        .replacen("<!DOCTYPE r", "<!DOCTYPE mime-type", 1);
        fs::write(mime.join("text/x-a.xml"), file).unwrap();
        let db = MimeDatabase::from_dirs_with([&mime], &Limits::unlimited());
        let untranslated = xdg_kit_mime::Locales::untranslated();
        assert_eq!(
            db.description(&m("text/x-a"), &untranslated),
            None,
            "{levels}"
        );
        assert!(!db.type_diagnostics(&m("text/x-a")).is_empty(), "{levels}");
        let document = bomb(levels, &format!("<r xmlns=\"&g{levels};\"/>"));
        assert_eq!(
            db.type_for_data(document.as_bytes()).mime,
            m("application/xml"),
            "{levels}"
        );
    }
}

#[test]
fn a_namespace_entry_wins_over_a_local_name_entry() {
    // The root element is looked up by namespace and local name, then by
    // namespace alone, then by local name alone; with both of the last two
    // listed, the namespace's entry is the one found.
    let dir = tempfile::tempdir().unwrap();
    let mime = text_folder(dir.path(), "50:text/x-a:*.a\n");
    fs::write(
        mime.join("magic"),
        b"MIME-Magic\0\n[50:application/xml]\n>0=\x00\x01<\n",
    )
    .unwrap();
    fs::write(
        mime.join("subclasses"),
        "text/x-ns application/xml\ntext/x-local application/xml\ntext/x-both application/xml\n",
    )
    .unwrap();
    fs::write(
        mime.join("XMLnamespaces"),
        "urn:a  text/x-ns\n r text/x-local\nurn:b r text/x-both\n",
    )
    .unwrap();
    let db = MimeDatabase::from_dirs([&mime]);
    for (document, expected) in [
        (r#"<r xmlns="urn:a"/>"#, "text/x-ns"),
        (r#"<s xmlns="urn:a"/>"#, "text/x-ns"),
        (r#"<r xmlns="urn:c"/>"#, "text/x-local"),
        (r#"<r xmlns="urn:b"/>"#, "text/x-both"),
        (
            r#"<!DOCTYPE r [<!ENTITY a "urn:a">]><r xmlns="&a;"/>"#,
            "text/x-ns",
        ),
    ] {
        assert_eq!(
            db.type_for_data(document.as_bytes()).mime,
            m(expected),
            "{document}"
        );
    }
}
