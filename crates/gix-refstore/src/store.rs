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
///
/// # Read consistency
///
/// Reads ([`RefStore::read`], [`RefStore::prefixed`]) are lock-free and are
/// **not** snapshot-isolated. Reading several refs — in one listing or in
/// successive calls — can observe a concurrent [`RefStore::apply_batch`]
/// only partially: one ref of the batch moved, another not yet. Nothing here
/// promises otherwise, and nothing over plain ref files shared across
/// processes could: a multi-ref transaction is committed one `rename(2)` at a
/// time. A caller needing a mutually consistent multi-ref view must re-read
/// and re-validate; there is no snapshot primitive.
///
/// What [`RefStore::apply_batch`] does promise is all-or-nothing *write*
/// atomicity: every expectation is checked before any edit is published, a
/// lost race changes nothing, and writers are serialized against each other
/// across threads and processes. That is a promise about writers, not about
/// what a concurrent reader observes.
///
/// [`MemoryRefStore`](crate::MemoryRefStore) is stricter than the trait: it
/// serializes reads against writes in-process, so its readers *are*
/// point-in-time consistent. That is a property of that one backend, not a
/// guarantee of this trait, and it does not extend across processes.
pub trait RefStore {
    /// A failure of the backend itself, as distinct from a lost race.
    type Error: std::error::Error + Send + Sync + 'static;

    /// The object `name` points at, or `None` when the ref does not exist.
    ///
    /// Lock-free and not snapshot-isolated; see the trait's
    /// [read consistency](RefStore#read-consistency) note.
    fn read(&self, name: &RefName) -> Result<Option<ObjectId>, Self::Error>;

    /// Every ref under `prefix`, ascending by name.
    ///
    /// The boundary is a whole segment: `<prefix>/foobar` is not under
    /// `<prefix>/foo`. Ordering is part of the contract — callers may rely
    /// on it instead of sorting.
    ///
    /// Lock-free and not snapshot-isolated: a concurrent batch can be caught
    /// mid-flight, so the listing is not a view of one instant. See the
    /// trait's [read consistency](RefStore#read-consistency) note.
    fn prefixed(&self, prefix: &RefPrefix) -> Result<Vec<(RefName, ObjectId)>, Self::Error>;

    /// Apply `edit` if its [`Expectation`] still holds, atomically against
    /// concurrent writers in other threads and other processes.
    ///
    /// Fails with [`ApplyError::LostRace`], having changed nothing, when it
    /// does not.
    fn apply(&self, edit: RefEdit) -> Result<(), ApplyError<Self::Error>> {
        self.apply_batch(vec![edit])
    }

    /// Apply all `edits` as one compare-and-swap transaction.
    ///
    /// Every expectation is checked before any edit is published. A lost race
    /// identifies the ref whose expectation failed, and leaves the batch
    /// unchanged. Implementations must not emulate this with sequential calls
    /// to [`RefStore::apply`].
    ///
    /// The promise is all-or-nothing atomicity against concurrent *writers*,
    /// not durability: it says nothing about crash safety, and a successful
    /// return does not imply the result has been fsynced. It is not read
    /// isolation — see the trait's [read consistency](RefStore#read-consistency)
    /// note for what a concurrent reader can observe.
    fn apply_batch(&self, edits: Vec<RefEdit>) -> Result<(), ApplyError<Self::Error>>;
}

/// Forward a [`RefStore`] through a shared reference, so a decorator such as
/// [`AsOfRefStore`](crate::AsOfRefStore) can borrow the store it wraps instead
/// of taking ownership of it.
impl<S: RefStore + ?Sized> RefStore for &S {
    type Error = S::Error;

    fn read(&self, name: &RefName) -> Result<Option<ObjectId>, Self::Error> {
        (**self).read(name)
    }

    fn prefixed(&self, prefix: &RefPrefix) -> Result<Vec<(RefName, ObjectId)>, Self::Error> {
        (**self).prefixed(prefix)
    }

    fn apply_batch(&self, edits: Vec<RefEdit>) -> Result<(), ApplyError<Self::Error>> {
        (**self).apply_batch(edits)
    }
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

    /// The author signature to stamp on objects written now.
    ///
    /// Defaults to [`Committer::signature`]; git resolves the two from
    /// separate configuration, so a backend that can tell them apart
    /// overrides this.
    fn author(&self) -> Result<Signature, Self::Error> {
        self.signature()
    }
}
