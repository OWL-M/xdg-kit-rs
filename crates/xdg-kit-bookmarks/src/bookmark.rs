//! Typed views over the entries of a [`BookmarkFile`]. The views read the
//! logical document: pending edits show through immediately.

use std::time::SystemTime;

use crate::BookmarkFile;
use crate::edit::{AppState, Entry, Overlay, OwnedBookmark};
use crate::xml::App;

/// One entry of a bookmark file: a URI and what is recorded about it.
#[derive(Debug, Clone, Copy)]
pub struct Bookmark<'a> {
    pub(crate) file: &'a BookmarkFile,
    pub(crate) entry: Entry,
}

impl<'a> Bookmark<'a> {
    fn node(&self) -> Option<&'a crate::xml::BookmarkNode> {
        match self.entry {
            Entry::Source(at) => Some(&self.file.parsed.bookmarks[at]),
            Entry::Inserted(_) => None,
        }
    }

    fn overlay(&self) -> Option<&'a Overlay> {
        match self.entry {
            Entry::Source(at) => self.file.edits.overlays.get(&at),
            Entry::Inserted(_) => None,
        }
    }

    fn owned(&self) -> Option<&'a OwnedBookmark> {
        match self.entry {
            Entry::Source(_) => None,
            Entry::Inserted(at) => self.file.edits.inserted[at].as_ref(),
        }
    }

    fn meta(&self) -> Option<&'a crate::xml::Meta> {
        self.node()?.info.as_ref()?.metadata.as_ref()
    }

    /// The entry's URI, as stored.
    #[must_use]
    pub fn href(&self) -> &'a str {
        match self.entry {
            Entry::Source(at) => &self.file.parsed.bookmarks[at].href.text,
            // Views are only handed out for live entries.
            Entry::Inserted(at) => self.file.edits.inserted[at]
                .as_ref()
                .map_or("", |owned| owned.href.as_str()),
        }
    }

    /// When the entry was first added, if recorded and readable.
    #[must_use]
    pub fn added(&self) -> Option<SystemTime> {
        match self.owned() {
            Some(owned) => Some(owned.added),
            None => self.node()?.added.as_ref()?.time,
        }
    }

    /// When the entry was last changed, if recorded and readable.
    #[must_use]
    pub fn modified(&self) -> Option<SystemTime> {
        if let Some(owned) = self.owned() {
            return Some(owned.modified);
        }
        if let Some(pending) = self.overlay().and_then(|overlay| overlay.modified) {
            return Some(pending);
        }
        self.node()?.modified.as_ref()?.time
    }

    /// When the resource was last visited, if recorded and readable.
    #[must_use]
    pub fn visited(&self) -> Option<SystemTime> {
        if let Some(owned) = self.owned() {
            return Some(owned.visited);
        }
        if let Some(pending) = self.overlay().and_then(|overlay| overlay.visited) {
            return Some(pending);
        }
        self.node()?.visited.as_ref()?.time
    }

    /// The stored MIME type, exactly as stored — the file is shared, so
    /// it may not parse as a [`MimeType`](xdg_kit_mime::MimeType); parsing
    /// it is the caller's call.
    #[must_use]
    pub fn mime_type(&self) -> Option<&'a str> {
        if let Some(owned) = self.owned() {
            return Some(&owned.mime);
        }
        if let Some(pending) = self.overlay().and_then(|overlay| overlay.mime.as_deref()) {
            return Some(pending);
        }
        Some(self.meta()?.mime.as_ref()?.value.as_ref()?.text.as_str())
    }

    /// The groups the entry is filed under: the stored ones in file order,
    /// then any pending additions.
    #[must_use]
    pub fn groups(&self) -> Vec<&'a str> {
        if let Some(owned) = self.owned() {
            return owned.groups.iter().map(String::as_str).collect();
        }
        let mut groups: Vec<&str> = self
            .meta()
            .and_then(|meta| meta.groups.as_ref())
            .map(|found| {
                found
                    .list
                    .iter()
                    .inspect(|_| crate::work::count(1))
                    .map(|group| group.name.as_str())
                    .collect()
            })
            .unwrap_or_default();
        if let Some(overlay) = self.overlay() {
            groups.extend(overlay.groups_added.iter().map(String::as_str));
        }
        groups
    }

    /// The applications that registered the entry, in file order, each
    /// with its pending update if one is recorded; newly registered ones
    /// follow. With several records of one name — junk other writers
    /// left — the first is the application's record and gets the update;
    /// the rest stay exactly as stored, in the view and in the written
    /// file alike.
    #[must_use]
    pub fn applications(&self) -> Vec<Application<'a>> {
        if let Some(owned) = self.owned() {
            return owned.apps.iter().map(Application::pending).collect();
        }
        let pending = self.overlay().map(|overlay| &overlay.apps);
        let mut apps: Vec<Application<'a>> = Vec::new();
        if let Some(found) = self.meta().and_then(|meta| meta.applications.as_ref()) {
            let mut replaced_names: Vec<&str> = Vec::new();
            for app in &found.list {
                crate::work::count(1);
                let name = app.name.as_ref().map(|name| name.text.as_str());
                let replaced = pending
                    .filter(|_| {
                        // Only the first record with the name is updated.
                        name.is_none_or(|name| !replaced_names.contains(&name))
                    })
                    .and_then(|states| {
                        states.iter().find(|state| {
                            crate::work::count(1);
                            Some(state.name.as_str()) == name
                        })
                    });
                match replaced {
                    Some(state) => {
                        replaced_names.extend(name);
                        apps.push(Application::pending(state));
                    }
                    None => apps.push(Application::stored(app)),
                }
            }
        }
        if let Some(states) = pending {
            let stored = |name: &str| {
                self.meta()
                    .and_then(|meta| meta.applications.as_ref())
                    .is_some_and(|found| {
                        found.list.iter().any(|app| {
                            crate::work::count(1);
                            app.name.as_ref().map(|had| had.text.as_str()) == Some(name)
                        })
                    })
            };
            apps.extend(
                states
                    .iter()
                    .filter(|state| !stored(&state.name))
                    .map(Application::pending),
            );
        }
        apps
    }

    /// Whether the entry is marked private.
    #[must_use]
    pub fn is_private(&self) -> bool {
        if let Some(owned) = self.owned() {
            return owned.private;
        }
        self.overlay().is_some_and(|overlay| overlay.private_set)
            || self.meta().is_some_and(|meta| meta.private.is_some())
    }
}

