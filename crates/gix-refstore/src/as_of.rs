//! A read-only [`RefStore`] view that answers as though a fixed set of refs
//! held different values, without ever writing to the store underneath.

use std::collections::BTreeMap;

use gix_hash::ObjectId;

use crate::edit::RefEdit;
use crate::name::{RefName, RefPrefix};
use crate::store::{ApplyError, RefStore};

/// `inner`, as it would answer if every name in `overrides` held the value
/// given there instead of whatever `inner` actually holds right now.
///
/// This is the seam for evaluating something against the repository *as it
/// was before a ref moved*, without mutating the repository to find out: no
/// rewind-then-restore, because that writes to answer a read, does not
/// survive a crash mid-window, produces synthetic reflog entries, and — on a
/// server where a [`RefStore`] is the ref transaction manager — makes a
/// momentarily-absent branch visible to every concurrent reader.
/// `AsOfRefStore` computes the same answer purely, from `inner`'s real state
/// plus the overrides, and issues no write of its own; [`RefStore::apply`]
/// through it always fails, see [`AsOfError::ReadOnly`].
///
/// An override maps a name to `Some(id)` — as if it held `id` — or `None` —
/// as if it did not exist. A name absent from the overrides is answered by
/// `inner`, unchanged.
pub struct AsOfRefStore<S> {
    inner: S,
    overrides: BTreeMap<RefName, Option<ObjectId>>,
}

impl<S> AsOfRefStore<S> {
    /// `inner`, viewed through `overrides`.
    pub fn new(inner: S, overrides: impl IntoIterator<Item = (RefName, Option<ObjectId>)>) -> Self {
        Self {
            inner,
            overrides: overrides.into_iter().collect(),
        }
    }

    /// The wrapped store, borrowed — for checking its real state alongside
    /// the view's, without disturbing either.
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// The wrapped store, recovered. Since `AsOfRefStore` never writes
    /// through it, this is exactly the store that was handed to
    /// [`AsOfRefStore::new`], in whatever state `inner`'s own callers left
    /// it in the meantime.
    pub fn into_inner(self) -> S {
        self.inner
    }
}

/// Why an operation on an [`AsOfRefStore`] failed.
#[derive(Debug, thiserror::Error)]
pub enum AsOfError<E> {
    /// [`RefStore::apply`] was called through the view. An as-of view
    /// answers for a fixed, historical set of ref values; there is no
    /// current state for a compare-and-swap to apply against, and no amount
    /// of retrying changes that — which is why this surfaces as
    /// [`ApplyError::Backend`] rather than [`ApplyError::LostRace`].
    /// `LostRace` promises that a re-read and a retry can succeed; here,
    /// neither ever will, because the view does not move.
    #[error("cannot write through a read-only as-of ref view")]
    ReadOnly,
    /// A read against the wrapped store failed.
    #[error(transparent)]
    Inner(#[from] E),
}

impl<S: RefStore> RefStore for AsOfRefStore<S> {
    type Error = AsOfError<S::Error>;

    /// The override for `name`, if any; otherwise `inner`'s own answer.
    fn read(&self, name: &RefName) -> Result<Option<ObjectId>, Self::Error> {
        match self.overrides.get(name) {
            Some(over) => Ok(*over),
            None => Ok(self.inner.read(name)?),
        }
    }

    /// `inner`'s listing with the overrides under `prefix` folded in: a name
    /// overridden to `None` is removed, a name overridden to `Some` is
    /// replaced or inserted.
    ///
    /// The merge goes through a `BTreeMap`, so the result comes out
    /// ascending by name for free, matching the ordering [`RefStore::prefixed`]
    /// documents as part of its contract — including for an inserted name,
    /// which lands wherever it sorts, not appended at the end.
    fn prefixed(&self, prefix: &RefPrefix) -> Result<Vec<(RefName, ObjectId)>, Self::Error> {
        let mut merged: BTreeMap<RefName, ObjectId> =
            self.inner.prefixed(prefix)?.into_iter().collect();
        for (name, over) in &self.overrides {
            if !name.is_under(prefix) {
                continue;
            }
            match over {
                Some(id) => {
                    merged.insert(name.clone(), *id);
                }
                None => {
                    merged.remove(name);
                }
            }
        }
        Ok(merged.into_iter().collect())
    }

    /// Always fails with [`AsOfError::ReadOnly`]: writing through a
    /// historical view is a category error, not a race to retry, so it is
    /// refused rather than delegated to `inner` or silently accepted and
    /// dropped.
    fn apply_batch(&self, _edits: Vec<RefEdit>) -> Result<(), ApplyError<Self::Error>> {
        Err(ApplyError::Backend(AsOfError::ReadOnly))
    }
}
