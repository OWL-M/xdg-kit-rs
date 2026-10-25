//! The thumbnail cache: where thumbnails live, how fresh they are, and
//! how new ones and failure markers are installed.

use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use xdg_kit_basedir::{
    BaseDirs, CreateMode, Durability, SymlinkPolicy, WriteFileError, WriteOptions,
    write_regular_file,
};

use crate::source::{Application, SourceInfo, ThumbnailSource, shared_uri};
use crate::{Diagnostic, Limits, md5, png, work};

/// The standard's size classes, by their largest dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ThumbnailSize {
    /// Up to 128 pixels.
    Normal,
    /// Up to 256 pixels.
    Large,
    /// Up to 512 pixels.
    XLarge,
    /// Up to 1024 pixels.
    XXLarge,
}

impl ThumbnailSize {
    /// Every size, ascending.
    pub const ALL: [Self; 4] = [Self::Normal, Self::Large, Self::XLarge, Self::XXLarge];

    /// The class's largest dimension in pixels.
    #[must_use]
    pub fn max_dimension(self) -> u32 {
        match self {
            Self::Normal => 128,
            Self::Large => 256,
            Self::XLarge => 512,
            Self::XXLarge => 1024,
        }
    }

    /// The directory name, in the personal cache and the shared
    /// repositories alike.
    #[must_use]
    pub fn dir_name(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Large => "large",
            Self::XLarge => "x-large",
            Self::XXLarge => "xx-large",
        }
    }

    /// This size and every larger one, ascending: the lookup order for
    /// one provenance.
    fn and_larger(self) -> impl Iterator<Item = Self> {
        Self::ALL.into_iter().filter(move |size| *size >= self)
    }
}

/// Where a thumbnail came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// The user's own cache under `$XDG_CACHE_HOME/thumbnails`.
    Personal,
    /// A `.sh_thumbnails` repository beside the file.
    Shared,
}

/// How a thumbnail relates to the source it claims to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Every piece of metadata both sides have agrees.
    Fresh,
    /// Something disagrees — or a personal entry lacks the keys the
    /// standard requires there, which makes it due for regeneration.
    Stale,
    /// A shared entry without timestamps: legitimate there, usable only
    /// by a caller that decides so — never a claim of verified
    /// freshness.
    Unknown,
}

/// A structurally sound thumbnail found in the cache.
#[derive(Debug)]
pub struct Thumbnail {
    path: PathBuf,
    size: ThumbnailSize,
    provenance: Provenance,
    /// What the entry should say its source is (the canonical URI, or
    /// the shared `./name` form).
    expected_uri: String,
    uri: Option<String>,
    /// Whole seconds; `Invalid` when the key is there but not a number —
    /// which is corrupt metadata, never the missing-metadata exception.
    mtime: Meta<u64>,
    /// The original's byte length; the same three states.
    len: Meta<u64>,
}

/// One optional metadata field: absent, present-but-corrupt, or a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Meta<T> {
    Absent,
    Invalid,
    Value(T),
}

impl<T: std::str::FromStr> Meta<T> {
    fn parsed(text: Option<&str>) -> Self {
        match text {
            None => Self::Absent,
            Some(text) => text.parse().map_or(Self::Invalid, Self::Value),
        }
    }
}

impl Thumbnail {
    /// Where the thumbnail is.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The size class it was found in.
    #[must_use]
    pub fn size(&self) -> ThumbnailSize {
        self.size
    }

    /// Where it came from.
    #[must_use]
    pub fn provenance(&self) -> Provenance {
        self.provenance
    }

    /// How the thumbnail relates to the source, comparing every piece of
    /// metadata both sides have: the URI, `Thumb::MTime` against the
    /// source's whole-second modification time, and `Thumb::Size`
    /// against its byte length when both are known. A personal entry
    /// must carry both `Thumb::URI` and `Thumb::MTime` — missing either
    /// is [`Freshness::Stale`], the standard requiring them there — and
    /// metadata that is present but corrupt is always `Stale`, never the
    /// shared missing-metadata exception; only a shared entry with keys
    /// genuinely absent reads [`Freshness::Unknown`].
    #[must_use]
    pub fn freshness(&self, source: &SourceInfo) -> Freshness {
        let expected_mtime = source
            .mtime_text()
            .and_then(|text| text.parse::<u64>().ok());
        if let Some(uri) = &self.uri
            && *uri != self.expected_uri
        {
            return Freshness::Stale;
        }
        if self.provenance == Provenance::Personal && self.uri.is_none() {
            return Freshness::Stale;
        }
        match self.len {
            Meta::Invalid => return Freshness::Stale,
            Meta::Value(stored) if source.len.is_some_and(|len| len != stored) => {
                return Freshness::Stale;
            }
            Meta::Absent | Meta::Value(_) => {}
        }
        match (self.mtime, expected_mtime) {
            (Meta::Value(stored), Some(expected)) if stored == expected => Freshness::Fresh,
            (Meta::Value(_) | Meta::Invalid, _) => Freshness::Stale,
            (Meta::Absent, _) => match self.provenance {
                // The standard requires the keys in the personal cache.
                Provenance::Personal => Freshness::Stale,
                Provenance::Shared => Freshness::Unknown,
            },
        }
    }
}

