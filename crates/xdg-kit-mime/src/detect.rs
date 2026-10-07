//! Detecting a type from a name, content, or a file (spec
//! "Recommended checking order", "Non-regular files", "Storing the MIME type
//! using Extended Attributes").

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;

use crate::database::{IndexedGlob, lowercase};
use crate::fnmatch::Name;
use crate::model::Matchlet;
use crate::{MimeDatabase, MimeType};

/// How sure a [`Detection`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Certainty {
    /// The rules give this type and no other.
    Certain,
    /// Picked among conflicting answers, or a fallback.
    Guess,
}

/// What settled a [`Detection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DecidedBy {
    /// The file name's patterns (possibly confirmed by the content).
    Name,
    /// Magic, or the content being empty.
    Content,
    /// The root element of an XML document (`XMLnamespaces`).
    XmlRoot,
    /// The kind of file: directory, device, link, ...
    FileKind,
    /// The `user.mime_type` extended attribute.
    Attribute,
    /// Nothing matched: text or binary, from the first 128 bytes.
    Fallback,
}

/// A detected type.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Detection {
    /// The type, canonical (never an alias).
    pub mime: MimeType,
    /// How sure the answer is.
    pub certainty: Certainty,
    /// What settled it.
    pub decided_by: DecidedBy,
}

impl Detection {
    fn new(mime: MimeType, certainty: Certainty, decided_by: DecidedBy) -> Self {
        Self {
            mime,
            certainty,
            decided_by,
        }
    }
}

/// One pattern a name matched.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NameMatch {
    /// The type, canonical.
    pub mime: MimeType,
    /// The pattern's weight, 0 to 100.
    pub weight: u32,
    /// The pattern as the database writes it.
    pub pattern: String,
}

/// Every pattern a name matched, best first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NameMatches {
    matches: Vec<NameMatch>,
}

impl NameMatches {
    /// The matches: highest weight first, then the longest pattern, then the
    /// more important folder, then file order.
    #[must_use]
    pub fn matches(&self) -> &[NameMatch] {
        &self.matches
    }

    /// Whether nothing matched.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.matches.is_empty()
    }

    /// The first match: certain when every match gives this type, a guess
    /// when the matches disagree.
    ///
    /// The spec would first keep only the matches of the highest weight and
    /// longest pattern; the reference implementation, and the expectations
    /// of shared-mime-info's test suite, count every match, so that a name
    /// two types claim (`*.png` for PNG and APNG) is settled by the content.
    #[must_use]
    pub fn best(&self) -> Option<Detection> {
        let first = self.matches.first()?;
        let certainty = if self.matches.iter().all(|found| found.mime == first.mime) {
            Certainty::Certain
        } else {
            Certainty::Guess
        };
        Some(Detection::new(
            first.mime.clone(),
            certainty,
            DecidedBy::Name,
        ))
    }

    /// [`NameMatches::best`] when it is certain.
    fn certain(&self) -> Option<Detection> {
        self.best()
            .filter(|best| best.certainty == Certainty::Certain)
    }
}

/// Options for [`MimeDatabase::type_for_path`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PathOptions {
    /// Type a symbolic link by what it points to; a broken link is
    /// `inode/symlink`. When off, every link is `inode/symlink`. On by
    /// default.
    pub follow_links: bool,
    /// Report a folder on another device than its parent (and `/`) as
    /// `inode/mount-point`, at the cost of a `stat` of the parent. On by
    /// default. Bind mounts of the same filesystem are not detected; btrfs
    /// subvolumes and snapshots give their own device number, so they are
    /// reported as mount points too.
    pub mount_points: bool,
    /// Use a regular file's `user.mime_type` extended attribute when it holds
    /// a valid type. Off by default, as GNOME and KDE do not read it.
    pub extended_attribute: bool,
}

impl Default for PathOptions {
    fn default() -> Self {
        Self {
            follow_links: true,
            mount_points: true,
            extended_attribute: false,
        }
    }
}

