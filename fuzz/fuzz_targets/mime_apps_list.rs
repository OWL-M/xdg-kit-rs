#![no_main]

// Any bytes as a mimeapps.list: written back unchanged, and edits read back
// like a fresh parse.
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    xdg_kit_mime_apps::fuzzing::mime_apps_list(data);
});
