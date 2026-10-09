//! Reads every `mimeapps.list` in the spec's order into one snapshot.
//! Uses the spec's two algorithms to find the applications associated with
//! a type and its default application.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use xdg_kit_basedir::{BaseDirs, FileStamp};
use xdg_kit_desktop_entry::{AppEntry, AppIndex, Desktops, EntryType};
use xdg_kit_mime::{MimeDatabase, MimeType};

use crate::error::quoted;
use crate::list::{Group, MIMEAPPS_LIST, MimeAppsList};
use crate::{Diagnostic, MediaType, TypePattern};

/// How an application matched a type.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Matched {
    /// The type itself, or an alias of it.
    Exact,
    /// A type it is a kind of: `text/plain` for `text/x-csrc`.
    Parent(MimeType),
    /// A wildcard: `image/*` or `*/*`.
    Wildcard(TypePattern),
}

/// An application for a type, and how it matched.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AppMatch {
    /// The application.
    pub app: AppEntry,
    /// How it matched.
    pub matched: Matched,
}

/// Where one `mimeapps.list` is in the search order.
#[derive(Debug, Clone)]
struct LevelPlace {
    path: PathBuf,
    desktop_specific: bool,
    /// The `applications` folder it sits in, for the data levels.
    applications: Option<PathBuf>,
    /// Whether the file is read here: not at the later place of a folder
    /// that is both a config folder and an `applications` folder.
    read: bool,
}

/// One `mimeapps.list`, read.
#[derive(Debug, Clone)]
struct Level {
    place: LevelPlace,
    stamp: FileStamp,
    /// The `AppIndex` folder whose desktop files count at this level: set
    /// for the `mimeapps.list` of an `applications` folder.
    folder: Option<usize>,
    /// The first `AppIndex` folder at this level or a lower one: added and
    /// removed associations count only for files found there or after.
    lowest: usize,
    defaults: HashMap<TypePattern, Vec<String>>,
    added: HashMap<TypePattern, Vec<String>>,
    removed: HashMap<TypePattern, Vec<String>>,
}

/// The applications for MIME types, from every `mimeapps.list` and the
/// installed desktop entries (MIME Applications Associations spec). An
/// immutable snapshot; [`MimeApps::refresh`] gives a new one when files
/// change.
#[derive(Debug, Clone)]
pub struct MimeApps {
    levels: Vec<Level>,
    apps: Arc<AppIndex>,
    mime: Arc<MimeDatabase>,
    /// For each `AppIndex` folder, the applications whose winning file is
    /// there, by each (canonical) type pattern their `MimeType=` lists, in
    /// ID byte order.
    folder_types: Vec<HashMap<TypePattern, Vec<String>>>,
    try_exec: Option<OsString>,
    /// Problems found before the files were read (desktop names that
    /// cannot name a file); kept across refreshes.
    setup: Vec<Diagnostic>,
    diagnostics: Vec<Diagnostic>,
}

impl MimeApps {
    /// Reads every `mimeapps.list` in the spec's order:
    /// `$XDG_CONFIG_HOME`, each `$XDG_CONFIG_DIRS` folder,
    /// `$XDG_DATA_HOME/applications` and each `$XDG_DATA_DIRS/applications`,
    /// at each one first `$desktop-mimeapps.list` for every name of
    /// `desktops` (lowercased), then `mimeapps.list`. A file reached again
    /// is read once, where it comes first; a folder that is both a config
    /// folder and an `applications` folder has its lists read at its config
    /// place and its desktop files counted at its data place. `apps` should
    /// be [`AppIndex::load`] of the same `base_dirs`, so that its folders
    /// are the data levels' `applications` folders.
    #[must_use]
    pub fn load(
        base_dirs: &BaseDirs,
        desktops: &Desktops,
        apps: Arc<AppIndex>,
        mime: Arc<MimeDatabase>,
    ) -> Self {
        let mut setup = Vec::new();
        let names: Vec<String> = desktops
            .names()
            .iter()
            .filter(|name| {
                // A name is part of a file name: it may not lead elsewhere.
                let usable =
                    !matches!(name.as_str(), "" | "." | "..") && !name.contains(['/', '\0']);
                if !usable {
                    setup.push(Diagnostic::new(
                        None,
                        None,
                        format!(
                            "desktop name \"{}\" cannot name a file; skipped",
                            quoted(name)
                        ),
                    ));
                }
                usable
            })
            .map(|name| name.to_ascii_lowercase())
            .collect();
        let mut places: Vec<LevelPlace> = Vec::new();
        let mut add = |dir: &Path, applications: bool| {
            for name in &names {
                places.push(LevelPlace {
                    path: dir.join(format!("{name}-{MIMEAPPS_LIST}")),
                    desktop_specific: true,
                    applications: applications.then(|| dir.to_owned()),
                    read: true,
                });
            }
            places.push(LevelPlace {
                path: dir.join(MIMEAPPS_LIST),
                desktop_specific: false,
                applications: applications.then(|| dir.to_owned()),
                read: true,
            });
        };
        add(base_dirs.config_home(), false);
        for dir in base_dirs.config_dirs() {
            add(dir, false);
        }
        add(&base_dirs.data_home().join("applications"), true);
        for dir in base_dirs.data_dirs() {
            add(&dir.join("applications"), true);
        }
        // A file reached twice is read once, where it comes first. A config
        // folder that is also an applications folder keeps its desktop
        // files at its data level, as in `GLib`.
        let mut kept: Vec<LevelPlace> = Vec::new();
        for mut place in places {
            let mut known = kept
                .iter()
                .filter(|known| known.path == place.path)
                .peekable();
            if known.peek().is_none() {
                kept.push(place);
                continue;
            }
            if known.all(|known| known.applications.is_none())
                && place.applications.is_some()
                && !place.desktop_specific
            {
                place.read = false;
                kept.push(place);
            }
        }
        Self::build(kept, apps, mime, None, setup)
    }