/// Bytes the text-or-binary fallback looks at (spec "Recommended checking
/// order").
pub(crate) const FALLBACK_LEN: usize = 128;

fn known(text: &'static str) -> MimeType {
    MimeType::known(text)
}

/// Text unless the first 128 bytes hold a control character other than tab,
/// line feed or carriage return.
fn text_or_binary(data: &[u8]) -> MimeType {
    let binary = data
        .iter()
        .take(FALLBACK_LEN)
        .any(|&byte| byte < 32 && !matches!(byte, b'\t' | b'\n' | b'\r'));
    if binary {
        known("application/octet-stream")
    } else {
        known("text/plain")
    }
}

fn compare(matchlet: &Matchlet, data: &[u8]) -> bool {
    let len = matchlet.value.len();
    let start = matchlet.start as usize;
    let end = start.saturating_add(matchlet.range as usize);
    for at in start..end {
        let Some(window) = at.checked_add(len).and_then(|stop| data.get(at..stop)) else {
            // Later offsets only reach further.
            return false;
        };
        let equal = match &matchlet.mask {
            Some(mask) => window
                .iter()
                .zip(&matchlet.value)
                .zip(mask)
                .all(|((&byte, &value), &mask)| byte & mask == value & mask),
            None => window == matchlet.value.as_slice(),
        };
        if equal {
            return true;
        }
    }
    false
}

pub(crate) fn matchlet_matches(matchlet: &Matchlet, data: &[u8]) -> bool {
    compare(matchlet, data)
        && (matchlet.children.is_empty()
            || matchlet
                .children
                .iter()
                .any(|child| matchlet_matches(child, data)))
}

impl MimeDatabase {
    /// The patterns `name` matches. Only the part after the last `/` is
    /// looked at. No I/O.
    ///
    /// The spec's rules: literal patterns (such as `Makefile`) first, and if
    /// one matches only literal matches count; otherwise `*`-then-literal
    /// patterns (`*.tar.gz`), and only if none of those matches the other
    /// patterns. Case-insensitive patterns are compared lowercased;
    /// case-sensitive ones as written.
    pub fn type_for_name(&self, name: impl AsRef<OsStr>) -> NameMatches {
        let bytes = name.as_ref().as_bytes();
        let name = bytes.rsplit(|&byte| byte == b'/').next().unwrap_or(bytes);
        let lower = lowercase(name);
        // A name that is not UTF-8 only matches ASCII patterns.
        let utf8 = std::str::from_utf8(name).is_ok();
        let globs = &self.globs;
        let compared =
            |glob: &IndexedGlob| -> &[u8] { if glob.case_sensitive { name } else { &lower } };

        let mut found: Vec<usize> = Vec::new();
        for key in [name, lower.as_slice()] {
            let Ok(key) = std::str::from_utf8(key) else {
                continue;
            };
            for &at in globs.literal.get(key).into_iter().flatten() {
                if compared(&globs.all[at]) == globs.all[at].compared.as_bytes()
                    && !found.contains(&at)
                {
                    found.push(at);
                }
            }
        }
        if found.is_empty() {
            for source in [name, lower.as_slice()] {
                for start in 0..=source.len() {
                    let Ok(tail) = std::str::from_utf8(&source[start..]) else {
                        continue;
                    };
                    if !utf8 && !tail.is_ascii() {
                        continue;
                    }
                    for &at in globs.suffix.get(tail).into_iter().flatten() {
                        let glob = &globs.all[at];
                        if compared(glob).ends_with(&glob.compared.as_bytes()[1..])
                            && !found.contains(&at)
                        {
                            found.push(at);
                        }
                    }
                }
            }
        }
        if found.is_empty() {
            let (name, lower) = (Name::new(name), Name::new(&lower));
            found = globs
                .full
                .iter()
                .filter(|(at, pattern)| {
                    pattern.matches(if globs.all[*at].case_sensitive {
                        &name
                    } else {
                        &lower
                    })
                })
                .map(|&(at, _)| at)
                .collect();
        }

        found.sort_by(|&a, &b| {
            let (a, b) = (&globs.all[a], &globs.all[b]);
            b.weight
                .cmp(&a.weight)
                .then(b.compared_len.cmp(&a.compared_len))
                .then(a.folder.cmp(&b.folder))
                .then(a.order.cmp(&b.order))
        });
        NameMatches {
            matches: found
                .into_iter()
                .map(|at| {
                    let glob = &globs.all[at];
                    NameMatch {
                        mime: self.canonical(&glob.mime),
                        weight: glob.weight,
                        pattern: glob.pattern.clone(),
                    }
                })
                .collect(),
        }
    }

