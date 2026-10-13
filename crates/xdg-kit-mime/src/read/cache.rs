//! Decodes `mime.cache` (spec "The mime.cache files") into the same model
//! as the text files.
//!
//! Treats the file as untrusted. Checks every offset, count and string
//! against its length, and limits recursion to
//! [`Limits::max_rule_depth`] and [`MAX_SUFFIX_LEN`]. The file size bounds
//! the number of suffix tree nodes and matchlets visited, preventing loops
//! from offsets that point back into the tree. With the default
//! [`Limits`], it also bounds the bytes copied and time spent finding
//! strings, so reading costs time and memory in proportion to the file
//! size. Reports at most [`Limits::max_diagnostics`] problems per file.

use std::path::Path;

use super::Problems;
use crate::model::{Folder, Glob, MagicRule, Matchlet, NO_GLOBS, NO_MAGIC};
use crate::{Diagnostic, Limits, Location, MimeType};

/// Longest suffix pattern read from the suffix tree, in characters, less
/// the `*`; the tree is read recursively, a level per character.
const MAX_SUFFIX_LEN: usize = 256;

/// The major version this reader understands.
const MAJOR: u16 = 1;
/// The newest minor version this reader supports explicitly. Version 1.1
/// has the same layout and is read as 1.2. Its weight words contain only
/// the weight, so the case-sensitive flag, absent in 1.1, reads as unset.
/// Version 1.0 has a shorter header and smaller entries and is not read.
const MINOR: u16 = 2;

/// Why a `mime.cache` file is not used at all.
#[derive(Debug)]
pub(crate) enum Unusable {
    /// A major version other than 1, or 1.0.
    Version(u16, u16),
    /// Too short, or the header points outside the file.
    Damaged(&'static str, usize),
}

impl std::fmt::Display for Unusable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Version(major, minor) => write!(f, "unsupported version {major}.{minor}"),
            Self::Damaged(what, at) => write!(f, "damaged: {what} at offset {at}"),
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    problems: Problems<'a>,
    /// Nodes and matchlets still allowed to be visited.
    budget: usize,
    /// Bytes still allowed to be copied out of the file (`None`: any). See
    /// [`Limits::cache_copy_per_byte`].
    byte_budget: Option<usize>,
    /// The longest string read ([`Limits::max_cache_string`]).
    max_string: Option<usize>,
    /// The deepest matchlet nesting read ([`Limits::max_rule_depth`]).
    max_depth: usize,
    /// Whether the byte budget has run out (reported once).
    exhausted: bool,
    /// Matchlets dropped so far because none of their children could be
    /// read.
    childless: usize,
}

