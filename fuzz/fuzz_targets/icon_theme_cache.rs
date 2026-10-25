#![no_main]

// A GTK icon-theme.cache: hostile bytes are a refusal or a bounded
// parse, never a panic, a hang or an oversized allocation.
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    xdg_kit_icon_theme::fuzzing::gtk_cache(data);
});
