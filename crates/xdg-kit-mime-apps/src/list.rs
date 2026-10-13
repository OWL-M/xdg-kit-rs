//! One `mimeapps.list` file (MIME Applications Associations spec "File name
//! and location"), read and edited without losing anything: comments, order
//! and unknown groups stay as written.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::DirBuilder;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use xdg_kit_basedir::BaseDirs;
use xdg_kit_desktop_entry::{KeyFile, KeyNames};
use xdg_kit_mime::MimeDatabase;

use crate::error::{quoted, quoted_pair};
use crate::{Diagnostic, Limits, TypePattern, work};

/// The name of a file that may hold every group; desktop-specific ones
/// (`$desktop-mimeapps.list`) may hold only the defaults.
pub const MIMEAPPS_LIST: &str = "mimeapps.list";

/// One of the spec's three groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Group {
    /// `[Default Applications]`.
    Defaults,
    /// `[Added Associations]`.
    Added,
    /// `[Removed Associations]`.
    Removed,
}

impl Group {
    /// The group's header name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Defaults => "Default Applications",
            Self::Added => "Added Associations",
            Self::Removed => "Removed Associations",
        }
    }
}

/// Why an edit was refused; the file is then unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EditError {
    /// Not a desktop file ID: it must end in `.desktop`, not start with white
    /// space, and hold no `;`, `/`, line break or other control character.
    DesktopId(String),
    /// Added and removed associations belong only in files named
    /// `mimeapps.list`, not in desktop-specific ones.
    DesktopSpecific,
    /// The key file refused the edit.
    KeyFile(xdg_kit_desktop_entry::EditError),
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DesktopId(id) => write!(f, "\"{}\" is not a desktop file ID", quoted(id)),
            Self::DesktopSpecific => write!(
                f,
                "a desktop-specific mimeapps.list holds only default applications"
            ),
            Self::KeyFile(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for EditError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::KeyFile(error) => Some(error),
            _ => None,
        }
    }
}

impl From<xdg_kit_desktop_entry::EditError> for EditError {
    fn from(error: xdg_kit_desktop_entry::EditError) -> Self {
        Self::KeyFile(error)
    }
}

/// The spec's three groups, in the order they are read.
const GROUPS: [Group; 3] = [Group::Defaults, Group::Added, Group::Removed];

/// Whether `id` can be written as a desktop file ID in a list.
fn valid_desktop_id(id: &str) -> bool {
    // The Desktop Entry spec ("Desktop File ID") turns each `/` of the path
    // under `applications` into `-`. Looked through for one.
    valid_but_for_slashes(id) && {
        work::count(id.len());
        !id.contains('/')
    }
}

/// Whether `id` is valid for a list except for containing `/`. Such an
/// ID can still be removed if it is already listed (see
/// [`MimeAppsList::remove_association`]).
fn valid_but_for_slashes(id: &str) -> bool {
    // Looked through once; the ends are compared with fixed names.
    work::count(id.len());
    id.len() > ".desktop".len()
        && id.ends_with(".desktop")
        // Reading trims an ID, so one starting with white space would come
        // back as another.
        && !id.starts_with(char::is_whitespace)
        && !id.chars().any(|c| c == ';' || c.is_control())
}

/// Whether `known` is the ID `id`: a step, and one per byte compared when
/// their lengths are the same (texts of different lengths differ without a
/// look at their bytes).
fn same_id(known: &str, id: &str) -> bool {
    work::count(1 + if known.len() == id.len() { id.len() } else { 0 });
    known == id
}

/// Whether `ids` has `id`, each compared with it.
fn contains(ids: &[String], id: &str) -> bool {
    ids.iter().any(|known| same_id(known, id))
}

/// The keys of each group that are type patterns, under the pattern each
/// resolves to, as spelled in the file, in file order.
type KeyIndex = HashMap<(Group, TypePattern), Vec<String>>;

/// One `mimeapps.list` file.
///
/// An edit ([`set_default`](MimeAppsList::set_default),
/// [`add_association`](MimeAppsList::add_association),
/// [`remove_association`](MimeAppsList::remove_association),
/// [`forget`](MimeAppsList::forget)) does constant amortized work in the
/// rest of the file, work linear in the length of the entries it edits (the
/// type's IDs are read, and each line is written again whole, as `GLib`
/// writes it), and log n work, n the number of group headers in the file
/// (for the line numbers the key file keeps). So adding n IDs to one type,
/// one at a time, is quadratic in that type's entry, and linear for n
/// types.
#[derive(Debug, Clone)]
pub struct MimeAppsList {
    file: KeyFile,
    path: Option<PathBuf>,
    desktop_specific: bool,
    /// When set, keys naming an alias of a type count as that type.
    database: Option<Arc<MimeDatabase>>,
    /// The keys each type has in each group, kept in step with `file` by
    /// every edit, so that an edit finds a type's keys without looking at
    /// the group's other keys.
    keys: KeyIndex,
}

impl MimeAppsList {
    /// The user's own file, `$XDG_CONFIG_HOME/mimeapps.list`: the spec's
    /// "recommended location for user configuration GUIs".
    ///
    /// # Errors
    ///
    /// When the file exists but cannot be read.
    pub fn for_user(base_dirs: &BaseDirs) -> io::Result<Self> {
        Self::for_user_with(base_dirs, &Limits::default())
    }

    /// [`MimeAppsList::for_user`] within `limits`.
    ///
    /// # Errors
    ///
    /// When the file exists but cannot be read, or is larger than
    /// [`Limits::max_file_len`].
    pub fn for_user_with(base_dirs: &BaseDirs, limits: &Limits) -> io::Result<Self> {
        Self::open_with(base_dirs.config_home().join(MIMEAPPS_LIST), limits)
    }

