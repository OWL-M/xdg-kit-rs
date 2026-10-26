//! Whether an entry starts at login, and why not; and its launch plan.

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use xdg_kit_desktop_entry::{
    DesktopEntry, Desktops, LaunchError, LaunchOptions, LaunchPlan, prepare_launch,
};

use crate::error::shown;
use crate::{Autostart, work};

/// What eligibility is judged against: the session's desktops (for
/// `OnlyShowIn` and `NotShowIn`) and its program search path (for
/// `TryExec`). Explicit, so a caller can judge a session other than its
/// own, and tests touch no environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    desktops: Desktops,
    search_path: OsString,
}

impl Context {
    /// The given desktops and a `PATH`-style search path (directories
    /// separated by `:`).
    #[must_use]
    pub fn new(desktops: Desktops, search_path: impl Into<OsString>) -> Self {
        Self {
            desktops,
            search_path: search_path.into(),
        }
    }

    /// This process's `XDG_CURRENT_DESKTOP` and `PATH`.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_vars(|name| std::env::var_os(name))
    }

    /// `XDG_CURRENT_DESKTOP` and `PATH` as `vars` returns them by name.
    /// An unset `PATH` searches nothing, so a bare `TryExec` name is then
    /// never found.
    #[must_use]
    pub fn from_vars(vars: impl Fn(&str) -> Option<OsString>) -> Self {
        Self {
            desktops: Desktops::from_vars(&vars),
            search_path: vars("PATH").unwrap_or_default(),
        }
    }

    /// The desktops, most specific first.
    #[must_use]
    pub fn desktops(&self) -> &Desktops {
        &self.desktops
    }

    /// The program search path.
    #[must_use]
    pub fn search_path(&self) -> &OsStr {
        &self.search_path
    }
}

/// Why an entry does not start at login. The checks of the effective
/// file stop at the first of [`Ineligible::Unreadable`],
/// [`Ineligible::Undecodable`], [`Ineligible::NoMainGroup`] and
/// [`Ineligible::UnsupportedEncoding`]
/// (nothing else in such a file can be trusted); otherwise every failing
/// check is reported, so a user interface can say what enabling an entry
/// would and would not change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Ineligible {
    /// A more important autostart directory could not be listed in full:
    /// it might hold a file of this name that hides this one, so this one
    /// is not started.
    OverridesUnknown {
        /// The directory.
        dir: PathBuf,
    },
    /// The effective file could not be read ([`SourceFile::problem`]
    /// says why). It still shadows every less important copy.
    ///
    /// [`SourceFile::problem`]: crate::SourceFile::problem
    Unreadable,
    /// The effective file has a line that is neither a comment nor UTF-8.
    /// Reading drops such a line, so what it meant to say is unknown — a
    /// `Hidden=true`, or a group header whose keys would otherwise count
    /// for another group — and the file is not started, as a damaged
    /// override must not start what it was written to stop. The file's
    /// bytes stay whole, for inspection and for editing.
    Undecodable {
        /// The first such line, counted from 1.
        line: usize,
    },
    /// The effective file has no `[Desktop Entry]` group (nor the
    /// deprecated `[KDE Desktop Entry]`): it is no desktop entry.
    NoMainGroup,
    /// The effective file is in an `Encoding` that is not supported; the
    /// Desktop Entry spec says to ignore such a file.
    UnsupportedEncoding,
    /// `Hidden=true`: the spec's way to disable an entry, here and in
    /// every less important copy.
    Hidden,
    /// `Hidden` has a value that is not a boolean. Taken as hidden: the
    /// key's only purpose is hiding, and a user's broken override must not
    /// start what it was written to stop (the systemd generator, which
    /// fails the whole file on a bad boolean, keeps it masking too).
    InvalidHidden {
        /// The value as written.
        value: String,
    },
    /// `OnlyShowIn` lists none of the session's desktops.
    OnlyShowIn {
        /// The desktops it lists.
        listed: Vec<String>,
    },
    /// `NotShowIn` lists this desktop of the session, before any desktop
    /// `OnlyShowIn` lists.
    NotShowIn {
        /// The desktop.
        desktop: String,
    },
    /// `TryExec` names no installed executable program.
    TryExec {
        /// The value as given.
        program: String,
        /// What is wrong with it.
        problem: TryExecProblem,
    },
}

/// Why a `TryExec` value does not match an installed executable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TryExecProblem {
    /// No executable regular file is at the absolute path, or in any
    /// absolute directory of the search path.
    NotFound,
    /// A relative path with directories (`bin/tool`): the spec allows
    /// only an absolute path or a bare name, and a relative path has no
    /// meaningful base at login.
    PathComponents,
}

