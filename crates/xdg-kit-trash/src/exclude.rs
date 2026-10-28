//! Volumes kept out of trashing, recognised from paths alone, before the
//! file system is touched.
//!
//! A volume that has stopped answering — a network mount, say — can hang
//! any system call that reaches it. [`Exclusions`] names such places, and
//! trashing checks a path against them before anything reaches the file
//! system: the check is lexical, on the path as given (anchored at the
//! context's working directory), so a path it refuses causes no system call
//! at all. Mount information comes from the caller ([`Exclusions::from_mounts`]
//! takes a [`mounts`](crate::mounts) snapshot, or the caller lists places
//! itself); trashing never reads mount tables or classifies file systems
//! on its own.
//!
//! # Limits
//!
//! A lexical check sees names, not files. A symbolic link or a bind mount
//! that leads into an excluded volume under another name is not recognised
//! from the path: trashing checks the resolved (canonical) path again once
//! it has it, but resolving it already reached the volume. Callers that
//! must never reach a volume list its aliases as rules too, or refuse
//! paths they cannot vouch for before trashing; [`Exclusions::refuse_parent_components`]
//! refuses `..` outright. A path that passes is trashed as usual: nothing
//! promises it cannot reach a hung volume some other way.

use std::ffi::OsString;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use crate::mounts::Mount;

/// How far a rule reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ExclusionScope {
    /// The volume mounted at the path: the path and everything below it,
    /// except below a known mount point nested inside it
    /// ([`Exclusions::with_mount_points`]), which is another volume.
    Mount,
    /// The path and everything below it, nested mounts included.
    Subtree,
}

/// One excluded place.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExclusionRule {
    /// The place: absolute, lexically clean (no `.`, `..` or repeated
    /// separators).
    pub path: PathBuf,
    /// How far it reaches.
    pub scope: ExclusionScope,
    /// What the caller calls it, for messages (a file system type, a
    /// configuration key).
    pub label: String,
}

/// The places trashing keeps out of: no per-volume trash directory is
/// created there, nothing there is trashed, and nothing there is probed
/// first. The empty default excludes nothing.
///
/// Rules are matched by path components (`/mnt/a` covers `/mnt/a/b`, not
/// `/mnt/ab`). Where several rules cover a path, it is excluded, and the
/// deepest rule is reported.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Exclusions {
    rules: Vec<ExclusionRule>,
    mount_points: Vec<PathBuf>,
    refuse_parent_components: bool,
}

/// A path trashing keeps out of.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Excluded {
    /// The path checked, anchored as it was checked.
    pub path: PathBuf,
    /// Why.
    pub reason: ExclusionReason,
}

/// Why a path is excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExclusionReason {
    /// Looking it up would visit `visited`, which this rule covers.
    Rule {
        /// The rule.
        rule: ExclusionRule,
        /// The place on the way that the rule covers.
        visited: PathBuf,
    },
    /// It has a `..` component, which
    /// [`Exclusions::refuse_parent_components`] refuses.
    ParentComponent,
}

impl fmt::Display for Excluded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.reason {
            ExclusionReason::Rule { rule, visited } => write!(
                f,
                "{} is excluded: {} is in {} ({})",
                self.path.display(),
                visited.display(),
                rule.path.display(),
                rule.label
            ),
            ExclusionReason::ParentComponent => {
                write!(
                    f,
                    "{} is excluded: it has a '..' component",
                    self.path.display()
                )
            }
        }
    }
}

impl std::error::Error for Excluded {}

