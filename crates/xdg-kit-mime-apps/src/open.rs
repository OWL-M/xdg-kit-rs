//! Opening a file or URI: its type, the default application for it, and the
//! launch plan.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use xdg_kit_desktop_entry::{
    DesktopEntry, EntryType, LaunchError, LaunchOptions, LaunchPlan, Target, prepare_launch,
};
use xdg_kit_mime::{MimeType, PathOptions};

use crate::MimeApps;
use crate::error::{quoted, shown_path};

/// Why there is nothing to launch.
#[derive(Debug)]
#[non_exhaustive]
pub enum OpenError {
    /// The file's type could not be found (its metadata or contents could
    /// not be read).
    Type(PathBuf, io::Error),
    /// A URI without a scheme, or with a raw NUL anywhere (as
    /// [`LaunchError::BadUri`]), or a `file:` URI that is not a local path.
    /// The scheme and the NUL are checked on the URI as given, for every
    /// scheme: before a `file:` URI is looked at or a link followed, and
    /// also when the application takes no targets.
    BadUri(String),
    /// No application is associated with the type.
    NoApplication(MimeType),
    /// A `Type=Link` desktop entry without a `URL`.
    LinkWithoutUrl(PathBuf),
    /// A `Type=Link` desktop entry in an `Encoding` that is not supported
    /// ([`DesktopEntry::supports_encoding`]), such as `Legacy-Mixed`: the
    /// spec says to ignore it, so its `URL` is not followed.
    UnsupportedEncoding(PathBuf),
    /// The application cannot be launched with the target. The default
    /// application may have neither `Exec` nor `DBusActivatable=true` (it
    /// still counts as installed, as in `GLib`): that is
    /// [`LaunchError::NoExec`].
    Launch(LaunchError),
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Type(path, err) => write!(f, "{}: type not found: {err}", shown_path(path)),
            Self::BadUri(uri) => write!(f, "\"{}\" is not a URI that can be opened", quoted(uri)),
            // A type has only safe characters, and is short.
            Self::NoApplication(mime) => write!(f, "no application opens {mime}"),
            Self::LinkWithoutUrl(path) => write!(f, "{}: a link without URL", shown_path(path)),
            Self::UnsupportedEncoding(path) => {
                write!(f, "{}: a link in an unsupported encoding", shown_path(path))
            }
            Self::Launch(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for OpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Type(_, err) => Some(err),
            Self::Launch(err) => Some(err),
            _ => None,
        }
    }
}

/// How to open `target` with its default application, without launching
/// anything: [`LaunchPlan::spawn`] does that.
///
/// - A file's type comes from the MIME database (by name and contents); a
///   desktop entry of `Type=Link` is followed to its `URL`, unless it is
///   larger than `apps`' [`Limits::max_file_len`](crate::Limits::max_file_len).
///   A link in an unsupported `Encoding` is an error.
/// - A URI without a scheme or with a raw NUL is refused first
///   ([`OpenError::BadUri`]). A `file:` URI is a file; any other URI has
///   the type `x-scheme-handler/<scheme>`.
/// - The default application is [`MimeApps::default_for`]'s, which may have
///   no `Exec`: then the error is [`LaunchError::NoExec`].
///
/// # Errors
///
/// See [`OpenError`].
pub fn open_plan(
    target: &Target,
    apps: &MimeApps,
    options: &LaunchOptions,
) -> Result<LaunchPlan, OpenError> {
    plan(target, apps, options, true)
}

fn plan(
    target: &Target,
    apps: &MimeApps,
    options: &LaunchOptions,
    follow_links: bool,
) -> Result<LaunchPlan, OpenError> {
    let mime = match target {
        Target::Path(path) => match file(path, apps, options, follow_links)? {
            Ok(mime) => mime,
            Err(link) => return Ok(link),
        },
        Target::Uri(uri) => {
            // The check `prepare_launch` makes on every URI it passes, made
            // here on the URI as given: before a `file:` URI's query is
            // dropped or a link followed, and also for an application that
            // takes no targets.
            let scheme = scheme(uri)
                .filter(|_| !uri.contains('\0'))
                .ok_or_else(|| OpenError::BadUri(uri.clone()))?;
            if scheme == "file" {
                let path = file_uri_path(uri).ok_or_else(|| OpenError::BadUri(uri.clone()))?;
                match file(&path, apps, options, follow_links)? {
                    Ok(mime) => mime,
                    Err(link) => return Ok(link),
                }
            } else {
                MimeType::new(&format!("x-scheme-handler/{scheme}"))
                    .map_err(|_| OpenError::BadUri(uri.clone()))?
            }
        }
    };
    let app = apps
        .default_for(&mime)
        .ok_or(OpenError::NoApplication(mime))?;
    prepare_launch(app.entry(), std::slice::from_ref(target), options).map_err(OpenError::Launch)
}