    /// The type magic gives `data`, refined by the root element when it is
    /// XML. `None` when no magic rule matches.
    fn magic_type(&self, data: &[u8]) -> Option<Detection> {
        let rule = self.magic.iter().find(|rule| {
            rule.matchlets
                .iter()
                .any(|matchlet| matchlet_matches(matchlet, data))
        })?;
        let mime = self.canonical(&rule.mime);
        if self.is_a(&mime, &known("application/xml"))
            && let Some(refined) = self.xml_root_type(data)
        {
            return Some(Detection::new(
                refined,
                Certainty::Certain,
                DecidedBy::XmlRoot,
            ));
        }
        Some(Detection::new(mime, Certainty::Certain, DecidedBy::Content))
    }

    /// The type `XMLnamespaces` gives the document's root element: the exact
    /// (namespace, local name) pair, else the namespace with any element,
    /// else the local name with any namespace.
    fn xml_root_type(&self, data: &[u8]) -> Option<MimeType> {
        let (namespace, local) = crate::xml::root_element(data)?;
        let namespaces = &self.merged.namespaces;
        namespaces
            .get(&(namespace.clone(), local.clone()))
            .or_else(|| namespaces.get(&(namespace, String::new())))
            .or_else(|| namespaces.get(&(String::new(), local)))
            .map(|mime| self.canonical(mime))
    }

    /// Resolves a built-in type such as `text/plain` or `inode/directory`
    /// through the database. The database may define it as an alias;
    /// detection results use the canonical type.
    fn fixed(&self, text: &'static str) -> MimeType {
        self.canonical(&known(text))
    }

    /// The type of content: magic, then (for XML) the root element, then
    /// text-or-binary from the first 128 bytes. Empty data is
    /// `application/x-zerosize`. No I/O; the first
    /// [`MimeDatabase::sniff_len`] bytes of a file are enough.
    #[must_use]
    pub fn type_for_data(&self, data: &[u8]) -> Detection {
        if data.is_empty() {
            return Detection::new(
                self.fixed("application/x-zerosize"),
                Certainty::Certain,
                DecidedBy::Content,
            );
        }
        self.magic_type(data).unwrap_or_else(|| {
            Detection::new(
                self.canonical(&text_or_binary(data)),
                Certainty::Guess,
                DecidedBy::Fallback,
            )
        })
    }

    /// The type of a file named `name` holding `data`, by the spec's
    /// recommended order: its name, when all its matching patterns give one
    /// type; empty data is `application/x-zerosize`; otherwise the content's
    /// type (magic, else text-or-binary) decides: a name match that is that
    /// type or a subclass of it wins (certain when magic gave the type, a
    /// guess when text-or-binary did), else the best name match (a guess),
    /// else the content's type. No I/O; the first
    /// [`MimeDatabase::sniff_len`] bytes of a file are enough.
    #[must_use]
    pub fn type_for_name_and_data(&self, name: impl AsRef<OsStr>, data: &[u8]) -> Detection {
        let names = self.type_for_name(name);
        if let Some(best) = names.certain() {
            return best;
        }
        if data.is_empty() {
            return Detection::new(
                self.fixed("application/x-zerosize"),
                Certainty::Certain,
                DecidedBy::Content,
            );
        }
        self.name_and_content(&names, data)
    }

