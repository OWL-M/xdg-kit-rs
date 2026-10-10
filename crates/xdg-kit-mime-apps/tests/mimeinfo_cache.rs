//! `mimeinfo_cache` against `update-desktop-database` (desktop-file-utils at
//! the pinned commit, from the dev shell), on the entries below and a copy
//! of the desktop entry crate's validation corpus, each once at the top and
//! once in a sub-folder.
//!
//! Each type's line must be the same in both, and the files byte for byte
//! once the listed lines are taken out. Where a line differs on purpose,
//! the type is listed with our line's value and a reason in
//! `mimeinfo-differences.txt`: every listed type must still differ, and our
//! value must be the listed one.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use common::update_desktop_database;
use xdg_kit_mime_apps::mimeinfo_cache;

/// The cache's lines by type (the text before `=`), without the header.
fn lines(cache: &[u8]) -> BTreeMap<String, String> {
    let text = String::from_utf8_lossy(cache);
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("[MIME Cache]"));
    lines
        .map(|line| {
            let (mime, ids) = line.split_once('=').expect("type=ids");
            (mime.to_owned(), ids.to_owned())
        })
        .collect()
}

/// The listed differences: type → (our line's value, or `-` for none;
/// reason).
fn listed() -> BTreeMap<String, (String, String)> {
    common::read_differences("mimeinfo-differences.txt", 3)
        .into_iter()
        .map(|fields| {
            let [mime, ours, reason] = <[String; 3]>::try_from(fields).unwrap();
            (mime, (ours, reason))
        })
        .collect()
}

/// `cache` without the lines of the `listed` types.
fn without(cache: &[u8], listed: &BTreeMap<String, (String, String)>) -> Vec<u8> {
    let mut kept = Vec::new();
    for line in cache.split_inclusive(|&byte| byte == b'\n') {
        let mime = line.split(|&byte| byte == b'=').next().unwrap_or_default();
        if !listed.contains_key(String::from_utf8_lossy(mime).as_ref()) {
            kept.extend_from_slice(line);
        }
    }
    kept
}

/// Entries for the cache's rules: (file name, `MimeType=` line and other
/// lines). Each gets `[Desktop Entry]`, `Type`, `Name` and `Exec` first.
const ENTRIES: &[(&str, &str)] = &[
    ("plain.desktop", "MimeType=text/plain;image/png;\n"),
    ("Upper.desktop", "MimeType=Text/Plain;IMAGE/PNG;\n"),
    ("wildcards.desktop", "MimeType=image/*;*/*;\n"),
    ("spaces.desktop", "MimeType=text/html ; text/css;\n"),
    ("empty-items.desktop", "MimeType=;text/x-c;;\n"),
    ("twice.desktop", "MimeType=text/plain;text/plain;\n"),
    (
        "escaped.desktop",
        "MimeType=text/a\\;b;application/x-c\\sd;\n",
    ),
    (
        "bad-types.desktop",
        "MimeType=nosubtype;text/;text/a b;text/a,b;/x;\n",
    ),
    (
        "odd-media.desktop",
        "MimeType=foo/bar;x-foo/bar;chemical/x-pdb;inode/directory;\n",
    ),
    (
        "schemes.desktop",
        "MimeType=x-scheme-handler/https;x-scheme-handler/x+y.z;\n",
    ),
    (
        "symbols.desktop",
        "MimeType=application/vnd.a+b;application/x-a_b;application/a!b#c$d&e^f;\n",
    ),
    (
        "discouraged.desktop",
        "MimeType=application/x-zip;text/x-c++;\n",
    ),
    ("hidden.desktop", "MimeType=text/plain;\nHidden=true\n"),
    ("hidden-one.desktop", "MimeType=text/plain;\nHidden=1\n"),
    ("not-hidden.desktop", "MimeType=text/plain;\nHidden=false\n"),
    ("no-types.desktop", ""),
    ("localized.desktop", "MimeType[de]=text/de;\n"),
    (
        "hidden-space.desktop",
        "MimeType=text/plain;\nHidden=true \n",
    ),
    (
        "legacy.desktop",
        "MimeType=text/legacy;\nEncoding=Legacy-Mixed\n",
    ),
    (
        "utf8-lower.desktop",
        "MimeType=text/utf8-lower;\nEncoding=utf-8\n",
    ),
    (
        "utf8-space.desktop",
        "MimeType=text/utf8-space;\nEncoding=UTF-8 \n",
    ),
    (
        "encoding-twice.desktop",
        "MimeType=text/encoding-twice;\nEncoding=Legacy-Mixed\nEncoding=UTF-8\n",
    ),
    ("notes.txt", "MimeType=text/not-a-desktop-file;\n"),
];

