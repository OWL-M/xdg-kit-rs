//! What a `mimeapps.list` key or a `MimeType=` item names: one type, or a
//! wildcard over several.

use std::fmt;

use xdg_kit_mime::{MimeDatabase, MimeType};

/// The first half of a MIME type, such as `image`. Accepts an RFC 6838
/// restricted name, just like [`MimeType`], and stores it in lower case
/// because media types are case-insensitive.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MediaType(String);

impl MediaType {
    /// Parses `text` and stores it in lower case. Returns `None` unless
    /// it is an RFC 6838 restricted name, using the same rules as
    /// [`MimeType::new`]: 1 to 127 characters, starting with a letter or
    /// digit, followed by letters, digits or `!#$&^_.+-`.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        // A media type is valid when it makes a valid type.
        MimeType::new(&format!("{text}/x")).ok()?;
        Some(Self(text.to_ascii_lowercase()))
    }

    /// The media type, in lower case.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for MediaType {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MediaType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One MIME type, a media wildcard (`image/*`), or the wildcard `*/*`.
///
/// The spec names only types; `media/*` and `*/*` occur in desktop entries
/// and `mimeapps.list` files and count here as the least specific matches.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TypePattern {
    /// One type, such as `image/png` or `x-scheme-handler/https`.
    Exact(MimeType),
    /// Every type of one media type: `image/*`.
    Media(MediaType),
    /// Every type: `*/*`.
    Any,
}

impl TypePattern {
    /// Parses `text`; `None` unless it is a type `MimeType` accepts (RFC
    /// 6838 restricted names), `media/*` with such a media type, or `*/*`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        if text == "*/*" {
            return Some(Self::Any);
        }
        if let Some(media) = text.strip_suffix("/*") {
            return MediaType::new(media).map(Self::Media);
        }
        MimeType::new(text).ok().map(Self::Exact)
    }

    /// The same pattern with an exact type replaced by the type it is an
    /// alias of, so that patterns compare as the database sees them.
    #[must_use]
    pub fn canonical(&self, database: &MimeDatabase) -> Self {
        match self {
            Self::Exact(mime) => Self::Exact(database.canonical(mime)),
            Self::Media(_) | Self::Any => self.clone(),
        }
    }
}

impl fmt::Display for TypePattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(mime) => write!(f, "{mime}"),
            Self::Media(media) => write!(f, "{media}/*"),
            Self::Any => write!(f, "*/*"),
        }
    }
}

impl From<MimeType> for TypePattern {
    fn from(mime: MimeType) -> Self {
        Self::Exact(mime)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_and_wildcards() {
        assert_eq!(
            TypePattern::parse("image/png"),
            Some(TypePattern::Exact(MimeType::new("image/png").unwrap()))
        );
        assert_eq!(
            TypePattern::parse("Image/*"),
            Some(TypePattern::Media(MediaType::new("image").unwrap()))
        );
        assert_eq!(TypePattern::parse("*/*"), Some(TypePattern::Any));
        assert!(TypePattern::parse("x-scheme-handler/https").is_some());
        for bad in [
            "",
            "image",
            "image/",
            "*/png",
            "*",
            "image/*/x",
            "bad/name/*",
            "/*",
            "a b/c",
            "text/plain;charset=x",
        ] {
            assert_eq!(TypePattern::parse(bad), None, "{bad:?}");
        }
        for text in ["image/png", "image/*", "*/*"] {
            assert_eq!(TypePattern::parse(text).unwrap().to_string(), text);
        }
    }

    #[test]
    fn media_types_are_checked_and_lowercased() {
        let image = MediaType::new("image").unwrap();
        assert_eq!(image.as_str(), "image");
        assert_eq!(image.as_ref(), "image");
        assert_eq!(MediaType::new("IMAGE"), Some(image));
        assert_eq!(
            MediaType::new("X-Scheme-Handler").unwrap().to_string(),
            "x-scheme-handler"
        );
        for bad in ["", "*", "bad/name", "a b", ".", "image/*"] {
            assert_eq!(MediaType::new(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn exact_types_compare_case_insensitively() {
        assert_eq!(
            TypePattern::parse("Text/Plain"),
            TypePattern::parse("text/plain")
        );
    }
}
