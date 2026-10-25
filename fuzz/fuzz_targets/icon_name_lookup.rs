#![no_main]

// An icon name through the lookups, the fallback-generating one
// included: adversarial hyphenation must terminate.
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    xdg_kit_icon_theme::fuzzing::lookup(data);
});