fn write_entries(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    for (name, rest) in ENTRIES {
        fs::write(
            dir.join(name),
            format!("[Desktop Entry]\nType=Application\nName=N\nExec=x\n{rest}"),
        )
        .unwrap();
    }
    // Files GLib's key file parser refuses, and other headers.
    let raw: &[(&str, &[u8])] = &[
        (
            "kde.desktop",
            b"[KDE Desktop Entry]\nType=Application\nName=K\nExec=k\nMimeType=text/kde;\n",
        ),
        (
            "garbage.desktop",
            b"[Desktop Entry]\nType=Application\nName=G\ngarbage\nMimeType=text/garbage;\n",
        ),
        (
            "before-group.desktop",
            b"MimeType=text/early;\n[Desktop Entry]\nName=B\nMimeType=text/before;\n",
        ),
        (
            "latin1.desktop",
            b"[Desktop Entry]\nName=caf\xe9\nMimeType=text/latin1;\n",
        ),
        (
            "second-group.desktop",
            b"[Desktop Entry]\nName=S\n[Other]\nMimeType=text/other-group;\n",
        ),
        (
            "repeated.desktop",
            b"[Desktop Entry]\nMimeType=text/first;\nMimeType=text/last;\n",
        ),
    ];
    for (name, bytes) in raw {
        fs::write(dir.join(name), bytes).unwrap();
    }
}

#[test]
fn caches_match_update_desktop_database() {
    let corpus =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../xdg-kit-desktop-entry/tests/corpus");
    let dir = tempfile::tempdir().unwrap();
    let applications = dir.path().join("applications");
    write_entries(&applications);
    write_entries(&applications.join("nested/deeper"));
    // A folder reached under two names.
    fs::create_dir_all(applications.join("zlinked")).unwrap();
    fs::write(
        applications.join("zlinked/x.desktop"),
        "[Desktop Entry]\nType=Application\nName=X\nExec=x\nMimeType=text/linked;\n",
    )
    .unwrap();
    std::os::unix::fs::symlink("zlinked", applications.join("alinked")).unwrap();
    // One file under two paths of one ID: `zsame-y` is `zsame/y`.
    fs::create_dir_all(applications.join("zsame/y")).unwrap();
    fs::write(
        applications.join("zsame/y/s.desktop"),
        "[Desktop Entry]\nType=Application\nName=S\nExec=s\nMimeType=text/same;\n",
    )
    .unwrap();
    std::os::unix::fs::symlink("zsame/y", applications.join("zsame-y")).unwrap();
    // One ID, `rep-a-b-y`, under three paths in three folders, each with
    // another entry of the type: whatever the listing order, another ID
    // comes between two of its paths, and the tool lists it again.
    let repeat = "[Desktop Entry]\nType=Application\nName=R\nExec=r\nMimeType=text/repeat;\n";
    for (folder, other) in [("rep", "p"), ("rep-a", "q"), ("rep-a-b", "r")] {
        fs::create_dir_all(applications.join(folder)).unwrap();
        fs::write(
            applications.join(format!("{folder}/{other}.desktop")),
            repeat,
        )
        .unwrap();
    }
    fs::write(applications.join("rep-a-b/y.desktop"), repeat).unwrap();
    for link in ["rep/a-b-y.desktop", "rep-a/b-y.desktop"] {
        std::os::unix::fs::symlink("../rep-a-b/y.desktop", applications.join(link)).unwrap();
    }
    for target in [applications.join("corpus"), applications.join("sub")] {
        fs::create_dir_all(&target).unwrap();
        for entry in fs::read_dir(&corpus).unwrap() {
            let entry = entry.unwrap();
            fs::copy(entry.path(), target.join(entry.file_name())).unwrap();
        }
    }
    let ours_bytes = mimeinfo_cache(&applications).unwrap();
    let ours = lines(&ours_bytes);
    update_desktop_database(&applications);
    let tool_bytes = fs::read(applications.join("mimeinfo.cache")).unwrap();
    let reference = lines(&tool_bytes);
    assert!(
        reference.len() > 10,
        "the entries give few types: {reference:?}"
    );
    let listed_all = listed();
    let mut listed = listed_all.clone();
    let mut unexpected = Vec::new();
    let mut stale = Vec::new();
    let types: std::collections::BTreeSet<&String> = ours.keys().chain(reference.keys()).collect();
    for mime in types {
        let (a, b) = (ours.get(mime), reference.get(mime));
        let ours_value = a.map_or("-", String::as_str);
        match (listed.remove(mime), a == b) {
            (None, false) => {
                unexpected.push(format!("{mime}\t{ours_value}\t<reason>\n  tool {b:?}"));
            }
            (Some(_), true) => stale.push(format!("{mime}\tnow the same")),
            (Some((expected, _)), false) if expected != ours_value => {
                stale.push(format!("{mime}\tlisted {expected}, ours now {ours_value}"));
            }
            _ => {}
        }
    }
    for mime in listed.keys() {
        stale.push(format!("{mime}\tin neither cache"));
    }
    // Line by line first, for a readable report; then the whole files, for
    // line order and the last byte.
    if unexpected.is_empty() && stale.is_empty() {
        assert_eq!(
            String::from_utf8_lossy(&without(&ours_bytes, &listed_all)),
            String::from_utf8_lossy(&without(&tool_bytes, &listed_all)),
            "the files differ outside the listed lines"
        );
    }
    assert!(
        unexpected.is_empty() && stale.is_empty(),
        "{} lines differ and are not listed:\n{}\n\n{} listed differences no longer hold:\n{}",
        unexpected.len(),
        unexpected.join("\n"),
        stale.len(),
        stale.join("\n"),
    );
}
