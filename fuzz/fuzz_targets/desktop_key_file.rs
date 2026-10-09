#![no_main]

// Any bytes as a key file: it must be written back unchanged, and after
// edits and removals of its keys and groups it must answer as a fresh parse
// of its text does.
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    xdg_kit_desktop_entry::fuzzing::key_file(data);
});