    /// Name matches that did not decide alone, weighed against `data`.
    fn name_and_content(&self, names: &NameMatches, data: &[u8]) -> Detection {
        // The content's type: magic, else text-or-binary (spec "Recommended
        // checking order"); a name that is that type or a subclass of it is
        // preferred, so text content picks a text type among names that
        // disagree.
        let content = self.magic_type(data);
        let (sniffed, certainty) = match &content {
            Some(found) => (found.mime.clone(), Certainty::Certain),
            None => (self.canonical(&text_or_binary(data)), Certainty::Guess),
        };
        let fitting = names
            .matches()
            .iter()
            .find(|found| self.is_a(&found.mime, &sniffed));
        match (fitting, names.best()) {
            (Some(found), _) => Detection::new(found.mime.clone(), certainty, DecidedBy::Name),
            (None, Some(best)) => Detection {
                certainty: Certainty::Guess,
                ..best
            },
            (None, None) => content
                .unwrap_or_else(|| Detection::new(sniffed, Certainty::Guess, DecidedBy::Fallback)),
        }
    }

    /// The type of the file at `path`, by the spec's recommended order.
    ///
    /// The file's kind is checked first (`lstat`, and `stat` for a followed
    /// link): anything but a regular file gets its `inode/*` type without
    /// being opened, so FIFOs and devices are never read. A regular file:
    /// its extended attribute (when asked for); its name, when all its
    /// matching patterns give one type; empty is `application/x-zerosize`; otherwise
    /// up to [`MimeDatabase::sniff_len`] bytes are read and weighed with the
    /// name as [`MimeDatabase::type_for_name_and_data`] does.
    ///
    /// # Errors
    ///
    /// If the file's metadata cannot be read, or its contents cannot be read
    /// when needed to determine its type. A broken link is not an error:
    /// it is `inode/symlink`.
    pub fn type_for_path(
        &self,
        path: impl AsRef<Path>,
        options: &PathOptions,
    ) -> io::Result<Detection> {
        let path = path.as_ref();
        let kind = |text| {
            Ok(Detection::new(
                self.fixed(text),
                Certainty::Certain,
                DecidedBy::FileKind,
            ))
        };
        let mut metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            if !options.follow_links {
                return kind("inode/symlink");
            }
            match fs::metadata(path) {
                Ok(target) => metadata = target,
                Err(err) if matches!(err.kind(), io::ErrorKind::NotFound) => {
                    return kind("inode/symlink");
                }
                Err(err) if err.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) => {
                    return kind("inode/symlink");
                }
                Err(err) => return Err(err),
            }
        }
        let file_type = metadata.file_type();
        if file_type.is_dir() {
            if options.mount_points && is_mount_point(path, &metadata) {
                return kind("inode/mount-point");
            }
            return kind("inode/directory");
        }
        if file_type.is_block_device() {
            return kind("inode/blockdevice");
        }
        if file_type.is_char_device() {
            return kind("inode/chardevice");
        }
        if file_type.is_fifo() {
            return kind("inode/fifo");
        }
        if file_type.is_socket() {
            return kind("inode/socket");
        }

        if options.extended_attribute
            && let Some(mime) = attribute_type(path)
        {
            return Ok(Detection::new(
                self.canonical(&mime),
                Certainty::Certain,
                DecidedBy::Attribute,
            ));
        }
        let names = path
            .file_name()
            .map(|name| self.type_for_name(name))
            .unwrap_or_default();
        if let Some(best) = names.certain() {
            return Ok(best);
        }
        if metadata.len() == 0 {
            return Ok(Detection::new(
                self.fixed("application/x-zerosize"),
                Certainty::Certain,
                DecidedBy::Content,
            ));
        }
        let data = read_head(path, self.sniff_len())?;
        Ok(self.name_and_content(&names, &data))
    }
}

/// Whether the folder `path` (with `metadata`) is on another device than
/// its parent, or is its own parent (`/`).
fn is_mount_point(path: &Path, metadata: &fs::Metadata) -> bool {
    match fs::metadata(path.join("..")) {
        Ok(parent) => parent.dev() != metadata.dev() || parent.ino() == metadata.ino(),
        Err(_) => false,
    }
}

