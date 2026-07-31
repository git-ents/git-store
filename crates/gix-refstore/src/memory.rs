//! An in-memory [`RefStore`]/[`Committer`] for tests.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::RwLock;

use gix::actor::Signature;
use gix_hash::ObjectId;

use crate::edit::RefEdit;
use crate::name::{RefName, RefPrefix};
use crate::store::{ApplyError, Committer, RefStore};

/// A [`RefStore`] with no repository behind it, for tests.
///
/// Its [`Signature`] is fixed, so commits written against it are
/// reproducible.
pub struct MemoryRefStore {
    refs: RwLock<BTreeMap<RefName, ObjectId>>,
    signature: Signature,
}

impl MemoryRefStore {
    /// An empty store with the default fixed signature.
    pub fn new() -> Self {
        Self::with_signature(Signature {
            name: "gix-refstore".into(),
            email: "refstore@example.invalid".into(),
            time: gix::date::Time {
                seconds: 0,
                offset: 0,
            },
        })
    }

    /// An empty store signing as `signature`.
    pub fn with_signature(signature: Signature) -> Self {
        Self {
            refs: RwLock::new(BTreeMap::new()),
            signature,
        }
    }
}

impl Default for MemoryRefStore {
    fn default() -> Self {
        Self::new()
    }
}

impl RefStore for MemoryRefStore {
    type Error = Infallible;

    fn read(&self, name: &RefName) -> Result<Option<ObjectId>, Self::Error> {
        let refs = self
            .refs
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(refs.get(name).copied())
    }

    fn prefixed(&self, prefix: &RefPrefix) -> Result<Vec<(RefName, ObjectId)>, Self::Error> {
        let refs = self
            .refs
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(refs
            .iter()
            .filter(|(name, _)| name.is_under(prefix))
            .map(|(name, id)| (name.clone(), *id))
            .collect())
    }

    fn apply_batch(&self, edits: Vec<RefEdit>) -> Result<(), ApplyError<Self::Error>> {
        let mut refs = self
            .refs
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        for edit in &edits {
            let name = edit.name().clone();
            let current = refs.get(&name).copied();
            let expectation = edit.expectation();
            let matches = match edit {
                RefEdit::Create { .. } => current.is_none(),
                RefEdit::Update { expected, .. } | RefEdit::Delete { expected, .. } => {
                    current == Some(*expected)
                }
            };
            if !matches {
                return Err(ApplyError::LostRace {
                    name,
                    expected: expectation,
                });
            }
        }

        for edit in edits {
            match edit {
                RefEdit::Create { name, new } | RefEdit::Update { name, new, .. } => {
                    refs.insert(name, new);
                }
                RefEdit::Delete { name, .. } => {
                    refs.remove(&name);
                }
            }
        }
        Ok(())
    }
}

impl Committer for MemoryRefStore {
    type Error = Infallible;

    fn signature(&self) -> Result<Signature, Self::Error> {
        Ok(self.signature.clone())
    }
}