impl fmt::Display for Ineligible {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OverridesUnknown { dir } => write!(
                f,
                "{} could not be listed in full and may hide this entry",
                shown(&dir.to_string_lossy())
            ),
            Self::Unreadable => write!(f, "the file cannot be read"),
            Self::Undecodable { line } => {
                write!(
                    f,
                    "line {line} of the file is not UTF-8, so its meaning is unknown"
                )
            }
            Self::NoMainGroup => write!(f, "the file has no [Desktop Entry] group"),
            Self::UnsupportedEncoding => write!(f, "the file's Encoding is not supported"),
            Self::Hidden => write!(f, "Hidden=true"),
            Self::InvalidHidden { value } => {
                write!(
                    f,
                    "Hidden has the value \"{}\", taken as true",
                    shown(value)
                )
            }
            Self::OnlyShowIn { listed } => {
                write!(f, "OnlyShowIn lists none of this session's desktops")?;
                if !listed.is_empty() {
                    write!(f, " (it lists \"{}\")", shown(&listed.join(";")))?;
                }
                Ok(())
            }
            Self::NotShowIn { desktop } => {
                write!(f, "NotShowIn lists this session's \"{}\"", shown(desktop))
            }
            Self::TryExec { program, problem } => match problem {
                TryExecProblem::NotFound => {
                    write!(
                        f,
                        "TryExec \"{}\" is not an installed program",
                        shown(program)
                    )
                }
                TryExecProblem::PathComponents => write!(
                    f,
                    "TryExec \"{}\" is neither an absolute path nor a bare name",
                    shown(program)
                ),
            },
        }
    }
}

/// The verdict on an entry: eligible when nothing stands in its way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Eligibility {
    reasons: Vec<Ineligible>,
}

impl Eligibility {
    /// Whether the entry starts at login, as far as the spec's keys go
    /// ([`Autostart::prepare`] may still fail to build its command).
    #[must_use]
    pub fn is_eligible(&self) -> bool {
        self.reasons.is_empty()
    }

    /// Why not, in the order checked; empty when eligible.
    #[must_use]
    pub fn reasons(&self) -> &[Ineligible] {
        &self.reasons
    }
}

/// Why an entry has no launch plan: kept apart, so "the user disabled
/// it" is never confused with "it is broken".
#[derive(Debug)]
#[non_exhaustive]
pub enum PrepareError {
    /// The entry does not start at login.
    Ineligible(Eligibility),
    /// The entry starts at login, but no command can be built for it (no
    /// `Exec`, not `Type=Application`, an unusable command line, …).
    Launch(LaunchError),
}

impl fmt::Display for PrepareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ineligible(eligibility) => {
                write!(f, "not started at login")?;
                for (at, reason) in eligibility.reasons.iter().enumerate() {
                    write!(f, "{}{reason}", if at == 0 { ": " } else { "; " })?;
                }
                Ok(())
            }
            Self::Launch(err) => write!(f, "cannot be launched: {err}"),
        }
    }
}

impl std::error::Error for PrepareError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Ineligible(_) => None,
            Self::Launch(err) => Some(err),
        }
    }
}

impl Autostart {
    /// Whether this entry starts at login in `context`, by the Autostart
    /// spec's keys: `Hidden`, `OnlyShowIn`/`NotShowIn` and `TryExec`, in
    /// the effective file only. `NoDisplay` is not one of them: it hides
    /// an entry from menus, which autostart files are not in. Extension
    /// keys (desktop-specific switches, delays, startup phases) are not
    /// interpreted; they stay readable through [`Autostart::entry`].
    ///
    /// `TryExec` follows this spec, not the `GLib` lookup
    /// [`AppEntry::try_exec`](xdg_kit_desktop_entry::AppEntry::try_exec)
    /// mirrors: an absolute path must be an executable regular file; a
    /// bare name must be one in an absolute directory of the search path
    /// (empty and relative components are skipped, as the systemd
    /// generator skips them: at login they name wherever the session
    /// happens to run); a relative path with directories never matches,
    /// and `Path=` is not consulted. An empty `TryExec` is no check.
    #[must_use]
    pub fn eligibility(&self, context: &Context) -> Eligibility {
        let mut reasons = Vec::new();
        if let Some(dir) = &self.uncertain {
            reasons.push(Ineligible::OverridesUnknown { dir: dir.clone() });
        }
        let Some(entry) = self.entry() else {
            reasons.push(Ineligible::Unreadable);
            return Eligibility { reasons };
        };
        if let Some(&line) = entry.key_file().undecodable_lines().first() {
            reasons.push(Ineligible::Undecodable { line });
            return Eligibility { reasons };
        }
        if !entry.key_file().has_group(entry.main_group()) {
            reasons.push(Ineligible::NoMainGroup);
            return Eligibility { reasons };
        }
        if !entry.supports_encoding() {
            reasons.push(Ineligible::UnsupportedEncoding);
            return Eligibility { reasons };
        }
        if let Some(raw) = entry.key_file().raw(entry.main_group(), "Hidden", None) {
            match entry.boolean("Hidden") {
                Some(true) => reasons.push(Ineligible::Hidden),
                Some(false) => {}
                None => reasons.push(Ineligible::InvalidHidden {
                    value: raw.to_owned(),
                }),
            }
        }
        if let Some(reason) = desktop_filter(entry, context.desktops()) {
            reasons.push(reason);
        }
        if let Some(reason) = try_exec(entry, context.search_path()) {
            reasons.push(reason);
        }
        Eligibility { reasons }
    }

