use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::str::FromStr;
use std::sync::Arc;

/// A MIME type such as `image/png`: a media type and a subtype, without
/// parameters or wildcards.
///
/// The spelling it was created with is kept, and is what [`MimeType::as_str`]
/// and `Display` give. Comparison, hashing and ordering ignore ASCII case,
/// as MIME types are case-insensitive (RFC 2045, section 5.1): two values
/// that compare equal can print differently. Cloning is cheap.
#[derive(Clone)]
pub struct MimeType(Arc<str>);

/// A string that is not a MIME type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidMimeType(String);

impl InvalidMimeType {
    /// The text that was refused.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for InvalidMimeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "not a MIME type: \"{}\"", crate::error::quoted(&self.0))
    }
}

/// Whether `name` is an RFC 6838 restricted name (section 4.2): 1 to 127
/// characters, the first a letter or digit, the rest letters, digits or
/// `!#$&^_.+-`.
fn restricted_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && name.len() <= 127
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&byte))
}

impl std::error::Error for InvalidMimeType {}

impl MimeType {
    /// Parses `media/subtype`.
    ///
    /// # Errors
    ///
    /// [`InvalidMimeType`] unless both parts are RFC 6838 restricted names:
    /// 1 to 127 characters each, starting with a letter or digit, then
    /// letters, digits and `!#$&^_.+-`. So no parameters (`;`), no
    /// wildcards (`*`), and no `.` or `..` as a part.
    pub fn new(text: &str) -> Result<Self, InvalidMimeType> {
        let valid = text
            .split_once('/')
            .is_some_and(|(media, subtype)| restricted_name(media) && restricted_name(subtype));
        if valid {
            Ok(Self(text.into()))
        } else {
            Err(InvalidMimeType(text.to_owned()))
        }
    }

    /// The type as spelled when it was created.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The part before the `/`, as in `image`.
    #[must_use]
    pub fn media(&self) -> &str {
        self.0.split_once('/').map_or("", |(media, _)| media)
    }

    /// The part after the `/`, as in `png`.
    #[must_use]
    pub fn subtype(&self) -> &str {
        self.0.split_once('/').map_or("", |(_, subtype)| subtype)
    }

    /// Whether the media part is `media`, ignoring ASCII case.
    #[must_use]
    pub fn is_media(&self, media: &str) -> bool {
        self.media().eq_ignore_ascii_case(media)
    }

    /// A type known to be valid, for constants inside this crate.
    pub(crate) fn known(text: &'static str) -> Self {
        Self(text.into())
    }
}

impl PartialEq for MimeType {
    fn eq(&self, other: &Self) -> bool {
        self.0.eq_ignore_ascii_case(&other.0)
    }
}

impl Eq for MimeType {}

impl Hash for MimeType {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Lowercased in chunks: one hasher call per chunk, not per byte.
        let mut chunk = [0u8; 64];
        for part in self.0.as_bytes().chunks(chunk.len()) {
            let lower = &mut chunk[..part.len()];
            lower.copy_from_slice(part);
            lower.make_ascii_lowercase();
            state.write(lower);
        }
        state.write_u8(0xff);
    }
}

impl Ord for MimeType {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .bytes()
            .map(|byte| byte.to_ascii_lowercase())
            .cmp(other.0.bytes().map(|byte| byte.to_ascii_lowercase()))
    }
}

impl PartialOrd for MimeType {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for MimeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for MimeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, f)
    }
}

impl FromStr for MimeType {
    type Err = InvalidMimeType;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::new(text)
    }
}

impl AsRef<str> for MimeType {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(feature = "mime")]
impl From<&MimeType> for mime::Mime {
    fn from(mime_type: &MimeType) -> Self {
        // A valid MimeType is always a valid essence for the mime crate.
        mime_type
            .as_str()
            .parse()
            .unwrap_or(mime::APPLICATION_OCTET_STREAM)
    }
}

#[cfg(feature = "mime")]
impl TryFrom<&mime::Mime> for MimeType {
    type Error = InvalidMimeType;

    /// The essence (`type/subtype`); parameters are dropped.
    fn try_from(mime: &mime::Mime) -> Result<Self, Self::Error> {
        Self::new(mime.essence_str())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn valid_types_parse_and_keep_their_spelling() {
        let excel = MimeType::new("application/vnd.ms-excel.sheet.macroEnabled.12").unwrap();
        assert_eq!(
            excel.as_str(),
            "application/vnd.ms-excel.sheet.macroEnabled.12"
        );
        assert_eq!(excel.media(), "application");
        assert_eq!(excel.subtype(), "vnd.ms-excel.sheet.macroEnabled.12");
        assert!(excel.is_media("APPLICATION"));
    }

    #[test]
    fn invalid_types_are_refused() {
        let long = format!("a/{}", "b".repeat(128));
        for bad in [
            "",
            "image",
            "/png",
            "image/",
            "a/b/c",
            "image/png; q=1",
            "text/ plain",
            "a/\nb",
            "../etc",
            "./x",
            "*/*",
            "image/*",
            "a/b\"c",
            "a/-b",
            "é/x",
            long.as_str(),
        ] {
            let err = MimeType::new(bad).unwrap_err();
            assert_eq!(err.text(), bad);
        }
        assert!(MimeType::new(&format!("a/{}", "b".repeat(127))).is_ok());
        assert!(MimeType::new("application/vnd.a+b_c-d.e!f#g$h&i^j").is_ok());
    }

    #[test]
    fn a_refused_text_is_shown_escaped_and_cut() {
        // The text may come from a file: it is shown as `quoted` shows it.
        let shown = |bad: &str| MimeType::new(bad).unwrap_err().to_string();
        assert_eq!(
            shown("a/\u{1b}[2J\n"),
            r#"not a MIME type: "a/\u{1b}[2J\n""#
        );
        assert_eq!(
            shown(&format!("a/{}", "b".repeat(5000))),
            format!("not a MIME type: \"a/{}…\"", "b".repeat(30))
        );
    }

    #[cfg(feature = "mime")]
    #[test]
    fn conversions_with_the_mime_crate_keep_the_essence() {
        let png = MimeType::new("image/png").unwrap();
        let converted = mime::Mime::from(&png);
        assert_eq!(converted, mime::IMAGE_PNG);
        let with_parameters: mime::Mime = "text/plain; charset=utf-8".parse().unwrap();
        assert_eq!(
            MimeType::try_from(&with_parameters).unwrap().as_str(),
            "text/plain"
        );
    }

    #[test]
    fn case_is_ignored_for_comparison_but_kept_for_display() {
        let a = MimeType::new("Application/PDF").unwrap();
        let b = MimeType::new("application/pdf").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.cmp(&b), Ordering::Equal);
        assert_eq!(a.to_string(), "Application/PDF");
        let set: HashSet<_> = [a, b].into_iter().collect();
        assert_eq!(set.len(), 1);
        assert!(MimeType::new("a/b").unwrap() < MimeType::new("A/C").unwrap());
    }
}