impl Reader<'_> {
    fn u32_at(&self, at: usize) -> Option<u32> {
        let bytes = self.bytes.get(at..at.checked_add(4)?)?;
        Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn offset_at(&self, at: usize) -> Option<usize> {
        self.u32_at(at).map(|value| value as usize)
    }

    /// A zero-terminated string starting at `at`, of at most
    /// [`Limits::max_cache_string`] bytes.
    fn bytes_at(&self, at: usize) -> Option<&[u8]> {
        let rest = self.bytes.get(at..)?;
        let rest = match self.max_string {
            Some(max) => &rest[..rest.len().min(max.saturating_add(1))],
            None => rest,
        };
        let end = rest.iter().position(|&byte| byte == 0)?;
        Some(&rest[..end])
    }

    fn str_at(&self, at: usize) -> Option<&str> {
        std::str::from_utf8(self.bytes_at(at)?).ok()
    }

    /// The string at `at`, copied, if the byte budget allows it.
    fn take_str(&mut self, at: usize) -> Option<String> {
        let len = self.str_at(at)?.len();
        if !self.charge(at, len) {
            return None;
        }
        self.str_at(at).map(str::to_owned)
    }

    /// Spends `len` bytes of the copy budget; `false` (reported once) when
    /// it runs out.
    fn charge(&mut self, at: usize, len: usize) -> bool {
        let Some(byte_budget) = self.byte_budget else {
            return true;
        };
        if self.exhausted || len > byte_budget {
            if !self.exhausted {
                self.exhausted = true;
                self.report(at, "more data than the file can hold, rest skipped");
            }
            // Nothing more is copied, and visiting stops too.
            self.budget = 0;
            return false;
        }
        self.byte_budget = Some(byte_budget - len);
        true
    }

    fn report(&mut self, at: usize, message: impl Into<String>) {
        self.problems.report(Some(Location::Offset(at)), message);
    }

    fn mime_at(&mut self, at: usize) -> Option<MimeType> {
        let Some(offset) = self.offset_at(at) else {
            self.report(at, "offset outside the file, entry skipped");
            return None;
        };
        let len = self.str_at(offset).map_or(0, str::len);
        if !self.charge(offset, len) {
            return None;
        }
        let parsed = self.str_at(offset).map(MimeType::new);
        if let Some(Ok(mime)) = parsed {
            Some(mime)
        } else {
            self.report(offset, "not a MIME type, entry skipped");
            None
        }
    }

    /// A list: a count at `at`, then `count` entries of `size` bytes. `None`
    /// (reported) if it does not fit in the file.
    fn list(&mut self, at: usize, size: usize, what: &str) -> Option<(usize, usize)> {
        let Some(count) = self.offset_at(at) else {
            self.report(at, format!("{what} count outside the file, skipped"));
            return None;
        };
        let start = at + 4;
        let fits = count
            .checked_mul(size)
            .and_then(|len| start.checked_add(len))
            .is_some_and(|end| end <= self.bytes.len());
        if fits {
            Some((count, start))
        } else {
            self.report(at, format!("{what} runs past the end, skipped"));
            None
        }
    }

    fn spend(&mut self, at: usize) -> bool {
        if self.budget == 0 {
            self.report(at, "more nodes than the file can hold, rest skipped");
            return false;
        }
        self.budget -= 1;
        true
    }
}

/// Reads `bytes`, the contents of `path`, into `folder`, within `limits`.
pub(crate) fn read(
    path: &Path,
    bytes: &[u8],
    folder: &mut Folder,
    diagnostics: &mut Vec<Diagnostic>,
    limits: &Limits,
) -> Result<(), Unusable> {
    if bytes.len() < 40 {
        return Err(Unusable::Damaged("header too short", 0));
    }
    let major = u16::from_be_bytes([bytes[0], bytes[1]]);
    let minor = u16::from_be_bytes([bytes[2], bytes[3]]);
    if major != MAJOR || minor == 0 {
        return Err(Unusable::Version(major, minor));
    }
    let mut reader = Reader {
        bytes,
        problems: Problems::new(path, diagnostics, limits.max_diagnostics),
        // Each node and matchlet takes at least 12 bytes.
        budget: bytes.len() / 12 + 1,
        byte_budget: limits.cache_copy_per_byte.map(|per_byte| {
            bytes
                .len()
                .saturating_mul(per_byte)
                .saturating_add(limits.cache_copy_extra)
        }),
        max_string: limits.max_cache_string,
        max_depth: limits.rule_depth(),
        exhausted: false,
        childless: 0,
    };
    if minor > MINOR {
        reader.report(
            2,
            format!("minor version {minor} is newer than {MINOR}; read as {MAJOR}.{MINOR}"),
        );
    }
    let header: Vec<usize> = (0..9)
        .map(|index| reader.offset_at(4 + 4 * index).unwrap_or(usize::MAX))
        .collect();
    if let Some(index) = header.iter().position(|&offset| offset >= bytes.len()) {
        return Err(Unusable::Damaged(
            "header offset outside the file",
            4 + 4 * index,
        ));
    }
    let [
        alias,
        parent,
        literal,
        suffix,
        glob,
        magic,
        namespace,
        icons,
        generic_icons,
    ] = header[..]
    else {
        unreachable!("nine offsets");
    };

    folder.aliases = read_pairs(&mut reader, alias, "alias list");
    read_parents(&mut reader, parent, folder);
    read_globs(
        &mut reader,
        literal,
        "literal list",
        std::borrow::ToOwned::to_owned,
        folder,
    );
    read_suffixes(&mut reader, suffix, folder);
    read_globs(
        &mut reader,
        glob,
        "glob list",
        std::borrow::ToOwned::to_owned,
        folder,
    );
    read_magic(&mut reader, magic, folder);
    read_namespaces(&mut reader, namespace, folder);
    folder.icons = read_icons(&mut reader, icons, "icon list");
    folder.generic_icons = read_icons(&mut reader, generic_icons, "generic icon list");
    folder.max_extent = folder.magic_extent();
    Ok(())
}

