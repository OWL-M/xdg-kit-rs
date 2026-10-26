//! User overrides: enabling, disabling and resetting an entry by the
//! file of its name in the user's autostart directory — the only
//! directory this crate ever writes to.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use xdg_kit_basedir::{
    CreateMode, Durability, FileStamp, PreparedWrite, SymlinkPolicy, WriteFileError, WriteOptions,
};
use xdg_kit_desktop_entry::{DesktopEntry, KeyFile};

use crate::discovery::Origin;
use crate::error::shown;
use crate::{AutostartEntries, Limits, work};

/// How an override is written: a fresh file is `0644`, an existing file
/// keeps its permission bits, and a symbolic link in the user's directory
/// is replaced by a regular file — never written through, so a link to a
/// system file can never make an override modify that file.
const OPTIONS: WriteOptions = WriteOptions {
    create: CreateMode::Exact(0o644),
    preserve_mode: true,
    symlinks: SymlinkPolicy::ReplaceLink,
};

/// What the user's file must still look like for a write to commit.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Baseline {
    /// Nothing had the name: a write installs without replacing.
    Absent,
    /// The file's identity when exactly the snapshot's bytes were read.
    Stamped(FileStamp),
    /// Something has the name that could not be read (or is not a file),
    /// or a write committed without its identity: a checked write reports
    /// a conflict rather than guess.
    Unknown,
}

/// What a write will do.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pending {
    Nothing,
    Write(Box<DesktopEntry>),
    Remove,
}

/// An override of one autostart entry in the user's directory, made from
/// a snapshot ([`AutostartEntries::edit`]) and committed with
/// [`Override::write`].
///
/// Three operations, deliberately distinct:
///
/// - [`Override::disable`] makes the user's file say `Hidden=true` — the
///   spec's way, which also hides every system copy.
/// - [`Override::enable`] makes it say `Hidden=false`: an *explicit*
///   enablement that keeps the entry on even if the system file later
///   says `Hidden=true` (the spec's minimal disabling override, which holds
///   nothing but `Hidden`, is replaced by a copy of the system file for
///   this; any richer user file is edited in place).
/// - [`Override::reset`] removes the user's file, so the system default
///   applies again, whatever it says now or later.
///
/// The user's file is edited in place when it exists (every comment and
/// unknown key kept, only the `Hidden` value rewritten); otherwise the
/// most important system file is copied byte for byte and the copy
/// edited. Nothing else is interpreted or changed: desktop-specific
/// switches such as `X-GNOME-Autostart-enabled` stay as written, so a
/// desktop honoring one may still not start an entry enabled here.
#[derive(Debug, Clone)]
pub struct Override {
    name: OsString,
    path: PathBuf,
    baseline: Baseline,
    /// The user's file as read, when there is one that could be read.
    current: Option<DesktopEntry>,
    /// The most important system file as read, when readable.
    default: Option<DesktopEntry>,
    /// Whether a system file of the name exists at all (readable or not).
    has_default: bool,
    pending: Pending,
    limits: Limits,
}

/// Why an override cannot be made.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EditError {
    /// The snapshot has no entry of this name.
    NotFound(OsString),
    /// The name is no autostart file name: it must end in `.desktop`, not
    /// start with `.`, and hold no `/`.
    BadName(OsString),
    /// The user's directory could not be listed in full, so whether the
    /// user has a file of this name is not known.
    UserDirUnknown(PathBuf),
    /// [`Override::reset`] with no system file of the name: removing the
    /// user's file would delete the entry, not restore a default.
    NoDefault,
    /// [`Override::enable`] with nothing to enable: no user file, and no
    /// readable system file to copy.
    NothingToEnable,
    /// The key file refused the edit.
    Rejected(xdg_kit_desktop_entry::EditError),
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(name) => {
                write!(
                    f,
                    "no autostart entry \"{}\"",
                    shown(&name.to_string_lossy())
                )
            }
            Self::BadName(name) => write!(
                f,
                "\"{}\" is not an autostart file name",
                shown(&name.to_string_lossy())
            ),
            Self::UserDirUnknown(dir) => write!(
                f,
                "{} could not be listed in full",
                shown(&dir.to_string_lossy())
            ),
            Self::NoDefault => write!(f, "there is no system default to reset to"),
            Self::NothingToEnable => write!(f, "there is no readable entry to enable"),
            Self::Rejected(err) => write!(f, "the edit was refused: {err}"),
        }
    }
}

impl std::error::Error for EditError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Rejected(err) => Some(err),
            _ => None,
        }
    }
}