/// One location probed by a lookup: a sound thumbnail, or why the file
/// there was skipped. Locations with no file at all produce no entry.
#[derive(Debug)]
pub enum Candidate {
    /// A structurally sound thumbnail (fresh or not — ask it).
    Sound(Thumbnail),
    /// A file was there but unusable: structurally broken, unreadable,
    /// or over a limit. This is where lookup diagnostics live, and how a
    /// miss (no entry) differs from an I/O failure (this entry).
    Skipped(Diagnostic),
}

/// The thumbnail cache: `$XDG_CACHE_HOME/thumbnails` and, for local
/// files, their shared repositories.
#[derive(Debug, Clone)]
pub struct ThumbnailCache {
    root: PathBuf,
    limits: Limits,
}

/// How personal cache files are installed: private, and never through a
/// symlink — everything under the cache is this crate's own data, so a
/// symlink there is nobody's legitimate state.
const PERSONAL: WriteOptions = WriteOptions {
    create: CreateMode::Exact(0o600),
    preserve_mode: false,
    symlinks: SymlinkPolicy::Refuse,
};

impl ThumbnailCache {
    /// The user's cache, under `cache_home()/thumbnails`.
    #[must_use]
    pub fn new(base_dirs: &BaseDirs) -> Self {
        Self::with_limits(base_dirs, &Limits::default())
    }

    /// [`ThumbnailCache::new`] within `limits`.
    #[must_use]
    pub fn with_limits(base_dirs: &BaseDirs, limits: &Limits) -> Self {
        Self {
            root: base_dirs.cache_home().join("thumbnails"),
            limits: limits.clone(),
        }
    }

    /// Whether `path` lies inside a thumbnail directory — the personal
    /// cache or any `.sh_thumbnails` repository. The standard prohibits
    /// caching thumbnails of such files; installs and failure markers
    /// refuse them, and callers can skip generating for them too.
    #[must_use]
    pub fn is_thumbnail_path(&self, path: &Path) -> bool {
        path.starts_with(&self.root)
            || path.components().any(|part| {
                work::count(1);
                part.as_os_str() == ".sh_thumbnails"
            })
    }

    /// Every location a thumbnail for `source` could be, in lookup
    /// order: the exact personal size, larger personal sizes ascending,
    /// then — for a local file — the shared repository, exact size
    /// first. Each entry is a sound [`Thumbnail`] or a skip with its
    /// reason; locations with no file produce no entry. An unreadable
    /// local original produces exactly one skip saying so: the standard
    /// forbids serving cached thumbnails past the reader's access.
    #[must_use]
    pub fn candidates(&self, source: &ThumbnailSource, size: ThumbnailSize) -> Vec<Candidate> {
        if let Some(path) = source.path()
            && !readable(path).unwrap_or(false)
        {
            return vec![Candidate::Skipped(Diagnostic::new(
                Some(path.to_owned()),
                None,
                "the original is not readable, so no thumbnail is served",
            ))];
        }
        let mut found = Vec::new();
        let name = source.cache_name();
        let uri = source.uri();
        for at in size.and_larger() {
            self.probe(
                &self.root.join(at.dir_name()).join(&name),
                at,
                Provenance::Personal,
                &uri,
                &mut found,
            );
        }
        if let Some(path) = source.path()
            && let (Some(dir), Some(file_name)) = (path.parent(), path.file_name())
        {
            let shared = shared_uri(file_name);
            let shared_name = format!("{}.png", md5::hex(shared.as_bytes()));
            for at in size.and_larger() {
                self.probe(
                    &dir.join(".sh_thumbnails")
                        .join(at.dir_name())
                        .join(&shared_name),
                    at,
                    Provenance::Shared,
                    &shared,
                    &mut found,
                );
            }
        }
        found
    }

    /// The first sound **and fresh** thumbnail for `source`, so a stale
    /// or broken exact entry falls through to a fresh larger or shared
    /// one. A shared entry of [`Freshness::Unknown`] is *not* accepted
    /// here — a caller with that policy walks
    /// [`ThumbnailCache::candidates`]. `Ok(None)` is a true miss (or an
    /// unreadable original, which must not be served).
    ///
    /// # Errors
    ///
    /// A readability check that itself failed (not one that said no —
    /// that is `Ok(None)`). Per-candidate problems are never errors:
    /// they are skips, visible through `candidates`.
    pub fn find(
        &self,
        source: &ThumbnailSource,
        size: ThumbnailSize,
        info: &SourceInfo,
    ) -> io::Result<Option<Thumbnail>> {
        if let Some(path) = source.path()
            && !readable(path)?
        {
            return Ok(None);
        }
        for candidate in self.candidates(source, size) {
            work::count(1);
            if let Candidate::Sound(thumbnail) = candidate
                && thumbnail.freshness(info) == Freshness::Fresh
            {
                return Ok(Some(thumbnail));
            }
        }
        Ok(None)
    }

