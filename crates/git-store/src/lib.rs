//! Store structured values in Git with versioned schemas.
//!
//! Values are read against the schema bound to each stored version.
//! [`Store`] works with configurable ref and object backends;
//! [`RepoStore`] provides the integration for a `gix::Repository`.
//!
//! [`tree`] documents the tree format used by this crate. [`Store::kind`]
//! provides typed access, while [`Store::dynamic`] provides access through
//! [`facet_value::Value`].
#![forbid(unsafe_code)]

mod address;
mod canonical;
mod doctor;
mod document;
mod encoding;
mod error;
mod identity;
mod index;
mod kind;

mod store;
mod tombstone;
mod transaction;

pub use address::At;
pub use doctor::{DoctorReport, SUPPORTED_OBJECT_FORMAT, check_repository};
pub use document::{
    DocumentBuilder, DocumentError, DocumentInspection, DocumentKind, DocumentShapeError,
    PreparedDocument, SchemaSnapshot,
};
pub use encoding::{Dynamic, Encoding, Typed};
pub use error::{Error, Subtree};
pub use identity::{
    DocumentTree, EntityId, SchemaTree, ValueTree, canonical_document_id, canonical_object_id,
};
pub use kind::{
    Entry, Kind, KindSchema, NamedEntries, Put, entity_id_name, entity_name, entity_name_under,
};

pub use canonical::CanonicalCommit;
pub use store::{Layout, Publication, PublishOptions, RepoStore, Store, decode};
pub use tombstone::{
    DeleteResult, EntityState, ReadResult, ReadState, Tombstone, TombstoneEntry, TombstoneState,
};
pub use transaction::Transaction;

pub use facet_git_tree::{ObjectId, Schema, schema_of};

/// The tree and schema format used by `git-store`.
pub use facet_git_tree as tree;
pub use gix_refstore::{
    ApplyError, Committer, Expectation, GixRefStore, InvalidRefName, MemoryRefStore, RefEdit,
    RefName, RefPath, RefPrefix, RefSegment, RefStore, SignatureBytes, Signer,
};

impl<'s, R, O> KindSchema<'s, R, O>
where
    R: RefStore,
    O: gix::objs::Find,
{
    /// Read the schema document stored by `commit` into an owned snapshot.
    ///
    /// The commit is addressed directly and the resulting schema does not
    /// borrow the publication ref. This permits callers to retain a historical
    /// schema while the kind advances or its publication ref is removed.
    pub fn snapshot_at(&self, commit: ObjectId) -> Result<SchemaSnapshot, Error> {
        let schema_tree = self.store.commit_tree(commit)?;
        let schema = self.store.schema(schema_tree)?;
        Ok(SchemaSnapshot {
            commit,
            schema_tree: SchemaTree::from(schema_tree),
            schema: (*schema).clone(),
        })
    }

    /// Capture the schema currently published for this kind.
    pub fn current_snapshot(&self) -> Result<SchemaSnapshot, Error> {
        let commit = self
            .store
            .refs()
            .read(&self.reference)
            .map_err(Error::backend)?
            .ok_or_else(|| Error::NoSchema {
                kind: self.kind.clone(),
            })?;
        self.snapshot_at(commit)
    }
}
