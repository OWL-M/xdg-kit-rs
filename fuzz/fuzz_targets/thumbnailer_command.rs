#![no_main]

// A thumbnailer Exec line through the GLib-style splitter and the field
// code expansion.
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    xdg_kit_thumbnails::fuzzing::command_line(data);
});
