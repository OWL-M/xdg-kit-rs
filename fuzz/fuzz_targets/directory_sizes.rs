#![no_main]

// A directorysizes cache: what is kept writes out and reads back the same.
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    xdg_kit_trash::fuzzing::directory_sizes(data);
});