/// Why a write did not complete.
#[derive(Debug)]
#[non_exhaustive]
pub enum WriteError {
    /// The user's file changed since the snapshot (or could not be read
    /// then): someone else wrote it. Nothing was replaced and the pending
    /// change stays; refresh, edit again, or write deliberately with
    /// [`Override::write_overwriting`].
    Conflict,
    /// The result is over [`Limits::max_file_len`], so the next snapshot
    /// could not read it. Nothing was replaced; the pending change stays.
    OverLimits,
    /// The filesystem refused, before anything was replaced.
    Io(io::Error),
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict => write!(f, "the user's autostart file changed since it was read"),
            Self::OverLimits => write!(f, "the result would be over the size limit"),
            Self::Io(err) => write!(f, "cannot write the override: {err}"),
        }
    }
}

impl std::error::Error for WriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Conflict | Self::OverLimits => None,
        }
    }
}

impl AutostartEntries {
    /// An override of the entry `name` in [`AutostartEntries::user_dir`],
    /// based on this snapshot: what a write later checks the user's file
    /// against.
    ///
    /// # Errors
    ///
    /// [`EditError::BadName`], [`EditError::NotFound`], or
    /// [`EditError::UserDirUnknown`] when the user's directory could not be
    /// listed in full.
    pub fn edit(&self, name: impl AsRef<OsStr>) -> Result<Override, EditError> {
        let name = name.as_ref();
        if !crate::discovery::is_candidate(name) || name.as_encoded_bytes().contains(&b'/') {
            return Err(EditError::BadName(name.to_owned()));
        }
        let entry = self
            .get(name)
            .ok_or_else(|| EditError::NotFound(name.to_owned()))?;
        if !self.user_dir_listed() {
            return Err(EditError::UserDirUnknown(self.user_dir().to_owned()));
        }
        let user = Some(entry.source()).filter(|source| source.origin() == Origin::User);
        let baseline = match user {
            Some(source) => source
                .stamp()
                .map_or(Baseline::Unknown, |stamp| Baseline::Stamped(stamp.clone())),
            // A non-file of the name is in the way: only a deliberate
            // overwrite replaces it.
            None if self.is_obstructed(name) => Baseline::Unknown,
            None => Baseline::Absent,
        };
        let default = entry.system_default();
        Ok(Override {
            name: name.to_owned(),
            path: self.user_dir().join(name),
            baseline,
            current: user.and_then(|source| source.entry().cloned()),
            default: default.and_then(|source| source.entry().cloned()),
            has_default: default.is_some(),
            pending: Pending::Nothing,
            limits: self.limits().clone(),
        })
    }
}

impl Override {
    /// The entry's file name.
    #[must_use]
    pub fn name(&self) -> &OsStr {
        &self.name
    }

    /// The user's file this override writes or removes.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether a system file of this name exists, which
    /// [`Override::reset`] would restore.
    #[must_use]
    pub fn has_default(&self) -> bool {
        self.has_default
    }

    /// The user's file the next write would install, as bytes; `None`
    /// when it would remove the file or do nothing.
    #[must_use]
    pub fn to_bytes(&self) -> Option<Vec<u8>> {
        match &self.pending {
            Pending::Write(entry) => Some(entry.key_file().to_bytes()),
            Pending::Nothing | Pending::Remove => None,
        }
    }

    /// Makes the user's file say `Hidden=true`: the entry, and every
    /// system copy, stops starting. With no user file, the system file is
    /// copied first (or, unreadable, a file holding only the
    /// `[Desktop Entry]` group and `Hidden=true` is made, as the spec's
    /// implementation note describes).
    ///
    /// # Errors
    ///
    /// [`EditError::Rejected`] if the key file refuses the value (it
    /// should not: the group and key are fixed, valid names).
    pub fn disable(&mut self) -> Result<(), EditError> {
        let mut entry = self
            .base()
            .unwrap_or_else(|| DesktopEntry::from_key_file(KeyFile::parse(b"[Desktop Entry]\n")));
        set_hidden(&mut entry, true)?;
        self.pending = Pending::Write(Box::new(entry));
        Ok(())
    }

