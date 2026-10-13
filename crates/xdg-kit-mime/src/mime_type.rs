use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::str::FromStr;
use std::sync::Arc;

use crate::work;

/// A MIME type such as `image/png`: a media type and a subtype, without
/// parameters or wildcards.
///
/// The spelling it was created with is kept, and is what [`MimeType::as_str`]
/// and `Display` give. Comparison, hashing and ordering ignore ASCII case,
/// as MIME types are case-insensitive (RFC 2045, section 5.1): two values
/// that compare equal can print differently. For the same reason it does
/// not implement `Borrow<str>`: a map of `MimeType`s looked up by `&str`
/// would compare case-sensitively. Cloning is cheap.
///
/// Made from text with [`MimeType::new`] (or `parse`, `TryFrom`), from a
/// `Content-Type` value with parameters with [`MimeType::from_media_type`],
/// and as a constant with [`MimeType::from_static`]. The types the
/// specifications give a meaning to are constants here, such as
/// [`MimeType::TEXT_PLAIN`].
///
/// To use it with another crate's type, go through the text:
/// `mime_type.as_str().parse()` there, and its text (without parameters, or
/// through `from_media_type`) here.
#[derive(Clone)]
pub struct MimeType(Text);

/// The text of a [`MimeType`]: a constant's needs no allocation.
#[derive(Clone)]
enum Text {
    Static(&'static str),
    Shared(Arc<str>),
}

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

impl std::error::Error for InvalidMimeType {}

/// Whether `name` is an RFC 6838 restricted name (section 4.2): 1 to 127
/// characters, the first a letter or digit, the rest letters, digits or
/// `!#$&^_.+-`.
const fn restricted_name(name: &[u8]) -> bool {
    if name.is_empty() || name.len() > 127 || !name[0].is_ascii_alphanumeric() {
        return false;
    }
    let mut at = 1;
    while at < name.len() {
        let byte = name[at];
        if !(byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-'
            ))
        {
            return false;
        }
        at += 1;
    }
    true
}

/// Whether `text` is `media/subtype` with both parts restricted names.
const fn valid(text: &[u8]) -> bool {
    let mut at = 0;
    while at < text.len() {
        if text[at] == b'/' {
            let (media, rest) = text.split_at(at);
            return restricted_name(media) && restricted_name(rest.split_at(1).1);
        }
        at += 1;
    }
    false
}

/// [`restricted_name`], counting the bytes checked: the first, then, if the
/// name is short enough, the rest.
fn counted_name(name: &str) -> bool {
    work::count(1);
    if name
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        && name.len() <= 127
    {
        work::count(name.len() - 1);
    }
    restricted_name(name.as_bytes())
}

/// [`valid`], counting the bytes scanned and checked.
fn counted_valid(text: &str) -> bool {
    split(text).is_some_and(|(media, subtype)| counted_name(media) && counted_name(subtype))
}

/// `text` split at its first `/`, counting the bytes scanned for it.
fn split(text: &str) -> Option<(&str, &str)> {
    let parts = text.split_once('/');
    work::count(parts.map_or(text.len(), |(media, _)| media.len() + 1));
    parts
}

/// RFC 9110 `OWS` (section 5.6.3): a space or a tab.
const fn ows(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t')
}

/// RFC 9110 `tchar` (section 5.6.2): a byte of a `token`.
const fn tchar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// RFC 9110 `qdtext` (section 5.6.4): a byte of a quoted string other than
/// `"` and `\`. A byte from 0x80 up is `obs-text`.
const fn qdtext(byte: u8) -> bool {
    matches!(byte, b'\t' | b' ' | 0x21 | 0x23..=0x5b | 0x5d..=0x7e | 0x80..)
}

/// RFC 9110 `quoted-pair` (section 5.6.4): the byte after a `\`.
const fn quoted(byte: u8) -> bool {
    matches!(byte, b'\t' | b' ' | 0x21..=0x7e | 0x80..)
}