    /// Reads the file at `path` as a candidate into `found`.
    fn probe(
        &self,
        path: &Path,
        size: ThumbnailSize,
        provenance: Provenance,
        expected_uri: &str,
        found: &mut Vec<Candidate>,
    ) {
        let bytes = match xdg_kit_basedir::read_regular_file_with(path, self.limits.max_file_len) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return,
            Err(err) => {
                found.push(Candidate::Skipped(Diagnostic::new(
                    Some(path.to_owned()),
                    None,
                    format!("cannot be read: {err}"),
                )));
                return;
            }
        };
        match png::read(&bytes, &self.limits) {
            Ok(png) => found.push(Candidate::Sound(Thumbnail {
                path: path.to_owned(),
                size,
                provenance,
                expected_uri: expected_uri.to_owned(),
                uri: png.key("Thumb::URI").map(str::to_owned),
                mtime: Meta::parsed(png.key("Thumb::MTime")),
                len: Meta::parsed(png.key("Thumb::Size")),
            })),
            Err(unsound) => found.push(Candidate::Skipped(Diagnostic::new(
                Some(path.to_owned()),
                None,
                format!("not a sound PNG: {unsound}"),
            ))),
        }
    }

    /// Installs `finished` — the PNG a thumbnailer produced — as the
    /// thumbnail of `source` at `size`: its structure is validated (a
    /// non-PNG is refused, never installed), `Thumb::URI`,
    /// `Thumb::MTime` and (when the length is known) `Thumb::Size` are
    /// spliced in, replacing any `Thumb::*` keys present, and the result
    /// lands atomically under the personal cache: directories `0700`,
    /// the file `0600`, never through a symlink. `info` is the metadata
    /// captured at generation time — nothing is re-statted here, which
    /// would stamp an old image as fresh. The [`Durability`] outcome
    /// keeps "not installed" and "installed but the folder sync failed"
    /// distinct.
    ///
    /// # Errors
    ///
    /// An unreadable or thumbnail-directory source (`InvalidInput`), a
    /// time before the epoch, an unsound `finished` (`InvalidData`), or
    /// any filesystem failure before the replacement.
    pub fn install(
        &self,
        finished: &Path,
        source: &ThumbnailSource,
        info: &SourceInfo,
        size: ThumbnailSize,
    ) -> io::Result<(PathBuf, Durability)> {
        self.writable(source)?;
        let dir = self.root.join(size.dir_name());
        self.put(
            finished,
            &dir,
            &source.cache_name(),
            &source.uri(),
            info,
            size,
        )
    }

    /// Writes the standard's failure marker — a minimal valid PNG with
    /// the same keys — under `fail/<application>/`, so the failure is
    /// recorded without retrying forever. The same refusals as
    /// [`ThumbnailCache::install`] apply.
    ///
    /// # Errors
    ///
    /// As [`ThumbnailCache::install`].
    pub fn mark_failed(
        &self,
        source: &ThumbnailSource,
        info: &SourceInfo,
        application: &Application,
    ) -> io::Result<(PathBuf, Durability)> {
        self.writable(source)?;
        let uri = source.uri();
        let mtime = before_epoch(info)?;
        let keys = keys(&uri, &mtime, info);
        let marker = png::minimal(&key_refs(&keys));
        self.sound_output(&marker, ThumbnailSize::Normal)?;
        let dir = self.root.join("fail").join(application.as_str());
        self.create_private(&dir)?;
        installed(
            write_regular_file(&dir.join(source.cache_name()), &marker, &PERSONAL),
            dir.join(source.cache_name()),
        )
    }

    /// The failure marker for `source` left by `application`, if one is
    /// there and still fresh — a stale marker is no failure (the source
    /// changed; generation is worth another try).
    ///
    /// # Errors
    ///
    /// None today; see [`ThumbnailCache::find`].
    pub fn failed(
        &self,
        source: &ThumbnailSource,
        application: &Application,
        info: &SourceInfo,
    ) -> io::Result<Option<Thumbnail>> {
        if let Some(path) = source.path()
            && !readable(path)?
        {
            return Ok(None);
        }
        let path = self
            .root
            .join("fail")
            .join(application.as_str())
            .join(source.cache_name());
        let mut found = Vec::new();
        self.probe(
            &path,
            ThumbnailSize::Normal,
            Provenance::Personal,
            &source.uri(),
            &mut found,
        );
        for candidate in found {
            if let Candidate::Sound(marker) = candidate
                && marker.freshness(info) == Freshness::Fresh
            {
                return Ok(Some(marker));
            }
        }
        Ok(None)
    }

    /// Installs `finished` into the **shared repository** beside `file`:
    /// `<dir>/.sh_thumbnails/<size>/`, keyed and `Thumb::URI`-stamped
    /// with the `./name` form. Nothing ever falls through to this; the
    /// caller selects it deliberately, asserting a world-visible
    /// thumbnail is appropriate for that directory. Permissions copy
    /// *modes only*: the thumbnail takes the source file's bits masked
    /// to `0666`, created repository directories take the containing
    /// directory's bits masked to `0777`, existing directories keep
    /// theirs — ownership and ACLs are not carried, so the audience can
    /// differ from the source's.
    ///
    /// # Errors
    ///
    /// As [`ThumbnailCache::install`].
    pub fn install_shared(
        &self,
        finished: &Path,
        file: &Path,
        info: &SourceInfo,
        size: ThumbnailSize,
    ) -> io::Result<(PathBuf, Durability)> {
        let file = std::path::absolute(file)?;
        let source = ThumbnailSource::Local(file.clone());
        self.writable(&source)?;
        let (Some(dir), Some(name)) = (file.parent(), file.file_name()) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the file has no containing directory",
            ));
        };
        let file_mode = std::fs::metadata(&file)?.permissions().mode() & 0o666;
        let dir_mode = std::fs::metadata(dir)?.permissions().mode() & 0o777;
        let repository = dir.join(".sh_thumbnails");
        for level in [&repository, &repository.join(size.dir_name())] {
            let mut round = 0;
            loop {
                round += 1;
                match std::fs::symlink_metadata(level) {
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {
                        match std::fs::DirBuilder::new().mode(dir_mode).create(level) {
                            Ok(()) => break,
                            // A concurrent creator wins the race; its
                            // component comes back for the same checks.
                            // A concurrent creator won the race; its
                            // component comes back for the same checks.
                            Err(err)
                                if err.kind() == io::ErrorKind::AlreadyExists && round == 1 => {}
                            Err(err) => return Err(err),
                        }
                    }
                    Err(err) => return Err(err),
                    Ok(found) if found.file_type().is_symlink() => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "a symlink inside the shared repository",
                        ));
                    }
                    // An existing directory keeps its mode, per the spec.
                    Ok(_) => break,
                }
            }
        }
        let shared = shared_uri(name);
        let shared_name = format!("{}.png", md5::hex(shared.as_bytes()));
        let target = repository.join(size.dir_name()).join(shared_name);
        let mtime = before_epoch(info)?;
        let bytes = xdg_kit_basedir::read_regular_file_with(finished, self.limits.max_file_len)?;
        let keys = keys(&shared, &mtime, info);
        let spliced = png::splice(&bytes, &key_refs(&keys), OWNED_KEYS, &self.limits)
            .map_err(|unsound| io::Error::new(io::ErrorKind::InvalidData, unsound))?;
        self.sound_output(&spliced, size)?;
        installed(
            write_regular_file(
                &target,
                &spliced,
                &WriteOptions {
                    create: CreateMode::Exact(file_mode),
                    preserve_mode: false,
                    symlinks: SymlinkPolicy::Refuse,
                },
            ),
            target.clone(),
        )
    }

    /// Validates, splices and installs into the personal cache.
    fn put(
        &self,
        finished: &Path,
        dir: &Path,
        name: &str,
        uri: &str,
        info: &SourceInfo,
        size: ThumbnailSize,
    ) -> io::Result<(PathBuf, Durability)> {
        let mtime = before_epoch(info)?;
        let bytes = xdg_kit_basedir::read_regular_file_with(finished, self.limits.max_file_len)?;
        let keys = keys(uri, &mtime, info);
        let spliced = png::splice(&bytes, &key_refs(&keys), OWNED_KEYS, &self.limits)
            .map_err(|unsound| io::Error::new(io::ErrorKind::InvalidData, unsound))?;
        self.sound_output(&spliced, size)?;
        self.create_private(dir)?;
        installed(
            write_regular_file(&dir.join(name), &spliced, &PERSONAL),
            dir.join(name),
        )
    }

    /// Refuses output this cache itself could not read back under its
    /// own limits — an install must never produce an entry the next
    /// lookup rejects — and output that does not fit `size`: a class is a
    /// bound on the largest dimension, not just a directory name.
    /// Rejecting is this crate's whole part; resizing stays the
    /// caller's.
    fn sound_output(&self, bytes: &[u8], size: ThumbnailSize) -> io::Result<()> {
        if let Some(max) = self.limits.max_file_len
            && bytes.len() as u64 > max
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the result would be over this cache's size limit",
            ));
        }
        let png = png::read(bytes, &self.limits).map_err(|unsound| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the result would not read back: {unsound}"),
            )
        })?;
        if png.width.max(png.height) > size.max_dimension() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{}x{} does not fit the {} class ({} at most)",
                    png.width,
                    png.height,
                    size.dir_name(),
                    size.max_dimension()
                ),
            ));
        }
        Ok(())
    }

    /// Prepares `dir` (a descendant of the cache root) component by
    /// component: a missing level is created `0700`; an existing level
    /// must be a real directory — a symlink anywhere under the root is
    /// refused, since the final rename's own symlink policy cannot see
    /// its ancestors — and one whose permission bits drifted from `0700`
    /// is set back, the cache being the user's private data. The check
    /// happens before the write, so a component replaced in between is
    /// the usual documented race, not a policed one.
    fn create_private(&self, dir: &Path) -> io::Result<()> {
        // Everything above the cache root (cache_home and up) is the
        // user's own arrangement, symlinks included.
        if let Some(parent) = self.root.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        let below = dir
            .strip_prefix(&self.root)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "outside the cache"))?;
        let mut level = self.root.clone();
        for part in std::iter::once(std::ffi::OsStr::new("")).chain(below.iter()) {
            work::count(1);
            if !part.is_empty() {
                level = level.join(part);
            }
            // Two rounds: a component another writer creates between the
            // inspection and the mkdir comes back for the same checks —
            // ordinary concurrent initialization succeeds, and whatever
            // appeared is never accepted blindly.
            let mut round = 0;
            loop {
                round += 1;
                match std::fs::symlink_metadata(&level) {
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {
                        match std::fs::DirBuilder::new().mode(0o700).create(&level) {
                            Ok(()) => break,
                            // A concurrent creator won the race; its
                            // component comes back for the same checks.
                            Err(err)
                                if err.kind() == io::ErrorKind::AlreadyExists && round == 1 => {}
                            Err(err) => return Err(err),
                        }
                    }
                    Err(err) => return Err(err),
                    Ok(found) if found.file_type().is_symlink() => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "a symlink inside the thumbnail cache",
                        ));
                    }
                    Ok(found) if !found.is_dir() => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "a non-directory inside the thumbnail cache",
                        ));
                    }
                    Ok(found) => {
                        if found.permissions().mode() & 0o7777 != 0o700 {
                            std::fs::set_permissions(
                                &level,
                                std::fs::Permissions::from_mode(0o700),
                            )?;
                        }
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// The gate every cache write passes: a local original must be
    /// readable (the standard forbids caching what the user cannot
    /// read), and a source inside a thumbnail directory is refused —
    /// thumbnails of thumbnails are prohibited. Remote sources pass; the
    /// caller vouches for them.
    fn writable(&self, source: &ThumbnailSource) -> io::Result<()> {
        let Some(path) = source.path() else {
            return Ok(());
        };
        if self.is_thumbnail_path(path) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the source is inside a thumbnail directory",
            ));
        }
        if !readable(path)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the original is not readable, so nothing about it is cached",
            ));
        }
        Ok(())
    }
}

