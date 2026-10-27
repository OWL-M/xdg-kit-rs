#![no_main]

// A menu file: bounded parsing, merging, moves, and resolution over fixed
// entries, deterministic and well-shaped.
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    xdg_kit_menu::fuzzing::menu(data);
});
