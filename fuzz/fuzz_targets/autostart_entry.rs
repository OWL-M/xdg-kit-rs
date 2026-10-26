#![no_main]

// An autostart file: lossless round trip, the verdict, and the Hidden
// override edit surviving a reparse.
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    xdg_kit_autostart::fuzzing::entry(data);
});
