//! Pending edits: the overlay that, together with the source spans, forms
//! the logical document. Reads consult it first (read-your-writes);
//! serialization splices it into the source bytes.

use std::time::SystemTime;

use xdg_kit_mime::MimeType;

use crate::work;
use crate::{BookmarkFile, EditError};

/// What a caller records when an application opens a resource. Built by
/// the caller field by field; a future field would be a breaking change,
/// which this crate prefers over hiding construction behind a builder.
#[derive(Debug, Clone)]
pub struct Opened {
    /// The application's name, as other readers of the file show it.
    pub name: String,
    /// The command line it registers (conventionally quoted, with `%u`).
    pub exec: String,
    /// The resource's MIME type. Used only when the entry is created; an
    /// existing entry's stored type is preserved — recording a visit never
    /// overwrites metadata the entry already has
    /// ([`BookmarkFile::set_mime_type`] changes it explicitly). A
    /// [`MimeType`] validates the name's syntax; it does not prove the
    /// resource has that type, nor that the name is canonical in any
    /// particular database — callers that detect types can check the
    /// detection's certainty before recording it over a known one.
    pub mime: MimeType,
    /// Groups to file the entry under. Merged into an existing entry's
    /// groups (the spec's recommendation when another application
    /// registers a known URI); never removes one.
    pub groups: Vec<String>,
    /// `true` marks the entry private. `false` leaves an existing mark
    /// alone: privacy is set when an application first asks for it, not
    /// cleared by one that does not.
    pub private: bool,
    /// When the resource was opened.
    pub when: SystemTime,
}

/// One application's new record: the full state its element will carry.
#[derive(Debug, Clone)]
pub(crate) struct AppState {
    pub name: String,
    pub exec: String,
    pub modified: SystemTime,
    pub count: u64,
}

/// Changes pending on one source entry.
#[derive(Debug, Default)]
pub(crate) struct Overlay {
    pub modified: Option<SystemTime>,
    pub visited: Option<SystemTime>,
    pub mime: Option<String>,
    /// Groups appended beyond the source's, in order.
    pub groups_added: Vec<String>,
    pub private_set: bool,
    /// Replaced or added application records, by name.
    pub apps: Vec<AppState>,
}

/// An entry that exists only in the overlay: inserted, not yet written.
#[derive(Debug)]
pub(crate) struct OwnedBookmark {
    pub href: String,
    pub added: SystemTime,
    pub modified: SystemTime,
    pub visited: SystemTime,
    pub mime: String,
    pub groups: Vec<String>,
    pub private: bool,
    pub apps: Vec<AppState>,
}

/// The pending edits of a document.
#[derive(Debug, Default)]
pub(crate) struct Edits {
    /// Overlays by source entry index.
    pub overlays: std::collections::HashMap<usize, Overlay>,
    /// Source entries removed, by index.
    pub removed: std::collections::HashSet<usize>,
    /// Entries inserted; `None` where one was removed again.
    pub inserted: Vec<Option<OwnedBookmark>>,
}

impl Edits {
    pub(crate) fn is_empty(&self) -> bool {
        self.overlays.is_empty() && self.removed.is_empty() && self.inserted.is_empty()
    }
}

/// Which logical entry a view or an edit addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Entry {
    /// An entry of the source, by its index there.
    Source(usize),
    /// An inserted entry, by its index in [`Edits::inserted`].
    Inserted(usize),
}