/// Whether `rest`, the text after `type/subtype`, is RFC 9110 `parameters`
/// (section 5.6.6) followed by optional `OWS`, and how many bytes were
/// scanned to tell:
///
/// ```text
/// parameters = *( OWS ";" OWS [ parameter ] )
/// parameter  = token "=" ( token / quoted-string )
/// ```
fn parameters(rest: &[u8]) -> (bool, usize) {
    let mut at = 0;
    let skip = |at: &mut usize, kind: fn(u8) -> bool| {
        while rest.get(*at).is_some_and(|&byte| kind(byte)) {
            *at += 1;
        }
    };
    // The byte at `at` refused, after the bytes before it.
    let refused = |at: usize| (false, (at + 1).min(rest.len()));
    loop {
        skip(&mut at, ows);
        match rest.get(at) {
            None => return (true, at),
            Some(b';') => at += 1,
            Some(_) => return refused(at),
        }
        skip(&mut at, ows);
        // An empty parameter.
        if matches!(rest.get(at), None | Some(b';')) {
            continue;
        }
        let name = at;
        skip(&mut at, tchar);
        if at == name || rest.get(at) != Some(&b'=') {
            return refused(at);
        }
        at += 1;
        if rest.get(at) == Some(&b'"') {
            at += 1;
            loop {
                match rest.get(at) {
                    Some(b'"') => break,
                    Some(b'\\') if rest.get(at + 1).is_some_and(|&byte| quoted(byte)) => at += 2,
                    Some(b'\\') => return refused(at + 1),
                    Some(&byte) if qdtext(byte) => at += 1,
                    _ => return refused(at),
                }
            }
            at += 1;
        } else {
            let value = at;
            skip(&mut at, tchar);
            if at == value {
                return refused(at);
            }
        }
    }
}

impl MimeType {
    /// `application/octet-stream`: any data; the spec's fallback for
    /// binary content.
    pub const APPLICATION_OCTET_STREAM: Self = Self::from_static("application/octet-stream");
    /// `text/plain`: any text; the spec's fallback for text content.
    pub const TEXT_PLAIN: Self = Self::from_static("text/plain");
    /// `application/xml`: XML, whose root element can refine the type.
    pub const APPLICATION_XML: Self = Self::from_static("application/xml");
    /// `application/x-zerosize`: an empty file.
    pub const APPLICATION_X_ZEROSIZE: Self = Self::from_static("application/x-zerosize");
    /// `application/x-desktop`: a desktop entry file.
    pub const APPLICATION_X_DESKTOP: Self = Self::from_static("application/x-desktop");
    /// `inode/directory`: a directory.
    pub const INODE_DIRECTORY: Self = Self::from_static("inode/directory");
    /// `inode/symlink`: a symbolic link.
    pub const INODE_SYMLINK: Self = Self::from_static("inode/symlink");
    /// `inode/mount-point`: a directory where a filesystem is mounted.
    pub const INODE_MOUNT_POINT: Self = Self::from_static("inode/mount-point");
    /// `inode/blockdevice`: a block device.
    pub const INODE_BLOCKDEVICE: Self = Self::from_static("inode/blockdevice");
    /// `inode/chardevice`: a character device.
    pub const INODE_CHARDEVICE: Self = Self::from_static("inode/chardevice");
    /// `inode/fifo`: a named pipe.
    pub const INODE_FIFO: Self = Self::from_static("inode/fifo");
    /// `inode/socket`: a socket.
    pub const INODE_SOCKET: Self = Self::from_static("inode/socket");

    /// Parses `media/subtype`.
    ///
    /// # Errors
    ///
    /// [`InvalidMimeType`] unless both parts are RFC 6838 restricted names:
    /// 1 to 127 characters each, starting with a letter or digit, then
    /// letters, digits and `!#$&^_.+-`. So no parameters (`;`), no
    /// wildcards (`*`), and no `.` or `..` as a part.
    pub fn new(text: &str) -> Result<Self, InvalidMimeType> {
        let valid = counted_valid(text);
        // Each byte copied, into the type or the error.
        work::count(text.len());
        if valid {
            Ok(Self(Text::Shared(text.into())))
        } else {
            Err(InvalidMimeType(text.to_owned()))
        }
    }

