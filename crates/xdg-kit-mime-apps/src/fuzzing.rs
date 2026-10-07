//! Entry points for the fuzz targets in `fuzz/`. Built only with
//! `--cfg fuzzing`, which `cargo fuzz` sets; not part of the API.

use crate::{Group, MimeAppsList, TypePattern};

const GROUPS: [Group; 3] = [Group::Defaults, Group::Added, Group::Removed];

/// Parses `bytes` as both a plain and a desktop-specific `mimeapps.list`
/// and checks that each writes back unchanged. Edits the first types and
/// `*/*`, checking that each edit behaves as documented and refused edits
/// change nothing. Checks that a fresh parse of the edited text gives the
/// same list.
pub fn mime_apps_list(bytes: &[u8]) {
    for desktop_specific in [false, true] {
        let list = MimeAppsList::parse(bytes, desktop_specific);
        assert_eq!(list.to_bytes(), bytes, "a list must round-trip");
        let mut patterns: Vec<TypePattern> = Vec::new();
        for group in GROUPS {
            for (pattern, _) in list.entries(group) {
                let _ = list.ids(group, &pattern);
                if !patterns.contains(&pattern) {
                    patterns.push(pattern);
                }
            }
        }
        patterns.truncate(3);
        patterns.push(TypePattern::Any);
        let mut edited = list.clone();
        for pattern in &patterns {
            edit(&mut edited, pattern, desktop_specific);
        }
        assert!(
            edited.forget(&patterns[0]) || edited.ids(Group::Defaults, &patterns[0]).is_empty()
        );
        for group in GROUPS {
            assert!(edited.ids(group, &patterns[0]).is_empty(), "forgotten");
        }
        let reread = MimeAppsList::parse(&edited.to_bytes(), desktop_specific);
        for group in GROUPS {
            assert_eq!(reread.entries(group), edited.entries(group), "{group:?}");
        }
        assert_eq!(reread.diagnostics(), edited.diagnostics());
    }
}

fn edit(list: &mut MimeAppsList, pattern: &TypePattern, desktop_specific: bool) {
    let listed = |list: &MimeAppsList, group, id: &str| {
        list.ids(group, pattern).iter().any(|known| known == id)
    };
    let before = list.to_bytes();
    assert!(list.set_default(pattern, "no id").is_err());
    assert_eq!(list.to_bytes(), before, "a refused edit changes nothing");
    list.set_default(pattern, "fuzz.desktop")
        .expect("a valid ID");
    assert_eq!(
        list.ids(Group::Defaults, pattern)
            .first()
            .map(String::as_str),
        Some("fuzz.desktop")
    );
    if desktop_specific {
        let before = list.to_bytes();
        assert!(list.add_association(pattern, "other.desktop").is_err());
        assert_eq!(list.to_bytes(), before, "a refused edit changes nothing");
        return;
    }
    assert_eq!(
        list.ids(Group::Added, pattern).first().map(String::as_str),
        Some("fuzz.desktop")
    );
    assert!(!listed(list, Group::Removed, "fuzz.desktop"));
    list.add_association(pattern, "other.desktop")
        .expect("a valid ID");
    assert!(listed(list, Group::Added, "other.desktop"));
    assert!(!listed(list, Group::Removed, "other.desktop"));
    list.remove_association(pattern, "fuzz.desktop")
        .expect("a valid ID");
    assert!(!listed(list, Group::Added, "fuzz.desktop"));
    assert!(!listed(list, Group::Defaults, "fuzz.desktop"));
    assert!(listed(list, Group::Removed, "fuzz.desktop"));
}