    /// The launch plan of this entry in `context`: nothing is executed,
    /// the caller spawns (or calls) it. The entry itself is launched,
    /// with no targets: `options.action` is ignored. A `DBusActivatable`
    /// entry gives a D-Bus call when [`LaunchOptions::dbus`] is set and
    /// the `dbus` feature is built.
    ///
    /// # Errors
    ///
    /// [`PrepareError::Ineligible`] when [`Autostart::eligibility`] says
    /// it does not start; [`PrepareError::Launch`] when it does but no
    /// launch can be built.
    pub fn prepare(
        &self,
        context: &Context,
        options: &LaunchOptions,
    ) -> Result<LaunchPlan, PrepareError> {
        let eligibility = self.eligibility(context);
        let Some(entry) = self.entry().filter(|_| eligibility.is_eligible()) else {
            return Err(PrepareError::Ineligible(eligibility));
        };
        let options = if options.action.is_some() {
            let mut own = options.clone();
            own.action = None;
            Cow::Owned(own)
        } else {
            Cow::Borrowed(options)
        };
        prepare_launch(entry, &[], &options).map_err(PrepareError::Launch)
    }
}

/// The `OnlyShowIn`/`NotShowIn` verdict, by the Desktop Entry crate's
/// rule (each desktop in turn: a match in `OnlyShowIn` shows, one in
/// `NotShowIn` hides; with no match it shows unless `OnlyShowIn` is set),
/// with the reason when it does not show.
fn desktop_filter(entry: &DesktopEntry, desktops: &Desktops) -> Option<Ineligible> {
    if entry.shown_in(desktops) {
        return None;
    }
    // Not shown, so no desktop matched `OnlyShowIn` before a `NotShowIn`
    // match: the first `NotShowIn` match decided, or else `OnlyShowIn`.
    let not_show_in = entry.not_show_in().unwrap_or_default();
    let not_show_in: std::collections::HashSet<&str> = not_show_in
        .iter()
        .map(|name| {
            // Hashed.
            work::count(1 + name.len());
            name.as_str()
        })
        .collect();
    for desktop in desktops.names() {
        // Hashed and compared.
        work::count(1 + 2 * desktop.len());
        if not_show_in.contains(desktop.as_str()) {
            return Some(Ineligible::NotShowIn {
                desktop: desktop.clone(),
            });
        }
    }
    Some(Ineligible::OnlyShowIn {
        listed: entry.only_show_in().unwrap_or_default(),
    })
}

/// The `TryExec` verdict under this spec's rule (see
/// [`Autostart::eligibility`]).
fn try_exec(entry: &DesktopEntry, search_path: &OsStr) -> Option<Ineligible> {
    let program = entry.try_exec().filter(|program| !program.is_empty())?;
    work::count(program.len());
    let path = Path::new(&program);
    let problem = if path.is_absolute() {
        (!executable(path)).then_some(TryExecProblem::NotFound)
    } else if program.contains('/') {
        Some(TryExecProblem::PathComponents)
    } else {
        let found = search_path
            .as_bytes()
            .split(|&byte| byte == b':')
            .map(|dir| {
                work::count(1 + dir.len());
                Path::new(OsStr::from_bytes(dir))
            })
            .filter(|dir| dir.is_absolute())
            .any(|dir| executable(&dir.join(&program)));
        (!found).then_some(TryExecProblem::NotFound)
    };
    problem.map(|problem| Ineligible::TryExec { program, problem })
}

/// Whether `path` is a regular file (links followed) this process may
/// execute.
fn executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
        && rustix::fs::access(path, rustix::fs::Access::EXEC_OK).is_ok()
}
