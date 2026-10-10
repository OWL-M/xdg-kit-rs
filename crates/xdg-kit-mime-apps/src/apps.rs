//! Reads every `mimeapps.list` in the spec's order into one snapshot.
//! Uses the spec's two algorithms to find the applications associated with
//! a type and its default application.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use xdg_kit_basedir::{BaseDirs, FileStamp};
use xdg_kit_desktop_entry::{AppEntry, AppIndex, Desktops, EntryType};
use xdg_kit_mime::{MimeDatabase, MimeType};

use crate::error::quoted;
use crate::list::{Group, MIMEAPPS_LIST, MimeAppsList};
use crate::{Diagnostic, MediaType, TypePattern, work};

/// One step's associations, in order and as a set.
type Associations<'a> = (Vec<&'a str>, HashSet<&'a str>);

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
    /// For each desktop file ID an `AppIndex` folder holds, the last
    /// folder holding it ([`AppIndex::dir_holds`]).
    held: HashMap<String, usize>,
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
                // Looked at for a `/` or NUL, and lowercased.
                work::count(1 + 2 * name.len());
                // A name is part of a file name: it may not lead elsewhere.
                let usable =
                    !matches!(name.as_str(), "" | "." | "..") && !name.contains(['/', '\0']);
                if !usable {
                    let message = format!(
                        "desktop name \"{}\" cannot name a file; skipped",
                        quoted(name)
                    );
                    // The name looked at to be quoted, and the message.
                    work::count(name.len() + message.len());
                    setup.push(Diagnostic::new(None, None, message));
                }
                usable
            })
            .map(|name| name.to_ascii_lowercase())
            .collect();
        let mut places: Vec<LevelPlace> = Vec::new();
        let mut add = |dir: &Path, applications: bool| {
            let dir_len = dir.as_os_str().len();
            for name in &names {
                // The name and the folder copied into the path, and the
                // folder copied for a data level.
                work::count(1 + name.len() + 2 * dir_len);
                places.push(LevelPlace {
                    path: dir.join(format!("{name}-{MIMEAPPS_LIST}")),
                    desktop_specific: true,
                    applications: applications.then(|| dir.to_owned()),
                    read: true,
                });
            }
            work::count(1 + 2 * dir_len);
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
        // Each data folder copied into its `applications` folder.
        work::count(base_dirs.data_home().as_os_str().len());
        add(&base_dirs.data_home().join("applications"), true);
        for dir in base_dirs.data_dirs() {
            work::count(dir.as_os_str().len());
            add(&dir.join("applications"), true);
        }
        // A file reached twice is read once, where it comes first. A config
        // folder that is also an applications folder keeps its desktop
        // files at its data level, as in `GLib`.
        // For each path, whether a place kept for it is a data level, and
        // for each place whether it is kept and read.
        let mut known: HashMap<&Path, bool> = HashMap::new();
        let mut keep: Vec<Option<bool>> = Vec::with_capacity(places.len());
        for place in &places {
            // The path hashed and compared.
            work::count(1 + 2 * place.path.as_os_str().len());
            let data = place.applications.is_some();
            keep.push(match known.entry(&place.path) {
                Entry::Vacant(entry) => {
                    entry.insert(data);
                    Some(true)
                }
                Entry::Occupied(mut entry) => (!*entry.get() && data && !place.desktop_specific)
                    .then(|| {
                        entry.insert(true);
                        false
                    }),
            });
        }
        let kept = places
            .into_iter()
            .zip(keep)
            .filter_map(|(mut place, read)| {
                place.read = read?;
                Some(place)
            })
            .collect();
        Self::build(kept, apps, mime, None, setup)
    }

    fn build(
        places: Vec<LevelPlace>,
        apps: Arc<AppIndex>,
        mime: Arc<MimeDatabase>,
        try_exec: Option<OsString>,
        setup: Vec<Diagnostic>,
    ) -> Self {
        let dirs = apps.dirs();
        // Each folder's index: the first, should one be listed twice.
        let mut indexes: HashMap<&Path, usize> = HashMap::new();
        for (at, dir) in dirs.iter().enumerate() {
            // Hashed and compared.
            work::count(1 + 2 * dir.as_os_str().len());
            indexes.entry(dir).or_insert(at);
        }
        // For each ID a folder holds, the last folder holding it.
        let mut held: HashMap<String, usize> = HashMap::new();
        for folder in 0..dirs.len() {
            for id in apps.dir_ids(folder) {
                // Copied, hashed and compared.
                work::count(1 + 3 * id.len());
                held.insert(id.to_owned(), folder);
            }
        }
        // The problems found before copied.
        work::count_with(|| setup.iter().map(|problem| 1 + problem.message.len()).sum());
        let mut diagnostics = setup.clone();
        let mut levels = Vec::new();
        for place in places {
            // Before the data levels every folder counts as lower; a data
            // level counts its own folder and the ones after it (none if
            // the index does not have the folder).
            let position = place.applications.as_deref().map(|dir| {
                // Hashed and compared.
                work::count(1 + 2 * dir.as_os_str().len());
                indexes.get(dir).copied()
            });
            let lowest = match position {
                None => 0,
                Some(found) => found.unwrap_or(dirs.len()),
            };
            if let (Some(None), false) = (position, place.desktop_specific) {
                // The folder copied.
                work::count_with(|| {
                    place
                        .applications
                        .as_ref()
                        .map_or(0, |dir| dir.as_os_str().len())
                });
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
                        // The path copied.
                        work::count(place.path.as_os_str().len());
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
                    // The type made canonical (`TypePattern::canonical`
                    // counts), then hashed and compared.
                    work::count(1 + 2 * pattern.text_len());
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
        let folder_types = folder_types(&apps, &mime);
        Self {
            levels,
            apps,
            mime,
            folder_types,
            held,
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
            // A step per level.
            .inspect(|_| work::count(1))
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
            for id in self.associated(&step).0 {
                // Hashed and compared.
                work::count(1 + 2 * id.len());
                if seen.insert(id)
                    && let Some(app) = self.installed(id)
                {
                    // How it matched, copied: a media wildcard's name byte
                    // by byte; a type is an `Arc`, copied in one step.
                    work::count(match &matched {
                        Matched::Wildcard(pattern) => pattern.clone_steps(),
                        Matched::Exact | Matched::Parent(_) => 1,
                    });
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
        let mut cache: Vec<Option<Associations<'_>>> = vec![None; steps.len()];
        for (at, (step, _)) in steps.iter().enumerate() {
            for level in &self.levels {
                // The step hashed and compared.
                work::count(1 + 2 * step.text_len());
                for id in level.defaults.get(step).into_iter().flatten() {
                    // Installed first: it is cheap, and a default that is
                    // gone needs no association worked out.
                    if let Some(app) = self.installed(id)
                        && (at..steps.len()).any(|later| {
                            // Hashed and compared.
                            work::count(1 + 2 * id.len());
                            self.step_ids(&mut cache, &steps, later)
                                .1
                                .contains(id.as_str())
                        })
                    {
                        return Some(app);
                    }
                }
            }
            let first = self
                .step_ids(&mut cache, &steps, at)
                .0
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
            && self.levels.iter().all(|level| {
                // A step per level; reading the file's stamp is I/O.
                work::count(1);
                FileStamp::of(&level.place.path) == level.stamp
            });
        if unchanged {
            return None;
        }
        let places = self
            .levels
            .iter()
            .map(|level| {
                // The place copied: its path, and the `applications`
                // folder of a data level.
                work::count(
                    1 + level.place.path.as_os_str().len()
                        + level
                            .place
                            .applications
                            .as_ref()
                            .map_or(0, |dir| dir.as_os_str().len()),
                );
                level.place.clone()
            })
            .collect();
        // The search path and the problems found before copied.
        work::count_with(|| {
            self.try_exec.as_ref().map_or(0, |path| path.len())
                + self
                    .setup
                    .iter()
                    .map(|problem| 1 + problem.message.len())
                    .sum::<usize>()
        });
        Some(Self::build(
            places,
            Arc::clone(apps),
            Arc::clone(mime),
            self.try_exec.clone(),
            self.setup.clone(),
        ))
    }

    /// The steps from most to least specific.
    ///
    /// A [`MimeType`] is an `Arc`: copying one is a step, whatever its
    /// length. A [`MediaType`] is a `String`: copying one is a step per
    /// byte.
    fn steps(&self, mime: &MimeType) -> Vec<(TypePattern, Matched)> {
        let canonical = self.mime.canonical(mime);
        // Calls into `xdg-kit-mime` count a step each (see `work`): the
        // alias looked up, and the media part of the type asked for and of
        // that result checked; the result copied (an `Arc`).
        work::count(4);
        let mut steps = vec![(TypePattern::Exact(canonical.clone()), Matched::Exact)];
        if mime.is_media("x-scheme-handler") || canonical.is_media("x-scheme-handler") {
            // A URL is no data: its scheme's handlers, or none (decision 8),
            // also when the database makes its type an alias of a type of
            // another media type.
            let urls =
                TypePattern::Media(MediaType::new("x-scheme-handler").expect("a valid name"));
            // The wildcard's name copied.
            work::count(urls.clone_steps());
            steps.push((urls.clone(), Matched::Wildcard(urls)));
            return steps;
        }
        // Its media part found again (a call); `MediaType::new` counts
        // the rest.
        work::count(1);
        let media = TypePattern::Media(MediaType::new(canonical.media()).expect("a type's media"));
        // As `ancestors` names it: the database may make it an alias. Three
        // calls (the fixed type made, its alias looked up, the ancestors
        // found), a step each.
        work::count(3);
        let any_data = self
            .mime
            .canonical(&MimeType::new("application/octet-stream").expect("a valid type"));
        let ancestors = self.mime.ancestors(&canonical);
        for ancestor in ancestors.iter().filter(|ancestor| {
            // Compared with `application/octet-stream`.
            work::count(1 + ancestor.as_str().len());
            **ancestor != any_data
        }) {
            // Copied twice: an `Arc`, a step each.
            work::count(2);
            steps.push((
                TypePattern::Exact(ancestor.clone()),
                Matched::Parent(ancestor.clone()),
            ));
        }
        // The media wildcard's name copied.
        work::count(media.clone_steps());
        steps.push((media.clone(), Matched::Wildcard(media)));
        // The ancestors looked through for `application/octet-stream`, each
        // compared with it.
        work::count_with(|| {
            ancestors
                .iter()
                .map(|ancestor| 1 + ancestor.as_str().len())
                .sum()
        });
        if ancestors.contains(&any_data) {
            // Copied: an `Arc`.
            work::count(1);
            steps.push((
                TypePattern::Exact(any_data.clone()),
                Matched::Parent(any_data),
            ));
        }
        steps.push((TypePattern::Any, Matched::Wildcard(TypePattern::Any)));
        steps
    }

    /// The associations of step `at`, from `cache` or worked out now.
    fn step_ids<'s, 'c>(
        &'s self,
        cache: &'c mut [Option<Associations<'s>>],
        steps: &[(TypePattern, Matched)],
        at: usize,
    ) -> &'c Associations<'s> {
        cache[at].get_or_insert_with(|| self.associated(&steps[at].0))
    }

    /// The spec's listing algorithm for one step, before checking which
    /// applications are installed.
    fn associated(&self, step: &TypePattern) -> Associations<'_> {
        let mut results: Vec<&str> = Vec::new();
        let mut listed: HashSet<&str> = HashSet::new();
        let mut excluded: HashSet<&str> = HashSet::new();
        for level in &self.levels {
            // The step hashed and compared in each of three maps.
            work::count(1 + 6 * step.text_len());
            for id in level.added.get(step).into_iter().flatten() {
                // Hashed and compared in each of two sets, then inserted.
                work::count(1 + 6 * id.len());
                if !excluded.contains(id.as_str())
                    && !listed.contains(id.as_str())
                    && self.held_from(level.lowest, id)
                {
                    listed.insert(id);
                    results.push(id);
                }
            }
            for id in level.removed.get(step).into_iter().flatten() {
                // Inserted: hashed and compared.
                work::count(1 + 2 * id.len());
                if self.held_from(level.lowest, id) {
                    excluded.insert(id);
                }
            }
            if let Some(folder) = level.folder {
                for id in self.folder_types[folder].get(step).into_iter().flatten() {
                    // Hashed and compared in each of two sets, then
                    // inserted.
                    work::count(1 + 6 * id.len());
                    if !excluded.contains(id.as_str()) && !listed.contains(id.as_str()) {
                        listed.insert(id);
                        results.push(id);
                    }
                }
                excluded.extend(
                    self.apps
                        .dir_ids(folder)
                        // Inserted: hashed and compared.
                        .inspect(|id| work::count(1 + 2 * id.len())),
                );
            }
        }
        (results, listed)
    }

    /// Whether a folder from `lowest` on holds a desktop file `id`.
    fn held_from(&self, lowest: usize, id: &str) -> bool {
        // Hashed and compared.
        work::count(1 + 2 * id.len());
        self.held.get(id).is_some_and(|&last| last >= lowest)
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

/// For each folder of `apps`, the applications whose winning file is there,
/// by each (canonical) type pattern their `MimeType=` lists, in ID byte
/// order.
fn folder_types(apps: &AppIndex, mime: &MimeDatabase) -> Vec<HashMap<TypePattern, Vec<String>>> {
    let mut folder_types: Vec<HashMap<TypePattern, Vec<String>>> =
        vec![HashMap::new(); apps.dirs().len()];
    for app in apps.iter() {
        let Some(folder) = apps.dir_of(app.id()) else {
            continue;
        };
        let mut patterns: Vec<TypePattern> = app
            .entry()
            .mime_types()
            .iter()
            // Trimmed.
            .inspect(|item| work::count(1 + item.len()))
            // As `update-desktop-database` reads them: a space before an
            // item makes it no type.
            .filter_map(|item| TypePattern::parse(item.trim_end()))
            // Made canonical (`TypePattern::canonical` counts).
            .map(|pattern| pattern.canonical(mime))
            .collect();
        // Each compared with the one before.
        work::count_with(|| patterns.iter().map(|pattern| 1 + pattern.text_len()).sum());
        patterns.dedup();
        for pattern in patterns {
            // Hashed and compared, and the ID compared with the last one
            // and copied.
            work::count(2 * pattern.text_len() + 2 * app.id().len());
            let ids = folder_types[folder].entry(pattern).or_default();
            if ids.last().map(String::as_str) != Some(app.id()) {
                ids.push(app.id().to_owned());
            }
        }
    }
    folder_types
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::fs;

    use super::*;

    /// An application listing `types`.
    fn app(types: &str) -> String {
        format!("[Desktop Entry]\nType=Application\nName=A\nExec=true\nMimeType={types}\n")
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// Fails at once when `path`, under the temporary folder, holds a `:`,
    /// which would split it in an XDG path list.
    fn assert_listable(path: &Path) {
        assert!(
            !path.as_os_str().as_encoded_bytes().contains(&b':'),
            "TMPDIR contains ':', which an XDG path list cannot hold; set TMPDIR to a path without ':'"
        );
    }

    /// The base directories under `root`: `config` and `data` as the homes,
    /// then the lists `config_dirs` and `data_dirs` (neither empty), of
    /// folders under `root`.
    fn base_dirs(root: &Path, config_dirs: &[PathBuf], data_dirs: &[PathBuf]) -> BaseDirs {
        assert_listable(root);
        let joined = |dirs: &[PathBuf]| std::env::join_paths(dirs).unwrap();
        let (config_dirs, data_dirs) = (joined(config_dirs), joined(data_dirs));
        BaseDirs::from_vars(|name| match name {
            "HOME" => Some(root.into()),
            "XDG_CONFIG_HOME" => Some(root.join("config").into()),
            "XDG_DATA_HOME" => Some(root.join("data").into()),
            "XDG_CONFIG_DIRS" => Some(config_dirs.clone()),
            "XDG_DATA_DIRS" => Some(data_dirs.clone()),
            _ => None,
        })
        .unwrap()
    }

    /// The installed applications and an empty MIME database for `base`.
    fn indexes(base: &BaseDirs) -> (Arc<AppIndex>, Arc<MimeDatabase>) {
        let none: [PathBuf; 0] = [];
        (
            Arc::new(AppIndex::load(base)),
            Arc::new(MimeDatabase::from_dirs(none)),
        )
    }

    fn probe() -> MimeType {
        MimeType::new("text/x-probe").unwrap()
    }

    /// The bytes of the paths of the lists `apps` reads.
    fn path_bytes(apps: &MimeApps) -> usize {
        apps.files().iter().map(|path| path.as_os_str().len()).sum()
    }

    #[test]
    fn the_default_is_chosen_in_linear_time() {
        // `n` installed applications set as the default, none of them
        // associated with the type or a less specific one, and `n`
        // applications associated with it: `n` IDs on the line, or one ID
        // `n` times.
        for repeated in [false, true] {
            let steps = |n: usize| {
                let root = tempfile::tempdir().unwrap();
                let applications = root.path().join("data/applications");
                let mut associated = 0;
                for at in 0..n {
                    write(&applications.join(format!("c{at}.desktop")), &app(""));
                    let id = format!("a{at}.desktop");
                    associated += id.len();
                    write(&applications.join(id), &app("text/x-probe;"));
                }
                let ids = if repeated {
                    "c0.desktop;".repeat(n)
                } else {
                    (0..n).fold(String::new(), |mut ids, at| {
                        let _ = write!(ids, "c{at}.desktop;");
                        ids
                    })
                };
                let list = format!("[Default Applications]\ntext/x-probe={ids}\n");
                write(&root.path().join("config/mimeapps.list"), &list);
                let base = base_dirs(
                    root.path(),
                    &[root.path().join("etc")],
                    &[root.path().join("share")],
                );
                let (apps, mime) = indexes(&base);
                let mime_apps = MimeApps::load(&base, &Desktops::default(), apps, mime);
                let (found, steps) = work::steps(|| mime_apps.default_for(&probe()));
                assert_eq!(
                    found.as_ref().map(AppEntry::id),
                    Some("a0.desktop"),
                    "repeated: {repeated}"
                );
                // Per default and association a step or two; per byte of
                // their IDs about 23: each default looked up, then looked
                // for in the associations of each step, which are
                // gathered, each step setting aside every ID of the folder
                // as seen. Each default, and the association chosen, is
                // installed: its entry copied, with its ID and path (the
                // folder, `/` and the ID), about 2 more per byte of the
                // defaults' IDs, and per default the bytes of the folder.
                // Twice that is allowed.
                let bytes = ids.len() + associated;
                let folders = (n + 1) * applications.as_os_str().len();
                (steps, 2 * (2 * n + 25 * bytes + folders))
            };
            work::assert_linear(
                &format!("default selection, repeated: {repeated}"),
                steps(500),
                steps(1_000),
            );
        }
    }

    #[test]
    fn many_folders_are_read_in_linear_time() {
        // `n` data folders, each `applications` folder with an application
        // for `text/x-probe`, and `n` config folders, every other one also
        // the `applications` folder of a data folder (its lists read at its
        // config place, its desktop files counted at its data place). The
        // user's list adds each application to the type and removes each
        // from another, so each ID's folders are looked for.
        let steps = |n: usize| {
            let root = tempfile::tempdir().unwrap();
            let data: Vec<PathBuf> = (0..n)
                .map(|at| root.path().join(format!("d{at}")))
                .collect();
            let mut ids = String::new();
            let mut id_bytes = 0;
            for (at, dir) in data.iter().enumerate() {
                let id = format!("a{at}.desktop");
                write(&dir.join("applications").join(&id), &app("text/x-probe;"));
                let _ = write!(ids, "{id};");
                id_bytes += id.len();
            }
            let config: Vec<PathBuf> = (0..n)
                .map(|at| {
                    if at % 2 == 0 {
                        data[at].join("applications")
                    } else {
                        root.path().join(format!("c{at}"))
                    }
                })
                .collect();
            let list = format!(
                "[Added Associations]\ntext/x-probe={ids}\n[Removed Associations]\ntext/x-other={ids}\n"
            );
            write(&root.path().join("config/mimeapps.list"), &list);
            let base = base_dirs(root.path(), &config, &data);
            let (apps, mime) = indexes(&base);
            let (changed, _) = indexes(&base);
            let other = MimeType::new("text/x-other").unwrap();
            let (bytes, steps) = work::steps(|| {
                let mime_apps = MimeApps::load(
                    &base,
                    &Desktops::default(),
                    Arc::clone(&apps),
                    Arc::clone(&mime),
                );
                // The lists of the config folders that are also
                // `applications` folders are read once.
                assert_eq!(mime_apps.files().len(), 2 * n + 2 - n / 2);
                assert_eq!(mime_apps.apps_for(&probe()).len(), n);
                assert!(mime_apps.apps_for(&other).is_empty());
                let again = mime_apps.refresh(&changed, &mime).expect("a new snapshot");
                assert_eq!(again.apps_for(&probe()).len(), n);
                path_bytes(&mime_apps)
            });
            // The work, phase by phase: the load (making the places and
            // building the snapshot), three queries (`apps_for` of the type,
            // of `text/x-other`, and of the type again after the refresh)
            // and the refresh (copying the places and building again).
            //
            // Per place (`2 * n + 2`), whatever its path, about 2,350:
            // - each build about 650 at the load and 690 at the refresh:
            //   its list made, its aliases resolved, its three groups read
            //   and its diagnostics gathered, each time looking up the
            //   group names in the key file (about 540 for a list that is
            //   absent or not read), and the types of the applications
            //   (one for every two places) read from their entries, made
            //   canonical and put in their maps (about 240 each);
            // - each query about 310 (350 when its applications match,
            //   each match's entry read for its type): five steps (the
            //   type, `text/plain`, `text/*`, `application/octet-stream`,
            //   `*/*`, 50 bytes), each looked up in three maps at each
            //   place (a step and 6 per byte).
            //
            // Per byte of the paths of the lists read about 18: each of
            // the places, four for every three lists read (the lists of a
            // config folder that is also an `applications` folder are read
            // at its config place only), costs about 13.5 per byte of its
            // path, and the other paths (the `applications` folders and
            // the desktop files) are about as long. At the load each path
            // made (2) and hashed and compared (2); at each data place its
            // folder (copied, 1) and looked up at each build (2 + 2); the
            // `AppIndex` folders hashed and compared at each build (2 + 2);
            // each list read copied when opened at each build (1 + 1); the
            // refresh copies each place (1, and its folder at a data place,
            // 1); each application's entry copied with its path at each
            // build (1 + 1) and by the two queries that match it (1 + 1).
            //
            // Per byte of the application IDs about 166 (each ID is
            // installed once and listed twice in the user's list): each
            // build about 46 (the list read, each ID trimmed and copied,
            // checked for being both added and removed; each ID's folder
            // kept, and its types' map), the queries that match it 30
            // each (gathered, looked up, its folder's set aside at each of
            // the five steps, and the match copied with its path), the
            // other query 14. Twice that is allowed.
            let places = 2 * n + 2;
            (steps, 2 * (2_350 * places + 18 * bytes + 166 * id_bytes))
        };
        work::assert_linear("many folders", steps(250), steps(500));
    }

    #[test]
    fn the_files_are_listed_in_linear_time() {
        // Four folders with `n` desktop names: a list per name and a plain
        // one at each.
        let steps = |n: usize| {
            let root = tempfile::tempdir().unwrap();
            let names: Vec<String> = (0..n).map(|at| format!("D{at}")).collect();
            let base = base_dirs(
                root.path(),
                &[root.path().join("etc")],
                &[root.path().join("share")],
            );
            let (apps, mime) = indexes(&base);
            let mime_apps = MimeApps::load(&base, &Desktops::new(&names), apps, mime);
            let (files, steps) = work::steps(|| mime_apps.files().len());
            let levels = 4 * (n + 1);
            assert_eq!(files, levels);
            assert!(steps >= levels, "{steps} steps for {levels} levels");
            // A step per level. Twice that.
            (steps, 2 * levels)
        };
        work::assert_linear("the files", steps(500), steps(1_000));
    }

    #[test]
    fn many_desktop_names_are_read_in_linear_time() {
        // `n` desktop names, each given twice, at four folders, one of
        // them both a config folder and an `applications` folder; the
        // last name's list in the config home sets the default, which is
        // not the first application associated.
        let steps = |n: usize| {
            let root = tempfile::tempdir().unwrap();
            let share = root.path().join("share");
            for id in ["a.desktop", "b.desktop"] {
                write(&share.join("applications").join(id), &app("text/x-probe;"));
            }
            let names: Vec<String> = (0..n).chain(0..n).map(|at| format!("D{at}")).collect();
            let last = n - 1;
            write(
                &root.path().join(format!("config/d{last}-mimeapps.list")),
                "[Default Applications]\ntext/x-probe=b.desktop;\n",
            );
            let base = base_dirs(root.path(), &[share.join("applications")], &[share]);
            let (apps, mime) = indexes(&base);
            let desktops = Desktops::new(&names);
            let (bytes, steps) = work::steps(|| {
                let mime_apps = MimeApps::load(&base, &desktops, apps, mime);
                // Each name once; at the folder that is both, at its config
                // place.
                assert_eq!(mime_apps.files().len(), 3 * (n + 1));
                assert_eq!(
                    mime_apps.default_for(&probe()).as_ref().map(AppEntry::id),
                    Some("b.desktop")
                );
                path_bytes(&mime_apps)
            });
            // The work, phase by phase: the load (making the places and
            // building the snapshot) and the query (`default_for`). The
            // names are given twice; `Desktops` keeps each once.
            //
            // Per list read (`3 * (n + 1)`), whatever its path, about 510:
            // - the load about 425: each name checked and lowercased, and
            //   each list made, its aliases resolved, its default group
            //   read and its diagnostics gathered, each time looking up
            //   the group names in the key file (about 470 for an absent
            //   desktop-specific list, of which the bytes of its file name
            //   are part of the per-byte term below);
            // - the query about 82: the default looked for at each level
            //   up to the last name's list in the config home (a step and
            //   2 per byte of the type, about a third of the levels), then
            //   the type's associations gathered at every level (a step
            //   and 6 per byte).
            //
            // Per byte of the paths of the lists read about 7: at the load
            // four places are made for every three lists read (at the
            // data place of the folder that is both, the lists were read
            // at its config place), each made (2) and hashed and compared
            // (2); a data place's folder looked up (2, a third of the
            // lists); each list read copied when opened (1). Twice that
            // is allowed.
            (steps, 2 * (510 * 3 * (n + 1) + 7 * bytes))
        };
        work::assert_linear("many desktop names", steps(500), steps(1_000));
    }
}
