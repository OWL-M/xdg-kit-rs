#![no_main]

// A trash info file: the Path round trip and the checked origin.
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    xdg_kit_trash::fuzzing::trash_info(data);
});
