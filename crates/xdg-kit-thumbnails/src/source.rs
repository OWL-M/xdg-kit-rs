//! What a thumbnail is of, and how the standard names it: the canonical
//! URI, its MD5 cache key, and the shared repositories' `./name` form.

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::{md5, work};

/// What a thumbnail is of: a local file, or any other resource by URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThumbnailSource {
    /// A local file, by its absolute path (a relative one is made
    /// absolute by [`ThumbnailSource::local`]). Its canonical URI
    /// derives from the path.
    Local(PathBuf),
    /// Any other resource, by its absolute URI. There is no local path
    /// to stat, to check for readability, or to hand to a thumbnailer's
    /// `%i`; those responsibilities are the caller's.
    Remote(String),
}

impl ThumbnailSource {
    /// A local source; a relative path is made absolute against the
    /// working directory.
    ///
    /// # Errors
    ///
    /// When the path cannot be made absolute (it is empty).
    pub fn local(path: impl AsRef<Path>) -> std::io::Result<Self> {
        Ok(Self::Local(std::path::absolute(path)?))
    }

    /// A remote source. The URI must look like one: a scheme before a
    /// `:`, and no byte a URI cannot carry (whitespace, controls, NUL).
    ///
    /// # Errors
    ///
    /// When `uri` is not an absolute URI by those checks.
    pub fn remote(uri: impl Into<String>) -> Result<Self, InvalidUri> {
        let uri = uri.into();
        work::count(uri.len());
        let scheme_end = uri.find(':').ok_or(InvalidUri)?;
        let mut scheme = uri[..scheme_end].bytes();
        let named = scheme.next().is_some_and(|byte| byte.is_ascii_alphabetic())
            && scheme
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'));
        if !named || uri.bytes().any(|byte| byte <= b' ' || byte == 0x7F) {
            return Err(InvalidUri);
        }
        Ok(Self::Remote(uri))
    }

    /// The canonical URI the cache is keyed by: for a local file,
    /// `file://` plus the encoded absolute path; for a remote resource,
    /// the URI as given.
    #[must_use]
    pub fn uri(&self) -> String {
        match self {
            Self::Local(path) => {
                let mut uri = String::from("file://");
                uri.push_str(&encode_uri_path(path.as_os_str().as_bytes()));
                uri
            }
            Self::Remote(uri) => uri.clone(),
        }
    }

    /// The local path, when there is one.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Local(path) => Some(path),
            Self::Remote(_) => None,
        }
    }

    /// The cache file name: the MD5 of the canonical URI, plus `.png`.
    #[must_use]
    pub fn cache_name(&self) -> String {
        let mut name = md5::hex(self.uri().as_bytes());
        name.push_str(".png");
        name
    }
}

/// The text is not an absolute URI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidUri;

impl std::fmt::Display for InvalidUri {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "not an absolute URI")
    }
}

impl std::error::Error for InvalidUri {}

/// The resource's metadata for freshness, captured by the caller — for a
/// generation, at generation time, so an install never stamps an old
/// image with a fresh-looking time read later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceInfo {
    /// When the resource was last modified. `Thumb::MTime` holds whole
    /// seconds since the epoch; the conversion truncates toward zero.
    pub mtime: SystemTime,
    /// The resource's length in bytes, when known (`Thumb::Size` is the
    /// original's byte length, nothing about the thumbnail).
    pub len: Option<u64>,
}

impl SourceInfo {
    /// Stats a local file. Remote resources get their metadata from the
    /// caller, which fetched them.
    ///
    /// # Errors
    ///
    /// Any error of reading the file's metadata.
    pub fn of_local(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let metadata = std::fs::metadata(path)?;
        Ok(Self {
            mtime: metadata.modified()?,
            len: Some(metadata.len()),
        })
    }

    /// The `Thumb::MTime` spelling: whole seconds since the epoch,
    /// truncated toward zero (`None` before the epoch — the standard has
    /// no spelling for that).
    #[must_use]
    pub(crate) fn mtime_text(&self) -> Option<String> {
        let since = self
            .mtime
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?
            .as_secs();
        Some(since.to_string())
    }
}

/// A validated application identifier for `fail/<application>/`: one
/// nonempty path component, so a marker can never land outside `fail/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Application(String);

impl Application {
    /// Validates `name` (conventionally `name-version`, taken as the
    /// caller spells it): nonempty, no `/`, no NUL, not `.` or `..`.
    ///
    /// # Errors
    ///
    /// [`InvalidApplication`] otherwise.
    pub fn new(name: impl Into<String>) -> Result<Self, InvalidApplication> {
        let name = name.into();
        work::count(name.len());
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.bytes().any(|byte| byte == b'/' || byte == 0)
        {
            return Err(InvalidApplication);
        }
        Ok(Self(name))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// The name cannot be a single path component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidApplication;

impl std::fmt::Display for InvalidApplication {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "an application name must be one nonempty path component")
    }
}