/// `AliasList`: (alias, type) pairs.
fn read_pairs(reader: &mut Reader<'_>, at: usize, what: &str) -> Vec<(MimeType, MimeType)> {
    let Some((count, start)) = reader.list(at, 8, what) else {
        return Vec::new();
    };
    let mut pairs = Vec::with_capacity(count);
    for index in 0..count {
        let entry = start + 8 * index;
        if let (Some(first), Some(second)) = (reader.mime_at(entry), reader.mime_at(entry + 4)) {
            pairs.push((first, second));
        }
    }
    pairs
}

/// `ParentList`: each type with its `Parents` list.
fn read_parents(reader: &mut Reader<'_>, at: usize, folder: &mut Folder) {
    let Some((count, start)) = reader.list(at, 8, "parent list") else {
        return;
    };
    for index in 0..count {
        let entry = start + 8 * index;
        let Some(mime) = reader.mime_at(entry) else {
            continue;
        };
        let Some(parents_at) = reader.offset_at(entry + 4) else {
            continue;
        };
        let Some((n_parents, parents_start)) = reader.list(parents_at, 4, "parents") else {
            continue;
        };
        for parent in 0..n_parents {
            // Many types can point at one long list: count every entry.
            if !reader.spend(parents_start + 4 * parent) {
                return;
            }
            if let Some(parent) = reader.mime_at(parents_start + 4 * parent) {
                folder.parents.push((mime.clone(), parent));
            }
        }
    }
}

/// A glob's weight and flags field: weight in the low 8 bits, `0x100` for
/// case-sensitive.
fn weight_and_case(field: u32) -> (u32, bool) {
    ((field & 0xff).min(100), field & 0x100 != 0)
}

fn add_glob(folder: &mut Folder, pattern: String, mime: MimeType, field: u32) {
    if pattern == NO_GLOBS {
        folder.no_globs.push(mime);
    } else {
        let (weight, case_sensitive) = weight_and_case(field);
        folder.globs.push(Glob {
            weight,
            pattern,
            mime,
            case_sensitive,
        });
    }
}

/// `LiteralList` and `GlobList`: (pattern, type, weight and flags).
fn read_globs(
    reader: &mut Reader<'_>,
    at: usize,
    what: &str,
    pattern_of: fn(&str) -> String,
    folder: &mut Folder,
) {
    let Some((count, start)) = reader.list(at, 12, what) else {
        return;
    };
    for index in 0..count {
        let entry = start + 12 * index;
        let pattern = reader
            .offset_at(entry)
            .and_then(|offset| reader.take_str(offset))
            .map(|pattern| pattern_of(&pattern));
        let Some(pattern) = pattern.filter(|pattern| !pattern.is_empty()) else {
            reader.report(entry, "bad pattern, entry skipped");
            continue;
        };
        let Some(mime) = reader.mime_at(entry + 4) else {
            continue;
        };
        let Some(field) = reader.u32_at(entry + 8) else {
            continue;
        };
        add_glob(folder, pattern, mime, field);
    }
}

