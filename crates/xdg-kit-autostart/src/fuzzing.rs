//! Entry points for the fuzz targets in `fuzz/`. Built only with
//! `--cfg fuzzing`, which `cargo fuzz` sets; not part of the API.

use std::path::PathBuf;

use xdg_kit_basedir::FileStamp;
use xdg_kit_desktop_entry::{DesktopEntry, Desktops, KeyFile};

use crate::discovery::{Content, SourceFile};
use crate::{Autostart, Context, Ineligible, Origin};

/// `bytes` as the effective file of an entry.
fn autostart(entry: DesktopEntry) -> Autostart {
    Autostart {
        name: "fuzz.desktop".into(),
        sources: vec![SourceFile {
            path: PathBuf::from("/fuzz/fuzz.desktop"),
            origin: Origin::User,
            content: Content::Read {
                entry: Box::new(entry),
                stamp: FileStamp::of("/nonexistent/fuzz"),
            },
        }],
        uncertain: None,
    }
}

/// Parses `bytes` as an autostart file and holds it to the crate's
/// contracts: an unedited file serializes byte-identically; the verdict
/// never panics; setting `Hidden` either way is never refused, survives a
/// reparse with that value, a dropped line is reported exactly when the
/// file has one, and the reparsed verdict says hidden exactly when it was
/// set (unless the file is one the checks stop at).
pub fn entry(bytes: &[u8]) {
    let entry = DesktopEntry::from_key_file(KeyFile::parse(bytes));
    assert_eq!(
        entry.key_file().to_bytes(),
        bytes,
        "lossless while unedited"
    );
    let context = Context::new(Desktops::new(["GNOME", "KDE"]), "/nonexistent-fuzz-bin");
    let _ = autostart(entry.clone()).eligibility(&context);
    for hidden in [true, false] {
        let mut edited = entry.clone();
        crate::edit::set_hidden(&mut edited, hidden).expect("fixed, valid names");
        let reparsed = DesktopEntry::from_key_file(KeyFile::parse(&edited.key_file().to_bytes()));
        assert_eq!(reparsed.boolean("Hidden"), Some(hidden));
        let undecodable = reparsed.key_file().undecodable_lines();
        let verdict = autostart(reparsed).eligibility(&context);
        // A dropped line stops the checks, and only then is reported.
        assert_eq!(
            verdict
                .reasons()
                .iter()
                .any(|reason| matches!(reason, Ineligible::Undecodable { .. })),
            !undecodable.is_empty()
        );
        let stopped = verdict.reasons().iter().any(|reason| {
            matches!(
                reason,
                Ineligible::Undecodable { .. } | Ineligible::UnsupportedEncoding
            )
        });
        if !stopped {
            assert_eq!(
                verdict.reasons().contains(&Ineligible::Hidden),
                hidden,
                "{:?}",
                verdict.reasons()
            );
        }
    }
}