/// The `user.mime_type` extended attribute, if set to a valid type.
fn attribute_type(path: &Path) -> Option<MimeType> {
    let mut buffer = [0u8; 256];
    let len = rustix::fs::getxattr(path, "user.mime_type", &mut buffer[..]).ok()?;
    attribute_value(buffer.get(..len)?)
}

/// A `user.mime_type` value: a type, with a trailing NUL or whitespace
/// allowed (tools differ in whether they write one).
fn attribute_value(value: &[u8]) -> Option<MimeType> {
    let value = std::str::from_utf8(value).ok()?;
    MimeType::new(value.trim_end_matches(['\0', ' ', '\t', '\n', '\r'])).ok()
}

/// Up to `len` bytes from the start of the regular file `path`. Opened
/// non-blocking and checked again after opening, so a file swapped for a
/// FIFO in between is not waited on.
fn read_head(path: &Path, len: usize) -> io::Result<Vec<u8>> {
    use rustix::fs::{Mode, OFlags};
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let file = File::from(fd);
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no longer a regular file",
        ));
    }
    let size = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    let mut data = Vec::with_capacity(len.min(size));
    file.take(len as u64).read_to_end(&mut data)?;
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matchlet(start: u32, range: u32, value: &[u8], mask: Option<&[u8]>) -> Matchlet {
        Matchlet {
            start,
            range,
            word_size: 1,
            value: value.to_vec(),
            mask: mask.map(<[u8]>::to_vec),
            children: Vec::new(),
        }
    }

    #[test]
    fn values_are_compared_over_the_range_with_masks() {
        assert!(compare(&matchlet(0, 1, b"PK", None), b"PK\x03\x04"));
        assert!(!compare(&matchlet(1, 1, b"PK", None), b"PK\x03\x04"));
        assert!(compare(&matchlet(0, 4, b"PK", None), b"..PK"));
        assert!(
            !compare(&matchlet(0, 4, b"PK", None), b"..P"),
            "data too short"
        );
        assert!(compare(&matchlet(0, 1, b"\xa0", Some(b"\xf0")), b"\xab"));
        assert!(!compare(&matchlet(0, 1, b"\xa0", Some(b"\xf0")), b"\xbb"));
        assert!(!compare(&matchlet(u32::MAX, u32::MAX, b"x", None), b"xxxx"));
    }

    #[test]
    fn nested_matchlets_need_one_child() {
        let mut parent = matchlet(0, 1, b"A", None);
        parent.children = vec![matchlet(1, 1, b"B", None), matchlet(1, 1, b"C", None)];
        assert!(matchlet_matches(&parent, b"AC"));
        assert!(!matchlet_matches(&parent, b"AD"));
        assert!(!matchlet_matches(&parent, b"XB"));
    }

    #[test]
    fn attribute_values_allow_a_trailing_nul_or_newline() {
        let value = |bytes: &[u8]| attribute_value(bytes).map(|mime| mime.to_string());
        assert_eq!(value(b"text/plain").as_deref(), Some("text/plain"));
        assert_eq!(value(b"text/plain\0").as_deref(), Some("text/plain"));
        assert_eq!(value(b"image/png\n").as_deref(), Some("image/png"));
        assert_eq!(value(b"not a type"), None);
        assert_eq!(value(b"\xff/\xfe"), None);
        assert_eq!(value(b""), None);
    }

    #[test]
    fn the_fallback_looks_for_control_characters_in_128_bytes() {
        assert_eq!(text_or_binary(b"plain\ttext\r\n").as_str(), "text/plain");
        assert_eq!(text_or_binary("héllo".as_bytes()).as_str(), "text/plain");
        assert_eq!(
            text_or_binary(b"bin\x00ary").as_str(),
            "application/octet-stream"
        );
        let mut late = vec![b'a'; 128];
        late.push(0);
        assert_eq!(text_or_binary(&late).as_str(), "text/plain");
    }
}
