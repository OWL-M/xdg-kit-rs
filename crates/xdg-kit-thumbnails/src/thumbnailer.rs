//! Discovering `.thumbnailer` files and choosing one for a MIME type.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use xdg_kit_basedir::__hashed::{Set, Str};
use xdg_kit_basedir::{BaseDirs, FileStamp};
use xdg_kit_desktop_entry::KeyFile;
use xdg_kit_mime::{MimeDatabase, MimeType};

use crate::work::{self, Counted};
use crate::{Diagnostic, Limits};

/// The group a thumbnailer file must carry.
const GROUP: &str = "Thumbnailer Entry";

/// One discovered thumbnailer definition.
#[derive(Debug)]
pub struct Thumbnailer {
    id: String,
    path: PathBuf,
    try_exec: Option<String>,
    exec: String,
    mime_types: Vec<MimeType>,
}

impl Thumbnailer {
    /// The id: the file name without `.thumbnailer`. An id in an earlier
    /// data directory shadows later ones entirely.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The file this was read from.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `TryExec`: the program whose presence the spec says to check
    /// before using the entry — at use, not at load (paths change).
    #[must_use]
    pub fn try_exec(&self) -> Option<&str> {
        self.try_exec.as_deref()
    }

    /// The `Exec` line, unparsed (see `command`).
    #[must_use]
    pub fn exec(&self) -> &str {
        &self.exec
    }

    /// The MIME types the thumbnailer declares.
    #[must_use]
    pub fn mime_types(&self) -> &[MimeType] {
        &self.mime_types
    }

    /// A bare entry for this crate's own tests.
    #[cfg(test)]
    pub(crate) fn for_tests(exec: &str) -> Self {
        Self {
            id: "test".to_owned(),
            path: PathBuf::new(),
            try_exec: None,
            exec: exec.to_owned(),
            mime_types: vec![MimeType::new("image/png").expect("a valid type")],
        }
    }

    /// Runs the `TryExec` check against `path_var` (the `PATH` value to
    /// search): an absolute `TryExec` must be an executable file, a bare
    /// name must be found on the path. No `TryExec` means available.
    #[must_use]
    pub fn available(&self, path_var: Option<&OsStr>) -> bool {
        let Some(program) = &self.try_exec else {
            return true;
        };
        work::count(program.len());
        let executable = |path: &Path| {
            rustix::fs::access(path, rustix::fs::Access::EXEC_OK).is_ok()
                && std::fs::metadata(path).is_ok_and(|found| found.is_file())
        };
        if program.contains('/') {
            return executable(Path::new(program));
        }
        let Some(path_var) = path_var else {
            return false;
        };
        std::env::split_paths(path_var).any(|dir| {
            work::count(1);
            !dir.as_os_str().is_empty() && executable(&dir.join(program))
        })
    }
}

/// The installed thumbnailers: an immutable snapshot of every
/// `thumbnailers` directory under the XDG data paths.
#[derive(Debug)]
pub struct Thumbnailers {
    entries: Vec<Thumbnailer>,
    diagnostics: Vec<Diagnostic>,
    /// Every directory looked at and every file seen — rejected ones
    /// included, so fixing one makes it discoverable — with the stamps
    /// `refresh` compares.
    stamps: Vec<(PathBuf, FileStamp)>,
    dirs: Vec<PathBuf>,
    limits: Limits,
}

impl Thumbnailers {
    /// Reads every `*.thumbnailer` under `data_home()/thumbnailers` and
    /// each `data_dirs()` one, with the default [`Limits`].
    #[must_use]
    pub fn load(base_dirs: &BaseDirs) -> Self {
        Self::load_with(base_dirs, &Limits::default())
    }