    /// Reads the file at `path`. A missing file gives an empty list;
    /// [`MimeAppsList::write`] creates the file. A file not named
    /// `mimeapps.list` counts as desktop-specific. Converts a relative path
    /// to an absolute path now, without following links, so later changes to
    /// the working directory do not change its location.
    ///
    /// # Errors
    ///
    /// If `path` cannot be made absolute because it is empty or the current
    /// directory is gone. The list needs a fixed location for writing it back.
    /// Also fails if the file exists but cannot be read.
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        Self::open_with(path, &Limits::default())
    }

    /// [`MimeAppsList::open`] within `limits`.
    ///
    /// # Errors
    ///
    /// When `path` cannot be made absolute, as for [`MimeAppsList::open`].
    /// When the file exists but cannot be read, or is larger than
    /// [`Limits::max_file_len`].
    pub fn open_with(path: impl Into<PathBuf>, limits: &Limits) -> io::Result<Self> {
        // Absolute now, so that a later change of folder does not move it.
        let path = std::path::absolute(path.into())?;
        // Each byte of the path copied.
        work::count(path.as_os_str().len());
        let desktop_specific = path.file_name().is_some_and(|name| name != MIMEAPPS_LIST);
        let file = match KeyFile::read_with_limits(&path, KeyNames::Any, &limits.for_entries()) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                KeyFile::parse_with(b"", KeyNames::Any)
            }
            Err(err) => return Err(err),
        };
        Ok(Self {
            file,
            path: Some(path),
            desktop_specific,
            database: None,
            keys: KeyIndex::new(),
        }
        .indexed())
    }

    /// Parses `bytes` as a `mimeapps.list` (`desktop_specific` for a
    /// `$desktop-mimeapps.list`).
    #[must_use]
    pub fn parse(bytes: &[u8], desktop_specific: bool) -> Self {
        Self {
            file: KeyFile::parse_with(bytes, KeyNames::Any),
            path: None,
            desktop_specific,
            database: None,
            keys: KeyIndex::new(),
        }
        .indexed()
    }

    /// Uses `database` to resolve aliases, so edits also reach keys that
    /// name an alias of the type, and an ID added under one name and
    /// removed under an alias is reported as both added and removed (see
    /// [`MimeAppsList::diagnostics`]). This matches how
    /// [`MimeApps`](crate::MimeApps) reads them. Without it, keys match
    /// only when they name the same type, ignoring case.
    #[must_use]
    pub fn resolving_aliases(mut self, database: Arc<MimeDatabase>) -> Self {
        self.database = Some(database);
        self.indexed()
    }

    /// The same list with its keys indexed by the type they resolve to.
    fn indexed(mut self) -> Self {
        let mut keys = KeyIndex::new();
        for group in GROUPS {
            for (key, pattern) in self.patterns(group) {
                let resolved = self.resolve(&pattern);
                // The type hashed and compared, and the key copied.
                work::count(2 * resolved.text_len() + key.len());
                keys.entry((group, resolved))
                    .or_default()
                    .push(key.to_owned());
            }
        }
        self.keys = keys;
        self
    }

    /// The file this list belongs to, if any.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Whether this is a desktop-specific file, which may hold only default
    /// applications.
    #[must_use]
    pub fn is_desktop_specific(&self) -> bool {
        self.desktop_specific
    }

    /// The desktop file IDs `group` lists for `pattern`, in order. Every key
    /// naming the type counts, in file order: one spelled in another case,
    /// or as an alias (see [`MimeAppsList::resolving_aliases`]).
    #[must_use]
    pub fn ids(&self, group: Group, pattern: &TypePattern) -> Vec<String> {
        let mut ids: Vec<String> = Vec::new();
        let mut seen = HashSet::new();
        for key in self.keys_for(group, pattern) {
            // A step per key.
            work::count(1);
            for id in self.items(group, key) {
                // Copied, hashed and compared for the repeat check.
                work::count(1 + 3 * id.len());
                if seen.insert(id.clone()) {
                    ids.push(id);
                }
            }
        }
        ids
    }

    /// Every type pattern `group` has an entry for, with its desktop file
    /// IDs, in file order. Keys that are not type patterns are left out
    /// (see [`MimeAppsList::diagnostics`]).
    #[must_use]
    pub fn entries(&self, group: Group) -> Vec<(TypePattern, Vec<String>)> {
        self.patterns(group)
            .into_iter()
            .map(|(key, pattern)| (pattern, self.items(group, key)))
            .collect()
    }

    /// The keys of `group` that are type patterns, as spelled in the file,
    /// with their patterns, in order.
    fn patterns(&self, group: Group) -> Vec<(&str, TypePattern)> {
        self.file
            .keys(group.name())
            .into_iter()
            .inspect(|_| work::count(1))
            .filter(|(_, locale)| locale.is_none())
            .filter_map(|(key, _)| TypePattern::parse(key).map(|pattern| (key, pattern)))
            .collect()
    }

    fn items(&self, group: Group, key: &str) -> Vec<String> {
        self.file
            .list(group.name(), key)
            .unwrap_or_default()
            .into_iter()
            // Trimmed and copied.
            .inspect(|item| work::count(1 + 2 * item.len()))
            .map(|item| item.trim().to_owned())
            .filter(|item| !item.is_empty())
            .collect()
    }

    /// The keys `group` uses for `pattern`, as spelled in the file, in
    /// order.
    fn keys_for(&self, group: Group, pattern: &TypePattern) -> &[String] {
        let wanted = self.resolve(pattern);
        // Hashed and compared.
        work::count(2 * wanted.text_len());
        self.keys.get(&(group, wanted)).map_or(&[], Vec::as_slice)
    }

    fn resolve(&self, pattern: &TypePattern) -> TypePattern {
        // Its alias looked up (`TypePattern::canonical` counts), or copied.
        if let Some(database) = &self.database {
            pattern.canonical(database)
        } else {
            work::count(pattern.clone_steps());
            pattern.clone()
        }
    }

    /// Problems found when reading: those of the key file, keys that are not
    /// type patterns, IDs with a `/` (kept as written; a desktop file ID has
    /// `-` in its place), associations in a desktop-specific file
    /// (ignored), and an ID both added and removed for one type (the spec:
    /// invalid; the addition counts, as it is read first), however the keys
    /// spell the type: in another case, or, when [resolving
    /// aliases](MimeAppsList::resolving_aliases), as an alias.
    ///
    /// Line numbers are those of the text as it is now (after any edits),
    /// counted from 1, as in [`KeyFile::diagnostics`]: a diagnostic about a
    /// key points to the line holding that key as written; key-file
    /// diagnostics keep the key file's line numbers; a diagnostic about a
    /// whole group in a desktop-specific file points to the group's header.
    #[must_use]
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        let path_len = self.path.as_ref().map_or(0, |path| path.as_os_str().len());
        let mut found: Vec<Diagnostic> = self
            .file
            .diagnostics()
            .iter()
            // Copied, with the path.
            .inspect(|diagnostic| work::count(1 + diagnostic.message.len() + path_len))
            .map(Diagnostic::from)
            .collect();
        let mut report = |group: Group, key: &str, locale: Option<&str>, message: String| {
            // The key looked up for its line, the message written, the path
            // copied.
            work::count(key.len() + message.len() + path_len);
            let line = self.file.line_of(group.name(), key, locale);
            found.push(Diagnostic::new(self.path.clone(), line, message));
        };
        for group in GROUPS {
            // A desktop-specific file's associations are reported as a whole.
            let read = group == Group::Defaults || !self.desktop_specific;
            for (key, locale) in self.file.keys(group.name()) {
                work::count(1);
                if let Some(locale) = locale {
                    // The key and locale copied into the key as written.
                    work::count(key.len() + locale.len());
                    report(
                        group,
                        key,
                        Some(locale),
                        format!(
                            "[{}] \"{}\": MIME type keys are not localized; ignored",
                            group.name(),
                            quoted(&format!("{key}[{locale}]"))
                        ),
                    );
                } else if TypePattern::parse(key).is_none() {
                    report(
                        group,
                        key,
                        None,
                        format!(
                            "[{}] \"{}\" is not a MIME type; ignored",
                            group.name(),
                            quoted(key)
                        ),
                    );
                } else if read {
                    let mut seen = HashSet::new();
                    for id in self.items(group, key) {
                        // Looked through for a `/`.
                        work::count(1 + id.len());
                        if !id.contains('/') {
                            continue;
                        }
                        // Copied, hashed and compared for the repeat check.
                        work::count(3 * id.len());
                        if seen.insert(id.clone()) {
                            // Copied with `-` in place of `/`.
                            work::count(2 * id.len());
                            let dashed = id.replace('/', "-");
                            // Both compared up to the first `/`, and gone
                            // through again to there to be cut: at most four
                            // steps per byte.
                            work::count(4 * id.len());
                            let (id, dashed) = quoted_pair(&id, &dashed);
                            report(
                                group,
                                key,
                                None,
                                format!(
                                    "[{}] \"{id}\" is not a desktop file ID (an ID has \"-\" in place of \"/\", as in \"{dashed}\"); kept as written",
                                    group.name(),
                                ),
                            );
                        }
                    }
                }
            }
        }
        if self.desktop_specific {
            for group in [Group::Added, Group::Removed] {
                if self.file.has_group(group.name()) {
                    work::count(path_len);
                    found.push(Diagnostic::new(
                        self.path.clone(),
                        self.file.group_line(group.name()),
                        format!(
                            "[{}] may only be in files named mimeapps.list; ignored",
                            group.name()
                        ),
                    ));
                }
            }
        } else {
            self.conflicts(&mut report);
        }
        found
    }

    /// Reports, with `report`, each ID both added and removed for one type,
    /// however the keys spell it, once per type and ID.
    fn conflicts(&self, report: &mut impl FnMut(Group, &str, Option<&str>, String)) {
        // The removed IDs of each type, however the keys spell it.
        let mut removed: HashMap<TypePattern, HashSet<String>> = HashMap::new();
        for (pattern, ids) in self.entries(Group::Removed) {
            let resolved = self.resolve(&pattern);
            // The type hashed and compared; each ID hashed and compared
            // into the set.
            work::count(2 * resolved.text_len());
            work::count_with(|| ids.iter().map(|id| 1 + 2 * id.len()).sum());
            removed.entry(resolved).or_default().extend(ids);
        }
        // Once per type and ID, however many spellings the key has.
        let mut reported = HashSet::new();
        for (key, pattern) in self.patterns(Group::Added) {
            let resolved = self.resolve(&pattern);
            // Hashed and compared.
            work::count(2 * resolved.text_len());
            let Some(removed) = removed.get(&resolved) else {
                continue;
            };
            for id in self.items(Group::Added, key) {
                // Hashed and compared to look it up.
                work::count(1 + 2 * id.len());
                if !removed.contains(&id) {
                    continue;
                }
                // The type and ID copied, hashed and compared.
                work::count(3 * (resolved.text_len() + id.len()));
                if !reported.insert((resolved.clone(), id.clone())) {
                    continue;
                }
                // The type written out to be shown.
                work::count(pattern.text_len());
                report(
                    Group::Added,
                    key,
                    None,
                    // A type has only safe characters, and is short.
                    format!(
                        "{} is both added and removed for {pattern}; the addition counts",
                        quoted(&id)
                    ),
                );
            }
        }
    }

    /// Makes `id` the first default application for `pattern`, keeping the
    /// others after it. In a file named `mimeapps.list` it also becomes the
    /// first added association (and is no longer removed): one file cannot
    /// see the others, so this makes sure the default is associated with
    /// the type, as the spec asks.
    ///
    /// The work is constant amortized in the rest of the file, linear in the
    /// length of the type's entries, and log n in the file's group headers
    /// (see [`MimeAppsList`]).
    ///
    /// # Errors
    ///
    /// When `id` is not a desktop file ID.
    pub fn set_default(&mut self, pattern: &TypePattern, id: &str) -> Result<(), EditError> {
        check_id(id)?;
        self.put_first(Group::Defaults, pattern, id)?;
        if !self.desktop_specific {
            self.put_first(Group::Added, pattern, id)?;
            self.drop_id(Group::Removed, pattern, id)?;
        }
        Ok(())
    }

    /// Associates `id` with `pattern`. Appends it to the added associations,
    /// or keeps its position if already listed, and removes it from the
    /// removed associations.
    ///
    /// The work is constant amortized in the rest of the file, linear in the
    /// length of the type's entries, and log n in the file's group headers
    /// (see [`MimeAppsList`]).
    ///
    /// # Errors
    ///
    /// When `id` is not a desktop file ID, or this file is desktop-specific.
    pub fn add_association(&mut self, pattern: &TypePattern, id: &str) -> Result<(), EditError> {
        check_id(id)?;
        if self.desktop_specific {
            return Err(EditError::DesktopSpecific);
        }
        let mut added = self.ids(Group::Added, pattern);
        if !contains(&added, id) {
            work::count(id.len());
            added.push(id.to_owned());
        }
        self.set_ids(Group::Added, pattern, &added)?;
        self.drop_id(Group::Removed, pattern, id)
    }

    /// Removes the association of `id` with `pattern`. Drops it from the
    /// added associations and defaults, and lists it as removed.
    ///
    /// An ID containing `/` is not a desktop file ID (see
    /// [`MimeAppsList::diagnostics`]), but is accepted if the file already
    /// lists that exact ID for the type in any group. It is deleted from
    /// the type's added associations, defaults and removed associations.
    /// No removal is recorded: no installed application has such an ID,
    /// so recording one would have no effect.
    ///
    /// The work is constant amortized in the rest of the file, linear in the
    /// length of the type's entries, and log n in the file's group headers
    /// (see [`MimeAppsList`]).
    ///
    /// # Errors
    ///
    /// When `id` is not a desktop file ID (and not such a listed one), or
    /// this file is desktop-specific.
    pub fn remove_association(&mut self, pattern: &TypePattern, id: &str) -> Result<(), EditError> {
        let listed = || {
            GROUPS
                .into_iter()
                .any(|group| contains(&self.ids(group, pattern), id))
        };
        let valid = valid_desktop_id(id);
        // Else only an ID valid but for its `/`s, and listed.
        let removable = valid || (valid_but_for_slashes(id) && listed());
        if !removable {
            return Err(EditError::DesktopId(id.to_owned()));
        }
        if self.desktop_specific {
            return Err(EditError::DesktopSpecific);
        }
        self.drop_id(Group::Added, pattern, id)?;
        self.drop_id(Group::Defaults, pattern, id)?;
        if !valid {
            return self.drop_id(Group::Removed, pattern, id);
        }
        let mut removed = self.ids(Group::Removed, pattern);
        if !contains(&removed, id) {
            work::count(id.len());
            removed.push(id.to_owned());
        }
        self.set_ids(Group::Removed, pattern, &removed)
    }

    /// Removes every entry for `pattern` from all three groups. Returns
    /// whether any entry was removed.
    ///
    /// The work is constant amortized in the rest of the file, linear in the
    /// length of the type's entries, and log n in the file's group headers
    /// (see [`MimeAppsList`]).
    pub fn forget(&mut self, pattern: &TypePattern) -> bool {
        let wanted = self.resolve(pattern);
        let mut found = false;
        for group in GROUPS {
            // Copied, hashed and compared.
            work::count(3 * wanted.text_len());
            for key in self
                .keys
                .remove(&(group, wanted.clone()))
                .unwrap_or_default()
            {
                work::count(1);
                found |= self.file.remove(group.name(), &key, None);
            }
        }
        found
    }

    /// The file's text: an unchanged file gives back exactly what was read.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        self.file.to_bytes()
    }

    /// Writes the list back to its file, atomically, creating missing
    /// parent folders with mode `0700`.
    ///
    /// # Errors
    ///
    /// When writing fails, or the list was parsed from bytes and has no file.
    pub fn write(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "this list was not read from a file",
            ));
        };
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty() && !parent.exists())
        {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        self.file.write(path)
    }

    /// Writes `ids` as `group`'s entry for `pattern`, under the first key
    /// the group has for it; other keys naming the type (in another case,
    /// or as an alias) are removed, so none of them keeps an old value. An
    /// empty list removes the entry. The keys come from the index, which
    /// this keeps in step, not from a look at the group's other keys.
    ///
    /// The key file refuses none of these edits: a key is a type pattern,
    /// which [`KeyNames::Any`] allows, and the items are written escaped. So
    /// an edit made of several is never left half done, and the file need
    /// not be copied first; a refusal would leave this entry unchanged.
    fn set_ids(
        &mut self,
        group: Group,
        pattern: &TypePattern,
        ids: &[String],
    ) -> Result<(), EditError> {
        let slot = (group, self.resolve(pattern));
        // Hashed and compared to take the keys, and again to put them back.
        work::count(4 * slot.1.text_len());
        let mut keys = self.keys.remove(&slot).unwrap_or_default();
        if ids.is_empty() {
            for key in &keys {
                work::count(1);
                self.file.remove(group.name(), key, None);
            }
            return Ok(());
        }
        // The first key, or the pattern written as a new one.
        let key = keys.first().cloned().unwrap_or_else(|| pattern.to_string());
        work::count(1 + key.len());
        if let Err(error) = self.file.set_list(group.name(), &key, ids) {
            self.keys.insert(slot, keys);
            return Err(error.into());
        }
        for other in keys.iter().skip(1) {
            work::count(1);
            self.file.remove(group.name(), other, None);
        }
        keys.truncate(1);
        if keys.is_empty() {
            keys.push(key);
        }
        self.keys.insert(slot, keys);
        Ok(())
    }

    /// Writes `id` first in `group`'s entry for `pattern`, the others after
    /// it.
    fn put_first(
        &mut self,
        group: Group,
        pattern: &TypePattern,
        id: &str,
    ) -> Result<(), EditError> {
        work::count(id.len());
        let mut ids = vec![id.to_owned()];
        for known in self.ids(group, pattern) {
            if !same_id(&known, id) {
                ids.push(known);
            }
        }
        self.set_ids(group, pattern, &ids)
    }

    /// Drops `id` from `group`'s entry for `pattern`, if it is there.
    fn drop_id(&mut self, group: Group, pattern: &TypePattern, id: &str) -> Result<(), EditError> {
        let mut ids = self.ids(group, pattern);
        let listed = ids.len();
        ids.retain(|known| !same_id(known, id));
        if ids.len() < listed {
            self.set_ids(group, pattern, &ids)?;
        }
        Ok(())
    }
}

