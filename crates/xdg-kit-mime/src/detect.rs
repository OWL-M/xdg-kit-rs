//! Detecting a type from a name, content, or a file (spec
//! "Recommended checking order", "Non-regular files", "Storing the MIME type
//! using Extended Attributes").

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::BorrowedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;

use crate::database::{IndexedGlob, lowercase};
use crate::fnmatch::Name;
use crate::model::Matchlet;
use crate::work::count;
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
    /// Type a symbolic link by what it points to; a broken link (one whose
    /// target cannot be reached through it: missing, through a non-folder,
    /// in a loop of links, or past a length limit) is `inode/symlink`. When
    /// off, every link is `inode/symlink`. On by default.
    pub follow_links: bool,
    /// Report a folder on another device than its parent (and `/`) as
    /// `inode/mount-point`, at the cost of a `stat` of the parent. On by
    /// default. Bind mounts of the same filesystem are not detected; btrfs
    /// subvolumes and snapshots give their own device number, so they are
    /// reported as mount points too.
    pub mount_points: bool,
    /// Use a regular file's `user.mime_type` extended attribute when it holds
    /// a valid type. Off by default, as GNOME and KDE do not read it. Read
    /// on Linux, Android, Apple systems and Hurd; elsewhere this crate cannot
    /// read extended attributes, and the option finds nothing.
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
        // Finding the last `/`, then lowercasing (checking and copying),
        // then checking UTF-8 again.
        count(4 * name.len() + 1);
        let lower = lowercase(name);
        // A name that is not UTF-8 only matches ASCII patterns.
        let utf8 = std::str::from_utf8(name).is_ok();
        let globs = &self.globs;
        let compared =
            |glob: &IndexedGlob| -> &[u8] { if glob.case_sensitive { name } else { &lower } };

        let mut found: Vec<usize> = Vec::new();
        for key in [name, lower.as_slice()] {
            // Checked as UTF-8, then hashed and compared.
            count(3 * key.len());
            let Ok(key) = std::str::from_utf8(key) else {
                continue;
            };
            for &at in globs.literal.get(key).into_iter().flatten() {
                count(1 + key.len());
                if compared(&globs.all[at]) == globs.all[at].compared.as_bytes() {
                    found.push(at);
                }
            }
        }
        if found.is_empty() {
            for source in [name, lower.as_slice()] {
                // Only the tails as long as some pattern's tail can match.
                for &len in globs
                    .suffix_lens
                    .iter()
                    .take_while(|&&len| len <= source.len())
                {
                    count(1 + len);
                    let Ok(tail) = std::str::from_utf8(&source[source.len() - len..]) else {
                        continue;
                    };
                    if !utf8 && !tail.is_ascii() {
                        continue;
                    }
                    // Checked as ASCII, then hashed and compared.
                    count(3 * len);
                    for &at in globs.suffix.get(tail).into_iter().flatten() {
                        let glob = &globs.all[at];
                        count(1 + len);
                        if compared(glob).ends_with(&glob.compared.as_bytes()[1..]) {
                            found.push(at);
                        }
                    }
                }
            }
        }
        if found.is_empty() {
            // Each name checked as UTF-8 and split into characters.
            count(2 * (name.len() + lower.len()) + globs.full.len());
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
            count(1);
            let (a, b) = (&globs.all[a], &globs.all[b]);
            b.weight
                .cmp(&a.weight)
                .then(b.compared_len.cmp(&a.compared_len))
                .then(a.folder.cmp(&b.folder))
                .then(a.order.cmp(&b.order))
        });
        // A pattern found under both the name and the name lowercased is
        // kept once: the order ends on folder and place in it, which no two
        // patterns share, so the two are next to each other.
        count(found.len());
        found.dedup();
        NameMatches {
            matches: found
                .into_iter()
                .map(|at| {
                    let glob = &globs.all[at];
                    // The pattern copied.
                    count(1 + glob.pattern.len());
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
        let mut key = crate::xml::root_element_within(data, self.limits())?;
        let namespaces = &self.merged.namespaces;
        // The key changed in place, not copied: the namespace may be as
        // large as memory allows.
        let mut found = namespaces.get(&key);
        if found.is_none() {
            let local = std::mem::take(&mut key.1);
            found = namespaces.get(&key);
            if found.is_none() {
                key = (String::new(), local);
                found = namespaces.get(&key);
            }
        }
        found.map(|mime| self.canonical(mime))
    }

    /// Resolves a built-in type such as `text/plain` or `inode/directory`
    /// through the database. The database may define it as an alias;
    /// detection results use the canonical type.
    fn fixed(&self, text: &'static str) -> MimeType {
        self.canonical(&known(text))
    }

    /// The type of content: magic, then (for XML) the root element, then
    /// text-or-binary from the first 128 bytes. Empty data is
    /// `application/x-zerosize`. Performs no I/O.
    ///
    /// For a file whose reported size is nonzero, supplying the first
    /// [`MimeDatabase::sniff_len`] bytes gives the content-based type used
    /// by [`MimeDatabase::type_for_path`]. For a regular file with a reported
    /// size of 0 (as for most files under `/proc`), `type_for_path` reads
    /// nothing. It returns `application/x-zerosize` from the size alone
    /// unless all matching name patterns give one type, or an extended
    /// attribute decides the type when that option is enabled.
    ///
    /// The prefix is not always enough: it misses magic matches beyond
    /// 64 KiB and XML root elements beyond the prefix.
    ///
    /// Reads an XML root element as UTF-8, or as UTF-16 if the document
    /// starts with its byte order mark. If the declared encoding conflicts
    /// with the mark, or declares UTF-16 without a mark, keeps the type
    /// found by magic without a root-element match. Other encodings are
    /// not decoded: without a mark, the document is read as UTF-8, which
    /// handles only its ASCII text correctly. Ignores the whole
    /// declaration if it violates XML's grammar: `version`, then optional
    /// `encoding`, then optional `standalone`, each appearing once with
    /// a valid value. Namespace defaults from the document's internal subset
    /// apply to the root as written declarations do (XML 1.0, 3.3.2). For
    /// written or defaulted `xmlns` or `xmlns:` values declared with a type
    /// other than `CDATA`, leading and trailing spaces are removed and runs
    /// of spaces collapse to one (3.3.3).
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
    /// else the content's type. Performs no I/O.
    ///
    /// For a regular file whose reported size is nonzero, supplying its
    /// name and first [`MimeDatabase::sniff_len`] bytes gives the same
    /// result as [`MimeDatabase::type_for_path`] without the extended
    /// attribute. For a reported size of 0 (as for most files under `/proc`),
    /// `type_for_path` returns `application/x-zerosize` from the size alone
    /// when the name does not decide.
    ///
    /// The prefix is not always enough: it misses magic matches beyond
    /// 64 KiB and XML root elements beyond the prefix.
    ///
    /// Reads an XML root element as UTF-8, or as UTF-16 if the document
    /// starts with its byte order mark. If the declared encoding conflicts
    /// with the mark, or declares UTF-16 without a mark, keeps the type
    /// found by magic without a root-element match. Other encodings are
    /// not decoded: without a mark, the document is read as UTF-8, which
    /// handles only its ASCII text correctly. Ignores the whole
    /// declaration if it violates XML's grammar: `version`, then optional
    /// `encoding`, then optional `standalone`, each appearing once with
    /// a valid value. Namespace defaults from the document's internal subset
    /// apply to the root as written declarations do (XML 1.0, 3.3.2). For
    /// written or defaulted `xmlns` or `xmlns:` values declared with a type
    /// other than `CDATA`, leading and trailing spaces are removed and runs
    /// of spaces collapse to one (3.3.3).
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
    /// when needed to determine its type. A broken link (as defined for
    /// [`PathOptions::follow_links`]) is not an error: it is
    /// `inode/symlink`. A link whose target cannot be looked up for another
    /// reason, such as permission denied or an I/O error, is an error.
    pub fn type_for_path(
        &self,
        path: impl AsRef<Path>,
        options: &PathOptions,
    ) -> io::Result<Detection> {
        let path = path.as_ref();
        self.type_at(rustix::fs::CWD, path, *options, || attribute_type(path))
    }

    /// The type of the entry `name` in the open folder `folder`: what
    /// [`MimeDatabase::type_for_path`] gives for the folder's path joined
    /// with `name`, with the default options, but looked up from the
    /// folder, so that the way to the folder does not count against the
    /// limits on links and path length.
    pub(crate) fn type_in(&self, folder: BorrowedFd<'_>, name: &OsStr) -> io::Result<Detection> {
        // The default options leave the extended attribute off.
        self.type_at(folder, Path::new(name), PathOptions::default(), || None)
    }

    /// [`MimeDatabase::type_for_path`] for `path` looked up from the folder
    /// `dir` (the current folder for a plain path); `attribute` reads the
    /// extended attribute when the options ask for it.
    fn type_at(
        &self,
        dir: BorrowedFd<'_>,
        path: &Path,
        options: PathOptions,
        attribute: impl FnOnce() -> Option<MimeType>,
    ) -> io::Result<Detection> {
        let kind = |text| {
            Ok(Detection::new(
                self.fixed(text),
                Certainty::Certain,
                DecidedBy::FileKind,
            ))
        };
        let mut metadata = rustix::fs::statat(dir, path, AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(metadata.st_mode) == FileType::Symlink {
            if !options.follow_links {
                return kind("inode/symlink");
            }
            match rustix::fs::statat(dir, path, AtFlags::empty()) {
                Ok(target) => metadata = target,
                // The target cannot be reached through this path at all.
                Err(Errno::NOENT | Errno::NOTDIR | Errno::LOOP | Errno::NAMETOOLONG) => {
                    return kind("inode/symlink");
                }
                Err(err) => return Err(err.into()),
            }
        }
        let file_type = FileType::from_raw_mode(metadata.st_mode);
        if file_type == FileType::Directory {
            if options.mount_points && is_mount_point(dir, path, &metadata) {
                return kind("inode/mount-point");
            }
            return kind("inode/directory");
        }
        if file_type == FileType::BlockDevice {
            return kind("inode/blockdevice");
        }
        if file_type == FileType::CharacterDevice {
            return kind("inode/chardevice");
        }
        if file_type == FileType::Fifo {
            return kind("inode/fifo");
        }
        if file_type == FileType::Socket {
            return kind("inode/socket");
        }

        if options.extended_attribute
            && let Some(mime) = attribute()
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
        if metadata.st_size == 0 {
            return Ok(Detection::new(
                self.fixed("application/x-zerosize"),
                Certainty::Certain,
                DecidedBy::Content,
            ));
        }
        let data = read_head(dir, path, self.sniff_len())?;
        Ok(self.name_and_content(&names, &data))
    }
}

/// Whether the folder `path` from `dir` (with `metadata`) is on another
/// device than its parent, or is its own parent (`/`).
fn is_mount_point(dir: BorrowedFd<'_>, path: &Path, metadata: &Stat) -> bool {
    match rustix::fs::statat(dir, path.join(".."), AtFlags::empty()) {
        Ok(parent) => parent.st_dev != metadata.st_dev || parent.st_ino == metadata.st_ino,
        Err(_) => false,
    }
}

/// The `user.mime_type` extended attribute, if set to a valid type.
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "hurd",
    target_vendor = "apple"
))]
fn attribute_type(path: &Path) -> Option<MimeType> {
    let mut buffer = [0u8; 256];
    let len = rustix::fs::getxattr(path, "user.mime_type", &mut buffer[..]).ok()?;
    attribute_value(buffer.get(..len)?)
}