    /// [`Thumbnailers::load`] within `limits`.
    #[must_use]
    pub fn load_with(base_dirs: &BaseDirs, limits: &Limits) -> Self {
        let mut dirs = vec![base_dirs.data_home().join("thumbnailers")];
        dirs.extend(
            base_dirs
                .data_dirs()
                .iter()
                .map(|dir| dir.join("thumbnailers")),
        );
        Self::from_dirs(dirs, limits.clone())
    }

    fn from_dirs(dirs: Vec<PathBuf>, limits: Limits) -> Self {
        let mut this = Self {
            entries: Vec::new(),
            diagnostics: Vec::new(),
            stamps: Vec::new(),
            dirs,
            limits,
        };
        let mut seen = Set::<Str<Counted>>::new();
        let mut attempted = 0usize;
        for dir in this.dirs.clone() {
            this.stamps.push((dir.clone(), FileStamp::of(&dir)));
            let Ok(listed) = std::fs::read_dir(&dir) else {
                continue;
            };
            // Byte order of the names, so "first" is deterministic and
            // never filesystem enumeration order.
            let mut names: Vec<_> = listed
                .filter_map(Result::ok)
                .map(|entry| entry.file_name())
                .filter(|name| name.as_bytes().ends_with(b".thumbnailer"))
                .collect();
            names.sort_unstable();
            for name in names {
                work::count(1 + name.len());
                this.read_file(&dir.join(&name), &name, &mut seen, &mut attempted);
            }
        }
        this
    }

    fn read_file(
        &mut self,
        path: &Path,
        name: &OsStr,
        seen: &mut Set<Str<Counted>>,
        attempted: &mut usize,
    ) {
        self.stamps.push((path.to_owned(), FileStamp::of(path)));
        let Some(id) = name
            .to_str()
            .and_then(|name| name.strip_suffix(".thumbnailer"))
        else {
            self.skip(path, "the file name is not valid UTF-8");
            return;
        };
        // The id hashed and, when new, copied (counted by the set).
        work::count(1);
        if !seen.insert(id) {
            // Shadowed entirely by an earlier directory's id.
            return;
        }
        // The limit bounds the files *read*, rejected ones included —
        // otherwise arbitrarily many broken definitions would still all
        // be read and parsed.
        *attempted += 1;
        if *attempted > self.limits.max_thumbnailers {
            self.skip(
                path,
                format!("more than {} thumbnailers", self.limits.max_thumbnailers),
            );
            return;
        }
        let bytes = match xdg_kit_basedir::read_regular_file_with(path, self.limits.max_file_len) {
            Ok(bytes) => bytes,
            Err(err) => {
                self.skip(path, format!("cannot be read: {err}"));
                return;
            }
        };
        // Each byte handed to the key-file parser (which counts its own
        // work on its crate's counter, not this one).
        work::count(bytes.len());
        let file = KeyFile::parse(&bytes);
        let value = |key: &str| file.string(GROUP, key);
        let Some(exec) = value("Exec").filter(|exec| !exec.is_empty()) else {
            self.skip(path, "no Exec in the Thumbnailer Entry group");
            return;
        };
        let Some(types) = value("MimeType") else {
            self.skip(path, "no MimeType in the Thumbnailer Entry group");
            return;
        };
        let mut mime_types = Vec::new();
        for item in types.split(';').filter(|item| !item.is_empty()) {
            work::count(item.len());
            match MimeType::new(item) {
                Ok(mime) => mime_types.push(mime),
                Err(_) => self.skip(path, format!("\"{item}\" is not a MIME type")),
            }
        }
        if mime_types.is_empty() {
            self.skip(path, "no usable MimeType");
            return;
        }
        self.entries.push(Thumbnailer {
            id: id.to_owned(),
            path: path.to_owned(),
            try_exec: value("TryExec").filter(|program| !program.is_empty()),
            exec,
            mime_types,
        });
    }

    fn skip(&mut self, path: &Path, message: impl Into<String>) {
        self.diagnostics
            .push(Diagnostic::new(Some(path.to_owned()), None, message));
    }