    fn build(
        places: Vec<LevelPlace>,
        apps: Arc<AppIndex>,
        mime: Arc<MimeDatabase>,
        try_exec: Option<OsString>,
        setup: Vec<Diagnostic>,
    ) -> Self {
        let dirs: Vec<PathBuf> = apps.dirs().into_iter().map(Path::to_owned).collect();
        let mut diagnostics = setup.clone();
        let mut levels = Vec::new();
        for place in places {
            // Before the data levels every folder counts as lower; a data
            // level counts its own folder and the ones after it (none if
            // the index does not have the folder).
            let position = place
                .applications
                .as_deref()
                .map(|dir| dirs.iter().position(|known| known == dir));
            let lowest = match position {
                None => 0,
                Some(found) => found.unwrap_or(dirs.len()),
            };
            if let (Some(None), false) = (position, place.desktop_specific) {
                diagnostics.push(Diagnostic::new(
                    place.applications.clone(),
                    None,
                    "this applications folder is not in the AppIndex; its applications are left out",
                ));
            }
            let associations = !place.desktop_specific;
            let stamp = FileStamp::of(&place.path);
            let list = if place.read {
                match MimeAppsList::open(&place.path) {
                    Ok(list) => list,
                    Err(err) => {
                        diagnostics.push(Diagnostic::new(
                            Some(place.path.clone()),
                            None,
                            format!("cannot be read: {err}"),
                        ));
                        MimeAppsList::parse(b"", place.desktop_specific)
                    }
                }
            } else {
                MimeAppsList::parse(b"", place.desktop_specific)
            }
            .resolving_aliases(Arc::clone(&mime));
            diagnostics.extend(list.diagnostics());
            let read = |group: Group| -> HashMap<TypePattern, Vec<String>> {
                let mut found: HashMap<TypePattern, Vec<String>> = HashMap::new();
                for (pattern, ids) in list.entries(group) {
                    found
                        .entry(pattern.canonical(&mime))
                        .or_default()
                        .extend(ids);
                }
                found
            };
            levels.push(Level {
                folder: position.flatten().filter(|_| associations),
                lowest,
                stamp,
                defaults: read(Group::Defaults),
                added: if associations {
                    read(Group::Added)
                } else {
                    HashMap::new()
                },
                removed: if associations {
                    read(Group::Removed)
                } else {
                    HashMap::new()
                },
                place,
            });
        }
        let mut folder_types: Vec<HashMap<TypePattern, Vec<String>>> =
            vec![HashMap::new(); dirs.len()];
        for app in apps.iter() {
            let Some(folder) = apps.dir_of(app.id()) else {
                continue;
            };
            let mut patterns: Vec<TypePattern> = app
                .entry()
                .mime_types()
                .iter()
                // As `update-desktop-database` reads them: a space before an
                // item makes it no type.
                .filter_map(|item| TypePattern::parse(item.trim_end()))
                .map(|pattern| pattern.canonical(&mime))
                .collect();
            patterns.dedup();
            for pattern in patterns {
                let ids = folder_types[folder].entry(pattern).or_default();
                if ids.last().map(String::as_str) != Some(app.id()) {
                    ids.push(app.id().to_owned());
                }
            }
        }
        Self {
            levels,
            apps,
            mime,
            folder_types,
            try_exec,
            setup,
            diagnostics,
        }
    }

