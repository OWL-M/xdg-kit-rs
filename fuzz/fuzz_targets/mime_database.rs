#![no_main]

// A whole database, built from fuzzed text files, and a lookup in it.
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    xdg_kit_mime::fuzzing::database(data);
});
