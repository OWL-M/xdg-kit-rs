//! Everything an operation needs from its surroundings, given explicitly.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::exclude::Exclusions;
use crate::info::AbsolutePaths;
use crate::limits::Limits;
use crate::time::{Clock, LocalZone, ZoneError};
use crate::transfer::Fallbacks;

/// Which trash directories an operation may use, and how it reads them.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Policy {
    /// Trash files from other mounts into trash directories on their own
    /// mount (`$topdir/.Trash/$uid`, `$topdir/.Trash-$uid`). Off: such
    /// files are refused, as the spec allows. Listing per-volume trash
    /// directories is not affected.
    pub volume_trash: bool,
    /// Use `$topdir/.Trash-$uid` when `$topdir/.Trash/$uid` cannot be used.
    /// The spec lets only an administrator turn this off.
    pub private_volume_trash: bool,
    /// Top directories whose `.Trash` is used without the sticky bit, for
    /// file systems without one. The spec allows this only by the
    /// administrator's choice: fill it from configuration only the
    /// administrator controls. Compared with top directories exactly;
    /// topdirs found while trashing are canonical paths.
    pub unchecked_sticky: Vec<PathBuf>,
    /// How absolute `Path` values in per-volume trash directories are read.
    pub absolute_paths: AbsolutePaths,
    /// What may stand in for the native non-replacing rename where it is
    /// refused, when trashing and restoring.
    pub fallbacks: Fallbacks,
    /// Places trashing keeps out of, recognised before touching them:
    /// [`Context::trash`], [`Context::trash_into`] and [`Context::plan`]
    /// refuse sources there and never use or probe trash directories
    /// there. Listing, restoring and erasing are not affected.
    pub exclusions: Exclusions,
    /// Follow a symbolic link at `$XDG_DATA_HOME/Trash`, the home trash's
    /// own name, to a directory of any name: a home trash kept elsewhere.
    /// Only that one component may be a link: `$XDG_DATA_HOME` is reached
    /// as always, and `files` and `info` in the target are walked without
    /// following links. The target must pass the home trash's own checks
    /// (a directory, the user's, not writable by group or others); a link
    /// that leads to no directory is a failed open
    /// ([`OpenErrorKind::BrokenLink`](crate::OpenErrorKind::BrokenLink)),
    /// and nothing is ever created through it. The link itself is then
    /// part of the home trash, and is not trashed. The trash's relative
    /// `Path` values stay relative to `$XDG_DATA_HOME`. Off: a link there
    /// fails the check like any ([`CheckFailure::Symlink`](crate::CheckFailure::Symlink)),
    /// and can be trashed as an ordinary link.
    ///
    /// The path alone vouches for nothing: who may change the link is who
    /// may write to `$XDG_DATA_HOME`. Following it is what keeps the
    /// checks meaningful — the directory is checked as opened, and used
    /// through that open handle.
    pub follow_home_link: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            volume_trash: true,
            private_volume_trash: true,
            unchecked_sticky: Vec::new(),
            absolute_paths: AbsolutePaths::default(),
            fallbacks: Fallbacks::default(),
            exclusions: Exclusions::default(),
            follow_home_link: true,
        }
    }
}

/// The user, their home trash, the clock, the time zone, the policy and
/// the limits: everything operations take from outside. Only
/// [`Context::from_env`] reads the environment; operations never do.
///
/// ```
/// use xdg_kit_trash::{Context, LocalZone};
///
/// let context = Context::new("/home/u/.local/share", 1000, "/home/u", LocalZone::utc())?;
/// assert_eq!(context.home_trash().path(), std::path::Path::new("/home/u/.local/share/Trash"));
/// # Ok::<(), xdg_kit_trash::ContextError>(())
/// ```
#[derive(Debug, Clone)]
pub struct Context {
    pub(crate) data_home: PathBuf,
    pub(crate) uid: u32,
    pub(crate) cwd: PathBuf,
    pub(crate) zone: LocalZone,
    pub(crate) clock: Clock,
    pub(crate) policy: Policy,
    pub(crate) limits: Limits,
}