    /// Enables the `TryExec` check, as in `GLib`. To count as installed, an
    /// application's `TryExec` program must be found at its absolute path
    /// or in a folder of the `PATH`-style `search_path`. This check is off
    /// by default.
    #[must_use]
    pub fn with_try_exec(mut self, search_path: impl Into<OsString>) -> Self {
        self.try_exec = Some(search_path.into());
        self
    }

    /// The `mimeapps.list` files, most important first, whether they exist
    /// or not.
    #[must_use]
    pub fn files(&self) -> Vec<&Path> {
        self.levels
            .iter()
            .filter(|level| level.place.read)
            .map(|level| level.place.path.as_path())
            .collect()
    }

    /// The installed applications this snapshot uses.
    #[must_use]
    pub fn app_index(&self) -> &Arc<AppIndex> {
        &self.apps
    }

    /// The MIME database this snapshot uses.
    #[must_use]
    pub fn mime_database(&self) -> &Arc<MimeDatabase> {
        &self.mime
    }

    /// Problems in the `mimeapps.list` files.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// The applications associated with `mime`, most preferred first: for
    /// the type itself, then each type it is a kind of, then its media
    /// wildcard (`image/*`), then `application/octet-stream` (any data),
    /// then `*/*`. At each step the spec's listing algorithm runs over every
    /// `mimeapps.list` and `applications` folder. Each application appears
    /// once, at its most specific match. A URL scheme's type
    /// (`x-scheme-handler/…`, as asked or as the database names it) has
    /// only its own handlers and the `x-scheme-handler/*` wildcard: a URL
    /// is no data.
    #[must_use]
    pub fn apps_for(&self, mime: &MimeType) -> Vec<AppMatch> {
        let mut found = Vec::new();
        let mut seen = HashSet::new();
        for (step, matched) in self.steps(mime) {
            for id in self.associated(&step) {
                if seen.insert(id.clone())
                    && let Some(app) = self.installed(&id)
                {
                    found.push(AppMatch {
                        app,
                        matched: matched.clone(),
                    });
                }
            }
        }
        found
    }

    /// The default application for `mime`. For each step of the order of
    /// [`MimeApps::apps_for`], most specific first:
    ///
    /// - the first installed application named under `[Default
    ///   Applications]` for that step, in the first `mimeapps.list` that
    ///   names one associated with the step or a less specific one (an
    ///   editor for `text/plain` set as the default for `text/x-csrc`
    ///   counts, as in `GLib`; decision 9);
    /// - else the most preferred installed application associated with the
    ///   step.
    ///
    /// The first step with an answer decides, so an application for the
    /// type itself wins over a default set for a type it is a kind of (as
    /// the spec says).
    #[must_use]
    pub fn default_for(&self, mime: &MimeType) -> Option<AppEntry> {
        let steps = self.steps(mime);
        // Each step's associations, worked out when first needed.
        let mut cache: Vec<Option<Vec<String>>> = vec![None; steps.len()];
        for (at, (step, _)) in steps.iter().enumerate() {
            for level in &self.levels {
                for id in level.defaults.get(step).into_iter().flatten() {
                    // Installed first: it is cheap, and a default that is
                    // gone needs no association worked out.
                    if let Some(app) = self.installed(id)
                        && (at..steps.len())
                            .any(|later| self.step_ids(&mut cache, &steps, later).contains(id))
                    {
                        return Some(app);
                    }
                }
            }
            let first = self
                .step_ids(&mut cache, &steps, at)
                .iter()
                .find_map(|id| self.installed(id));
            if first.is_some() {
                return first;
            }
        }
        None
    }

