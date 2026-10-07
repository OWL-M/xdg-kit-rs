#![no_main]

// globs2 and the other line-based files share their line handling.
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    for name in [
        "globs2",
        "globs",
        "aliases",
        "subclasses",
        "icons",
        "generic-icons",
        "XMLnamespaces",
        "types",
    ] {
        xdg_kit_mime::fuzzing::text_file(name, data);
    }
});