impl BookmarkFile {
    /// Records that an application opened `uri`: creates the entry, or
    /// updates the existing one — `modified` and `visited` move to
    /// [`Opened::when`], the application's record is added or its count
    /// incremented (saturating), the supplied groups are merged in, and
    /// `private: true` marks the entry. An existing entry's MIME type,
    /// `added` time and private mark are never downgraded by this call.
    ///
    /// # Errors
    ///
    /// [`EditError::NotEditable`] for a document that cannot be edited;
    /// [`EditError::AmbiguousUri`] when several entries share `uri`;
    /// [`EditError::InvalidText`] when the URI, name, exec or a group
    /// holds characters XML cannot carry; [`EditError::TimeOutOfRange`]
    /// when the time cannot be written; [`EditError::TooManyBookmarks`]
    /// when creating the entry would pass the limit. A refused call
    /// changes nothing.
    pub fn open(&mut self, uri: &str, opened: &Opened) -> Result<(), EditError> {
        self.editable()?;
        valid_text(uri, "URI")?;
        valid_text(&opened.name, "application name")?;
        valid_text(&opened.exec, "application command")?;
        for group in &opened.groups {
            valid_text(group, "group")?;
        }
        if crate::time::format(opened.when).is_none() {
            return Err(EditError::TimeOutOfRange);
        }
        match self.find(uri)? {
            Some(Entry::Source(at)) => {
                // Everything the new state needs, read before the overlay
                // is touched, so a failure cannot leave it half-made.
                let view = crate::Bookmark {
                    file: self,
                    entry: Entry::Source(at),
                };
                // No record yet is a basis of zero; a record without a
                // count means one registration (the spec's default), and
                // a malformed count is taken as that default too.
                let count = view
                    .applications()
                    .iter()
                    .find(|app| app.name() == Some(opened.name.as_str()))
                    .map_or(0, |app| app.count().unwrap_or(1));
                let known: Vec<String> = view
                    .groups()
                    .iter()
                    .map(|&group| group.to_owned())
                    .collect();
                let new_groups: Vec<String> = opened
                    .groups
                    .iter()
                    .inspect(|group| work::count(group.len()))
                    .filter(|group| !known.iter().any(|had| had == *group))
                    .cloned()
                    .collect();
                let overlay = self.edits.overlays.entry(at).or_default();
                overlay.modified = Some(opened.when);
                overlay.visited = Some(opened.when);
                overlay.groups_added.extend(new_groups);
                overlay.private_set |= opened.private;
                set_app(&mut overlay.apps, opened, count.saturating_add(1));
            }
            Some(Entry::Inserted(at)) => {
                // `find` returns only live entries.
                let Some(owned) = self.edits.inserted[at].as_mut() else {
                    return Err(EditError::NoSuchUri(uri.to_owned()));
                };
                owned.modified = opened.when;
                owned.visited = opened.when;
                for group in &opened.groups {
                    work::count(group.len());
                    if !owned.groups.iter().any(|had| had == group) {
                        owned.groups.push(group.clone());
                    }
                }
                owned.private |= opened.private;
                let count = owned
                    .apps
                    .iter()
                    .find(|app| app.name == opened.name)
                    .map_or(0, |app| app.count);
                set_app(&mut owned.apps, opened, count.saturating_add(1));
            }
            None => {
                if self.bookmarks().count() >= self.limits.max_bookmarks {
                    return Err(EditError::TooManyBookmarks);
                }
                let at = self.edits.inserted.len();
                self.edits.inserted.push(Some(OwnedBookmark {
                    href: uri.to_owned(),
                    added: opened.when,
                    modified: opened.when,
                    visited: opened.when,
                    mime: opened.mime.to_string(),
                    groups: opened.groups.clone(),
                    private: opened.private,
                    apps: vec![AppState {
                        name: opened.name.clone(),
                        exec: opened.exec.clone(),
                        modified: opened.when,
                        count: 1,
                    }],
                }));
                // The insertion; the map counts the href's bytes.
                work::count(1 + uri.len());
                self.inserted_index.insert(uri, at);
            }
        }
        Ok(())
    }

    /// Sets the stored MIME type of `uri`'s entry: the explicit way to
    /// change it ([`BookmarkFile::open`] preserves it).
    ///
    /// # Errors
    ///
    /// [`EditError::NotEditable`], [`EditError::NoSuchUri`] or
    /// [`EditError::AmbiguousUri`]. A refused call changes nothing.
    pub fn set_mime_type(&mut self, uri: &str, mime: &MimeType) -> Result<(), EditError> {
        self.editable()?;
        match self.find(uri)? {
            Some(Entry::Source(at)) => {
                self.edits.overlays.entry(at).or_default().mime = Some(mime.to_string());
            }
            Some(Entry::Inserted(at)) => {
                // `find` returns only live entries.
                let Some(owned) = self.edits.inserted[at].as_mut() else {
                    return Err(EditError::NoSuchUri(uri.to_owned()));
                };
                owned.mime = mime.to_string();
            }
            None => return Err(EditError::NoSuchUri(uri.to_owned())),
        }
        Ok(())
    }

