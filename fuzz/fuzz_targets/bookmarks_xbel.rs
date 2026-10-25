#![no_main]

// An XBEL document: parse, byte-identical round trip, an edit sequence,
// and the edited output loading back editable.
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    xdg_kit_bookmarks::fuzzing::xbel(data);
});
