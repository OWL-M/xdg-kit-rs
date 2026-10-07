#![no_main]

// The pattern, a NUL, then the name.
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let (pattern, name) = match data.iter().position(|&byte| byte == 0) {
        Some(nul) => (&data[..nul], &data[nul + 1..]),
        None => (data, &b""[..]),
    };
    if let Ok(pattern) = std::str::from_utf8(pattern) {
        xdg_kit_mime::fuzzing::fnmatch(pattern, name);
    }
});