    /// Removes every entry with exactly this URI (several are junk to
    /// clean up, not entries to pick between). `Ok(false)` when none
    /// existed.
    ///
    /// # Errors
    ///
    /// [`EditError::NotEditable`]. A refused call changes nothing.
    pub fn remove(&mut self, uri: &str) -> Result<bool, EditError> {
        self.editable()?;
        let mut removed = false;
        work::count(1);
        if let Some(&(_, _)) = self.index.get(uri) {
            for (at, bookmark) in self.parsed.bookmarks.iter().enumerate() {
                work::count(1);
                if bookmark.href.text == uri && self.edits.removed.insert(at) {
                    self.edits.overlays.remove(&at);
                    removed = true;
                }
            }
        }
        work::count(1);
        if let Some(&at) = self.inserted_index.get(uri) {
            if self.edits.inserted[at].take().is_some() {
                removed = true;
            }
            self.inserted_index.remove(uri);
        }
        Ok(removed)
    }

    /// Removes every entry; the document frame stays.
    ///
    /// # Errors
    ///
    /// [`EditError::NotEditable`]. A refused call changes nothing.
    pub fn clear(&mut self) -> Result<(), EditError> {
        self.editable()?;
        self.edits.overlays.clear();
        for at in 0..self.parsed.bookmarks.len() {
            work::count(1);
            self.edits.removed.insert(at);
        }
        self.edits.inserted.clear();
        self.inserted_index.clear();
        Ok(())
    }

    fn editable(&self) -> Result<(), EditError> {
        if self.is_editable() {
            Ok(())
        } else {
            Err(EditError::NotEditable)
        }
    }

    /// The live entry `uri` addresses, or `None` for none.
    ///
    /// # Errors
    ///
    /// [`EditError::AmbiguousUri`] when several source entries share it.
    fn find(&self, uri: &str) -> Result<Option<Entry>, EditError> {
        // The lookups; the maps count the bytes hashed and compared.
        work::count(2);
        if let Some(&(first, count)) = self.index.get(uri)
            && !self.edits.removed.contains(&first)
        {
            if count > 1 {
                return Err(EditError::AmbiguousUri(uri.to_owned()));
            }
            return Ok(Some(Entry::Source(first)));
        }
        if let Some(&at) = self.inserted_index.get(uri)
            && self.edits.inserted[at].is_some()
        {
            return Ok(Some(Entry::Inserted(at)));
        }
        Ok(None)
    }
}

/// Replaces or adds the record of `opened`'s application with the new
/// full state.
fn set_app(apps: &mut Vec<AppState>, opened: &Opened, count: u64) {
    let state = AppState {
        name: opened.name.clone(),
        exec: opened.exec.clone(),
        modified: opened.when,
        count,
    };
    work::count(opened.name.len() + opened.exec.len());
    match apps.iter_mut().find(|app| {
        work::count(1);
        app.name == opened.name
    }) {
        Some(app) => *app = state,
        None => apps.push(state),
    }
}

/// Refuses `text` holding a character XML cannot carry, however escaped:
/// NUL and the other characters outside the XML 1.0 ranges.
fn valid_text(text: &str, what: &'static str) -> Result<(), EditError> {
    work::count(text.len());
    let allowed = |c: char| {
        matches!(c, '\u{9}' | '\u{A}' | '\u{D}')
            || ('\u{20}'..='\u{D7FF}').contains(&c)
            || ('\u{E000}'..='\u{FFFD}').contains(&c)
            || c >= '\u{10000}'
    };
    if text.chars().all(allowed) {
        Ok(())
    } else {
        Err(EditError::InvalidText(what))
    }
}
