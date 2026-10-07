//! shared-mime-info's detection suite (`tests/mime-detection`), scored as
//! xdgmime's `test-mime-data` scores it, against a database built from the
//! same commit.
//!
//! Every lookup the suite does not mark `x` must give the expected type.
//! An `x` marks a lookup the reference gets wrong. If this crate gets it
//! right, the case must have an entry and a reason in
//! `conformance-differences.txt`. Each listed case must remain a reference
//! failure that this crate gets right.
//! Tree magic (`tree-list`) is scored as shared-mime-info's
//! `src/test-tree-magic.c` scores it: a case passes when something is found
//! and every type found is among the listed ones, in any order.
//!
//! Needs the dev shell: `XDG_KIT_SMI_SRC` (the pinned shared-mime-info) and
//! `update-mime-database` from the same commit.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use xdg_kit_mime::{MimeDatabase, MimeType, PathOptions, TreeOptions};

mod common;

fn source() -> PathBuf {
    PathBuf::from(
        std::env::var_os("XDG_KIT_SMI_SRC")
            .expect("XDG_KIT_SMI_SRC is not set: run the tests in the dev shell (nix develop)"),
    )
}

/// The reference database in `dir`, built as shared-mime-info's own test
/// builds it: `freedesktop.org.xml.in` as the only package.
fn reference_database(dir: &Path) -> MimeDatabase {
    let mime = dir.join("mime");
    fs::create_dir_all(mime.join("packages")).unwrap();
    fs::copy(
        source().join("data/freedesktop.org.xml.in"),
        mime.join("packages/freedesktop.org.xml"),
    )
    .unwrap();
    common::update_mime_database(&mime, dir);
    let db = MimeDatabase::from_dirs(&[mime]);
    assert!(db.diagnostics().is_empty(), "{:?}", db.diagnostics());
    db
}

/// A differing answer, as listed in the differences file: lookup kind
/// (`name`, `data`, `file`, `tree`), case, our answer.
type Difference = (String, String, String);

fn listed_differences() -> BTreeSet<Difference> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/conformance-differences.txt");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("{}: cannot be read: {err}", path.display()));
    let mut listed = BTreeSet::new();
    for line in text
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
    {
        let fields: Vec<&str> = line.split('\t').collect();
        assert!(
            fields.len() == 4 && !fields[3].trim().is_empty(),
            "{}: want lookup, case, answer and a reason, tab-separated: {line:?}",
            path.display()
        );
        let difference = (
            fields[0].to_owned(),
            fields[1].to_owned(),
            fields[2].to_owned(),
        );
        assert!(
            listed.insert(difference),
            "{}: listed twice: {line:?}",
            path.display()
        );
    }
    listed
}

fn mime(text: &str) -> MimeType {
    MimeType::new(text).unwrap_or_else(|err| panic!("{err}"))
}

/// What a run found: lookups the reference gets right that this crate gets
/// wrong, and lookups the reference gets wrong that this crate gets right.
#[derive(Default)]
struct Score {
    comparisons: usize,
    failures: Vec<String>,
    successes: BTreeSet<Difference>,
}

impl Score {
    fn record(
        &mut self,
        lookup: &str,
        case: &str,
        passed: bool,
        xfail: bool,
        answer: String,
        expected: &str,
    ) {
        self.comparisons += 1;
        match (passed, xfail) {
            (true, true) => {
                self.successes
                    .insert((lookup.to_owned(), case.to_owned(), answer));
            }
            (false, false) => self.failures.push(format!(
                "{case} by {lookup}: expected {expected}, got {answer}"
            )),
            _ => {}
        }
    }
}

/// `list`: each case by name, by data and by file.
fn score_detection(db: &MimeDatabase, suite: &Path, score: &mut Score) {
    let list = fs::read_to_string(suite.join("list")).unwrap();
    for line in list.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let (Some(case), Some(expected_text)) = (fields.next(), fields.next()) else {
            panic!("bad list line {line:?}");
        };
        let flags: Vec<bool> = fields
            .next()
            .unwrap_or("")
            .chars()
            .map(|flag| flag == 'x')
            .collect();
        let xfail = |index: usize| flags.get(index).copied().unwrap_or(false);
        let expected = mime(expected_text);
        let path = suite.join(case);

        let by_name = db
            .type_for_name(case)
            .best()
            .map_or_else(|| mime("application/octet-stream"), |found| found.mime);
        let data = fs::read(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        let by_data = db
            .type_for_data(&data[..data.len().min(db.sniff_len())])
            .mime;
        let by_file = db
            .type_for_path(&path, &PathOptions::default())
            .unwrap_or_else(|err| panic!("{}: {err}", path.display()))
            .mime;
        for (index, (lookup, got)) in [("name", by_name), ("data", by_data), ("file", by_file)]
            .into_iter()
            .enumerate()
        {
            score.record(
                lookup,
                case,
                got == expected,
                xfail(index),
                got.to_string(),
                expected_text,
            );
        }
    }
}

/// `tree-list`, scored as shared-mime-info's `src/test-tree-magic.c` does: a
/// case passes when something is found and every type found is listed.
fn score_tree_magic(db: &MimeDatabase, suite: &Path, score: &mut Score) {
    let tree_list = fs::read_to_string(suite.join("tree-list")).unwrap();
    for line in tree_list.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields: Vec<&str> = line.split_whitespace().collect();
        let xfail = fields.first() == Some(&"x");
        if xfail {
            fields.remove(0);
        }
        let Some(&case) = fields.first() else {
            panic!("bad tree-list line {line:?}");
        };
        let expected: Vec<MimeType> = fields[1..].iter().map(|text| mime(text)).collect();
        // The suite's expected failure names a folder that does not exist;
        // the reference finds no types there, this crate an error.
        let got = match db.content_types_for_root(suite.join(case), &TreeOptions::default()) {
            Ok(got) => got,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => panic!("{case}: {err}"),
        };
        let passed = !got.is_empty() && got.iter().all(|found| expected.contains(found));
        let answer = got
            .iter()
            .map(MimeType::as_str)
            .collect::<Vec<_>>()
            .join(" ");
        score.record("tree", case, passed, xfail, answer, &fields[1..].join(" "));
    }
}

fn lines(differences: &[&Difference]) -> String {
    differences
        .iter()
        .map(|(lookup, case, answer)| format!("{lookup}\t{case}\t{answer}\t<reason>"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn detection_and_tree_magic_match_the_reference_suite() {
    let dir = tempfile::tempdir().unwrap();
    let db = reference_database(dir.path());
    let suite = source().join("tests/mime-detection");
    let mut score = Score::default();
    score_detection(&db, &suite, &mut score);
    score_tree_magic(&db, &suite, &mut score);

    let listed = listed_differences();
    let unlisted: Vec<_> = score.successes.difference(&listed).collect();
    let stale: Vec<_> = listed.difference(&score.successes).collect();
    assert!(
        score.failures.is_empty() && unlisted.is_empty() && stale.is_empty(),
        "{} comparisons\n\n{} failures:\n{}\n\n{} unexpected successes not in conformance-differences.txt:\n{}\n\n{} listed differences that no longer happen:\n{}",
        score.comparisons,
        score.failures.len(),
        score.failures.join("\n"),
        unlisted.len(),
        lines(&unlisted),
        stale.len(),
        lines(&stale),
    );
}