impl std::error::Error for InvalidApplication {}

/// Percent-encodes `bytes` as the deployed `GLib` file-URI encoder does
/// (checked against `gio info`'s URIs): a byte survives when it is RFC
/// 3986 unreserved (`A–Z a–z 0–9 - . _ ~`), one of the sub-delimiters
/// `GLib` keeps (`! $ & ' ( ) * + , =` — not `;`), or `: @ /`; every other
/// byte — bytes, not characters — becomes uppercase `%XX`. Anything
/// stricter (bare unreserved) would hash `/tmp/a+b.png` differently from
/// every other implementation and hide their thumbnails.
pub(crate) fn encode_uri_path(bytes: &[u8]) -> String {
    work::count(bytes.len());
    let kept = |byte: u8| {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'.'
                    | b'_'
                    | b'~'
                    | b'!'
                    | b'$'
                    | b'&'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b'+'
                    | b','
                    | b'='
                    | b':'
                    | b'@'
                    | b'/'
            )
    };
    let mut encoded = String::with_capacity(bytes.len());
    for &byte in bytes {
        if kept(byte) {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

/// The shared repositories' key text for a file name: `./` plus the name
/// encoded as the canonical URI encodes it. The same string is what
/// `Thumb::URI` holds there — never the bare name.
pub(crate) fn shared_uri(name: &std::ffi::OsStr) -> String {
    let mut uri = String::from("./");
    uri.push_str(&encode_uri_path(name.as_bytes()));
    uri
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_specs_example_keys_right() {
        let source = ThumbnailSource::local("/home/jens/photos/me.png").unwrap();
        assert_eq!(source.uri(), "file:///home/jens/photos/me.png");
        assert_eq!(source.cache_name(), "c6ee772d9e49320e97ec29a7eb5b1697.png");
    }

    #[test]
    fn the_encoding_is_glibs_profile_not_bare_unreserved() {
        // Kept, exactly as `gio info` spells these names.
        assert_eq!(
            encode_uri_path(b"/tmp/a+b,c:d@e(f)g='h'i!j$k&l*m.png"),
            "/tmp/a+b,c:d@e(f)g='h'i!j$k&l*m.png"
        );
        // Escaped: the semicolon (GLib escapes it), space, quote, hash,
        // question mark, percent, controls, non-UTF-8 bytes — uppercase
        // hex, byte by byte.
        assert_eq!(
            encode_uri_path(b"/t m\"p#q?r%s;t"),
            "/t%20m%22p%23q%3Fr%25s%3Bt"
        );
        assert_eq!(encode_uri_path(&[b'/', 0xFF, 0x01]), "/%FF%01");
    }

    #[test]
    fn shared_keys_use_the_dot_slash_form() {
        assert_eq!(shared_uri(std::ffi::OsStr::new("me.png")), "./me.png");
        assert_eq!(shared_uri(std::ffi::OsStr::new("a b.png")), "./a%20b.png");
        // Hashing the ./ form, not the bare name.
        assert_ne!(
            md5::hex(shared_uri(std::ffi::OsStr::new("me.png")).as_bytes()),
            md5::hex(b"me.png")
        );
    }

    #[test]
    fn remote_uris_are_validated_and_local_paths_absolutized() {
        assert!(ThumbnailSource::remote("https://example.org/a b").is_err());
        assert!(ThumbnailSource::remote("no-scheme").is_err());
        assert!(ThumbnailSource::remote("1ad:x").is_err());
        let remote = ThumbnailSource::remote("sftp://host/x").unwrap();
        assert_eq!(remote.uri(), "sftp://host/x");
        assert_eq!(remote.path(), None);
        let local = ThumbnailSource::local("relative.png").unwrap();
        assert!(local.path().unwrap().is_absolute());
    }

    #[test]
    fn applications_are_one_component() {
        assert!(Application::new("gnome-thumbnail-factory").is_ok());
        for bad in ["", ".", "..", "a/b", "/a", "a\u{0}b"] {
            assert!(Application::new(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn mtime_is_whole_seconds_truncated() {
        let info = SourceInfo {
            mtime: SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(1_999),
            len: None,
        };
        assert_eq!(info.mtime_text().as_deref(), Some("1"));
        let before = SourceInfo {
            mtime: SystemTime::UNIX_EPOCH - std::time::Duration::from_secs(1),
            len: None,
        };
        assert_eq!(before.mtime_text(), None);
    }
}
