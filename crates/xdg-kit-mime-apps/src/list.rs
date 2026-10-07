//! One `mimeapps.list` file (MIME Applications Associations spec "File name
//! and location"), read and edited without losing anything: comments, order
//! and unknown groups stay as written.

use std::fmt;
use std::fs::DirBuilder;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use xdg_kit_basedir::BaseDirs;
use xdg_kit_desktop_entry::{KeyFile, KeyNames};
use xdg_kit_mime::MimeDatabase;

use crate::{Diagnostic, TypePattern};

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
    /// space, and hold no `;`, line break or other control character.
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
            Self::DesktopId(id) => write!(f, "{id:?} is not a desktop file ID"),
            Self::DesktopSpecific => write!(
                f,
                "a desktop-specific mimeapps.list holds only default applications"
            ),
            Self::KeyFile(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for EditError {}

impl From<xdg_kit_desktop_entry::EditError> for EditError {
    fn from(error: xdg_kit_desktop_entry::EditError) -> Self {
        Self::KeyFile(error)
    }
}

/// Whether `id` can be written as a desktop file ID in a list.
fn valid_desktop_id(id: &str) -> bool {
    id.len() > ".desktop".len()
        && id.ends_with(".desktop")
        // Reading trims an ID, so one starting with white space would come
        // back as another.
        && !id.starts_with(char::is_whitespace)
        && !id.chars().any(|c| c == ';' || c.is_control())
}

/// One `mimeapps.list` file.
#[derive(Debug, Clone)]
pub struct MimeAppsList {
    file: KeyFile,
    path: Option<PathBuf>,
    desktop_specific: bool,
    /// When set, keys naming an alias of a type count as that type.
    database: Option<Arc<MimeDatabase>>,
}

impl MimeAppsList {
    /// The user's own file, `$XDG_CONFIG_HOME/mimeapps.list`: the spec's
    /// "recommended location for user configuration GUIs".
    ///
    /// # Errors
    ///
    /// When the file exists but cannot be read.
    pub fn for_user(base_dirs: &BaseDirs) -> io::Result<Self> {
        Self::open(base_dirs.config_home().join(MIMEAPPS_LIST))
    }

    /// The file at `path`; a missing file is an empty list, created by
    /// [`MimeAppsList::write`]. A file not named `mimeapps.list` counts as
    /// desktop-specific.
    ///
    /// # Errors
    ///
    /// When the file exists but cannot be read.
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        // Absolute now, so that a later change of folder does not move it.
        let path = std::path::absolute(path.into())?;
        let desktop_specific = path.file_name().is_some_and(|name| name != MIMEAPPS_LIST);
        let file = match KeyFile::read_with(&path, KeyNames::Any) {
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
        })
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
        }
    }

    /// Uses `database` to resolve aliases, so edits also reach keys that
    /// name an alias of the type. This matches how
    /// [`MimeApps`](crate::MimeApps) reads them. Without it, keys match
    /// only when they name the same type, ignoring case.
    #[must_use]
    pub fn resolving_aliases(mut self, database: Arc<MimeDatabase>) -> Self {
        self.database = Some(database);
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
        for key in self.keys_for(group, pattern) {
            for id in self.items(group, &key) {
                if !ids.contains(&id) {
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
        self.file
            .keys(group.name())
            .into_iter()
            .filter(|(_, locale)| locale.is_none())
            .filter_map(|(key, _)| {
                TypePattern::parse(key).map(|pattern| (pattern, self.items(group, key)))
            })
            .collect()
    }

    fn items(&self, group: Group, key: &str) -> Vec<String> {
        self.file
            .list(group.name(), key)
            .unwrap_or_default()
            .into_iter()
            .map(|item| item.trim().to_owned())
            .filter(|item| !item.is_empty())
            .collect()
    }

    /// The keys `group` uses for `pattern`, as spelled in the file, in
    /// order.
    fn keys_for(&self, group: Group, pattern: &TypePattern) -> Vec<String> {
        let wanted = self.resolve(pattern);
        self.file
            .keys(group.name())
            .into_iter()
            .filter(|(key, locale)| {
                locale.is_none()
                    && TypePattern::parse(key).is_some_and(|found| self.resolve(&found) == wanted)
            })
            .map(|(key, _)| key.to_owned())
            .collect()
    }

    fn resolve(&self, pattern: &TypePattern) -> TypePattern {
        match &self.database {
            Some(database) => pattern.canonical(database),
            None => pattern.clone(),
        }
    }

    /// Problems found when reading: those of the key file, keys that are not
    /// type patterns, associations in a desktop-specific file (ignored), and
    /// an ID both added and removed for one type (the spec: invalid; the
    /// addition counts, as it is read first).
    #[must_use]
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        let mut found: Vec<Diagnostic> = self
            .file
            .diagnostics()
            .iter()
            .map(Diagnostic::from)
            .collect();
        let mut report = |key: &str, group: Group, message: String| {
            let line = self.file.line_of(group.name(), key, None);
            found.push(Diagnostic::new(self.path.clone(), line, message));
        };
        for group in [Group::Defaults, Group::Added, Group::Removed] {
            for (key, locale) in self.file.keys(group.name()) {
                if locale.is_some() || TypePattern::parse(key).is_none() {
                    report(
                        key,
                        group,
                        format!("[{}] {key:?} is not a MIME type; ignored", group.name()),
                    );
                }
            }
        }
        if self.desktop_specific {
            for group in [Group::Added, Group::Removed] {
                if self.file.has_group(group.name()) {
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
            // Once per type and ID, however many spellings the key has.
            let mut reported = std::collections::HashSet::new();
            for (pattern, added) in self.entries(Group::Added) {
                let removed = self.ids(Group::Removed, &pattern);
                for id in added.iter().filter(|id| removed.contains(id)) {
                    if !reported.insert((self.resolve(&pattern), id.clone())) {
                        continue;
                    }
                    report(
                        &pattern.to_string(),
                        Group::Added,
                        format!(
                            "{id} is both added and removed for {pattern}; the addition counts"
                        ),
                    );
                }
            }
        }
        found
    }

    /// Makes `id` the first default application for `pattern`, keeping the
    /// others after it. In a file named `mimeapps.list` it also becomes the
    /// first added association (and is no longer removed): one file cannot
    /// see the others, so this makes sure the default is associated with
    /// the type, as the spec asks.
    ///
    /// # Errors
    ///
    /// When `id` is not a desktop file ID.
    pub fn set_default(&mut self, pattern: &TypePattern, id: &str) -> Result<(), EditError> {
        check_id(id)?;
        let mut file = self.file.clone();
        put_first(&mut file, self, Group::Defaults, pattern, id)?;
        if !self.desktop_specific {
            put_first(&mut file, self, Group::Added, pattern, id)?;
            drop_id(&mut file, self, Group::Removed, pattern, id)?;
        }
        self.file = file;
        Ok(())
    }

    /// Associates `id` with `pattern`. Appends it to the added associations,
    /// or keeps its position if already listed, and removes it from the
    /// removed associations.
    ///
    /// # Errors
    ///
    /// When `id` is not a desktop file ID, or this file is desktop-specific.
    pub fn add_association(&mut self, pattern: &TypePattern, id: &str) -> Result<(), EditError> {
        check_id(id)?;
        if self.desktop_specific {
            return Err(EditError::DesktopSpecific);
        }
        let mut file = self.file.clone();
        let mut added = self.ids(Group::Added, pattern);
        if !added.iter().any(|known| known == id) {
            added.push(id.to_owned());
        }
        set_ids(&mut file, self, Group::Added, pattern, &added)?;
        drop_id(&mut file, self, Group::Removed, pattern, id)?;
        self.file = file;
        Ok(())
    }

    /// Removes the association of `id` with `pattern`. Drops it from the
    /// added associations and defaults, and lists it as removed.
    ///
    /// # Errors
    ///
    /// When `id` is not a desktop file ID, or this file is desktop-specific.
    pub fn remove_association(&mut self, pattern: &TypePattern, id: &str) -> Result<(), EditError> {
        check_id(id)?;
        if self.desktop_specific {
            return Err(EditError::DesktopSpecific);
        }
        let mut file = self.file.clone();
        drop_id(&mut file, self, Group::Added, pattern, id)?;
        drop_id(&mut file, self, Group::Defaults, pattern, id)?;
        let mut removed = self.ids(Group::Removed, pattern);
        if !removed.iter().any(|known| known == id) {
            removed.push(id.to_owned());
        }
        set_ids(&mut file, self, Group::Removed, pattern, &removed)?;
        self.file = file;
        Ok(())
    }

    /// Removes every entry for `pattern` from all three groups. Returns
    /// whether any entry was removed.
    pub fn forget(&mut self, pattern: &TypePattern) -> bool {
        let mut found = false;
        for group in [Group::Defaults, Group::Added, Group::Removed] {
            for key in self.keys_for(group, pattern) {
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
}

fn check_id(id: &str) -> Result<(), EditError> {
    if valid_desktop_id(id) {
        Ok(())
    } else {
        Err(EditError::DesktopId(id.to_owned()))
    }
}

/// Writes `ids` as `group`'s entry for `pattern` into `file`, under the
/// first key `list` already has for it; other keys naming the type (in
/// another case, or as an alias) are removed, so none of them keeps an old
/// value. An empty list removes the entry.
fn set_ids(
    file: &mut KeyFile,
    list: &MimeAppsList,
    group: Group,
    pattern: &TypePattern,
    ids: &[String],
) -> Result<(), EditError> {
    let keys = list.keys_for(group, pattern);
    let key = keys.first().cloned().unwrap_or_else(|| pattern.to_string());
    for other in keys.iter().skip(1) {
        file.remove(group.name(), other, None);
    }
    if ids.is_empty() {
        file.remove(group.name(), &key, None);
    } else {
        file.set_list(group.name(), &key, ids)?;
    }
    Ok(())
}

fn put_first(
    file: &mut KeyFile,
    list: &MimeAppsList,
    group: Group,
    pattern: &TypePattern,
    id: &str,
) -> Result<(), EditError> {
    let mut ids = vec![id.to_owned()];
    ids.extend(
        list.ids(group, pattern)
            .into_iter()
            .filter(|known| known != id),
    );
    set_ids(file, list, group, pattern, &ids)
}

fn drop_id(
    file: &mut KeyFile,
    list: &MimeAppsList,
    group: Group,
    pattern: &TypePattern,
    id: &str,
) -> Result<(), EditError> {
    let ids = list.ids(group, pattern);
    if ids.iter().any(|known| known == id) {
        let kept: Vec<String> = ids.into_iter().filter(|known| known != id).collect();
        set_ids(file, list, group, pattern, &kept)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(text: &str) -> TypePattern {
        TypePattern::parse(text).unwrap()
    }

    const SAMPLE: &str = "# mine\n[Default Applications]\ntext/plain=a.desktop;b.desktop;\n\n[Added Associations]\ntext/plain=b.desktop;\nimage/*=v.desktop;\n\n[Removed Associations]\nText/Plain=c.desktop;\n\n[X-Other]\nkeep=1\n";

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