/// `path` with `.` and repeated separators dropped and `..` taken
/// lexically, for rules: they name places, not lookups.
fn clean(path: &Path) -> PathBuf {
    let mut parts: Vec<OsString> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => parts.push(name.to_os_string()),
            Component::ParentDir => {
                parts.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    let mut clean = PathBuf::from("/");
    clean.extend(parts);
    clean
}

impl Exclusions {
    /// No exclusions.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Also excludes the volume mounted at `path` ([`ExclusionScope::Mount`]).
    /// A relative path is taken from the working directory when the policy
    /// is set ([`Context::with_policy`](crate::Context::with_policy)).
    #[must_use]
    pub fn mount(mut self, path: impl Into<PathBuf>, label: impl Into<String>) -> Self {
        self.rules.push(ExclusionRule {
            path: path.into(),
            scope: ExclusionScope::Mount,
            label: label.into(),
        });
        self
    }

    /// Also excludes `path` and everything below it
    /// ([`ExclusionScope::Subtree`]).
    #[must_use]
    pub fn subtree(mut self, path: impl Into<PathBuf>, label: impl Into<String>) -> Self {
        self.rules.push(ExclusionRule {
            path: path.into(),
            scope: ExclusionScope::Subtree,
            label: label.into(),
        });
        self
    }

    /// The mount points known, so that a [`ExclusionScope::Mount`] rule
    /// stops at volumes mounted inside its own. Without them, a mount rule
    /// reaches as far as a subtree rule.
    #[must_use]
    pub fn with_mount_points(mut self, points: impl IntoIterator<Item = PathBuf>) -> Self {
        self.mount_points.extend(points);
        self
    }

    /// Refuses any path with a `..` component, as
    /// [`ExclusionReason::ParentComponent`]: for callers that would rather
    /// not reason about where `..` leads.
    #[must_use]
    pub fn refuse_parent_components(mut self, refuse: bool) -> Self {
        self.refuse_parent_components = refuse;
        self
    }

    /// Exclusions from a mount list (a [`mounts`](crate::mounts) snapshot,
    /// or the caller's own): each mount `exclude` picks becomes a
    /// [`ExclusionScope::Mount`] rule labelled with its file system type,
    /// and every mount point bounds those rules.
    #[must_use]
    pub fn from_mounts(mounts: &[Mount], exclude: impl Fn(&Mount) -> bool) -> Self {
        let mut exclusions =
            Self::new().with_mount_points(mounts.iter().map(|mount| mount.mount_point.clone()));
        for mount in mounts.iter().filter(|mount| exclude(mount)) {
            exclusions = exclusions.mount(mount.mount_point.clone(), mount.fs_type.clone());
        }
        exclusions
    }

    /// The rules.
    #[must_use]
    pub fn rules(&self) -> &[ExclusionRule] {
        &self.rules
    }

    /// Whether there are no rules and nothing is refused.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty() && !self.refuse_parent_components
    }

    /// Anchors relative rules and mount points at `cwd`, and cleans them.
    pub(crate) fn anchored(mut self, cwd: &Path) -> Self {
        for rule in &mut self.rules {
            rule.path = clean(&cwd.join(&rule.path));
        }
        for point in &mut self.mount_points {
            *point = clean(&cwd.join(&*point));
        }
        self
    }

    /// Checks the absolute `path`, without touching the file system: every
    /// place looking it up would visit, in order — each ancestor, including
    /// one a later `..` leaves again — against every rule.
    ///
    /// A [`ExclusionScope::Subtree`] rule covers every place it reaches.
    /// A [`ExclusionScope::Mount`] rule covers the places of its own
    /// volume, but not a directory of it passed through, descending, on the
    /// way to a known mount point nested inside it where the path ends:
    /// reaching that volume needs only the excluded volume's directories on
    /// the way. A place reached by `..` is no such passage — `..` returns
    /// into the excluded volume — and counts.
    /// Whether that lookup touches the excluded file system is up to the
    /// system (it usually finds the mount point in its caches); a subtree
    /// rule forbids it.
    ///
    /// # Errors
    ///
    /// [`Excluded`] when a rule covers one of those places, or the path has
    /// a `..` component and those are refused.
    pub fn check(&self, path: &Path) -> Result<(), Excluded> {
        if self.is_empty() {
            return Ok(());
        }
        let excluded = |reason| {
            Err(Excluded {
                path: path.to_path_buf(),
                reason,
            })
        };
        // Each place, and whether it was reached descending (not by `..`,
        // which returns into a directory and looks it up again): only such
        // a place can be passed through on the way to a nested volume.
        let mut place = PathBuf::from("/");
        let mut visited = vec![(place.clone(), true)];
        for component in path.components() {
            let descending = !matches!(component, Component::ParentDir);
            match component {
                Component::RootDir => place = PathBuf::from("/"),
                Component::CurDir | Component::Prefix(_) => continue,
                Component::ParentDir if self.refuse_parent_components => {
                    return excluded(ExclusionReason::ParentComponent);
                }
                Component::ParentDir => {
                    place.pop();
                }
                Component::Normal(name) => place.push(name),
            }
            visited.push((place.clone(), descending));
        }
        let end = visited
            .last()
            .map(|(place, _)| place.clone())
            .unwrap_or_default();
        for (place, descending) in visited {
            if let Some(rule) = self.covering(&place, &end, descending) {
                return excluded(ExclusionReason::Rule {
                    rule,
                    visited: place,
                });
            }
        }
        Ok(())
    }

    /// The deepest rule covering the clean absolute `place`, visited on the
    /// way to `end` — `descending` when not reached by `..`.
    fn covering(&self, place: &Path, end: &Path, descending: bool) -> Option<ExclusionRule> {
        self.rules
            .iter()
            .filter(|rule| place.starts_with(&rule.path))
            .filter(|rule| match rule.scope {
                ExclusionScope::Subtree => true,
                ExclusionScope::Mount => !self.mount_points.iter().any(|point| {
                    // A volume nested inside this one: the place is in it,
                    // or passed through to reach it where the path ends.
                    point != &rule.path
                        && point.starts_with(&rule.path)
                        && (place.starts_with(point)
                            || (descending && point.starts_with(place) && end.starts_with(point)))
                }),
            })
            .max_by_key(|rule| rule.path.components().count())
            .cloned()
    }
}