    /// Makes the user's file say `Hidden=false`: an explicit enablement,
    /// kept even if the system file later says `Hidden=true`. With no user
    /// file, the system file is copied first. The user's file is otherwise
    /// edited in place, every line kept — its own command, its own
    /// `OnlyShowIn` and every other key stay in force, so an entry kept from
    /// starting by `OnlyShowIn`, `NotShowIn` or `TryExec` stays so, and an
    /// incomplete one stays incomplete.
    ///
    /// One exception: the spec's minimal disabling override — a file whose
    /// only group is `[Desktop Entry]`, whose only key is `Hidden`, and in
    /// which nothing failed to parse (a damaged line could have said
    /// anything) — holds nothing but the switch, so enabling replaces it with a copy of the
    /// system file (its comments, the only other thing it can hold, are not
    /// kept); with no readable system file, the stub itself is edited. To
    /// discard a richer user file and enable the system one instead, say
    /// so: [`Override::reset`], then [`Override::enable`].
    ///
    /// # Errors
    ///
    /// [`EditError::NothingToEnable`] when there is no user file and no
    /// readable system file to copy; [`EditError::Rejected`] as for
    /// [`Override::disable`].
    pub fn enable(&mut self) -> Result<(), EditError> {
        let base = self
            .base()
            .filter(|entry| !is_minimal_stub(entry))
            .or_else(|| self.default.clone())
            .or_else(|| self.base());
        let mut entry = base.ok_or(EditError::NothingToEnable)?;
        set_hidden(&mut entry, false)?;
        self.pending = Pending::Write(Box::new(entry));
        Ok(())
    }

    /// Removes the user's file, so the system default applies again.
    ///
    /// # Errors
    ///
    /// [`EditError::NoDefault`] when no system file has the name:
    /// removing the user's file would delete the entry outright.
    pub fn reset(&mut self) -> Result<(), EditError> {
        if !self.has_default {
            return Err(EditError::NoDefault);
        }
        self.pending = Pending::Remove;
        Ok(())
    }

    /// What an edit starts from: the pending file, else the user's file,
    /// else a copy of the system file.
    fn base(&self) -> Option<DesktopEntry> {
        match &self.pending {
            Pending::Write(entry) => Some((**entry).clone()),
            Pending::Nothing => self.current.clone().or_else(|| self.default.clone()),
            Pending::Remove => self.default.clone(),
        }
    }

    /// Commits the pending change unless the user's file changed since
    /// the snapshot. A new file is fully prepared and synced first; just
    /// before it is renamed into place (or the old file removed), the
    /// destination's identity **and bytes** are compared with what the
    /// snapshot read, and a name that was free must still be free — a
    /// new file is then installed without replacing, so a competing
    /// creator is a conflict, never silently overwritten. Missing parent
    /// folders are created with mode `0700`.
    ///
    /// The check and the rename (or removal) are two steps, not one
    /// atomic operation: another writer can slip between them, or change
    /// the file afterwards. This detects conflicts with what the snapshot
    /// saw; it does not serialize applications that do not cooperate.
    ///
    /// After a committed change — [`Durability::NotSynced`] included,
    /// since it happened — this override moves forward (its baseline is
    /// the written file, taken through the file's own descriptor), so
    /// editing and writing again needs no new snapshot; the snapshot
    /// itself does not change ([`AutostartEntries::refresh`] it). With no
    /// pending change this does nothing.
    ///
    /// # Errors
    ///
    /// [`WriteError::Conflict`] when the check fails (nothing replaced,
    /// the change still pending); [`WriteError::OverLimits`];
    /// [`WriteError::Io`] for what the filesystem refuses.
    pub fn write(&mut self) -> Result<Durability, WriteError> {
        self.commit(true)
    }

    /// [`Override::write`] without the check: the explicitly named
    /// last-writer-wins operation. Whatever has the name — a changed
    /// file, an unreadable one, a dangling link — is replaced or removed.
    ///
    /// # Errors
    ///
    /// As [`Override::write`], without the conflict outcome.
    pub fn write_overwriting(&mut self) -> Result<Durability, WriteError> {
        self.commit(false)
    }

    fn commit(&mut self, check: bool) -> Result<Durability, WriteError> {
        match self.pending.clone() {
            Pending::Nothing => Ok(Durability::Synced),
            Pending::Write(entry) => self.install(*entry, check),
            Pending::Remove => self.remove(check),
        }
    }