/// `ReverseSuffixTree`: each path from a root to a leaf spells a simple
/// pattern `*xyz` backwards.
fn read_suffixes(reader: &mut Reader<'_>, at: usize, folder: &mut Folder) {
    let (Some(count), Some(first)) = (reader.offset_at(at), reader.offset_at(at + 4)) else {
        reader.report(at, "suffix tree header outside the file, skipped");
        return;
    };
    let mut reversed = Vec::new();
    read_suffix_nodes(reader, count, first, &mut reversed, folder);
}

fn read_suffix_nodes(
    reader: &mut Reader<'_>,
    count: usize,
    first: usize,
    reversed: &mut Vec<char>,
    folder: &mut Folder,
) {
    if reversed.len() > MAX_SUFFIX_LEN {
        reader.report(first, "suffix tree too deep, branch skipped");
        return;
    }
    let fits = count
        .checked_mul(12)
        .and_then(|len| first.checked_add(len))
        .is_some_and(|end| end <= reader.bytes.len());
    if !fits {
        reader.report(first, "suffix tree nodes run past the end, skipped");
        return;
    }
    for index in 0..count {
        let node = first + 12 * index;
        if !reader.spend(node) {
            return;
        }
        let (Some(character), Some(second), Some(third)) = (
            reader.u32_at(node),
            reader.u32_at(node + 4),
            reader.u32_at(node + 8),
        ) else {
            return;
        };
        if character == 0 {
            // A leaf: `second` is the type, `third` the weight and flags.
            let Some(mime) = reader.mime_at(node + 4) else {
                continue;
            };
            // The pattern's UTF-8 length, as the byte budget is documented.
            let len = 1 + reversed.iter().map(|c| c.len_utf8()).sum::<usize>();
            if !reader.charge(node, len) {
                return;
            }
            let pattern: String = std::iter::once('*')
                .chain(reversed.iter().rev().copied())
                .collect();
            add_glob(folder, pattern, mime, third);
            continue;
        }
        let Some(character) = char::from_u32(character) else {
            reader.report(node, "not a Unicode character, branch skipped");
            continue;
        };
        reversed.push(character);
        read_suffix_nodes(reader, second as usize, third as usize, reversed, folder);
        reversed.pop();
    }
}

/// `MagicList`: matches with their matchlet trees. A damaged matchlet fails
/// closed: one with children of which none could be read is dropped, and
/// that passes up the tree; a match left with no matchlets is dropped.
fn read_magic(reader: &mut Reader<'_>, at: usize, folder: &mut Folder) {
    // The header's MAX_EXTENT is not trusted: the extent is worked out from
    // the matchlets read, as for the text files (see `read`).
    let (Some(count), Some(_max_extent), Some(first)) = (
        reader.offset_at(at),
        reader.u32_at(at + 4),
        reader.offset_at(at + 8),
    ) else {
        reader.report(at, "magic list header outside the file, skipped");
        return;
    };
    let fits = count
        .checked_mul(16)
        .and_then(|len| first.checked_add(len))
        .is_some_and(|end| end <= reader.bytes.len());
    if !fits {
        reader.report(at, "magic list runs past the end, skipped");
        return;
    }
    for index in 0..count {
        let entry = first + 16 * index;
        let (Some(priority), Some(n_matchlets), Some(matchlets_at)) = (
            reader.u32_at(entry),
            reader.offset_at(entry + 8),
            reader.offset_at(entry + 12),
        ) else {
            continue;
        };
        let Some(mime) = reader.mime_at(entry + 4) else {
            continue;
        };
        let childless = reader.childless;
        let matchlets = read_matchlets(reader, n_matchlets, matchlets_at, 0);
        if matchlets.is_empty() && reader.childless > childless {
            reader.report(
                entry,
                "no matchlet left whole (one lost all its children), entry skipped",
            );
        }
        let marker = |matchlet: &Matchlet| {
            matchlet.start == 0 && matchlet.value == NO_MAGIC && matchlet.children.is_empty()
        };
        let mut rule = MagicRule {
            priority: priority.min(100),
            mime,
            matchlets,
        };
        if rule.matchlets.iter().any(marker) {
            folder.no_magic.push(rule.mime.clone());
            rule.matchlets.retain(|matchlet| !marker(matchlet));
        }
        if !rule.matchlets.is_empty() {
            folder.magic.push(rule);
        }
    }
}