fn check_id(id: &str) -> Result<(), EditError> {
    if valid_desktop_id(id) {
        Ok(())
    } else {
        Err(EditError::DesktopId(id.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(text: &str) -> TypePattern {
        TypePattern::parse(text).unwrap()
    }

    const SAMPLE: &str = "# mine\n[Default Applications]\ntext/plain=a.desktop;b.desktop;\n\n[Added Associations]\ntext/plain=b.desktop;\nimage/*=v.desktop;\n\n[Removed Associations]\nText/Plain=c.desktop;\n\n[X-Other]\nkeep=1\n";

    #[test]
    fn a_long_type_in_a_problem_is_shown_whole() {
        let long = "a".repeat(120);
        let text = format!(
            "[Added Associations]\ntext/{long}=a.desktop;\n\
             [Removed Associations]\ntext/{long}=a.desktop;\n"
        );
        let found = MimeAppsList::parse(text.as_bytes(), false).diagnostics();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            found[0].message,
            format!("a.desktop is both added and removed for text/{long}; the addition counts")
        );
    }

    #[test]
    fn keys_and_ids_in_problems_are_shown_escaped_and_cut() {
        // An escape character, a line feed once unescaped, and length.
        let bad = format!("\x1b\\n{}", "a".repeat(5000));
        let long = "a".repeat(5000);
        let text = format!(
            "[Default Applications]\n{bad}=a.desktop;\nx{long}=a.desktop;\n\
             text/plain[{bad}]=a.desktop;\ntext/plain=x/{bad}.desktop;\n\
             [Added Associations]\ntext/html={bad}.desktop;\n\
             [Removed Associations]\ntext/html={bad}.desktop;\n"
        );
        let mut list = MimeAppsList::parse(text.as_bytes(), false);
        let found = list.diagnostics();
        assert_eq!(found.len(), 6, "{found:?}");
        for diagnostic in &found {
            crate::error::assert_shown_safely(&diagnostic.message);
        }
        let refused = list.set_default(&p("text/plain"), &bad).unwrap_err();
        crate::error::assert_shown_safely(&refused.to_string());
        assert_eq!(
            refused.to_string(),
            format!(
                "\"\\u{{1b}}\\\\n{}…\" is not a desktop file ID",
                "a".repeat(29)
            )
        );
    }

    #[test]
    fn diagnostics_give_the_lines_of_the_text_as_it_is() {
        // `BAD` is no type pattern; `garbage` is no entry, which the key
        // file reports. Removing the line above both moves them up.
        let mut list = MimeAppsList::parse(
            b"[Added Associations]\ntext/plain=a.desktop;\nBAD=x;\ngarbage\n",
            false,
        );
        let lines = |list: &MimeAppsList| {
            let mut lines: Vec<(Option<usize>, bool)> = list
                .diagnostics()
                .into_iter()
                .map(|diagnostic| (diagnostic.line, diagnostic.message.contains("BAD")))
                .collect();
            lines.sort_unstable();
            lines
        };
        assert_eq!(lines(&list), [(Some(3), true), (Some(4), false)]);
        assert!(list.forget(&p("text/plain")));
        assert_eq!(list.to_bytes(), b"[Added Associations]\nBAD=x;\ngarbage\n");
        assert_eq!(lines(&list), [(Some(2), true), (Some(3), false)]);
    }

    #[test]
    fn reads_every_group() {
        let list = MimeAppsList::parse(SAMPLE.as_bytes(), false);
        assert_eq!(
            list.ids(Group::Defaults, &p("text/plain")),
            ["a.desktop", "b.desktop"]
        );
        assert_eq!(
            list.ids(Group::Removed, &p("text/plain")),
            ["c.desktop"],
            "any case"
        );
        assert_eq!(
            list.entries(Group::Added),
            [
                (p("text/plain"), vec!["b.desktop".to_owned()]),
                (p("image/*"), vec!["v.desktop".to_owned()])
            ]
        );
        assert!(list.diagnostics().is_empty(), "{:?}", list.diagnostics());
        assert_eq!(list.to_bytes(), SAMPLE.as_bytes(), "unchanged");
    }

    #[test]
    fn set_default_also_associates() {
        let mut list = MimeAppsList::parse(SAMPLE.as_bytes(), false);
        list.set_default(&p("text/plain"), "c.desktop").unwrap();
        assert_eq!(
            list.ids(Group::Defaults, &p("text/plain")),
            ["c.desktop", "a.desktop", "b.desktop"]
        );
        assert_eq!(
            list.ids(Group::Added, &p("text/plain")),
            ["c.desktop", "b.desktop"]
        );
        assert!(list.ids(Group::Removed, &p("text/plain")).is_empty());
        let text = String::from_utf8(list.to_bytes()).unwrap();
        assert!(text.starts_with("# mine\n"), "{text}");
        assert!(text.contains("[X-Other]\nkeep=1\n"), "{text}");
        assert!(
            !text.contains("Text/Plain"),
            "the emptied entry is gone: {text}"
        );
    }

    #[test]
    fn associations_move_between_added_and_removed() {
        let mut list = MimeAppsList::parse(SAMPLE.as_bytes(), false);
        list.remove_association(&p("text/plain"), "b.desktop")
            .unwrap();
        assert!(list.ids(Group::Added, &p("text/plain")).is_empty());
        assert_eq!(list.ids(Group::Defaults, &p("text/plain")), ["a.desktop"]);
        assert_eq!(
            list.ids(Group::Removed, &p("text/plain")),
            ["c.desktop", "b.desktop"]
        );
        list.add_association(&p("text/plain"), "c.desktop").unwrap();
        assert_eq!(list.ids(Group::Added, &p("text/plain")), ["c.desktop"]);
        assert_eq!(list.ids(Group::Removed, &p("text/plain")), ["b.desktop"]);
        list.add_association(&p("image/png"), "n.desktop").unwrap();
        assert_eq!(list.ids(Group::Added, &p("image/png")), ["n.desktop"]);
        assert!(list.forget(&p("text/plain")));
        assert!(!list.forget(&p("text/plain")));
        for group in [Group::Defaults, Group::Added, Group::Removed] {
            assert!(list.ids(group, &p("text/plain")).is_empty());
        }
    }

    #[test]
    fn edits_reach_every_spelling_of_a_type() {
        let text = "[Added Associations]\ntext/plain=a.desktop;\nTEXT/PLAIN=b.desktop;a.desktop;\n";
        let mut list = MimeAppsList::parse(text.as_bytes(), false);
        assert_eq!(
            list.ids(Group::Added, &p("text/plain")),
            ["a.desktop", "b.desktop"]
        );
        list.remove_association(&p("text/plain"), "a.desktop")
            .unwrap();
        assert_eq!(list.ids(Group::Added, &p("text/plain")), ["b.desktop"]);
        let text = String::from_utf8(list.to_bytes()).unwrap();
        assert!(!text.contains("TEXT/PLAIN"), "one key is left: {text}");
        list.add_association(&p("Text/Plain"), "c.desktop").unwrap();
        assert_eq!(
            list.ids(Group::Added, &p("text/plain")),
            ["b.desktop", "c.desktop"]
        );
        assert!(list.forget(&p("text/plain")));
        assert!(list.ids(Group::Added, &p("text/plain")).is_empty());
    }

    #[test]
    fn bad_edits_are_refused() {
        let mut list = MimeAppsList::parse(SAMPLE.as_bytes(), false);
        for bad in [
            "a",
            ".desktop",
            "a;b.desktop",
            "a\n.desktop",
            " a.desktop",
            "\ta.desktop",
            "kde/a.desktop",
            "/usr/share/applications/a.desktop",
        ] {
            assert_eq!(
                list.set_default(&p("text/plain"), bad),
                Err(EditError::DesktopId(bad.into()))
            );
        }
        let mut desktop = MimeAppsList::parse(b"", true);
        assert_eq!(
            desktop.add_association(&p("text/plain"), "a.desktop"),
            Err(EditError::DesktopSpecific)
        );
        desktop.set_default(&p("text/plain"), "a.desktop").unwrap();
        assert_eq!(
            desktop.to_bytes(),
            b"[Default Applications]\ntext/plain=a.desktop;\n"
        );
        assert_eq!(list.to_bytes(), SAMPLE.as_bytes(), "unchanged");
    }

    #[test]
    fn a_key_file_edit_error_is_the_source() {
        let refused = xdg_kit_desktop_entry::KeyFile::new()
            .set_list("bad]group", "key", &["a"])
            .unwrap_err();
        let error = EditError::from(refused.clone());
        assert_eq!(error.to_string(), refused.to_string());
        let source = std::error::Error::source(&error).expect("the key file's error");
        assert_eq!(source.to_string(), refused.to_string());
        assert!(std::error::Error::source(&EditError::DesktopSpecific).is_none());
    }

    #[test]
    fn problems_are_reported() {
        let list = MimeAppsList::parse(
            b"[Added Associations]\nnot a type=a.desktop;\ntext/plain=a.desktop;\n[Removed Associations]\ntext/plain=a.desktop;\n",
            false,
        );
        let messages: Vec<String> = list.diagnostics().into_iter().map(|d| d.message).collect();
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(messages[0].contains("not a MIME type"));
        assert!(messages[1].contains("both added and removed"));
        let desktop = MimeAppsList::parse(b"[Added Associations]\ntext/plain=a.desktop;\n", true);
        assert!(desktop.diagnostics()[0].message.contains("may only be in"));
        let spellings = MimeAppsList::parse(
            b"[Added Associations]\ntext/plain=a.desktop;\nText/Plain=a.desktop;\n[Removed Associations]\ntext/plain=a.desktop;\n",
            false,
        );
        assert_eq!(
            spellings.diagnostics().len(),
            1,
            "{:?}",
            spellings.diagnostics()
        );
    }

    #[test]
    fn diagnostics_point_at_the_key_as_written() {
        let list = MimeAppsList::parse(
            b"[Added Associations]\ntext/plain=a.desktop;\nImage/*=v.desktop;\ntext/plain[en]=b.desktop;\n[Removed Associations]\nimage/*=v.desktop;\n",
            false,
        );
        let found: Vec<(Option<usize>, String)> = list
            .diagnostics()
            .into_iter()
            .map(|d| (d.line, d.message))
            .collect();
        assert_eq!(
            found,
            [
                (
                    Some(4),
                    "[Added Associations] \"text/plain[en]\": MIME type keys are not localized; ignored".to_owned()
                ),
                (
                    Some(3),
                    "v.desktop is both added and removed for image/*; the addition counts"
                        .to_owned()
                ),
            ]
        );
    }

    #[test]
    fn ids_with_a_slash_are_reported_and_kept() {
        let text = "[Default Applications]\ntext/plain=kde/a.desktop;b.desktop;\n";
        let list = MimeAppsList::parse(text.as_bytes(), false);
        let found: Vec<(Option<usize>, String)> = list
            .diagnostics()
            .into_iter()
            .map(|d| (d.line, d.message))
            .collect();
        assert_eq!(
            found,
            [(
                Some(2),
                "[Default Applications] \"kde/a.desktop\" is not a desktop file ID (an ID has \"-\" in place of \"/\", as in \"kde-a.desktop\"); kept as written".to_owned()
            )]
        );
        assert_eq!(list.to_bytes(), text.as_bytes(), "unchanged");
        // A long ID is shown, with its dashed form, around the `/`.
        let vendor = "org.example.averylongvendorname";
        let long = format!("[Default Applications]\ntext/plain={vendor}.sub/app.desktop;\n");
        assert_eq!(
            MimeAppsList::parse(long.as_bytes(), false).diagnostics()[0].message,
            "[Default Applications] \"…name.sub/app.desktop\" is not a desktop file ID (an ID has \"-\" in place of \"/\", as in \"…name.sub-app.desktop\"); kept as written"
        );
        let mut list = list;
        list.set_default(&p("text/plain"), "c.desktop").unwrap();
        assert_eq!(
            list.ids(Group::Defaults, &p("text/plain")),
            ["c.desktop", "kde/a.desktop", "b.desktop"],
            "kept as written"
        );
    }

    #[test]
    fn wildcards_match_and_are_written_in_any_case() {
        // The key is not in lower case; the pattern is.
        let list = MimeAppsList::parse(b"[Added Associations]\nImage/*=v.desktop;\n", false);
        let image = TypePattern::Media(crate::MediaType::new("image").unwrap());
        assert_eq!(list.ids(Group::Added, &image), ["v.desktop"]);
        let mut list = MimeAppsList::parse(b"", false);
        let image = TypePattern::Media(crate::MediaType::new("IMAGE").unwrap());
        list.add_association(&image, "a.desktop").unwrap();
        assert_eq!(
            list.to_bytes(),
            b"[Added Associations]\nimage/*=a.desktop;\n"
        );
    }

    /// A database where `text/x-old` is an alias of `text/plain`.
    fn aliases() -> (tempfile::TempDir, Arc<MimeDatabase>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("aliases"), "text/x-old text/plain\n").unwrap();
        let database = Arc::new(MimeDatabase::from_dirs([dir.path()]));
        assert_eq!(
            p("text/x-old").canonical(&database),
            p("text/plain"),
            "the alias is read"
        );
        (dir, database)
    }

    /// `i{at}.desktop;` for each `at` in `range`.
    fn id_list(range: std::ops::Range<usize>) -> String {
        use std::fmt::Write as _;
        let mut ids = String::new();
        for at in range {
            let _ = write!(ids, "i{at}.desktop;");
        }
        ids
    }

    #[test]
    fn diagnostics_are_found_in_linear_time() {
        use std::fmt::Write as _;
        // Each added type is also removed, in another case and with another
        // ID; `text/plain` has `n` IDs both added and removed, half of them
        // under an alias.
        let text = |n: usize| {
            let mut text = String::from("[Added Associations]\n");
            for at in 0..n {
                let _ = writeln!(text, "application/x-a{at}=a{at}.desktop;");
            }
            let _ = writeln!(text, "text/plain={}", id_list(0..n));
            text.push_str("[Removed Associations]\n");
            for at in 0..n {
                let _ = writeln!(text, "Application/X-A{at}=r{at}.desktop;");
            }
            let _ = writeln!(text, "Text/Plain={}", id_list(0..n / 2));
            let _ = writeln!(text, "text/x-old={}", id_list(n / 2..n));
            text
        };
        let (_dir, database) = aliases();
        for resolving in [false, true] {
            let steps = |n: usize| {
                let text = text(n);
                let mut list = MimeAppsList::parse(text.as_bytes(), false);
                if resolving {
                    list = list.resolving_aliases(database.clone());
                }
                let (found, steps) = work::steps(|| list.diagnostics());
                let conflicts = if resolving { n } else { n / 2 };
                assert_eq!(found.len(), conflicts, "{:?}", found.first());
                // Per type: about 14 steps for its keys and IDs looked at,
                // gathered and looked up (making its type and looking up
                // its alias are calls into `xdg-kit-mime`, a step each);
                // and about 14.5 per byte of the text, each key and ID
                // checked, copied, hashed and compared a few times, here
                // and in the key file (listing the keys, splitting the
                // values, finding the lines reported). Twice that is
                // allowed.
                let bytes = text.len();
                (steps, 2 * 14 * n + 29 * bytes)
            };
            work::assert_linear("diagnostics", steps(2_000), steps(4_000));
        }
    }

    #[test]
    fn slash_ids_and_localized_keys_are_reported_in_linear_time() {
        use std::fmt::Write as _;
        // `n` localized keys, and `n` IDs with a `/`, each listed twice.
        let text = |n: usize| {
            let mut text = String::from("[Added Associations]\n");
            for at in 0..n {
                let _ = writeln!(text, "text/plain[l{at}]=a.desktop;");
            }
            let mut ids = String::new();
            for at in 0..n {
                let _ = write!(ids, "kde/i{at}.desktop;");
            }
            let _ = writeln!(text, "text/plain={ids}{ids}");
            text
        };
        let steps = |n: usize| {
            let text = text(n);
            let list = MimeAppsList::parse(text.as_bytes(), false);
            let (found, steps) = work::steps(|| list.diagnostics());
            assert_eq!(found.len(), 2 * n, "{:?}", found.first());
            // Per key and ID: a few steps looked at and gathered; per byte
            // of the text about 12.9, as each ID with a `/` is read, checked,
            // copied and hashed, compared with its form with `-` and written
            // into a message twice (as written, and with `-`), and the key
            // file lists the keys, splits the values and finds the lines
            // reported. Twice that is allowed.
            let bytes = text.len();
            (steps, 2 * (2 * n + 13 * bytes))
        };
        work::assert_linear("slash IDs and localized keys", steps(2_000), steps(4_000));
    }

    #[test]
    fn ids_are_gathered_in_linear_time() {
        let steps = |n: usize| {
            let ids = id_list(0..n);
            let text = format!("[Added Associations]\ntext/plain={ids}\nText/Plain={ids}\n");
            let list = MimeAppsList::parse(text.as_bytes(), false);
            let (found, steps) = work::steps(|| list.ids(Group::Added, &p("text/plain")));
            assert_eq!(found.len(), n);
            // A step per ID read and checked for repeats (each ID is listed
            // twice), and per character of each ID trimmed, copied, hashed
            // and compared, and checked for UTF-8, split off and unescaped
            // by the key file: about 8 per byte of the text. Twice that is
            // allowed.
            let bytes = text.len();
            (steps, 2 * (2 * n + 8 * bytes))
        };
        work::assert_linear("ids", steps(10_000), steps(20_000));
    }

    #[test]
    fn long_values_are_read_in_linear_time() {
        // One long item of each kind: an ID, keys that are not types (one a
        // media wildcard, one the key file reports too), a localized key,
        // an ID with a `/`; each listed, edited and removed, and a new ID
        // added.
        let steps = |len: usize| {
            let long = "a".repeat(len);
            let id = format!("{long}.desktop");
            let slash = format!("{long}/a.desktop");
            let other = format!("b{long}.desktop");
            let text = format!(
                "[Added Associations]\ntext/plain={id};{slash};\ntext/{long}=a.desktop;\n{long}/*=a.desktop;\na]{long}=a.desktop;\ntext/plain[{long}]=a.desktop;\n[Removed Associations]\ntext/plain={id};\n"
            );
            let mut list = MimeAppsList::parse(text.as_bytes(), false);
            let ((), steps) = work::steps(|| {
                let found = list.diagnostics();
                assert_eq!(
                    found.len(),
                    7,
                    "three keys that are not types, one of them twice, a localized key, an ID with a `/`, and a conflict"
                );
                assert_eq!(
                    list.ids(Group::Added, &p("text/plain")),
                    [id.clone(), slash.clone()]
                );
                assert_eq!(
                    list.set_default(&p("text/plain"), &slash),
                    Err(EditError::DesktopId(slash.clone()))
                );
                list.add_association(&p("text/plain"), &id).unwrap();
                list.add_association(&p("text/plain"), &other).unwrap();
                list.set_default(&p("text/plain"), &id).unwrap();
                list.remove_association(&p("text/plain"), &id).unwrap();
                list.remove_association(&p("text/plain"), &slash).unwrap();
            });
            // A step per character each time a key or ID is checked, copied,
            // hashed or compared, here and in the key file (values split and
            // unescaped, lines found, written and their keys looked up), and
            // per byte of the messages, which show at most 32 characters of
            // one: about 41 per byte of the text, which has 7 long items
            // (a long key made a type is a call into `xdg-kit-mime`, a
            // step). Twice that is allowed.
            (steps, 2 * 41 * text.len())
        };
        work::assert_linear("long values", steps(1 << 19), steps(1 << 20));
    }

    #[test]
    fn associations_are_edited_in_linear_time() {
        // `n` types, each given two associations, a default and a removal,
        // one edit at a time.
        let steps = |n: usize| {
            let types: Vec<TypePattern> = (0..n)
                .map(|at| p(&format!("application/x-t{at}")))
                .collect();
            let mut list = MimeAppsList::parse(b"[Added Associations]\n", false);
            let ((), steps) = work::steps(|| {
                for pattern in &types {
                    list.add_association(pattern, "a.desktop").unwrap();
                    list.add_association(pattern, "b.desktop").unwrap();
                    list.set_default(pattern, "b.desktop").unwrap();
                    list.remove_association(pattern, "a.desktop").unwrap();
                }
            });
            assert_eq!(list.ids(Group::Added, &types[n - 1]), ["b.desktop"]);
            // Per edit, about 36 steps per byte of the type and ID, as the
            // type is hashed and compared (its copies are `Arc`s, a step
            // each) and the IDs read, copied, hashed and compared a few
            // times each, here and in the key file (the entry's line found,
            // split, escaped and written, its key looked up and indexed).
            // Twice that is allowed.
            let bytes: usize = types
                .iter()
                .map(|pattern| 4 * (pattern.to_string().len() + "a.desktop".len()))
                .sum();
            (steps, 2 * 36 * bytes)
        };
        work::assert_linear("edits", steps(1_000), steps(2_000));
    }

    #[test]
    fn a_listed_id_with_a_slash_is_dropped() {
        // Listed in each group, and only as removed for another type: it is
        // dropped everywhere for the type, and not listed as removed.
        let text = "[Default Applications]\ntext/plain=kde/a.desktop;b.desktop;\n[Added Associations]\ntext/plain=kde/a.desktop;b.desktop;\n[Removed Associations]\nText/Plain=kde/a.desktop;c.desktop;\nimage/png=kde/a.desktop;\n";
        let mut list = MimeAppsList::parse(text.as_bytes(), false);
        list.remove_association(&p("text/plain"), "kde/a.desktop")
            .unwrap();
        assert_eq!(
            String::from_utf8(list.to_bytes()).unwrap(),
            "[Default Applications]\ntext/plain=b.desktop;\n[Added Associations]\ntext/plain=b.desktop;\n[Removed Associations]\nText/Plain=c.desktop;\nimage/png=kde/a.desktop;\n"
        );
        list.remove_association(&p("image/png"), "kde/a.desktop")
            .unwrap();
        assert!(list.ids(Group::Removed, &p("image/png")).is_empty());
        assert!(!list.to_bytes().windows(3).any(|w| w == b"kde"), "gone");
        // Not listed for that type, or not an ID for another reason: refused.
        for (pattern, bad) in [
            ("image/png", "kde/a.desktop"),
            ("text/plain", "kde/b.desktop"),
            ("text/plain", "kde/a;.desktop"),
        ] {
            assert_eq!(
                list.remove_association(&p(pattern), bad),
                Err(EditError::DesktopId(bad.into()))
            );
        }
        assert_eq!(
            list.add_association(&p("text/plain"), "kde/a.desktop"),
            Err(EditError::DesktopId("kde/a.desktop".into())),
            "adding one stays refused"
        );
    }

    #[test]
    fn a_path_that_cannot_be_made_absolute_is_refused() {
        // Unlike `KeyFile::read`, which keeps such a path as given: the list
        // is written back, so it needs a fixed place.
        let err = MimeAppsList::open("").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        let err = MimeAppsList::open_with("", &Limits::default()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn files_are_created_with_private_folders() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new/config/mimeapps.list");
        let mut list = MimeAppsList::open(&path).unwrap();
        assert!(!list.is_desktop_specific());
        list.set_default(&p("text/plain"), "a.desktop").unwrap();
        list.write().unwrap();
        let mode = std::fs::metadata(dir.path().join("new"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        let again = MimeAppsList::open(&path).unwrap();
        assert_eq!(again.ids(Group::Defaults, &p("text/plain")), ["a.desktop"]);
        assert!(
            MimeAppsList::open(dir.path().join("gnome-mimeapps.list"))
                .unwrap()
                .is_desktop_specific()
        );
        assert!(MimeAppsList::parse(b"", false).write().is_err());
    }
}