    fn install(&mut self, entry: DesktopEntry, check: bool) -> Result<Durability, WriteError> {
        let bytes = entry.key_file().to_bytes();
        if let Some(max) = self.limits.max_file_len
            && bytes.len() as u64 > max
        {
            return Err(WriteError::OverLimits);
        }
        let folders = match self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            Some(parent) => {
                xdg_kit_basedir::create_dir_all_synced(parent, 0o700).map_err(WriteError::Io)?
            }
            None => Durability::Synced,
        };
        let prepared = PreparedWrite::new(&self.path, &bytes, &OPTIONS).map_err(io_error)?;
        // Revalidation, after all the write latency and immediately
        // before the rename.
        let fresh = check && self.revalidate()?;
        let committed = if fresh {
            prepared.commit_unless_exists()
        } else {
            prepared.commit()
        }
        .map_err(|err| match err {
            WriteFileError::Exists => WriteError::Conflict,
            other => io_error(other),
        })?;
        self.baseline = committed.stamp.map_or(Baseline::Unknown, Baseline::Stamped);
        self.current = Some(entry);
        self.pending = Pending::Nothing;
        // A new folder whose entry could not be synced leaves the file
        // reachable only until a crash, however the file itself synced.
        Ok(match folders {
            Durability::NotSynced(err) => Durability::NotSynced(err),
            Durability::Synced => committed.durability,
        })
    }

    fn remove(&mut self, check: bool) -> Result<Durability, WriteError> {
        if check && self.revalidate()? {
            // Nothing had the name and nothing has it now: already reset.
            self.pending = Pending::Nothing;
            return Ok(Durability::Synced);
        }
        match fs::remove_file(&self.path) {
            Ok(()) => {}
            // Gone meanwhile: the reset's end state.
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(WriteError::Io(err)),
        }
        // The removal is made durable by syncing the folder.
        let durability = match self
            .path
            .parent()
            .map_or_else(|| fs::File::open("."), fs::File::open)
            .and_then(|dir| dir.sync_all())
        {
            Ok(()) => Durability::Synced,
            Err(err) => Durability::NotSynced(err),
        };
        self.baseline = Baseline::Absent;
        self.current = None;
        self.pending = Pending::Nothing;
        Ok(durability)
    }

    /// Whether the user's file still is what the snapshot saw: `true`
    /// when the name was free and still is (an install must not
    /// replace), `false` when it holds exactly the snapshot's file.
    ///
    /// # Errors
    ///
    /// [`WriteError::Conflict`] when it changed, [`WriteError::Io`] when
    /// it could not be looked at.
    fn revalidate(&self) -> Result<bool, WriteError> {
        match &self.baseline {
            Baseline::Unknown => Err(WriteError::Conflict),
            // Anything with the name now — a dangling link included, which
            // a no-replace install would trip over — is a change.
            Baseline::Absent => match fs::symlink_metadata(&self.path) {
                Ok(_) => Err(WriteError::Conflict),
                Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(true),
                Err(err) => Err(WriteError::Io(err)),
            },
            Baseline::Stamped(stamp) => {
                let expected = self
                    .current
                    .as_ref()
                    .map(|entry| entry.key_file().to_bytes())
                    .unwrap_or_default();
                // Equal stamps cannot tell same-sized writes within one
                // timestamp tick apart: compare the bytes, always —
                // reading no more than what could still match.
                let read =
                    match xdg_kit_basedir::read_stamped(&self.path, Some(expected.len() as u64)) {
                        Ok(Some(read)) => read,
                        Ok(None) => return Err(WriteError::Conflict),
                        Err(err)
                            if matches!(
                                err.kind(),
                                io::ErrorKind::InvalidInput | io::ErrorKind::FileTooLarge
                            ) =>
                        {
                            return Err(WriteError::Conflict);
                        }
                        Err(err) => return Err(WriteError::Io(err)),
                    };
                work::count(read.bytes.len());
                if read.stamp == *stamp && read.bytes == expected {
                    Ok(false)
                } else {
                    Err(WriteError::Conflict)
                }
            }
        }
    }
}

/// Whether `entry` is the spec's minimal disabling override: its only
/// group is the main group, its only key there is an untranslated
/// `Hidden`, and it parsed without a single problem — so no line was
/// dropped (one not UTF-8, one the parser could not read), and nothing a
/// copy of the system file would lose. A file with any problem is the
/// user's to fix, never replaced.
fn is_minimal_stub(entry: &DesktopEntry) -> bool {
    let file = entry.key_file();
    let group = entry.main_group();
    file.diagnostics().is_empty()
        && file.groups() == [group]
        && file
            .keys(group)
            .iter()
            .all(|&(key, locale)| key == "Hidden" && locale.is_none())
}

/// Sets `Hidden` in the entry's main group.
pub(crate) fn set_hidden(entry: &mut DesktopEntry, hidden: bool) -> Result<(), EditError> {
    let group = entry.main_group();
    entry
        .key_file_mut()
        .set_boolean(group, "Hidden", hidden)
        .map_err(EditError::Rejected)
}

fn io_error(err: WriteFileError) -> WriteError {
    match err {
        WriteFileError::Before(err) => WriteError::Io(err),
        // Links are replaced, and `Exists` is mapped by the caller.
        other => WriteError::Io(io::Error::other(other.to_string())),
    }
}
