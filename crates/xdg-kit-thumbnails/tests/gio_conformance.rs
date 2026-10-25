//! The canonical `file://` URI against `GLib`'s own (from the dev
//! shell's `gio`): the thumbnail cache is keyed by the MD5 of this URI,
//! so a single byte of divergence in the escaping means a cache miss
//! against every GLib-based generator.

use std::path::Path;
use std::process::Command;

use xdg_kit_thumbnails::ThumbnailSource;

/// `gio info -a standard::* <path>` prints the file's URI as `GLib` builds
/// it (`g_file_get_uri`).
fn gio_uri(path: &Path) -> String {
    let out = Command::new("gio")
        .args(["info", "-a", "standard::name"])
        .arg(path)
        .output()
        .expect("gio runs in the dev shell");
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).expect("gio info prints UTF-8");
    text.lines()
        .find_map(|line| line.strip_prefix("uri: "))
        .unwrap_or_else(|| panic!("no uri line in {text:?}"))
        .to_owned()
}

#[test]
fn file_uris_escape_exactly_as_glib_does() {
    let dir = tempfile::tempdir().unwrap();
    // The bytes whose treatment differs between escaping profiles:
    // sub-delims (GLib keeps all but `;`… which it escapes), spaces,
    // percent, controls would not survive a file name portably — the
    // printable troublemakers cover the claim.
    for name in [
        "plain.png",
        "with space.png",
        "per%cent.png",
        "semi;colon.png",
        "quo'te\"s.png",
        "br[ack]ets.png",
        "pl+us&and=eq.png",
        "at@comma,.png",
        "caf\u{e9}-\u{4e2d}\u{6587}.png",
        "ti~lde!bang$dollar.png",
        "paren(the)ses*star.png",
        "hash#que?stion.png",
    ] {
        let path = dir.path().join(name);
        std::fs::write(&path, "").unwrap();
        let source = ThumbnailSource::local(&path).unwrap();
        assert_eq!(source.uri(), gio_uri(&path), "{name:?}");
    }
}