    /// Returns a new snapshot if a `mimeapps.list` changed, appeared or
    /// disappeared (by its [`FileStamp`]), or if `apps` or `mime` is a
    /// different snapshot from the one used to build this one. Returns
    /// `None` if nothing changed. The caller must refresh `apps` and `mime`
    /// separately.
    ///
    /// Checks the same files as [`MimeApps::load`]. Changes to the base
    /// directories or desktop names need a new `load`. This does not detect
    /// `TryExec` programs appearing or disappearing on the search path.
    #[must_use]
    pub fn refresh(&self, apps: &Arc<AppIndex>, mime: &Arc<MimeDatabase>) -> Option<Self> {
        let unchanged = Arc::ptr_eq(apps, &self.apps)
            && Arc::ptr_eq(mime, &self.mime)
            && self
                .levels
                .iter()
                .all(|level| FileStamp::of(&level.place.path) == level.stamp);
        if unchanged {
            return None;
        }
        let places = self
            .levels
            .iter()
            .map(|level| level.place.clone())
            .collect();
        Some(Self::build(
            places,
            Arc::clone(apps),
            Arc::clone(mime),
            self.try_exec.clone(),
            self.setup.clone(),
        ))
    }

    /// The steps from most to least specific.
    fn steps(&self, mime: &MimeType) -> Vec<(TypePattern, Matched)> {
        let canonical = self.mime.canonical(mime);
        let mut steps = vec![(TypePattern::Exact(canonical.clone()), Matched::Exact)];
        if mime.is_media("x-scheme-handler") || canonical.is_media("x-scheme-handler") {
            // A URL is no data: its scheme's handlers, or none (decision 8),
            // also when the database makes its type an alias of a type of
            // another media type.
            let urls =
                TypePattern::Media(MediaType::new("x-scheme-handler").expect("a valid name"));
            steps.push((urls.clone(), Matched::Wildcard(urls)));
            return steps;
        }
        let media = TypePattern::Media(MediaType::new(canonical.media()).expect("a type's media"));
        // As `ancestors` names it: the database may make it an alias.
        let any_data = self
            .mime
            .canonical(&MimeType::new("application/octet-stream").expect("a valid type"));
        let ancestors = self.mime.ancestors(&canonical);
        for ancestor in ancestors.iter().filter(|ancestor| **ancestor != any_data) {
            steps.push((
                TypePattern::Exact(ancestor.clone()),
                Matched::Parent(ancestor.clone()),
            ));
        }
        steps.push((media.clone(), Matched::Wildcard(media)));
        if ancestors.contains(&any_data) {
            steps.push((
                TypePattern::Exact(any_data.clone()),
                Matched::Parent(any_data),
            ));
        }
        steps.push((TypePattern::Any, Matched::Wildcard(TypePattern::Any)));
        steps
    }

    /// The associations of step `at`, from `cache` or worked out now.
    fn step_ids<'c>(
        &self,
        cache: &'c mut [Option<Vec<String>>],
        steps: &[(TypePattern, Matched)],
        at: usize,
    ) -> &'c [String] {
        cache[at].get_or_insert_with(|| self.associated(&steps[at].0))
    }

    /// The spec's listing algorithm for one step, before checking which
    /// applications are installed.
    fn associated(&self, step: &TypePattern) -> Vec<String> {
        let mut results: Vec<String> = Vec::new();
        let mut listed: HashSet<&str> = HashSet::new();
        let mut excluded: HashSet<&str> = HashSet::new();
        for level in &self.levels {
            for id in level.added.get(step).into_iter().flatten() {
                if !excluded.contains(id.as_str())
                    && !listed.contains(id.as_str())
                    && self.held_from(level.lowest, id)
                {
                    listed.insert(id);
                    results.push(id.clone());
                }
            }
            for id in level.removed.get(step).into_iter().flatten() {
                if self.held_from(level.lowest, id) {
                    excluded.insert(id);
                }
            }
            if let Some(folder) = level.folder {
                for id in self.folder_types[folder].get(step).into_iter().flatten() {
                    if !excluded.contains(id.as_str()) && !listed.contains(id.as_str()) {
                        listed.insert(id);
                        results.push(id.clone());
                    }
                }
                excluded.extend(self.apps.dir_ids(folder));
            }
        }
        results
    }

    /// Whether a folder from `lowest` on holds a desktop file `id`.
    fn held_from(&self, lowest: usize, id: &str) -> bool {
        (lowest..self.folder_types.len()).any(|folder| self.apps.dir_holds(folder, id))
    }

    /// The application `id`, if it is installed: a visible `Type=Application`
    /// entry (links and folders open nothing), whose `TryExec` is found when
    /// asked to check it.
    fn installed(&self, id: &str) -> Option<AppEntry> {
        let app = self.apps.get(id)?;
        if app.entry().entry_type() != Some(EntryType::Application) {
            return None;
        }
        match &self.try_exec {
            Some(search_path) if !app.try_exec(search_path) => None,
            _ => Some(app),
        }
    }
}
