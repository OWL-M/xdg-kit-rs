//! Content types of mounted volumes from tree magic (spec "Content types
//! for volumes", "The treemagic files").

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use crate::model::{TreeKind, TreeMatchlet};
use crate::{MimeDatabase, MimeType, PathOptions};

/// Options for [`MimeDatabase::content_types_for_root`]. None yet; it exists
/// so options can be added without breaking callers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TreeOptions {}

/// Finds entries in `dir` named `name`. With `match_case`, requires an exact
/// match; otherwise compares Unicode lowercase for UTF-8 names and ASCII
/// lowercase for other names. Both `DCIM` and `dcim` can match.
fn find_entries(dir: &Path, name: &[u8], match_case: bool) -> Vec<PathBuf> {
    if match_case {
        let path = dir.join(OsStr::from_bytes(name));
        return fs::symlink_metadata(&path)
            .is_ok()
            .then_some(path)
            .into_iter()
            .collect();
    }
    let wanted = crate::database::lowercase(name);
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| crate::database::lowercase(entry.file_name().as_bytes()) == wanted)
        .map(|entry| entry.path())
        .collect()
}

/// Whether some path the rule path names under `root` passes `accept`.
/// Where a component matches several entries case-insensitively, each is
/// tried in turn, as `GLib` does, until one leads to a path that passes. Rule
/// paths with `..` or a leading `/` never match.
fn any_path(
    root: &Path,
    rule_path: &str,
    match_case: bool,
    accept: &mut impl FnMut(&Path) -> bool,
) -> bool {
    let mut components: Vec<&OsStr> = Vec::new();
    for component in Path::new(rule_path).components() {
        match component {
            Component::Normal(name) => components.push(name),
            Component::CurDir => {}
            Component::RootDir | Component::ParentDir | Component::Prefix(_) => return false,
        }
    }
    walk(root, &components, match_case, accept)
}

fn walk(
    current: &Path,
    rest: &[&OsStr],
    match_case: bool,
    accept: &mut impl FnMut(&Path) -> bool,
) -> bool {
    let Some((first, rest)) = rest.split_first() else {
        return accept(current);
    };
    find_entries(current, first.as_bytes(), match_case)
        .iter()
        .any(|next| walk(next, rest, match_case, accept))
}

impl MimeDatabase {
    fn tree_matchlet_matches(&self, root: &Path, matchlet: &TreeMatchlet) -> bool {
        any_path(root, &matchlet.path, matchlet.match_case, &mut |path| {
            self.path_passes(path, matchlet)
        }) && (matchlet.children.is_empty()
            || matchlet
                .children
                .iter()
                .any(|child| self.tree_matchlet_matches(root, child)))
    }

    /// Whether `path` has the kind, permission, content and type `matchlet`
    /// asks for.
    fn path_passes(&self, path: &Path, matchlet: &TreeMatchlet) -> bool {
        let Ok(link) = fs::symlink_metadata(path) else {
            return false;
        };
        let kind_matches = match matchlet.kind {
            TreeKind::Any => true,
            TreeKind::Link => link.file_type().is_symlink(),
            TreeKind::File => fs::metadata(path).is_ok_and(|target| target.is_file()),
            TreeKind::Directory => fs::metadata(path).is_ok_and(|target| target.is_dir()),
        };
        if !kind_matches {
            return false;
        }
        if matchlet.executable && rustix::fs::access(path, rustix::fs::Access::EXEC_OK).is_err() {
            return false;
        }
        if matchlet.non_empty
            && !fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_some())
        {
            return false;
        }
        if let Some(wanted) = &matchlet.mimetype {
            let found = self.type_for_path(path, &PathOptions::default());
            if !found.is_ok_and(|found| found.mime == self.canonical(wanted)) {
                return false;
            }
        }
        true
    }

    /// The `x-content/*` types of the volume mounted at `root`, highest
    /// priority first (ties: the more important folder, then file order),
    /// each once.
    ///
    /// Each rule's paths are looked up under `root` one component at a time,
    /// case-insensitively unless the rule says `match-case`. Rule paths with
    /// `..` or a leading `/` never match (the database reports them), but
    /// symbolic links on the volume are followed like any other path.
    ///
    /// # Errors
    ///
    /// When `root` cannot be read as a folder.
    pub fn content_types_for_root(
        &self,
        root: impl AsRef<Path>,
        _options: &TreeOptions,
    ) -> io::Result<Vec<MimeType>> {
        let root = root.as_ref();
        fs::read_dir(root)?;
        let mut types: Vec<MimeType> = Vec::new();
        for ranked in &self.merged.tree {
            let rule = &ranked.rule;
            let mime = self.canonical(&rule.mime);
            if types.contains(&mime) {
                continue;
            }
            if rule
                .matchlets
                .iter()
                .any(|matchlet| self.tree_matchlet_matches(root, matchlet))
            {
                types.push(mime);
            }
        }
        Ok(types)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(root: &Path, rule_path: &str, match_case: bool) -> Vec<PathBuf> {
        let mut found = Vec::new();
        any_path(root, rule_path, match_case, &mut |path| {
            found.push(path.to_owned());
            false
        });
        found.sort();
        found
    }

    #[test]
    fn components_are_found_case_insensitively_unless_asked() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("DCIM/Photos")).unwrap();
        assert_eq!(
            paths(root.path(), "dcim/photos", false),
            [root.path().join("DCIM/Photos")]
        );
        assert!(paths(root.path(), "dcim", true).is_empty());
        assert_eq!(paths(root.path(), "DCIM", true), [root.path().join("DCIM")]);
        assert!(paths(root.path(), "../etc", false).is_empty());
        assert!(paths(root.path(), "/etc", false).is_empty());
        assert!(paths(root.path(), "missing", false).is_empty());
    }

    #[test]
    fn every_entry_differing_only_in_case_is_tried() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("DCIM")).unwrap();
        fs::create_dir_all(root.path().join("dcim/photos")).unwrap();
        assert_eq!(
            paths(root.path(), "dcim/photos", false),
            [root.path().join("dcim/photos")],
            "the empty DCIM does not hide dcim/photos"
        );
        assert_eq!(
            paths(root.path(), "DCIM", false),
            [root.path().join("DCIM"), root.path().join("dcim")]
        );
        // A constraint that the first entry fails is tried on the next.
        let non_empty = |path: &Path| fs::read_dir(path).is_ok_and(|mut e| e.next().is_some());
        assert!(any_path(root.path(), "DCIM", false, &mut |path| non_empty(
            path
        )));
    }
}