    /// The thumbnailers, in directory precedence and then id order.
    pub fn iter(&self) -> impl Iterator<Item = &Thumbnailer> {
        self.entries.iter()
    }

    /// Problems found while loading: skipped or partly read files.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// The thumbnailers for `mime`, nearest relation first: those
    /// declaring the canonical type, then an alias, then an ancestor —
    /// no duplicates, and discovery order within one relation. A caller
    /// that wants exactly one takes the first.
    #[must_use]
    pub fn for_type<'a>(
        &'a self,
        database: &MimeDatabase,
        mime: &MimeType,
    ) -> Vec<&'a Thumbnailer> {
        let canonical = database.canonical(mime);
        // The canonical type and its aliases match the *declared*
        // spelling, so an alias declaration never jumps the queue ahead
        // of an exact one; ancestors compare canonically, equivalence
        // being the point there.
        let mut spelled = vec![canonical.clone()];
        spelled.extend(database.aliases(&canonical));
        let ancestors = database.ancestors(&canonical);
        let mut found: Vec<&Thumbnailer> = Vec::new();
        // Which entries are taken, by position: a flag per entry, so the
        // duplicate check costs one step however many match.
        let mut taken = vec![false; self.entries.len()];
        work::count(self.entries.len());
        let mut take =
            |relation: &MimeType, by_spelling: bool, found: &mut Vec<&'a Thumbnailer>| {
                for (at, entry) in self.entries.iter().enumerate() {
                    // Type comparisons are bounded by a type's length (at
                    // most 255 bytes).
                    work::count(1);
                    let declares = entry.mime_types.iter().any(|declared| {
                        work::count(1);
                        if by_spelling {
                            declared == relation
                        } else {
                            database.canonical(declared) == *relation
                        }
                    });
                    if declares && !taken[at] {
                        taken[at] = true;
                        found.push(entry);
                    }
                }
            };
        for relation in &spelled {
            take(relation, true, &mut found);
        }
        for relation in &ancestors {
            take(relation, false, &mut found);
        }
        found
    }

    /// A fresh load, when anything changed since this one — `None` while
    /// every directory and every file seen (rejected ones included)
    /// still looks the same.
    #[must_use]
    pub fn refresh(&self) -> Option<Self> {
        let unchanged = self.stamps.iter().all(|(path, stamp)| {
            work::count(1);
            FileStamp::of(path) == *stamp
        });
        if unchanged {
            return None;
        }
        Some(Self::from_dirs(self.dirs.clone(), self.limits.clone()))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs;

    use super::*;

    pub(crate) fn base(dirs: &[&Path]) -> BaseDirs {
        let home = dirs[0].to_owned();
        let rest = std::env::join_paths(&dirs[1..]).unwrap();
        BaseDirs::from_vars(move |name| match name {
            "HOME" => Some("/home/u".into()),
            "XDG_DATA_HOME" => Some(home.clone().into_os_string()),
            "XDG_DATA_DIRS" => Some(rest.clone()),
            _ => None,
        })
        .unwrap()
    }

    pub(crate) fn write(dir: &Path, id: &str, body: &str) -> PathBuf {
        let folder = dir.join("thumbnailers");
        fs::create_dir_all(&folder).unwrap();
        let path = folder.join(format!("{id}.thumbnailer"));
        fs::write(&path, body).unwrap();
        path
    }

    const GOOD: &str = "[Thumbnailer Entry]\nTryExec=convert\nExec=convert %i -thumbnail %sx%s %o\n\
         MimeType=image/png;image/x-foo;\n";

    #[test]
    fn discovery_orders_shadows_and_skips_with_diagnostics() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        write(first.path(), "b-two", GOOD);
        write(first.path(), "a-one", GOOD);
        write(
            second.path(),
            "a-one",
            "[Thumbnailer Entry]\nExec=shadowed %i %o\nMimeType=image/png;\n",
        );
        write(second.path(), "c-three", GOOD);
        write(
            second.path(),
            "broken",
            "[Thumbnailer Entry]\nMimeType=image/png;\n",
        );
        let loaded = Thumbnailers::load(&base(&[first.path(), second.path()]));
        let ids: Vec<_> = loaded.iter().map(Thumbnailer::id).collect();
        // Byte order within a directory, precedence across directories,
        // the shadowed id taken from the earlier one.
        assert_eq!(ids, ["a-one", "b-two", "c-three"]);
        assert!(loaded.iter().all(|entry| entry.exec() != "shadowed %i %o"));
        assert!(
            loaded
                .diagnostics()
                .iter()
                .any(|d| d.message.contains("no Exec")),
            "{:?}",
            loaded.diagnostics()
        );
    }

    #[test]
    fn refresh_sees_in_place_edits_and_fixed_rejects() {
        let dir = tempfile::tempdir().unwrap();
        let good = write(dir.path(), "a", GOOD);
        let broken = write(dir.path(), "b", "junk");
        let loaded = Thumbnailers::load(&base(&[dir.path()]));
        assert_eq!(loaded.iter().count(), 1);
        assert!(loaded.refresh().is_none(), "nothing changed");
        // An in-place edit leaves the directory stamp alone; the file
        // stamp catches it.
        fs::write(&good, GOOD.replace("image/x-foo", "image/x-bar")).unwrap();
        let edited = loaded.refresh().expect("the edit is seen");
        assert!(edited.refresh().is_none());
        // Fixing a previously rejected file makes it discoverable.
        fs::write(&broken, GOOD).unwrap();
        let fixed = edited.refresh().expect("the fix is seen");
        assert_eq!(fixed.iter().count(), 2);
    }

    #[test]
    fn available_checks_try_exec_at_use() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("tool");
        fs::write(&tool, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        let entry = |try_exec: &str| Thumbnailer {
            id: "x".into(),
            path: PathBuf::new(),
            try_exec: Some(try_exec.to_owned()),
            exec: "x %i %o".into(),
            mime_types: vec![MimeType::new("image/png").unwrap()],
        };
        assert!(entry(tool.to_str().unwrap()).available(None), "absolute");
        assert!(!entry("/nonexistent/tool").available(None));
        let path_var = std::env::join_paths([dir.path()]).unwrap();
        assert!(entry("tool").available(Some(&path_var)));
        assert!(!entry("tool").available(None), "no PATH, no bare names");
        assert!(!entry("missing").available(Some(&path_var)));
        let none = Thumbnailer {
            try_exec: None,
            ..entry("x")
        };
        assert!(none.available(None), "no TryExec means available");
    }

    #[test]
    fn for_type_orders_by_relation() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "exact",
            "[Thumbnailer Entry]\nExec=a %i %o\nMimeType=image/png;\n",
        );
        write(
            dir.path(),
            "parent",
            "[Thumbnailer Entry]\nExec=b %i %o\nMimeType=application/octet-stream;\n",
        );
        write(
            dir.path(),
            "unrelated",
            "[Thumbnailer Entry]\nExec=c %i %o\nMimeType=text/x-nothing;\n",
        );
        let loaded = Thumbnailers::load(&base(&[dir.path()]));
        // A database with known relations: the system's is not
        // guaranteed here, so write the merged files a database folder
        // holds.
        let mime_dir = tempfile::tempdir().unwrap();
        fs::write(
            mime_dir.path().join("aliases"),
            "image/x-alias image/png
",
        )
        .unwrap();
        fs::write(
            mime_dir.path().join("subclasses"),
            "image/png image/x-root
",
        )
        .unwrap();
        let database = MimeDatabase::from_dirs([mime_dir.path()]);
        let png = MimeType::new("image/png").unwrap();
        // Every non-text type implicitly descends from
        // application/octet-stream, so "parent" matches as an ancestor —
        // after the exact declaration.
        let chosen: Vec<_> = loaded
            .for_type(&database, &png)
            .iter()
            .map(|entry| entry.id().to_owned())
            .collect();
        assert_eq!(chosen, ["exact", "parent"]);
        // The alias resolves to the same thumbnailers.
        let alias = MimeType::new("image/x-alias").unwrap();
        let via_alias: Vec<_> = loaded
            .for_type(&database, &alias)
            .iter()
            .map(|entry| entry.id().to_owned())
            .collect();
        assert_eq!(via_alias, ["exact", "parent"]);
        // A nearer ancestor comes before the implicit octet-stream one.
        write(
            dir.path(),
            "zz-ancestor",
            "[Thumbnailer Entry]\nExec=d %i %o\nMimeType=image/x-root;\n",
        );
        let loaded = loaded.refresh().unwrap();
        let chosen: Vec<_> = loaded
            .for_type(&database, &png)
            .iter()
            .map(|entry| entry.id().to_owned())
            .collect();
        assert_eq!(chosen, ["exact", "zz-ancestor", "parent"]);
        // An unrelated type reaches only the octet-stream fallback:
        // nothing leaks from the exact or x-root declarations.
        let none = MimeType::new("video/x-nowhere").unwrap();
        let fallback: Vec<_> = loaded
            .for_type(&database, &none)
            .iter()
            .map(|entry| entry.id().to_owned())
            .collect();
        assert_eq!(fallback, ["parent"]);
    }
}

