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
            .filter(|(name, _)| name.strip_prefix(prefix).is_some())
            .map(|(name, id)| (name.clone(), *id))
            .collect())
    }

    fn apply(&self, edit: RefEdit) -> Result<(), ApplyError<Self::Error>> {
        let mut refs = self
            .refs
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let name = edit.name().clone();
        let expectation = edit.expectation();
        let current = refs.get(&name).copied();

        let race = || ApplyError::LostRace {
            name: name.clone(),
            expected: expectation,
        };

        match edit {
            RefEdit::Create { new, .. } => {
                if current.is_some() {
                    return Err(race());
                }
                refs.insert(name, new);
            }
            RefEdit::Update { expected, new, .. } => {
                if current != Some(expected) {
                    return Err(race());
                }
                refs.insert(name, new);
            }
            RefEdit::Delete { expected, .. } => {
                if current != Some(expected) {
                    return Err(race());
                }
                refs.remove(&name);
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