/// One application's record on a bookmark: who registered it, how it is
/// run, how often, and when last.
#[derive(Debug, Clone, Copy)]
pub struct Application<'a>(Record<'a>);

#[derive(Debug, Clone, Copy)]
enum Record<'a> {
    Stored(&'a App),
    Pending(&'a AppState),
}

impl<'a> Application<'a> {
    pub(crate) fn stored(app: &'a App) -> Self {
        Self(Record::Stored(app))
    }

    pub(crate) fn pending(state: &'a AppState) -> Self {
        Self(Record::Pending(state))
    }
    /// The application's name.
    #[must_use]
    pub fn name(&self) -> Option<&'a str> {
        match self.0 {
            Record::Stored(app) => Some(app.name.as_ref()?.text.as_str()),
            Record::Pending(state) => Some(&state.name),
        }
    }

    /// The command line it registered (conventionally quoted, with `%u`).
    #[must_use]
    pub fn exec(&self) -> Option<&'a str> {
        match self.0 {
            Record::Stored(app) => Some(app.exec.as_ref()?.text.as_str()),
            Record::Pending(state) => Some(&state.exec),
        }
    }

    /// How often it registered the entry. An absent count means one
    /// registration (the spec's default); `None` only when the stored
    /// value is not a number.
    #[must_use]
    pub fn count(&self) -> Option<u64> {
        match self.0 {
            Record::Stored(app) => match &app.count {
                None => Some(1),
                Some((_, parsed)) => *parsed,
            },
            Record::Pending(state) => Some(state.count),
        }
    }

    /// When it last registered the entry, if recorded and readable.
    #[must_use]
    pub fn modified(&self) -> Option<SystemTime> {
        match self.0 {
            Record::Stored(app) => app.modified.as_ref()?.time,
            Record::Pending(state) => Some(state.modified),
        }
    }
}