/// A local file's type; or, for a desktop entry of `Type=Link` when links
/// are followed, the plan for its `URL`.
fn file(
    path: &Path,
    apps: &MimeApps,
    options: &LaunchOptions,
    follow_links: bool,
) -> Result<Result<MimeType, LaunchPlan>, OpenError> {
    let mime = type_of_file(apps, path)?;
    // Detection gives canonical types: the database may make this an alias.
    let desktop_entry = apps
        .mime_database()
        .canonical(&MimeType::APPLICATION_X_DESKTOP);
    if follow_links
        && mime == desktop_entry
        && let Ok(entry) = DesktopEntry::read_with_limits(path, &apps.limits().for_entries())
        && entry.entry_type() == Some(EntryType::Link)
    {
        // Checked for links only: another entry in such an encoding is still
        // a file of its type.
        if !entry.supports_encoding() {
            return Err(OpenError::UnsupportedEncoding(path.to_owned()));
        }
        let url = entry
            .url()
            .ok_or_else(|| OpenError::LinkWithoutUrl(path.to_owned()))?;
        return plan(&Target::Uri(url), apps, options, false).map(Err);
    }
    Ok(Ok(mime))
}

fn type_of_file(apps: &MimeApps, path: &Path) -> Result<MimeType, OpenError> {
    apps.mime_database()
        .type_for_path(path, &PathOptions::default())
        .map(|detection| detection.mime)
        .map_err(|err| OpenError::Type(path.to_owned(), err))
}

/// The scheme of `uri` (RFC 3986: a letter, then letters, digits, `+`, `-`
/// and `.`), lowercased.
fn scheme(uri: &str) -> Option<String> {
    let (scheme, _) = uri.split_once(':')?;
    let mut chars = scheme.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then(|| scheme.to_ascii_lowercase())
}

/// The local path of a `file:` URI (`file:///p`, `file://localhost/p` or
/// `file:/p`), percent-decoded.
fn file_uri_path(uri: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let rest = uri.get(5..)?;
    let path = if let Some(after) = rest.strip_prefix("//") {
        let slash = after.find('/')?;
        let host = &after[..slash];
        if !(host.is_empty() || host.eq_ignore_ascii_case("localhost")) {
            return None;
        }
        &after[slash..]
    } else if rest.starts_with('/') {
        rest
    } else {
        return None;
    };
    let path = path.split(['?', '#']).next()?;
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = bytes.get(at + 1..at + 3)?;
            if !hex.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            let byte = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
            if byte == 0 {
                return None;
            }
            out.push(byte);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_type_is_shown_whole() {
        let long = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
        let none = OpenError::NoApplication(MimeType::new(long).unwrap());
        assert_eq!(none.to_string(), format!("no application opens {long}"));
    }

    #[test]
    fn schemes_and_file_uris() {
        assert_eq!(scheme("HTTPS://example.org").as_deref(), Some("https"));
        assert_eq!(scheme("mailto:a@b").as_deref(), Some("mailto"));
        for bad in ["no scheme", "1http://x", ":x", "a b:c"] {
            assert_eq!(scheme(bad), None, "{bad:?}");
        }
        assert_eq!(file_uri_path("file:///tmp/a%20b"), Some("/tmp/a b".into()));
        assert_eq!(file_uri_path("file://localhost/x"), Some("/x".into()));
        assert_eq!(file_uri_path("file:/x?q#f"), Some("/x".into()));
        for bad in [
            "file://host/x",
            "file:x",
            "file:///a%2",
            "file:///a%00",
            "file:///a%+1",
        ] {
            assert_eq!(file_uri_path(bad), None, "{bad:?}");
        }
    }
}