/// Why a [`Context`] could not be made.
#[derive(Debug)]
#[non_exhaustive]
pub enum ContextError {
    /// The base directories could not be determined.
    BaseDirs(xdg_kit_basedir::Error),
    /// The current directory could not be determined.
    CurrentDir(io::Error),
    /// The working directory given is not absolute.
    RelativeCwd(PathBuf),
    /// The system's time zone could not be loaded.
    Zone(ZoneError),
}

impl fmt::Display for ContextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BaseDirs(err) => write!(f, "no base directories: {err}"),
            Self::CurrentDir(err) => write!(f, "no current directory: {err}"),
            Self::RelativeCwd(path) => {
                write!(
                    f,
                    "the working directory {} is not absolute",
                    path.display()
                )
            }
            Self::Zone(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for ContextError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BaseDirs(err) => Some(err),
            Self::CurrentDir(err) => Some(err),
            Self::RelativeCwd(_) => None,
            Self::Zone(err) => Some(err),
        }
    }
}

impl Context {
    /// A context from explicit values: `$XDG_DATA_HOME`, the numeric user
    /// ID, the absolute working directory that relative paths are taken
    /// from, and the zone deletion times are written in. A relative
    /// `data_home` is taken from `cwd`, once, here.
    ///
    /// # Errors
    ///
    /// [`ContextError::RelativeCwd`] when `cwd` is not absolute.
    pub fn new(
        data_home: impl Into<PathBuf>,
        uid: u32,
        cwd: impl Into<PathBuf>,
        zone: LocalZone,
    ) -> Result<Self, ContextError> {
        let cwd = cwd.into();
        if !cwd.is_absolute() {
            return Err(ContextError::RelativeCwd(cwd));
        }
        Ok(Self {
            data_home: cwd.join(data_home.into()),
            uid,
            cwd,
            zone,
            clock: Clock::System,
            policy: Policy::default(),
            limits: Limits::default(),
        })
    }

    /// A context from the environment: `$XDG_DATA_HOME` (or its default)
    /// by the Base Directory spec, the real user ID, the current
    /// directory and the system's time zone ([`LocalZone::system`]).
    ///
    /// # Errors
    ///
    /// When any of these cannot be determined.
    pub fn from_env() -> Result<Self, ContextError> {
        let dirs = xdg_kit_basedir::BaseDirs::from_env().map_err(ContextError::BaseDirs)?;
        let cwd = std::env::current_dir().map_err(ContextError::CurrentDir)?;
        let zone = LocalZone::system().map_err(ContextError::Zone)?;
        Self::new(
            dirs.data_home(),
            rustix::process::getuid().as_raw(),
            cwd,
            zone,
        )
    }

    /// With this policy. Relative paths in it are taken from the working
    /// directory, here.
    #[must_use]
    pub fn with_policy(mut self, mut policy: Policy) -> Self {
        for path in &mut policy.unchecked_sticky {
            *path = self.cwd.join(&*path);
        }
        policy.exclusions = policy.exclusions.anchored(&self.cwd);
        self.policy = policy;
        self
    }

    /// With these limits.
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// With this clock for deletion times.
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// `$XDG_DATA_HOME`, absolute.
    #[must_use]
    pub fn data_home(&self) -> &Path {
        &self.data_home
    }

    /// The numeric user ID whose trash directories are used.
    #[must_use]
    pub fn uid(&self) -> u32 {
        self.uid
    }

    /// The directory relative paths are taken from.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The zone deletion times are written in.
    #[must_use]
    pub fn zone(&self) -> &LocalZone {
        &self.zone
    }

    /// The policy.
    #[must_use]
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// The limits.
    #[must_use]
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// `path` taken from the working directory when relative.
    pub(crate) fn anchor(&self, path: &Path) -> PathBuf {
        self.cwd.join(path)
    }
}