    /// A type from a constant, checked as [`MimeType::new`] checks it,
    /// without allocating.
    ///
    /// For constants: in a `const` item, invalid text does not compile.
    ///
    /// ```
    /// use xdg_kit_mime::MimeType;
    ///
    /// const SEED: MimeType = MimeType::from_static("text/x-seed");
    /// assert_eq!(SEED, MimeType::new("text/x-seed").unwrap());
    /// ```
    ///
    /// ```compile_fail
    /// use xdg_kit_mime::MimeType;
    ///
    /// const BAD: MimeType = MimeType::from_static("text/x seed");
    /// ```
    ///
    /// # Panics
    ///
    /// If `text` is not a MIME type, when called outside a `const`
    /// context. Use [`MimeType::new`] for text known only at run time.
    #[must_use]
    pub const fn from_static(text: &'static str) -> Self {
        assert!(valid(text.as_bytes()), "not a MIME type");
        Self(Text::Static(text))
    }

    /// Parses an RFC 9110 `media-type` (section 8.3.1), the value of a
    /// `Content-Type` field such as `text/html; charset="utf-8"`, and keeps
    /// `media/subtype` as spelled.
    ///
    /// The parameters are checked against RFC 9110's grammar (section
    /// 5.6.6) and dropped:
    ///
    /// ```text
    /// media-type = type "/" subtype parameters
    /// parameters = *( OWS ";" OWS [ parameter ] )
    /// parameter  = token "=" ( token / quoted-string )
    /// ```
    ///
    /// Spaces and tabs around the whole text are allowed, as a field value
    /// excludes them (section 5.5), and so are empty parameters
    /// (`text/plain;`). A parameter named twice is accepted: RFC 9110's
    /// grammar allows it, though RFC 6838 (section 4.3) calls it an error;
    /// that is for whoever reads the parameters to judge.
    ///
    /// ```
    /// use xdg_kit_mime::MimeType;
    ///
    /// let html = MimeType::from_media_type("Text/HTML; Charset=\"utf-8\"")?;
    /// assert_eq!(html.as_str(), "Text/HTML");
    /// # Ok::<(), xdg_kit_mime::InvalidMimeType>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`InvalidMimeType`], holding all of `text`, unless `media/subtype`
    /// is valid for [`MimeType::new`] and the parameters follow the
    /// grammar: no whitespace inside `media/subtype` or around `=`, a
    /// non-empty name and value, and a closed quoted string.
    pub fn from_media_type(text: &str) -> Result<Self, InvalidMimeType> {
        let bytes = text.as_bytes();
        let refused = || {
            // Each byte copied into the error.
            work::count(text.len());
            Err(InvalidMimeType(text.to_owned()))
        };
        let Some(start) = bytes.iter().position(|&byte| !ows(byte)) else {
            work::count(bytes.len());
            return refused();
        };
        let end = bytes[start..]
            .iter()
            .position(|&byte| ows(byte) || byte == b';')
            .map_or(bytes.len(), |length| start + length);
        // The bytes scanned for the start and the end (the first byte once).
        work::count((end + 1).min(bytes.len()));
        // Both at an ASCII byte or the end, so at character boundaries.
        let essence = &text[start..end];
        if !counted_valid(essence) {
            return refused();
        }
        let (valid, scanned) = parameters(&bytes[end..]);
        work::count(scanned);
        if !valid {
            return refused();
        }
        // Each byte of the type copied.
        work::count(essence.len());
        Ok(Self(Text::Shared(essence.into())))
    }

