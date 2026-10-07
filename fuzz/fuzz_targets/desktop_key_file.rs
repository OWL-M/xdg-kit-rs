#![no_main]

// Any bytes as a key file: it must be written back unchanged.
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    xdg_kit_desktop_entry::fuzzing::key_file(data);
});