#[cfg(test)]
mod linearity {
    use std::fs;

    use super::*;
    use crate::work;

    #[test]
    fn many_thumbnailer_files_load_in_linear_time() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("thumbnailers");
        fs::create_dir_all(&folder).unwrap();
        let base = {
            let home = dir.path().to_owned();
            BaseDirs::from_vars(move |name| match name {
                "HOME" => Some("/home/u".into()),
                "XDG_DATA_HOME" => Some(home.clone().into_os_string()),
                _ => None,
            })
            .unwrap()
        };
        let grow = |n: usize| {
            let mut bytes = 0;
            for at in 0..n {
                let body =
                    format!("[Thumbnailer Entry]\nExec=tool-{at} %i %o\nMimeType=image/x-t{at};\n");
                bytes += body.len();
                fs::write(folder.join(format!("t{at:06}.thumbnailer")), body).unwrap();
            }
            bytes
        };
        work::assert_linear(
            400,
            |n| {
                let bytes = grow(n);
                let (loaded, steps) = work::steps(|| Thumbnailers::load(&base));
                assert_eq!(loaded.iter().count(), n);
                (steps, bytes)
            },
            // Per file: the name examined, the id hashed and copied, the
            // bytes read and examined, the one type parsed; a handful of
            // steps each, twice the analysed cost.
            |n, bytes| 2 * (20 * (n + 4) + 60 * n + 4 * bytes),
        );
    }

    #[test]
    fn choosing_among_many_entries_is_linear() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("thumbnailers");
        fs::create_dir_all(&folder).unwrap();
        let base = {
            let home = dir.path().to_owned();
            BaseDirs::from_vars(move |name| match name {
                "HOME" => Some("/home/u".into()),
                "XDG_DATA_HOME" => Some(home.clone().into_os_string()),
                _ => None,
            })
            .unwrap()
        };
        let database = MimeDatabase::from_dirs(Vec::<PathBuf>::new());
        work::assert_linear(
            400,
            |n| {
                let mut names = 0;
                for at in 0..n {
                    let body =
                        format!("[Thumbnailer Entry]\nExec=t %i %o\nMimeType=image/x-t{at};\n");
                    names += format!("image/x-t{at}").len();
                    fs::write(folder.join(format!("t{at:06}.thumbnailer")), body).unwrap();
                }
                let loaded = Thumbnailers::load(&base);
                let wanted = MimeType::new("image/x-t0").unwrap();
                let (chosen, steps) = work::steps(|| loaded.for_type(&database, &wanted));
                assert_eq!(chosen.len(), 1);
                (steps, names)
            },
            // Two relations (the type and its implicit ancestor), each a
            // pass over the entries' declarations; type comparisons are
            // bounded by a type's length (at most 255 bytes).
            |n, names| 2 * (6 * (n + 4) + names),
        );
    }
}

