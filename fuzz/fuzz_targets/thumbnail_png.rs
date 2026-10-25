#![no_main]

// A PNG through the structural walk and the metadata splice: read and
// splice agree, and splice output always reads back.
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    xdg_kit_thumbnails::fuzzing::png(data);
});
