#![no_main]

// /proc/self/mountinfo: never panics, mount points absolute.
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    xdg_kit_trash::fuzzing::mountinfo(data);
});