/// The keys an install owns and therefore replaces — and nothing else:
/// unrelated `Thumb::*` fields other writers add stay.
const OWNED_KEYS: &[&str] = &["Thumb::URI", "Thumb::MTime", "Thumb::Size"];

/// The borrowed view the PNG writers take.
fn key_refs<'a>(keys: &'a [(&'static str, String)]) -> Vec<(&'a str, &'a str)> {
    keys.iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect()
}

/// The `Thumb::*` keys an install writes.
fn keys(uri: &str, mtime: &str, info: &SourceInfo) -> Vec<(&'static str, String)> {
    let mut keys = vec![
        ("Thumb::URI", uri.to_owned()),
        ("Thumb::MTime", mtime.to_owned()),
    ];
    if let Some(len) = info.len {
        keys.push(("Thumb::Size", len.to_string()));
    }
    keys
}

/// The whole-second `Thumb::MTime` text, or the error for a time the
/// standard cannot spell.
fn before_epoch(info: &SourceInfo) -> io::Result<String> {
    info.mtime_text().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "a modification time before the epoch cannot be written",
        )
    })
}

/// Whether the user can read `path` (`faccessat` with the real IDs). A
/// definite "no" — denied, absent, or an unresolvable path — is
/// `Ok(false)`; anything else the check itself failed on is `Err`.
fn readable(path: &Path) -> io::Result<bool> {
    use rustix::io::Errno;
    match rustix::fs::access(path, rustix::fs::Access::READ_OK) {
        Ok(()) => Ok(true),
        Err(Errno::ACCESS | Errno::NOENT | Errno::NOTDIR | Errno::LOOP | Errno::PERM) => Ok(false),
        Err(err) => Err(err.into()),
    }
}

