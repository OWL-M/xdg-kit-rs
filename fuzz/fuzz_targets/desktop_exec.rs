#![no_main]

// An `Exec` value, parsed and expanded.
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        xdg_kit_desktop_entry::fuzzing::exec(text);
    }
});