fn read_matchlets(
    reader: &mut Reader<'_>,
    count: usize,
    first: usize,
    depth: usize,
) -> Vec<Matchlet> {
    // A leaf's empty child list nests nothing, whatever its depth.
    if count > 0 && depth > reader.max_depth {
        reader.report(first, "magic nested too deep, branch skipped");
        return Vec::new();
    }
    let fits = count
        .checked_mul(32)
        .and_then(|len| first.checked_add(len))
        .is_some_and(|end| end <= reader.bytes.len());
    if !fits {
        reader.report(first, "matchlets run past the end, skipped");
        return Vec::new();
    }
    let mut matchlets = Vec::with_capacity(count.min(64));
    for index in 0..count {
        let at = first + 32 * index;
        if !reader.spend(at) {
            break;
        }
        let fields: Vec<u32> = (0..8)
            .filter_map(|field| reader.u32_at(at + 4 * field))
            .collect();
        let [
            start,
            range,
            word_size,
            value_len,
            value_at,
            mask_at,
            n_children,
            children_at,
        ] = fields[..]
        else {
            continue;
        };
        // A length that overflows is more than any budget.
        let copied = copied_len(value_len as usize, mask_at != 0).unwrap_or(usize::MAX);
        if !reader.charge(at, copied) {
            break;
        }
        let value = reader
            .bytes
            .get(value_at as usize..(value_at as usize).saturating_add(value_len as usize))
            .map(<[u8]>::to_vec);
        let mask = if mask_at == 0 {
            Some(None)
        } else {
            reader
                .bytes
                .get(mask_at as usize..(mask_at as usize).saturating_add(value_len as usize))
                .map(|mask| Some(mask.to_vec()))
        };
        let (Some(value), Some(mask)) = (value, mask) else {
            reader.report(at, "matchlet value or mask outside the file, skipped");
            continue;
        };
        let children = read_matchlets(reader, n_children as usize, children_at as usize, depth + 1);
        // Fails closed: without its children the matchlet would match more
        // than the rule says.
        if n_children > 0 && children.is_empty() {
            reader.childless += 1;
            continue;
        }
        matchlets.push(Matchlet {
            start,
            range,
            word_size,
            value,
            mask,
            children,
        });
    }
    matchlets
}

/// The bytes a matchlet copies: its value of `value_len` bytes, and as
/// many again for its mask if `masked`. `None` when that overflows `usize`
/// (on 32-bit targets).
fn copied_len(value_len: usize, masked: bool) -> Option<usize> {
    value_len.checked_mul(if masked { 2 } else { 1 })
}

/// `NamespaceList`: (namespace URI, local name, type).
fn read_namespaces(reader: &mut Reader<'_>, at: usize, folder: &mut Folder) {
    let Some((count, start)) = reader.list(at, 12, "namespace list") else {
        return;
    };
    for index in 0..count {
        let entry = start + 12 * index;
        let namespace = reader
            .offset_at(entry)
            .and_then(|offset| reader.take_str(offset));
        let local = reader
            .offset_at(entry + 4)
            .and_then(|offset| reader.take_str(offset));
        let (Some(namespace), Some(local)) = (namespace, local) else {
            reader.report(entry, "bad namespace entry, skipped");
            continue;
        };
        if let Some(mime) = reader.mime_at(entry + 8) {
            folder.namespaces.push((namespace, local, mime));
        }
    }
}