/// Maps a completed write to the install outcome.
fn installed(
    written: Result<xdg_kit_basedir::Committed, WriteFileError>,
    path: PathBuf,
) -> io::Result<(PathBuf, Durability)> {
    match written {
        Ok(committed) => Ok((path, committed.durability)),
        Err(WriteFileError::Before(err)) => Err(err),
        Err(other) => Err(io::Error::other(other.to_string())),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs;
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::png;

    /// A cache in a fresh home, and the folder standing in for the world.
    pub(crate) fn setup() -> (tempfile::TempDir, ThumbnailCache) {
        let home = tempfile::tempdir().unwrap();
        let cache_home = home.path().join("cache");
        let base = BaseDirs::from_vars(move |name| match name {
            "HOME" => Some("/home/u".into()),
            "XDG_CACHE_HOME" => Some(cache_home.clone().into_os_string()),
            _ => None,
        })
        .unwrap();
        let cache = ThumbnailCache::new(&base);
        (home, cache)
    }

    /// A source file with content, its handle types, and a finished PNG.
    pub(crate) fn a_source(dir: &Path) -> (ThumbnailSource, SourceInfo, PathBuf) {
        let original = dir.join("photo me.png");
        fs::write(&original, "original bytes").unwrap();
        let source = ThumbnailSource::local(&original).unwrap();
        let info = SourceInfo::of_local(&original).unwrap();
        let finished = dir.join("finished.png");
        fs::write(&finished, png::minimal(&[])).unwrap();
        (source, info, finished)
    }

    pub(crate) fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn install_find_round_trip_with_permissions() {
        let (home, cache) = setup();
        let (source, info, finished) = a_source(home.path());
        let (path, durability) = cache
            .install(&finished, &source, &info, ThumbnailSize::Normal)
            .unwrap();
        assert!(matches!(durability, Durability::Synced));
        assert_eq!(mode_of(&path), 0o600);
        assert_eq!(mode_of(path.parent().unwrap()), 0o700);
        let found = cache
            .find(&source, ThumbnailSize::Normal, &info)
            .unwrap()
            .expect("fresh");
        assert_eq!(found.provenance(), Provenance::Personal);
        assert_eq!(found.size(), ThumbnailSize::Normal);
        assert_eq!(found.path(), path);
        assert_eq!(found.freshness(&info), Freshness::Fresh);
        // Touch the original: the entry goes stale and find misses.
        let later = SourceInfo {
            mtime: info.mtime + Duration::from_secs(90),
            len: info.len,
        };
        assert_eq!(found.freshness(&later), Freshness::Stale);
        assert!(
            cache
                .find(&source, ThumbnailSize::Normal, &later)
                .unwrap()
                .is_none()
        );
        // A size mismatch is stale too.
        let grown = SourceInfo {
            mtime: info.mtime,
            len: info.len.map(|len| len + 1),
        };
        assert_eq!(found.freshness(&grown), Freshness::Stale);
    }

    #[test]
    fn a_stale_exact_entry_falls_through_to_a_fresh_larger_one() {
        let (home, cache) = setup();
        let (source, info, finished) = a_source(home.path());
        // Install large fresh; normal with wrong (old) metadata.
        let old = SourceInfo {
            mtime: info.mtime - Duration::from_secs(1000),
            len: info.len,
        };
        let _ = cache
            .install(&finished, &source, &old, ThumbnailSize::Normal)
            .unwrap();
        let _ = cache
            .install(&finished, &source, &info, ThumbnailSize::Large)
            .unwrap();
        let found = cache
            .find(&source, ThumbnailSize::Normal, &info)
            .unwrap()
            .expect("the larger one");
        assert_eq!(found.size(), ThumbnailSize::Large);
        // Both are visible as candidates, in order.
        let listed = cache.candidates(&source, ThumbnailSize::Normal);
        assert_eq!(listed.len(), 2);
        assert!(matches!(&listed[0], Candidate::Sound(first)
            if first.size() == ThumbnailSize::Normal));
    }

    #[test]
    fn broken_and_metadata_less_personal_entries_are_skipped() {
        let (home, cache) = setup();
        let (source, info, finished) = a_source(home.path());
        let (path, _) = cache
            .install(&finished, &source, &info, ThumbnailSize::Normal)
            .unwrap();
        // Corrupt it in place: no longer sound, visible as a skip.
        fs::write(&path, "junk").unwrap();
        assert!(
            cache
                .find(&source, ThumbnailSize::Normal, &info)
                .unwrap()
                .is_none()
        );
        let listed = cache.candidates(&source, ThumbnailSize::Normal);
        assert!(
            matches!(&listed[0], Candidate::Skipped(skip)
            if skip.message.contains("not a sound PNG")),
            "{listed:?}"
        );
        // A personal entry without its required keys is stale, never
        // unknown.
        fs::write(&path, png::minimal(&[])).unwrap();
        let listed = cache.candidates(&source, ThumbnailSize::Normal);
        let Candidate::Sound(bare) = &listed[0] else {
            panic!("{listed:?}");
        };
        assert_eq!(bare.freshness(&info), Freshness::Stale);
    }

    #[test]
    fn shared_repositories_serve_fallbacks_but_unknown_is_not_auto_accepted() {
        let (home, cache) = setup();
        let (source, info, finished) = a_source(home.path());
        let _ = cache
            .install_shared(
                &finished,
                source.path().unwrap(),
                &info,
                ThumbnailSize::Normal,
            )
            .unwrap();
        let found = cache
            .find(&source, ThumbnailSize::Normal, &info)
            .unwrap()
            .expect("the shared one");
        assert_eq!(found.provenance(), Provenance::Shared);
        assert_eq!(found.freshness(&info), Freshness::Fresh);
        // Strip the keys: unknown, visible, but find no longer takes it.
        fs::write(found.path(), png::minimal(&[])).unwrap();
        assert!(
            cache
                .find(&source, ThumbnailSize::Normal, &info)
                .unwrap()
                .is_none()
        );
        let listed = cache.candidates(&source, ThumbnailSize::Normal);
        let Candidate::Sound(shared) = &listed[0] else {
            panic!("{listed:?}");
        };
        assert_eq!(
            shared.freshness(&info),
            Freshness::Unknown,
            "shared may lack keys"
        );
    }

    #[test]
    fn shared_authoring_keys_and_modes_follow_the_source() {
        let (home, cache) = setup();
        let (source, info, finished) = a_source(home.path());
        let original = source.path().unwrap();
        fs::set_permissions(original, fs::Permissions::from_mode(0o741)).unwrap();
        let (path, _) = cache
            .install_shared(&finished, original, &info, ThumbnailSize::XXLarge)
            .unwrap();
        // ./name keys: the spelled-out form, not the bare basename.
        assert!(path.ends_with(format!(
            ".sh_thumbnails/xx-large/{}.png",
            crate::md5::hex(b"./photo%20me.png")
        )));
        // Modes only, masked: no special or execute bits on the file.
        assert_eq!(mode_of(&path), 0o640, "0741 & 0666");
        let png = png::read(&fs::read(&path).unwrap(), &Limits::default()).unwrap();
        assert_eq!(png.key("Thumb::URI"), Some("./photo%20me.png"));
    }

    #[test]
    fn unreadable_originals_are_neither_served_nor_cached() {
        let (home, cache) = setup();
        let (source, info, finished) = a_source(home.path());
        let _ = cache
            .install(&finished, &source, &info, ThumbnailSize::Normal)
            .unwrap();
        fs::set_permissions(source.path().unwrap(), fs::Permissions::from_mode(0o000)).unwrap();
        if readable(source.path().unwrap()).unwrap() {
            // root reads anything; the gate cannot be exercised.
            return;
        }
        assert!(
            cache
                .find(&source, ThumbnailSize::Normal, &info)
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            &cache.candidates(&source, ThumbnailSize::Normal)[..],
            [Candidate::Skipped(skip)] if skip.message.contains("not readable")
        ));
        let refused = cache.install(&finished, &source, &info, ThumbnailSize::Large);
        assert_eq!(refused.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn thumbnails_of_thumbnails_are_refused() {
        let (home, cache) = setup();
        let (_, info, finished) = a_source(home.path());
        let inside = cache.root.join("normal").join("x.png");
        fs::create_dir_all(inside.parent().unwrap()).unwrap();
        fs::write(&inside, "x").unwrap();
        assert!(cache.is_thumbnail_path(&inside));
        let shared = home.path().join(".sh_thumbnails/normal/y.png");
        assert!(cache.is_thumbnail_path(&shared));
        let source = ThumbnailSource::local(&inside).unwrap();
        let refused = cache.install(&finished, &source, &info, ThumbnailSize::Normal);
        assert_eq!(refused.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn failure_markers_round_trip_and_go_stale() {
        let (home, cache) = setup();
        let (source, info, finished) = a_source(home.path());
        let app = Application::new("xdg-kit-tests-1").unwrap();
        assert!(cache.failed(&source, &app, &info).unwrap().is_none());
        let (path, _) = cache.mark_failed(&source, &info, &app).unwrap();
        assert!(path.to_string_lossy().contains("/fail/xdg-kit-tests-1/"));
        assert_eq!(mode_of(&path), 0o600);
        assert!(cache.failed(&source, &app, &info).unwrap().is_some());
        // The source changed since: the marker is no failure.
        let later = SourceInfo {
            mtime: info.mtime + Duration::from_secs(5),
            len: info.len,
        };
        assert!(cache.failed(&source, &app, &later).unwrap().is_none());
        // A finished install still validates its input.
        let refused = cache.install(
            source.path().unwrap(),
            &source,
            &info,
            ThumbnailSize::Normal,
        );
        assert_eq!(refused.unwrap_err().kind(), io::ErrorKind::InvalidData);
        let _ = finished;
    }

    #[test]
    fn a_pre_epoch_time_cannot_be_written() {
        let (home, cache) = setup();
        let (source, _, finished) = a_source(home.path());
        let before = SourceInfo {
            mtime: SystemTime::UNIX_EPOCH - Duration::from_secs(5),
            len: None,
        };
        let refused = cache.install(&finished, &source, &before, ThumbnailSize::Normal);
        assert_eq!(refused.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }
}

#[cfg(test)]
mod review_regressions {
    use std::fs;
    use std::time::Duration;

    use super::tests::{a_source, mode_of, setup};
    use super::*;
    use crate::png;

    #[test]
    fn a_personal_entry_without_a_uri_is_never_fresh() {
        let (home, cache) = setup();
        let (source, info, finished) = a_source(home.path());
        let (path, _) = cache
            .install(&finished, &source, &info, ThumbnailSize::Normal)
            .unwrap();
        // The right timestamp, but the required URI is gone.
        let mtime = info
            .mtime
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap();
        fs::write(
            &path,
            png::minimal(&[("Thumb::MTime", &mtime.as_secs().to_string())]),
        )
        .unwrap();
        assert!(
            cache
                .find(&source, ThumbnailSize::Normal, &info)
                .unwrap()
                .is_none(),
            "both mandatory keys are required in the personal cache"
        );
    }

    #[test]
    fn malformed_metadata_is_stale_not_absent() {
        let (home, cache) = setup();
        let (source, info, finished) = a_source(home.path());
        let (path, _) = cache
            .install(&finished, &source, &info, ThumbnailSize::Normal)
            .unwrap();
        let mtime = info
            .mtime
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        // A corrupt Thumb::Size with everything else matching.
        fs::write(
            &path,
            png::minimal(&[
                ("Thumb::URI", &source.uri()),
                ("Thumb::MTime", &mtime),
                ("Thumb::Size", "not-a-number"),
            ]),
        )
        .unwrap();
        assert!(
            cache
                .find(&source, ThumbnailSize::Normal, &info)
                .unwrap()
                .is_none()
        );
        // A corrupt shared timestamp is stale, never the missing-metadata
        // exception.
        let (shared_path, _) = cache
            .install_shared(
                &finished,
                source.path().unwrap(),
                &info,
                ThumbnailSize::Normal,
            )
            .unwrap();
        fs::write(
            &shared_path,
            png::minimal(&[("Thumb::MTime", "yesterday-ish")]),
        )
        .unwrap();
        let listed = cache.candidates(&source, ThumbnailSize::Normal);
        let shared = listed
            .iter()
            .find_map(|candidate| match candidate {
                Candidate::Sound(found) if found.provenance() == Provenance::Shared => Some(found),
                _ => None,
            })
            .expect("{listed:?}");
        assert_eq!(shared.freshness(&info), Freshness::Stale);
    }

    #[test]
    fn output_that_could_not_be_read_back_is_refused() {
        let (home, cache) = setup();
        let tight = ThumbnailCache {
            limits: Limits {
                max_chunks: 3,
                ..Limits::default()
            },
            ..cache
        };
        let (source, info, finished) = a_source(home.path());
        // The finished PNG has 3 chunks; the splice adds the keys and
        // pushes it over this cache's own chunk limit.
        let refused = tight.install(&finished, &source, &info, ThumbnailSize::Normal);
        assert_eq!(refused.unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert!(
            tight.candidates(&source, ThumbnailSize::Normal).is_empty(),
            "nothing was installed"
        );
    }

    #[test]
    fn symlinked_cache_directories_are_refused_and_modes_repaired() {
        let (home, cache) = setup();
        let (source, info, finished) = a_source(home.path());
        // thumbnails/normal as a symlink pointing outside the cache.
        let outside = home.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(&cache.root).unwrap();
        std::os::unix::fs::symlink(&outside, cache.root.join("normal")).unwrap();
        let refused = cache.install(&finished, &source, &info, ThumbnailSize::Normal);
        assert_eq!(refused.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            fs::read_dir(&outside).unwrap().count(),
            0,
            "nothing escaped"
        );
        // An existing directory whose bits drifted is set back to 0700.
        fs::remove_file(cache.root.join("normal")).unwrap();
        fs::create_dir(cache.root.join("normal")).unwrap();
        fs::set_permissions(cache.root.join("normal"), fs::Permissions::from_mode(0o755)).unwrap();
        let _ = cache
            .install(&finished, &source, &info, ThumbnailSize::Normal)
            .unwrap();
        assert_eq!(mode_of(&cache.root.join("normal")), 0o700);
        // A symlinked shared repository level is refused too.
        let repo = source
            .path()
            .unwrap()
            .parent()
            .unwrap()
            .join(".sh_thumbnails");
        std::os::unix::fs::symlink(&outside, &repo).unwrap();
        let refused = cache.install_shared(
            &finished,
            source.path().unwrap(),
            &info,
            ThumbnailSize::Normal,
        );
        assert_eq!(refused.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        let _ = info;
        let _ = Duration::ZERO;
    }
}

#[cfg(test)]
mod concurrency_regressions {
    use super::tests::{a_source, setup};
    use super::*;

    #[test]
    fn concurrent_first_time_installs_all_succeed() {
        let (home, cache) = setup();
        let (source, info, finished) = a_source(home.path());
        // Everyone races to create the same fresh directories.
        std::thread::scope(|threads| {
            for _ in 0..16 {
                let (cache, source, finished) = (&cache, &source, &finished);
                let racing = info;
                threads.spawn(move || {
                    let _ = cache
                        .install(finished, source, &racing, ThumbnailSize::Normal)
                        .expect("concurrent initialization is ordinary");
                });
            }
        });
        assert!(
            cache
                .find(&source, ThumbnailSize::Normal, &info)
                .unwrap()
                .is_some()
        );
    }
}

#[cfg(test)]
mod fit_regressions {
    use std::fs;

    use super::tests::{a_source, setup};
    use super::*;
    use crate::crc32;
    use crate::png;

    /// The minimal marker PNG with its IHDR dimensions rewritten.
    fn sized(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = png::minimal(&[]);
        bytes[16..20].copy_from_slice(&width.to_be_bytes());
        bytes[20..24].copy_from_slice(&height.to_be_bytes());
        let crc = crc32::of(&[b"IHDR", &bytes[16..29]]);
        bytes[29..33].copy_from_slice(&crc.to_be_bytes());
        bytes
    }

    #[test]
    fn output_must_fit_its_size_class() {
        let (home, cache) = setup();
        let (source, info, finished) = a_source(home.path());
        fs::write(&finished, sized(4096, 1)).unwrap();
        let refused = cache.install(&finished, &source, &info, ThumbnailSize::Normal);
        assert_eq!(refused.unwrap_err().kind(), io::ErrorKind::InvalidData);
        let refused = cache.install_shared(
            &finished,
            source.path().unwrap(),
            &info,
            ThumbnailSize::Large,
        );
        assert_eq!(refused.unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert!(
            cache.candidates(&source, ThumbnailSize::Normal).is_empty(),
            "nothing was installed"
        );
        // The class bounds the larger dimension; 4096 fits xx-large.
        fs::write(&finished, sized(1024, 128)).unwrap();
        let _ = cache
            .install(&finished, &source, &info, ThumbnailSize::XXLarge)
            .unwrap();
    }
}