    /// The type as spelled when it was created.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match &self.0 {
            Text::Static(text) => text,
            Text::Shared(text) => text,
        }
    }

    /// The part before the `/`, as in `image`.
    #[must_use]
    pub fn media(&self) -> &str {
        split(self.as_str()).map_or("", |(media, _)| media)
    }

    /// The part after the `/`, as in `png`.
    #[must_use]
    pub fn subtype(&self) -> &str {
        split(self.as_str()).map_or("", |(_, subtype)| subtype)
    }

    /// Whether the media part is `media`, ignoring ASCII case.
    #[must_use]
    pub fn is_media(&self, media: &str) -> bool {
        // The media part's scan counts in `media()`; the comparison, with a
        // fixed name, does not.
        self.media().eq_ignore_ascii_case(media)
    }
}

impl PartialEq for MimeType {
    fn eq(&self, other: &Self) -> bool {
        self.as_str().eq_ignore_ascii_case(other.as_str())
    }
}

impl Eq for MimeType {}

impl Hash for MimeType {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Lowercased in chunks: one hasher call per chunk, not per byte.
        let mut chunk = [0u8; 64];
        for part in self.as_str().as_bytes().chunks(chunk.len()) {
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
        self.as_str()
            .bytes()
            .map(|byte| byte.to_ascii_lowercase())
            .cmp(other.as_str().bytes().map(|byte| byte.to_ascii_lowercase()))
    }
}

impl PartialOrd for MimeType {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for MimeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for MimeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl FromStr for MimeType {
    type Err = InvalidMimeType;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::new(text)
    }
}

impl TryFrom<&str> for MimeType {
    type Error = InvalidMimeType;

    fn try_from(text: &str) -> Result<Self, Self::Error> {
        Self::new(text)
    }
}

impl TryFrom<String> for MimeType {
    type Error = InvalidMimeType;

    /// As [`MimeType::new`]; the text is still copied.
    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::new(&text)
    }
}

impl From<MimeType> for String {
    fn from(mime: MimeType) -> Self {
        mime.as_str().to_owned()
    }
}

impl From<MimeType> for Arc<str> {
    /// Free for a type made at run time; a constant's text is copied.
    fn from(mime: MimeType) -> Self {
        match mime.0 {
            Text::Static(text) => text.into(),
            Text::Shared(text) => text,
        }
    }
}