#[cfg(test)]
mod review_regressions {
    use std::fs;

    use super::tests::{base, write};
    use super::*;

    #[test]
    fn an_alias_declaration_never_beats_an_exact_one() {
        let dir = tempfile::tempdir().unwrap();
        // "a-alias" sorts before "z-exact"; only correct relation
        // ordering can put the exact declaration first.
        write(
            dir.path(),
            "a-alias",
            "[Thumbnailer Entry]\nExec=a %i %o\nMimeType=image/x-alias;\n",
        );
        write(
            dir.path(),
            "z-exact",
            "[Thumbnailer Entry]\nExec=z %i %o\nMimeType=image/png;\n",
        );
        let loaded = Thumbnailers::load(&base(&[dir.path()]));
        let mime_dir = tempfile::tempdir().unwrap();
        fs::write(mime_dir.path().join("aliases"), "image/x-alias image/png\n").unwrap();
        let database = MimeDatabase::from_dirs([mime_dir.path()]);
        let png = MimeType::new("image/png").unwrap();
        let chosen: Vec<_> = loaded
            .for_type(&database, &png)
            .iter()
            .map(|entry| entry.id().to_owned())
            .collect();
        assert_eq!(chosen, ["z-exact", "a-alias"]);
    }

    #[test]
    fn the_limit_bounds_files_read_not_entries_kept() {
        let dir = tempfile::tempdir().unwrap();
        for at in 0..5 {
            // All invalid: no Exec.
            write(
                dir.path(),
                &format!("broken-{at}"),
                "[Thumbnailer Entry]\nMimeType=image/png;\n",
            );
        }
        let limits = Limits {
            max_thumbnailers: 1,
            ..Limits::default()
        };
        let loaded = Thumbnailers::load_with(&base(&[dir.path()]), &limits);
        assert_eq!(loaded.iter().count(), 0);
        let over: usize = loaded
            .diagnostics()
            .iter()
            .filter(|d| d.message.contains("more than 1 thumbnailers"))
            .count();
        assert_eq!(over, 4, "only the first file was read and parsed");
    }

