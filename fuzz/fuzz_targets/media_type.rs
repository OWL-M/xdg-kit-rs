#![no_main]

use xdg_kit_mime::MimeType;

// A `Content-Type` value.
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let parsed = MimeType::from_media_type(text);
    if MimeType::new(text).is_ok() {
        assert!(parsed.is_ok(), "a type is a media type");
    }
    if let Ok(mime) = parsed {
        // The text before the first `;`, without the spaces and tabs around it.
        let essence = text
            .split(';')
            .next()
            .unwrap_or_default()
            .trim_matches([' ', '\t']);
        assert_eq!(mime.as_str(), essence);
        assert_eq!(MimeType::new(essence).as_ref(), Ok(&mime));
    }
});