/// Where rustix reads no extended attributes, there is none to use.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "hurd",
    target_vendor = "apple"
)))]
fn attribute_type(_path: &Path) -> Option<MimeType> {
    None
}

/// A `user.mime_type` value: a type, with a trailing NUL or whitespace
/// allowed (tools differ in whether they write one).
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "hurd",
    target_vendor = "apple"
))]
fn attribute_value(value: &[u8]) -> Option<MimeType> {
    let value = std::str::from_utf8(value).ok()?;
    MimeType::new(value.trim_end_matches(['\0', ' ', '\t', '\n', '\r'])).ok()
}

/// Up to `len` bytes from the start of the regular file `path` from `dir`.
/// Opened non-blocking and checked again after opening, so a file swapped
/// for a FIFO in between is not waited on.
fn read_head(dir: BorrowedFd<'_>, path: &Path, len: usize) -> io::Result<Vec<u8>> {
    let fd = rustix::fs::openat(
        dir,
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
    use std::fs;

    use super::*;
    use crate::work::{assert_linear, steps};

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

    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "hurd",
        target_vendor = "apple"
    ))]
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
    fn a_name_in_a_folder_is_typed_as_its_path() {
        use std::os::fd::AsFd;
        use std::os::unix::fs::symlink;
        let db_dir = tempfile::tempdir().unwrap();
        fs::write(
            db_dir.path().join("globs2"),
            "50:text/x-diff:*.diff\n50:image/png:*.png\n50:text/x-c:*.c\n50:text/x-h:*.c\n",
        )
        .unwrap();
        fs::write(
            db_dir.path().join("magic"),
            b"MIME-Magic\0\n[50:image/png]\n>0=\0\x04\x89PNG\n",
        )
        .unwrap();
        let db = MimeDatabase::from_dirs([db_dir.path()]);
        assert!(db.diagnostics().is_empty(), "{:?}", db.diagnostics());

        let files = tempfile::tempdir().unwrap();
        let path = |name: &str| files.path().join(name);
        for (name, content) in [
            ("a.diff", b"diff".as_slice()),
            ("plain", b"words"),
            ("binary", b"\x00\x01"),
            ("picture", b"\x89PNG\r\n"),
            ("picture.diff", b"\x89PNG\r\n"),
            ("both.c", b"int x;"),
            ("empty", b""),
        ] {
            fs::write(path(name), content).unwrap();
        }
        fs::write(files.path().join(OsStr::from_bytes(b"\xffname.diff")), "x").unwrap();
        fs::create_dir(path("folder")).unwrap();
        symlink("a.diff", path("link")).unwrap();
        symlink("folder", path("folder-link")).unwrap();
        symlink("missing", path("broken")).unwrap();
        symlink("loop", path("loop")).unwrap();
        // `mknodat` is not in rustix on Apple systems.
        #[cfg(not(target_vendor = "apple"))]
        rustix::fs::mknodat(
            rustix::fs::CWD,
            path("fifo"),
            rustix::fs::FileType::Fifo,
            Mode::RUSR | Mode::WUSR,
            0,
        )
        .unwrap();
        let _listener = std::os::unix::net::UnixListener::bind(path("socket")).unwrap();

        let open = |path: &Path| {
            let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
            rustix::fs::open(path, flags, Mode::empty()).unwrap()
        };
        let agree = |folder: &Path, name: &OsStr| {
            let by_handle = db.type_in(open(folder).as_fd(), name);
            let by_path = db.type_for_path(folder.join(name), &PathOptions::default());
            assert_eq!(
                by_handle.map_err(|err| err.kind()),
                by_path.map_err(|err| err.kind()),
                "{}",
                folder.join(name).display()
            );
        };
        let mut names: Vec<_> = fs::read_dir(files.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.push("missing".into());
        assert_eq!(
            names.len(),
            if cfg!(target_vendor = "apple") {
                15
            } else {
                16
            }
        );
        for name in &names {
            agree(files.path(), name);
        }
        // Mount points, links and folders the system has.
        for entry in fs::read_dir("/").unwrap() {
            agree(Path::new("/"), &entry.unwrap().file_name());
        }
    }

    /// A database of one folder whose `globs2` holds `lines`, and the
    /// file's length.
    fn globs_database(lines: impl Iterator<Item = String>) -> (MimeDatabase, usize) {
        let text: String = lines.map(|line| line + "\n").collect();
        let db = MimeDatabase::from_files(&[vec![("globs2", text.as_bytes())]]);
        (db, text.len())
    }

    #[test]
    fn the_rules_a_name_matches_are_gathered_in_linear_work() {
        // `n` patterns `same` (or `*.same`), each of its own type: each is
        // found under the name and the name lowercased, then sorted, once.
        // Per pattern: two finds (a step and the name compared each), a few
        // steps sorting, a step dropping the second and the pattern copied
        // (about 21, or 25 for `*.same`), then its type made canonical (a
        // step and two per byte: `text/x-t` and up to four digits, about
        // 24).
        for (pattern, name, per) in [("same", "same", 45), ("*.same", "X.same", 49)] {
            let run = |n: usize| {
                let (db, bytes) =
                    globs_database((0..n).map(|at| format!("50:text/x-t{at}:{pattern}")));
                let (found, steps) = steps(|| db.type_for_name(name));
                assert_eq!(found.matches().len(), n, "{pattern}");
                (steps, bytes)
            };
            assert_linear(1_000, run, |items, _| 2 * (per * items + 100));
        }
    }

    #[test]
    fn a_long_name_is_matched_in_linear_work() {
        // Few short suffixes: a long name is looked up by its tails of
        // their lengths only.
        let (db, _) = globs_database((0..50).map(|at| format!("50:text/x-t{at}:*.e{at}")));
        let run = |n: usize| {
            let name = "a".repeat(n);
            let (found, steps) = steps(|| db.type_for_name(&name));
            assert!(found.is_empty());
            (steps, n)
        };
        // The name read, lowercased, looked up whole and split for the
        // other patterns: a few steps per byte.
        assert_linear(10_000, run, |_, bytes| 2 * (14 * bytes + 100));
        // Long suffixes, eight of them, each a tail of the name: checked,
        // looked up and compared for each of the name and its lowercased
        // copy, about ten steps per byte of the suffixes.
        let run = |n: usize| {
            let (db, bytes) = globs_database(
                (1..=8).map(|at| format!("50:text/x-t{at}:*{}.x", "a".repeat(at * n / 8))),
            );
            let name = format!("{}.x", "a".repeat(n));
            let (found, steps) = steps(|| db.type_for_name(&name));
            assert_eq!(found.matches().len(), 8);
            (steps, bytes + name.len())
        };
        assert_linear(10_000, run, |_, bytes| 2 * (11 * bytes + 100));
    }

    #[test]
    fn the_patterns_of_a_type_are_listed_in_linear_work() {
        // One type with `n` patterns, each kept once.
        let run = |n: usize| {
            let (db, bytes) = globs_database((0..n).map(|at| format!("50:text/x-a:*.p{at}")));
            let mime = MimeType::new("text/x-a").unwrap();
            let (patterns, steps) = steps(|| db.patterns_for(&mime));
            assert_eq!(patterns.len(), n);
            (steps, bytes)
        };
        // Per pattern: a step, the pattern hashed, compared and copied;
        // each line of the file is over three times its pattern.
        assert_linear(1_000, run, |items, bytes| 2 * (items + bytes));
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
