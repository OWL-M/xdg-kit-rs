#![no_main]

use std::path::Path;

// A user-dirs.dirs file, through the public parser.
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let dirs = xdg_kit_basedir::UserDirs::from_bytes(Path::new("/home/fuzz"), data);
    for (kind, path) in dirs.iter() {
        let _ = (kind.name(), path);
    }
    for diagnostic in dirs.diagnostics() {
        let _ = (&diagnostic.source, &diagnostic.message);
    }
});
