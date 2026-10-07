//! Entry points for the fuzz targets in `fuzz/`. Built only with
//! `--cfg fuzzing`, which `cargo fuzz` sets; not part of the API.

use std::ffi::OsString;

use xdg_kit_locale::Locales;

use crate::exec::{CommandLine, Values};
use crate::{DesktopEntry, KeyFile, LaunchOptions, Target, prepare_launch, validate};

/// Parses `bytes` as a key file, checks that writing preserves the bytes,
/// and looks up every key. Then edits the file and compares it with a fresh
/// parse of the edited text. Each lookup scans its key's lines, so the work
/// is quadratic in the number of translations for one key. The worst
/// 4 KiB input takes about 0.4 s under the address sanitizer, within
/// `-timeout=5` at the default `-max_len`. Raising `-max_len` substantially
/// can exceed that timeout.
pub fn key_file(bytes: &[u8]) {
    let file = KeyFile::parse(bytes);
    assert_eq!(file.to_bytes(), bytes, "a key file must round-trip");
    let locales = Locales::new(["de_AT@euro", "sr@latin"]);
    for group in file.groups() {
        for (key, locale) in file.keys(group) {
            let _ = file.raw(group, key, locale);
            let _ = file.locale_string(group, key, &locales);
            let _ = file.list(group, key);
        }
    }
    let mut edited = file.clone();
    edited
        .set_string("Desktop Entry", "Name", " fuzz;\\\n")
        .unwrap();
    edited.set_list("X-Fuzz", "List", &["a;b", "c\\"]).unwrap();
    let reread = KeyFile::parse(&edited.to_bytes());
    assert_eq!(
        reread.list("X-Fuzz", "List"),
        Some(vec!["a;b".to_owned(), "c\\".to_owned()])
    );
    assert_eq!(
        reread.string("Desktop Entry", "Name").as_deref(),
        Some(" fuzz;\\\n")
    );
    // The edits update the index in place; it must match a fresh read.
    assert_eq!(reread.groups(), edited.groups());
    for group in reread.groups() {
        assert_eq!(reread.keys(group), edited.keys(group), "[{group}]");
        for (key, locale) in reread.keys(group) {
            assert_eq!(
                reread.line_of(group, key, locale),
                edited.line_of(group, key, locale)
            );
        }
    }
    assert_eq!(reread.diagnostics(), edited.diagnostics());
}

/// Parses `text` as an `Exec` value and expands it, both as given and with
/// the key file's escapes undone (as a launch reads it; the seeds are the
/// text written after `Exec=`).
pub fn exec(text: &str) {
    exec_value(text);
    exec_value(&crate::key_file::unescape(text));
}

fn exec_value(text: &str) {
    if let Ok(line) = CommandLine::parse(text) {
        let files = [OsString::from("/a b"), OsString::from("file:///c")];
        let _ = line.file_code();
        let _ = line.expand(&Values {
            files: &files,
            icon: Some("icon"),
            name: "Name",
            location: "/x.desktop".as_ref(),
        });
    }
}

/// Validates `bytes` as a desktop entry and plans launches with and without
/// a terminal, as well as launches of its first actions.
pub fn entry(bytes: &[u8]) {
    let file = KeyFile::parse(bytes);
    for issue in validate(&file) {
        let _ = issue.to_string();
    }
    let entry = DesktopEntry::from_key_file(file);
    let locales = Locales::new(["fr_CA"]);
    let _ = (
        entry.name(&locales),
        entry.keywords(&locales),
        entry.actions().len(),
    );
    let targets = [
        Target::Path("/tmp/a".into()),
        Target::Uri("file:///b%20c".into()),
    ];
    let mut options = LaunchOptions::default();
    let _ = prepare_launch(&entry, &targets, &options);
    options.terminal = Some(vec!["term".into(), "--".into()]);
    let _ = prepare_launch(&entry, &targets, &options);
    for id in entry.action_ids().into_iter().take(4) {
        options.action = Some(id);
        let _ = prepare_launch(&entry, &targets, &options);
    }
}
