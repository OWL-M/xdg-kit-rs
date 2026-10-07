//! Compares `validate` with `desktop-file-validate` on the corpus in
//! `tests/corpus/`, written for this repository. The dev shell builds
//! desktop-file-utils from the pinned flake input.
//!
//! Both must give each file the same verdict: valid (no error) or invalid.
//! Intentional differences, where this crate follows the specs and the
//! tool does not, are listed with reasons in `validate-differences.txt`.
//! Every listed file must still produce different verdicts.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use xdg_kit_desktop_entry::{KeyFile, Level, validate};

fn corpus() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("{}: {err}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .collect();
    files.sort();
    files
}

fn verdict(valid: bool) -> &'static str {
    if valid { "valid" } else { "invalid" }
}

/// Whether the tool finds `path` valid. It exits with 1 for an invalid
/// file; anything else (a crash, a usage error) is not a verdict.
fn reference(path: &Path) -> bool {
    let output = Command::new("desktop-file-validate")
        .arg(path)
        .output()
        .expect(
            "desktop-file-validate is not on PATH: run the tests in the dev shell (nix develop)",
        );
    match output.status.code() {
        Some(0) => true,
        Some(1) => false,
        _ => panic!(
            "desktop-file-validate {}: {}\n{}{}",
            path.display(),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    }
}

fn ours(path: &Path) -> bool {
    let file = KeyFile::read(path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
    !validate(&file)
        .iter()
        .any(|issue| issue.level == Level::Error)
}

/// The listed differences: file name → (ours, reference), true for valid.
fn listed() -> BTreeMap<String, (bool, bool)> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/validate-differences.txt");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("{}: cannot be read: {err}", path.display()));
    let parse = |field: &str, line: &str| match field {
        "valid" => true,
        "invalid" => false,
        _ => panic!(
            "{}: a verdict is valid or invalid, not {field:?}: {line:?}",
            path.display()
        ),
    };
    let mut listed = BTreeMap::new();
    for line in text
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
    {
        let fields: Vec<&str> = line.split('\t').collect();
        assert!(
            fields.len() == 4 && fields[0] == fields[0].trim() && !fields[3].trim().is_empty(),
            "{}: want file, ours, reference and a reason, tab-separated: {line:?}",
            path.display()
        );
        let previous = listed.insert(
            fields[0].to_owned(),
            (parse(fields[1], line), parse(fields[2], line)),
        );
        assert!(
            previous.is_none(),
            "{}: listed twice: {line:?}",
            path.display()
        );
    }
    listed
}

#[test]
fn verdicts_match_desktop_file_validate() {
    let listed = listed();
    let mut unexpected = Vec::new();
    let mut stale = Vec::new();
    let files = corpus();
    assert!(
        files.len() > 50,
        "the corpus is missing: {} files",
        files.len()
    );
    let mut names = BTreeSet::new();
    for path in &files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let (ours, reference) = (ours(path), reference(path));
        match listed.get(&name) {
            Some(&listed) if listed == (ours, reference) && ours != reference => {}
            Some(&(listed_ours, listed_reference)) => stale.push(format!(
                "{name}\tlisted: ours {}, reference {}; now: ours {}, reference {}",
                verdict(listed_ours),
                verdict(listed_reference),
                verdict(ours),
                verdict(reference)
            )),
            None if ours != reference => unexpected.push(format!(
                "{name}\t{}\t{}\t<reason>",
                verdict(ours),
                verdict(reference)
            )),
            None => {}
        }
        names.insert(name);
    }
    for name in listed.keys().filter(|name| !names.contains(*name)) {
        stale.push(format!("{name}\tnot in the corpus"));
    }
    assert!(
        unexpected.is_empty() && stale.is_empty(),
        "{} files\n\n{} verdicts differ and are not listed:\n{}\n\n{} listed differences no longer hold:\n{}",
        files.len(),
        unexpected.len(),
        unexpected.join("\n"),
        stale.len(),
        stale.join("\n"),
    );
}
