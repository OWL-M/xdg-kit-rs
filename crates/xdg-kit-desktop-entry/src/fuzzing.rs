//! Entry points for the fuzz targets in `fuzz/`. Built only with
//! `--cfg fuzzing`, which `cargo fuzz` sets; not part of the API.

use std::ffi::OsString;

use xdg_kit_locale::Locales;

use crate::exec::{CommandLine, Values};
use crate::{DesktopEntry, KeyFile, LaunchOptions, Target, prepare_launch, validate};

/// Parses `bytes` as a key file, checks that writing preserves the bytes,
/// and looks up every key. Then edits the file and compares it with a fresh
/// parse of the edited text; then removes keys and groups of the file and
/// adds keys, picked by the first bytes of the input, comparing it with a
/// fresh parse after each step. Each lookup finds its line by locale
/// without looking through the key's other translations.
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
    assert_like_reparse(&edited);
    // Removals and additions mixed, each followed by every query, so that
    // a stale index, count of lines or cache shows.
    let groups: Vec<String> = edited.groups().into_iter().map(str::to_owned).collect();
    let keys: Vec<(String, String, Option<String>)> = groups
        .iter()
        .flat_map(|group| {
            edited
                .keys(group)
                .into_iter()
                .map(|(key, locale)| (group.clone(), key.to_owned(), locale.map(str::to_owned)))
        })
        .collect();
    for (step, &byte) in bytes.iter().take(32).enumerate() {
        let pick = usize::from(byte / 4) + step;
        match byte % 4 {
            0 | 1 if !keys.is_empty() => {
                let (group, key, locale) = &keys[pick % keys.len()];
                edited.remove(group, key, locale.as_deref());
            }
            2 if !groups.is_empty() => {
                edited.remove_group(&groups[pick % groups.len()]);
            }
            _ => {
                let group = groups
                    .get(pick % groups.len().max(1))
                    .map_or("X-Fuzz", String::as_str);
                // Refused for a group name the file read but cannot write.
                let _ = edited.set_string(group, &format!("Added{}", pick % 3), "v");
            }
        }
        assert_like_reparse(&edited);
    }
}

/// Checks that `file` answers every query as a fresh parse of its text does.
fn assert_like_reparse(file: &KeyFile) {
    let fresh = KeyFile::parse_with(&file.to_bytes(), file.key_names());
    assert_eq!(&fresh, file);
    assert_eq!(fresh.groups(), file.groups());
    for group in fresh.groups() {
        assert_eq!(fresh.group_line(group), file.group_line(group), "[{group}]");
        assert_eq!(fresh.keys(group), file.keys(group), "[{group}]");
        for (key, locale) in fresh.keys(group) {
            assert_eq!(fresh.raw(group, key, locale), file.raw(group, key, locale));
            assert_eq!(
                fresh.line_of(group, key, locale),
                file.line_of(group, key, locale)
            );
        }
    }
    assert_eq!(fresh.diagnostics(), file.diagnostics());
}

/// Parses `text` as an `Exec` value and expands it, both as given and with
/// the key file's escapes undone (as a launch reads it; the seeds are the
/// text written after `Exec=`). Tests every combination of present and
/// absent files, icon, name and location, including the empty expansion
/// of every code that can vanish. Checks that preparing non-file expansions,
/// as a launch does, gives the same result as expanding each argument in full.
pub fn exec(text: &str) {
    exec_value(text);
    exec_value(&crate::key_file::unescape(text));
}

fn exec_value(text: &str) {
    if let Ok(line) = CommandLine::parse(text) {
        let files = [OsString::from("/a b"), OsString::from("file:///c")];
        let _ = line.file_code();
        // One bit per value: set, it is absent (no files, no icon, no name,
        // an empty location).
        for absent in 0..16_u8 {
            let files: &[OsString] = if absent & 1 == 0 { &files } else { &[] };
            let values = Values {
                icon: (absent & 2 == 0).then_some("icon"),
                name: (absent & 4 == 0).then_some("Name"),
                location: if absent & 8 == 0 { "/x.desktop" } else { "" }.as_ref(),
            };
            assert_eq!(
                line.prepare(values).expand(files),
                line.expand_each_argument(&values, files),
                "expanding everything but the file codes ahead changes the command"
            );
        }
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
