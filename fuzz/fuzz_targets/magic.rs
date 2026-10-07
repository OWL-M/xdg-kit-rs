#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    xdg_kit_mime::fuzzing::magic(data);
});