impl AsRef<str> for MimeType {
    fn as_ref(&self) -> &str {
        self.as_str()
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

    /// Parses `text`, checks the result, and counts the steps: at least
    /// one per byte, as each byte is copied.
    fn counted(text: &str, valid: bool) -> (usize, usize) {
        let (parsed, steps) = work::steps(|| MimeType::new(text));
        assert_eq!(parsed.is_ok(), valid, "{text}");
        let bytes = text.len();
        assert!(steps >= bytes, "{steps} steps for {bytes} bytes");
        (steps, bytes)
    }

    #[test]
    fn a_long_type_is_parsed_in_linear_time() {
        // Each part may have at most 127 characters.
        work::assert_linear(
            60,
            |n| counted(&format!("{}/{}", "a".repeat(n), "b".repeat(n)), true),
            // The analysed cost, with `n` bytes in each part: the media
            // type and the `/` scanned for it (n + 1), each byte of both
            // parts checked (2n, that is bytes - 1), and every byte copied
            // (bytes): 2 per byte and 1 per item. Twice that.
            |items, bytes| 2 * (2 * bytes + items),
        );
    }

    #[test]
    fn a_long_text_that_is_not_a_type_is_refused_in_linear_time() {
        // Each case with the analysed cost of a text of `bytes` bytes: the
        // bytes scanned for the `/`, those checked, and every byte copied
        // into the error (bytes). A part longer than 127 bytes is refused
        // once its first byte is checked, and a refused media type leaves
        // the subtype unchecked. Each bound is twice the analysed cost.
        let refused = |text: fn(usize) -> String, cost: fn(usize) -> usize| {
            work::assert_linear(
                10_000,
                |n| counted(&text(n), false),
                |_, bytes| 2 * cost(bytes),
            );
        };
        // No `/`: every byte scanned for it (bytes), none checked.
        refused(|n| "a".repeat(n), |bytes| 2 * bytes);
        // A long subtype: `a/` scanned (2), the first byte of each part
        // checked (2).
        refused(|n| format!("a/{}\n", "b".repeat(n)), |bytes| bytes + 4);
        // A long media type: scanned up to and with the `/` (bytes - 1),
        // its first byte checked (1).
        refused(|n| format!("{}\n/b", "a".repeat(n)), |bytes| 2 * bytes);
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

    #[test]
    fn media_types_keep_the_type_and_check_the_parameters() {
        for (text, kept) in [
            ("text/plain", "text/plain"),
            ("Text/HTML;Charset=UTF-8", "Text/HTML"),
            ("text/html; charset=\"utf-8\"", "text/html"),
            (" \ttext/plain ; a=b ;\tc=\"d e\"\t ", "text/plain"),
            ("text/plain;", "text/plain"),
            ("text/plain;;a=b; ;", "text/plain"),
            ("a/b; q=\"\\\"x\\\\\\ \\\t\"", "a/b"),
            ("a/b; q=\"caf\u{e9} \\\u{e9}\"", "a/b"),
            ("a/b; charset=x; CHARSET=y", "a/b"),
            ("a/b; !#$%&'*+-.^_`|~09az=!#$%&'*+-.^_`|~", "a/b"),
        ] {
            let mime = MimeType::from_media_type(text).unwrap();
            assert_eq!(mime.as_str(), kept, "{text:?}");
        }
    }

    #[test]
    fn media_types_out_of_the_grammar_are_refused() {
        for bad in [
            "",
            " \t ",
            ";a=b",
            "text /plain",
            "text/ plain",
            "text/plain x",
            "text/plain; a = b",
            "text/plain; a =b",
            "text/plain; a= b",
            "text/plain; a",
            "text/plain; a=",
            "text/plain; =b",
            "text/plain; a=b c",
            "text/plain; a=\"b",
            "text/plain; a=\"b\\\"",
            "text/plain; a=\"b\\\u{1}\"",
            "text/plain; a=\"b\u{7f}\"",
            "text/plain; a=b\u{e9}",
            "text/plain; a/b=c",
            "text/plain; a=b\n",
            "text/plain\n",
            "text/*; a=b",
            "*/*",
            "../x; a=b",
        ] {
            let err = MimeType::from_media_type(bad).unwrap_err();
            assert_eq!(err.text(), bad);
        }
    }

    #[test]
    fn a_media_type_is_a_type_when_new_accepts_it() {
        for text in ["image/png", "Application/PDF", "a/b!c"] {
            assert_eq!(
                MimeType::from_media_type(text).unwrap().as_str(),
                MimeType::new(text).unwrap().as_str()
            );
        }
    }

    /// Parses `text` as a media type, checks the result, and counts the
    /// steps: at least one per byte, as each byte is scanned.
    fn counted_media_type(text: &str, valid: bool) -> (usize, usize) {
        let (parsed, steps) = work::steps(|| MimeType::from_media_type(text));
        assert_eq!(parsed.is_ok(), valid, "{text}");
        let bytes = text.len();
        assert!(steps >= bytes, "{steps} steps for {bytes} bytes");
        (steps, bytes)
    }

    #[test]
    fn long_media_types_are_parsed_in_linear_time() {
        // The analysed cost of a text of `bytes` bytes: each byte scanned
        // once for the type's end or in the parameters, the type checked
        // and copied (at most 3 per byte of it), and, when refused, every
        // byte copied into the error: at most 4 per byte, plus a few for
        // the first. Each bound is twice that.
        let bound = |_, bytes: usize| 2 * (4 * bytes + 4);
        // Many parameters.
        work::assert_linear(
            1_000,
            |n| counted_media_type(&format!("a/b{}", "; c=d".repeat(n)), true),
            bound,
        );
        // A long quoted value, with escapes.
        work::assert_linear(
            1_000,
            |n| counted_media_type(&format!("a/b; c=\"{}\"", "x\\\"".repeat(n)), true),
            bound,
        );
        // Refused at the end: an unclosed quoted value.
        work::assert_linear(
            1_000,
            |n| counted_media_type(&format!("a/b; c=\"{}", "x".repeat(n)), false),
            bound,
        );
        // Only whitespace.
        work::assert_linear(1_000, |n| counted_media_type(&" ".repeat(n), false), bound);
    }

    #[test]
    fn conversion_traits_agree_with_new() {
        let mime = MimeType::try_from("Image/PNG").unwrap();
        assert_eq!(mime.as_str(), "Image/PNG");
        assert_eq!(MimeType::try_from(String::from("Image/PNG")).unwrap(), mime);
        assert_eq!(String::from(mime.clone()), "Image/PNG");
        assert_eq!(&*Arc::<str>::from(mime), "Image/PNG");
        assert_eq!(&*Arc::<str>::from(MimeType::TEXT_PLAIN), "text/plain");
        for bad in ["image", "image/png; a=b"] {
            assert_eq!(
                MimeType::try_from(bad),
                Err(MimeType::new(bad).unwrap_err())
            );
            assert_eq!(
                MimeType::try_from(bad.to_owned()),
                Err(MimeType::new(bad).unwrap_err())
            );
        }
    }

    #[test]
    fn constants_are_the_types_they_name() {
        for (constant, text) in [
            (
                MimeType::APPLICATION_OCTET_STREAM,
                "application/octet-stream",
            ),
            (MimeType::TEXT_PLAIN, "text/plain"),
            (MimeType::APPLICATION_XML, "application/xml"),
            (MimeType::APPLICATION_X_ZEROSIZE, "application/x-zerosize"),
            (MimeType::APPLICATION_X_DESKTOP, "application/x-desktop"),
            (MimeType::INODE_DIRECTORY, "inode/directory"),
            (MimeType::INODE_SYMLINK, "inode/symlink"),
            (MimeType::INODE_MOUNT_POINT, "inode/mount-point"),
            (MimeType::INODE_BLOCKDEVICE, "inode/blockdevice"),
            (MimeType::INODE_CHARDEVICE, "inode/chardevice"),
            (MimeType::INODE_FIFO, "inode/fifo"),
            (MimeType::INODE_SOCKET, "inode/socket"),
        ] {
            let parsed = MimeType::new(&text.to_uppercase()).unwrap();
            assert_eq!(constant.as_str(), text);
            assert_eq!(constant, parsed);
            assert_eq!(constant.cmp(&parsed), Ordering::Equal);
            let set: HashSet<_> = [constant, parsed].into_iter().collect();
            assert_eq!(set.len(), 1);
        }
    }

    #[test]
    fn from_static_checks_as_new_does() {
        for text in ["image/png", "application/vnd.a+b_c-d.e!f#g$h&i^j"] {
            assert_eq!(MimeType::from_static(text), MimeType::new(text).unwrap());
        }
        for bad in [
            "", "image", "image/", "/png", "a/b/c", "image/*", "a/b; c=d", "a/-b",
        ] {
            let caught = std::panic::catch_unwind(|| MimeType::from_static(bad));
            assert!(caught.is_err(), "{bad:?} accepted");
            assert!(MimeType::new(bad).is_err());
        }
        let long: &'static str = format!("a/{}", "b".repeat(128)).leak();
        assert!(std::panic::catch_unwind(|| MimeType::from_static(long)).is_err());
        let longest: &'static str = format!("a/{}", "b".repeat(127)).leak();
        assert_eq!(MimeType::from_static(longest).as_str(), longest);
    }
}
