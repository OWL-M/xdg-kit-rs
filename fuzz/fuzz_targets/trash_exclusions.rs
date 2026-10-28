#![no_main]

// Exclusion rules: a lexical check that never panics and ignores repeated
// separators and `.`.
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    xdg_kit_trash::fuzzing::exclusions(data);
});
