//! Entry points for the fuzz targets in `fuzz/`. Built only with
//! `--cfg fuzzing`, which `cargo fuzz` sets; not part of the API.

use std::path::Path;

use crate::{Limits, ThumbnailRequest, ThumbnailSource};

/// Walks `bytes` as a PNG. Read and splice must agree on soundness, and
/// the splice output must always read back — with the spliced fields in
/// place — under limits with room for the chunks the splice added.
pub fn png(bytes: &[u8]) {
    let limits = Limits::default();
    let readable = crate::png::read(bytes, &limits);
    let keys = [("Thumb::URI", "file:///fuzz"), ("Thumb::MTime", "123")];
    let spliced = crate::png::splice(bytes, &keys, &["Thumb::URI", "Thumb::MTime"], &limits);
    assert_eq!(
        readable.is_ok(),
        spliced.is_ok(),
        "read and splice agree on soundness"
    );
    let Ok(spliced) = spliced else {
        return;
    };
    let room = Limits {
        max_chunks: limits.max_chunks.saturating_add(keys.len()),
        ..limits
    };
    let back = crate::png::read(&spliced, &room).expect("splice output reads back");
    assert_eq!(back.key("Thumb::URI"), Some("file:///fuzz"));
    assert_eq!(back.key("Thumb::MTime"), Some("123"));
}

/// Splits `data` as a thumbnailer `Exec` line the way `GLib`'s shell
/// would, and builds the command — the validation and expansion a real
/// request runs — against local and remote sources: adversarial lines
/// must refuse or expand, never panic.
pub fn command_line(data: &[u8]) {
    let Ok(line) = std::str::from_utf8(data) else {
        return;
    };
    let _ = crate::command::split(line);
    let thumbnailer = crate::thumbnailer::Thumbnailer::for_tests(line);
    let local = ThumbnailSource::local("/fuzz/in put.png").expect("absolute");
    let remote = ThumbnailSource::remote("sftp://host/x").expect("a URI");
    for source in [&local, &remote] {
        let request = ThumbnailRequest {
            source,
            output: Path::new("/fuzz/out.png"),
            size: 128,
        };
        let _ = thumbnailer.command(&request);
    }
}
