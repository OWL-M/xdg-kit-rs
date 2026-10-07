#![no_main]

// A desktop entry, validated and planned for launching.
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    xdg_kit_desktop_entry::fuzzing::entry(data);
});
