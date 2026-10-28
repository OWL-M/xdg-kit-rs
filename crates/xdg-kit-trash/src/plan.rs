//! Where a path would be trashed, found without trashing it.

use std::path::{Path, PathBuf};

use crate::context::Context;
use crate::dir::{self, HomeRoot, Mode, TrashDir, TrashKind, Walked};
use crate::insert::{Attempt, AttemptFailure, TrashError, recorded_path};

/// Where [`Context::trash`] would put a path, as far as can be told now.
///
/// Advisory: the file system can change before a trashing runs, and the
/// trashing decides — it may still fail where this says a trash directory
/// is usable, or create one this says it would. Nothing is created or
/// changed to find this out.
#[derive(Debug)]
#[non_exhaustive]
pub struct TrashPlan {
    /// The trash directories trashing would try, in its order: the first
    /// that is not [`CandidateState::Unusable`] is where the file would go,
    /// unless moving it fails there.
    pub candidates: Vec<Candidate>,
    /// The ones trashing would pass over, and why (another mount, per-volume
    /// trashing disabled, the top directory unknown), as trashing records
    /// them in [`Trashed::fallbacks`](crate::Trashed::fallbacks).
    pub skipped: Vec<Attempt>,
}

impl TrashPlan {
    /// The first candidate that is usable as it is or could be created:
    /// where the file would go if nothing changes and its move succeeds.
    #[must_use]
    pub fn destination(&self) -> Option<&Candidate> {
        self.candidates
            .iter()
            .find(|candidate| !matches!(candidate.state, CandidateState::Unusable(_)))
    }
}

/// A trash directory trashing would try.
#[derive(Debug)]
#[non_exhaustive]
pub struct Candidate {
    /// Where it is.
    pub trash: TrashDir,
    /// Whether it can take the file.
    pub state: CandidateState,
}

/// Whether a candidate trash directory can take a file.
#[derive(Debug)]
#[non_exhaustive]
pub enum CandidateState {
    /// It exists, passes its checks, and this process may write to its
    /// `files` and `info`.
    Usable,
    /// Directories are missing, and the checks so far would let trashing
    /// make them: this one first, then everything below it.
    WouldCreate(PathBuf),
    /// Trashing would not use it, and why.
    Unusable(AttemptFailure),
}

impl Context {
    /// Where [`Context::trash`] would put `path`, creating and changing
    /// nothing: the same inspection of the source and the same choice of
    /// trash directories, then each candidate checked as trashing would
    /// open it — but stopping at the first missing directory, judging only
    /// whether it could be made (permission is judged as the kernel would
    /// for this process, by its effective user and groups; on Android by
    /// the real ones).
    ///
    /// The answer is advisory ([`TrashPlan`]). The spec's rules on creating
    /// trash directories without delay apply to trashing, not to this.
    /// Exclusions ([`Policy::exclusions`](crate::Policy::exclusions)) apply
    /// as in trashing: an excluded source is refused from its path alone,
    /// and excluded trash directories are passed over unprobed.
    ///
    /// How the payload would be moved is not predicted: whether the native
    /// non-replacing rename, or a fallback ([`Policy::fallbacks`](crate::Policy::fallbacks)),
    /// works on a file system is known only by trying it, and trying moves
    /// the file.
    ///
    /// # Errors
    ///
    /// What [`Context::trash`] would refuse before trying any trash
    /// directory, with the same [`TrashError`] variants: an unsupported
    /// system, a path with no name, a missing or uninspectable source, a
    /// source that is or holds a trash directory or is inside one.
    pub fn plan(&self, path: impl AsRef<Path>) -> Result<TrashPlan, TrashError> {
        let source = self.inspect(path.as_ref())?;
        let selection = self.select(&source)?;
        let original = source.path();
        let mut home = selection.home;
        let candidates = selection
            .candidates
            .into_iter()
            .map(|trash| {
                let root = if trash.kind() == TrashKind::Home {
                    std::mem::replace(&mut home, HomeRoot::NoLink)
                } else {
                    HomeRoot::NoLink
                };
                let state = match dir::walk_with(self, &trash, Mode::Probe, root) {
                    Ok(Walked::Opened(opened, _)) => match recorded_path(&opened, &original) {
                        Some(_) => CandidateState::Usable,
                        None => CandidateState::Unusable(AttemptFailure::OutsideBase),
                    },
                    Ok(Walked::WouldCreate(first)) => CandidateState::WouldCreate(first),
                    Err(err) => CandidateState::Unusable(AttemptFailure::Open(err)),
                };
                Candidate { trash, state }
            })
            .collect();
        Ok(TrashPlan {
            candidates,
            skipped: selection.skipped,
        })
    }
}
