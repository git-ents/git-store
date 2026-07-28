//! The two traits a backend implements: [`RefStore`] for refs, [`Committer`]
//! for the identity to attribute writes to.

use gix::actor::Signature;
use gix_hash::ObjectId;

use crate::edit::{Expectation, RefEdit};
use crate::name::{RefName, RefPrefix};

/// Why a [`RefEdit`] did not apply.
#[derive(Debug, thiserror::Error)]
pub enum ApplyError<E> {
    /// The store could not confirm the edit's precondition, so nothing was
    /// written: either the ref no longer holds `expected`, or the backend's
    /// own lock on it was momentarily held elsewhere. Re-read the ref and
    /// retry.
    #[error("compare-and-swap on {name} did not apply: expected {expected}")]
    LostRace {
        /// The ref the edit targeted.
        name: RefName,
        /// The precondition that no longer held.
        expected: Expectation,
    },
    /// The backend failed for a reason retrying will not fix.
    #[error(transparent)]
    Backend(#[from] E),
}

/// Compare-and-swap storage for Git refs.
///
/// Objects are written through `gix_object::Write`; this is refs only.
pub trait RefStore {
    /// A failure of the backend itself, as distinct from a lost race.
    type Error: std::error::Error + Send + Sync + 'static;

    /// The object `name` points at, or `None` when the ref does not exist.
    fn read(&self, name: &RefName) -> Result<Option<ObjectId>, Self::Error>;

    /// Every ref under `prefix`, ascending by name.
    ///
    /// The boundary is a whole segment: `<prefix>/foobar` is not under
    /// `<prefix>/foo`. Ordering is part of the contract — callers may rely
    /// on it instead of sorting.
    fn prefixed(&self, prefix: &RefPrefix) -> Result<Vec<(RefName, ObjectId)>, Self::Error>;

    /// Apply `edit` if its [`Expectation`] still holds, atomically against
    /// concurrent writers in other threads and other processes.
    ///
    /// Fails with [`ApplyError::LostRace`], having changed nothing, when it
    /// does not.
    fn apply(&self, edit: RefEdit) -> Result<(), ApplyError<Self::Error>>;
}

/// The identity to attribute writes to.
///
/// Separate from [`RefStore`] because a store's refs and a repository's
/// configured identity are independent concerns.
pub trait Committer {
    /// A failure to determine the identity.
    type Error: std::error::Error + Send + Sync + 'static;

    /// The committer signature to stamp on objects written now.
    fn signature(&self) -> Result<Signature, Self::Error>;
}
