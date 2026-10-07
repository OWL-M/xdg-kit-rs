//! What a `mimeapps.list` key or a `MimeType=` item names: one type, or a
//! wildcard over several.

use std::fmt;

use xdg_kit_mime::{MimeDatabase, MimeType};

/// One MIME type, a media wildcard (`image/*`), or the wildcard `*/*`.
///
/// The spec names only types; `media/*` and `*/*` occur in desktop entries
/// and `mimeapps.list` files and count here as the least specific matches.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TypePattern {
    /// One type, such as `image/png` or `x-scheme-handler/https`.
    Exact(MimeType),
    /// Every type of one media type: `image/*`. The media type is in lower
    /// case ([`TypePattern::parse`] and [`TypePattern::canonical`] make it
    /// so).
    Media(String),
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
            // A media type is valid when it makes a valid type.
            MimeType::new(&format!("{media}/x")).ok()?;
            return Some(Self::Media(media.to_ascii_lowercase()));
        }
        MimeType::new(text).ok().map(Self::Exact)
    }

    /// The same pattern with an exact type replaced by the type it is an
    /// alias of (and a media type lowercased), so that patterns compare as
    /// the database sees them.
    #[must_use]
    pub fn canonical(&self, database: &MimeDatabase) -> Self {
        match self {
            Self::Exact(mime) => Self::Exact(database.canonical(mime)),
            Self::Media(media) => Self::Media(media.to_ascii_lowercase()),
            Self::Any => Self::Any,
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
            Some(TypePattern::Media("image".into()))
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
    fn exact_types_compare_case_insensitively() {
        assert_eq!(
            TypePattern::parse("Text/Plain"),
            TypePattern::parse("text/plain")
        );
    }
}
