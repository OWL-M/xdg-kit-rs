#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    xdg_kit_mime::fuzzing::text_file("treemagic", data);
});