    #[test]
    fn many_simultaneous_matches_stay_linear() {
        use std::fmt::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("thumbnailers");
        fs::create_dir_all(&folder).unwrap();
        let data_home = dir.path().to_owned();
        let base = BaseDirs::from_vars(move |name| match name {
            "HOME" => Some("/home/u".into()),
            "XDG_DATA_HOME" => Some(data_home.clone().into_os_string()),
            _ => None,
        })
        .unwrap();
        let database = MimeDatabase::from_dirs(Vec::<PathBuf>::new());
        crate::work::assert_linear(
            400,
            |n| {
                // Every entry declares the queried type: the worst case
                // for duplicate checking.
                for at in 0..n {
                    let mut body = String::new();
                    let _ = write!(
                        body,
                        "[Thumbnailer Entry]\nExec=t %i %o\nMimeType=image/png;\n"
                    );
                    fs::write(folder.join(format!("t{at:06}.thumbnailer")), body).unwrap();
                }
                let loaded = Thumbnailers::load(&base);
                let wanted = MimeType::new("image/png").unwrap();
                let (chosen, steps) = crate::work::steps(|| loaded.for_type(&database, &wanted));
                assert_eq!(chosen.len(), n);
                (steps, n)
            },
            // Two relations (the type, its implicit ancestor), a pass
            // each over the entries, the taken flags initialized once.
            |n, _| 2 * (8 * (n + 4)),
        );
    }
}