/// `IconsList` and `GenericIconsList`: (type, icon name).
fn read_icons(reader: &mut Reader<'_>, at: usize, what: &str) -> Vec<(MimeType, String)> {
    let Some((count, start)) = reader.list(at, 8, what) else {
        return Vec::new();
    };
    let mut icons = Vec::with_capacity(count);
    for index in 0..count {
        let entry = start + 8 * index;
        let Some(mime) = reader.mime_at(entry) else {
            continue;
        };
        let icon = reader
            .offset_at(entry + 4)
            .and_then(|offset| reader.take_str(offset));
        match icon.filter(|icon| !icon.is_empty()) {
            Some(icon) => icons.push((mime, icon)),
            None => reader.report(entry, "bad icon name, entry skipped"),
        }
    }
    icons
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::testing;

    const PACKAGE: &str = r#"<?xml version="1.0"?>
<mime-info xmlns="http://www.freedesktop.org/standards/shared-mime-info">
  <mime-type type="text/x-test">
    <alias type="text/x-old-test"/>
    <sub-class-of type="text/plain"/>
    <glob pattern="*.test"/><glob pattern="Testfile"/><glob pattern="t?st.*"/>
    <magic priority="70"><match type="string" offset="0:4" value="TEST"><match type="byte" offset="5" value="1"/></match></magic>
    <root-XML namespaceURI="urn:test" localName="test"/>
    <generic-icon name="text-x-generic"/>
  </mime-type>
</mime-info>"#;

    fn cache_bytes() -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let mime = testing::build(dir.path(), &[("test.xml", PACKAGE)]);
        std::fs::read(mime.join("mime.cache")).unwrap()
    }

    fn decode(bytes: &[u8]) -> (Result<(), Unusable>, Folder, Vec<Diagnostic>) {
        let mut folder = Folder::default();
        let mut diagnostics = Vec::new();
        let result = read(
            Path::new("mime.cache"),
            bytes,
            &mut folder,
            &mut diagnostics,
            &Limits::default(),
        );
        (result, folder, diagnostics)
    }

    #[test]
    fn a_whole_cache_is_read_without_complaint() {
        let (result, folder, diagnostics) = decode(&cache_bytes());
        assert!(result.is_ok() && diagnostics.is_empty(), "{diagnostics:?}");
        let mut patterns: Vec<_> = folder.globs.iter().map(|g| g.pattern.as_str()).collect();
        patterns.sort_unstable();
        assert_eq!(patterns, ["*.test", "t?st.*", "testfile"]);
        assert_eq!(folder.magic[0].matchlets[0].range, 5);
        assert_eq!(folder.magic[0].matchlets[0].children.len(), 1);
        assert_eq!(folder.aliases.len(), 1);
        assert_eq!(folder.namespaces.len(), 1);
        assert_eq!(folder.generic_icons.len(), 1);
    }

    #[test]
    fn unknown_versions_are_refused_or_reported() {
        let mut bytes = cache_bytes();
        bytes[1] = 2;
        assert!(matches!(decode(&bytes).0, Err(Unusable::Version(2, _))));
        let mut bytes = cache_bytes();
        bytes[3] = 9;
        let (result, _, diagnostics) = decode(&bytes);
        assert!(result.is_ok());
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    }

    /// The 1.0 and 1.1 caches in `tests/old-writers` were written by
    /// `update-mime-database` 0.30 and 0.60.
    #[test]
    fn version_1_0_is_refused_and_1_1_read_as_1_2() {
        let mut bytes = cache_bytes();
        bytes[3] = 0;
        assert!(matches!(decode(&bytes).0, Err(Unusable::Version(1, 0))));
        let old = include_bytes!("../../tests/old-writers/0.30/mime.cache");
        assert!(matches!(decode(old).0, Err(Unusable::Version(1, 0))));

        let (result, folder, diagnostics) =
            decode(include_bytes!("../../tests/old-writers/0.60/mime.cache"));
        assert!(result.is_ok() && diagnostics.is_empty(), "{diagnostics:?}");
        let mut globs: Vec<_> = folder
            .globs
            .iter()
            .map(|g| {
                (
                    g.pattern.as_str(),
                    g.mime.as_str(),
                    g.weight,
                    g.case_sensitive,
                )
            })
            .collect();
        globs.sort_unstable();
        assert_eq!(
            globs,
            [
                ("*.aa", "text/x-a", 50, false),
                ("*.bb", "text/x-b", 50, false),
                ("Afile", "text/x-a", 50, false),
                ("Bfile", "text/x-b", 50, false),
                ("a*z", "text/x-a", 50, false),
                ("b*z", "text/x-b", 50, false),
            ]
        );
    }

    /// A version 1.2 cache of `len` zero bytes, its nine header offsets
    /// `others` except the `list`th, which is `at`.
    fn header_only(len: usize, list: usize, at: usize, others: usize) -> Vec<u8> {
        let mut bytes = vec![0; len];
        bytes[..4].copy_from_slice(&[0, 1, 0, 2]);
        for index in 0..9 {
            set_u32(
                &mut bytes,
                4 + 4 * index,
                if index == list { at } else { others },
            );
        }
        bytes
    }

    #[test]
    fn a_list_count_outside_the_file_is_reported() {
        // The alias list's count at 59 of 60 bytes.
        let (result, folder, diagnostics) = decode(&header_only(60, 0, 59, 40));
        assert!(result.is_ok());
        assert_eq!(folder.aliases, []);
        assert_eq!(
            diagnostics,
            [Diagnostic::new(
                "mime.cache",
                Some(Location::Offset(59)),
                "alias list count outside the file, skipped"
            )]
        );
        // A type's parents list at 79 of 80 bytes.
        let mut bytes = header_only(80, 1, 40, 60);
        set_u32(&mut bytes, 40, 1);
        set_u32(&mut bytes, 44, 52);
        set_u32(&mut bytes, 48, 79);
        bytes[52..58].copy_from_slice(b"text/a");
        let (result, folder, diagnostics) = decode(&bytes);
        assert!(result.is_ok());
        assert_eq!(folder.parents, []);
        assert_eq!(
            diagnostics,
            [Diagnostic::new(
                "mime.cache",
                Some(Location::Offset(79)),
                "parents count outside the file, skipped"
            )]
        );
    }

    #[test]
    fn every_truncation_is_survived() {
        let bytes = cache_bytes();
        for len in 0..bytes.len() {
            let _ = decode(&bytes[..len]);
        }
    }

    #[test]
    fn wrecked_offsets_are_survived() {
        let bytes = cache_bytes();
        for at in (0..bytes.len()).step_by(4) {
            for value in [0u32, 1, 0xffff_ffff, u32::try_from(bytes.len()).unwrap(), 4] {
                let mut damaged = bytes.clone();
                let end = (at + 4).min(damaged.len());
                damaged[at..end].copy_from_slice(&value.to_be_bytes()[..end - at]);
                let _ = decode(&damaged);
            }
        }
    }

    fn u32_at(bytes: &[u8], at: usize) -> usize {
        u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
    }

    fn set_u32(bytes: &mut [u8], at: usize, value: usize) {
        bytes[at..at + 4].copy_from_slice(&u32::try_from(value).unwrap().to_be_bytes());
    }

    fn copied(matchlets: &[Matchlet]) -> usize {
        matchlets
            .iter()
            .map(|m| m.value.len() + m.mask.as_ref().map_or(0, Vec::len) + copied(&m.children))
            .sum()
    }

    /// A matchlet that is its own child, with a value as long as the file:
    /// without a byte budget, memory grows with the square of the file size.
    #[test]
    fn a_matchlet_that_copies_the_whole_file_again_and_again_is_bounded() {
        let mut bytes = cache_bytes();
        bytes.resize(100 * 1024, 0);
        let len = bytes.len();
        let magic = u32_at(&bytes, 24);
        let first_match = u32_at(&bytes, magic + 8);
        let matchlet = u32_at(&bytes, first_match + 12);
        set_u32(&mut bytes, matchlet + 12, len - 1); // value length
        set_u32(&mut bytes, matchlet + 16, 0); // value offset
        set_u32(&mut bytes, matchlet + 20, 0); // no mask
        set_u32(&mut bytes, matchlet + 24, 1); // one child...
        set_u32(&mut bytes, matchlet + 28, matchlet); // ...itself
        let (result, folder, diagnostics) = decode(&bytes);
        assert!(result.is_ok());
        let total: usize = folder
            .magic
            .iter()
            .map(|rule| copied(&rule.matchlets))
            .sum();
        assert!(
            total <= 8 * len + 64 * 1024,
            "{total} bytes copied from a {len}-byte file"
        );
        assert!(
            diagnostics.len() <= crate::read::MAX_DIAGNOSTICS + 1,
            "{} diagnostics",
            diagnostics.len()
        );
        assert_ne!(diagnostics, []);
    }

    #[test]
    fn a_copied_length_that_overflows_is_refused() {
        assert_eq!(copied_len(7, false), Some(7));
        assert_eq!(copied_len(7, true), Some(14));
        assert_eq!(copied_len(usize::MAX, false), Some(usize::MAX));
        assert_eq!(copied_len(usize::MAX, true), None);
        assert_eq!(copied_len(usize::MAX / 2 + 1, true), None);
    }

    /// Value and mask together overflow `usize` on 32-bit targets. A guard
    /// only: on 64-bit the sum does not overflow and the length is refused
    /// as more than the file holds, fixed or not.
    /// `a_copied_length_that_overflows_is_refused` tells the two apart.
    #[test]
    fn a_masked_value_of_2_gib_is_refused() {
        let mut bytes = cache_bytes();
        let magic = u32_at(&bytes, 24);
        let first_match = u32_at(&bytes, magic + 8);
        let matchlet = u32_at(&bytes, first_match + 12);
        set_u32(&mut bytes, matchlet + 12, 0x8000_0000); // value length
        set_u32(&mut bytes, matchlet + 20, 4); // a mask
        let (result, folder, diagnostics) = decode(&bytes);
        assert!(result.is_ok());
        assert!(folder.magic.is_empty(), "{:?}", folder.magic);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("more data than the file can hold")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn a_suffix_tree_that_points_back_at_itself_ends() {
        let mut bytes = cache_bytes();
        let tree = u32::from_be_bytes(bytes[16..20].try_into().unwrap()) as usize;
        let first = u32::from_be_bytes(bytes[tree + 4..tree + 8].try_into().unwrap()) as usize;
        // The first root claims one child: itself.
        bytes[first + 4..first + 8].copy_from_slice(&1u32.to_be_bytes());
        bytes[first + 8..first + 12].copy_from_slice(&u32::try_from(first).unwrap().to_be_bytes());
        let (result, _, diagnostics) = decode(&bytes);
        assert!(result.is_ok());
        assert_ne!(diagnostics, []);
    }

    #[test]
    fn a_suffix_is_charged_its_utf8_length() {
        // `*😀😀` is 9 bytes, and `a/b` 3: with no budget per byte, 12
        // extra bytes read it and 11 do not: its two characters are charged
        // their 8 bytes, not as two.
        let dir = tempfile::tempdir().unwrap();
        let package = r#"<?xml version="1.0"?>
<mime-info xmlns="http://www.freedesktop.org/standards/shared-mime-info">
  <mime-type type="a/b"><glob pattern="*😀😀"/></mime-type>
</mime-info>"#;
        let mime = testing::build(dir.path(), &[("p.xml", package)]);
        let bytes = std::fs::read(mime.join("mime.cache")).unwrap();
        for (extra, read_it) in [(12, true), (11, false)] {
            let limits = Limits {
                cache_copy_per_byte: Some(0),
                cache_copy_extra: extra,
                ..Limits::default()
            };
            let mut folder = Folder::default();
            let mut diagnostics = Vec::new();
            let _ = read(
                Path::new("mime.cache"),
                &bytes,
                &mut folder,
                &mut diagnostics,
                &limits,
            );
            let found = folder.globs.iter().any(|glob| glob.pattern == "*😀😀");
            assert_eq!(found, read_it, "{extra}: {diagnostics:?}");
        }
    }
}
