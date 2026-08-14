//! The single error type for every [`Store`](crate::Store) operation.

use std::fmt;

use facet_git_tree::ObjectId;
use gix_refstore::{RefName, RefSegment};

/// Which half of a data commit's `{schema/, value/}` split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subtree {
    /// The `value/` entry: the encoded entity.
    Value,
    /// The `schema/` entry: the schema it was validated against.
    Schema,
}

impl Subtree {
    /// The tree entry name: `"value"` or `"schema"`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Subtree::Value => "value",
            Subtree::Schema => "schema",
        }
    }
}

impl fmt::Display for Subtree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An error from a [`Store`](crate::Store) operation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No schema is published for the kind, so publish one before storing values.
    #[error("no schema published for kind \"{kind}\"; publish a schema before storing values")]
    NoSchema {
        /// The kind with no published schema.
        kind: RefSegment,
    },
    /// A data commit does not contain the bound value and schema required for reading.
    #[error("commit {commit} does not contain a readable bound value (found [{found}])")]
    NotSubtreeBound {
        /// The data commit whose tree was not the expected split.
        commit: ObjectId,
        /// The entry names actually found, comma-separated, for diagnosis.
        found: String,
    },
    /// A data commit references an object needed for reading that is not present.
    #[error("commit {commit} references a missing {subtree} object {oid}")]
    SubtreeMissing {
        /// Which half of the split is absent.
        subtree: Subtree,
        /// The absent object.
        oid: ObjectId,
        /// The data commit naming it.
        commit: ObjectId,
    },
    /// An object a read needed is not present.
    #[error("object {oid} is not present")]
    MissingObject {
        /// The absent object.
        oid: ObjectId,
    },
    /// An object a read needed is present but is not a commit.
    #[error("object {oid} is not a commit")]
    NotACommit {
        /// The object of the wrong kind.
        oid: ObjectId,
    },
    /// An object a read needed is present but is not a tree.
    #[error("object {oid} is not a tree")]
    NotATree {
        /// The object of the wrong kind.
        oid: ObjectId,
    },
    /// A data commit has no schema binding.
    #[error("commit {commit} is missing its schema binding")]
    MissingTrailer {
        /// The commit that lacked the trailer.
        commit: ObjectId,
    },
    /// A data commit's schema binding is not a valid object id.
    #[error("commit {commit} has an invalid schema binding {text:?}")]
    InvalidTrailer {
        /// The commit carrying the malformed trailer.
        commit: ObjectId,
        /// The trailer text that failed to parse.
        text: String,
    },
    /// A caller-supplied commit message contains a trailer reserved by the
    /// store's historical metadata format.
    #[error(
        "commit message contains reserved trailer line {trailer:?}; gix-store does not write schema or provenance trailers"
    )]
    ReservedTrailer {
        /// The reserved trailer name found at the start of a message line.
        trailer: &'static str,
    },
    /// An alias attempted to overwrite an immutable canonical entity ref.
    #[error("{name} is a canonical entity ref and cannot be used as an alias")]
    NameTaken {
        /// The ref that is reserved by another entity.
        name: RefName,
    },
    /// A canonical ref points at a document tree different from the tree its
    /// name claims. This indicates an out-of-band ref corruption or collision.
    #[error("entity id {id} points at document tree {found}, expected {expected}")]
    EntityIdCollision {
        /// The derived id encoded by the ref name.
        id: crate::EntityId,
        /// The document tree represented by the id.
        expected: ObjectId,
        /// The tree actually found at the canonical ref.
        found: ObjectId,
    },
    /// A document or tombstone belongs to a different kind than the handle
    /// used to read or recognize it.
    #[error("document kind {found:?} does not match requested kind {expected}")]
    KindMismatch {
        /// The kind selected by the caller.
        expected: RefSegment,
        /// The kind embedded in the document or tombstone.
        found: String,
    },
    /// A tombstone was found where an ordinary value was requested.
    #[error("commit {commit} is a deleted entity tombstone")]
    Deleted {
        /// The tombstone publication commit.
        commit: ObjectId,
    },
    /// The embedded schema claims to be a tombstone schema but is not the
    /// canonical tombstone schema this library understands.
    #[error("embedded tombstone schema is not the canonical gix-store tombstone schema")]
    TombstoneSchemaMismatch,
    /// A tombstone document does not carry a valid explicit deletion state.
    #[error("embedded tombstone value is invalid")]
    InvalidTombstone,
    /// A schema declares an identity- or key-bearing subtree that leaves the
    /// identity normal form's universe, so it cannot be registered: a value
    /// under it could never be given a stable identity.
    ///
    /// The source names the field path within the subtree and the schema node
    /// found there.
    #[error("kind {kind} declares an identity subtree {subtree} outside the identity normal form")]
    IdentityUniverse {
        /// The kind whose schema was refused.
        kind: RefSegment,
        /// The marked subtree's definition name.
        subtree: String,
        /// Where, and how, the subtree left the universe.
        #[source]
        source: facet_git_tree::UniverseError,
    },
    /// A schema could not be derived from a Rust type.
    #[error(transparent)]
    Schema(#[from] facet_git_tree::SchemaError),
    /// Typed serialization of a value failed.
    #[error(transparent)]
    Serialize(#[from] facet_git_tree::SerializeError),
    /// Typed deserialization of a value failed.
    #[error(transparent)]
    Deserialize(#[from] facet_git_tree::DeserializeError),
    /// Schema-directed serialization of a value failed — the value did not
    /// conform to its schema, with the offending path in the message.
    #[error(transparent)]
    SchemaWrite(#[from] facet_git_tree::SchemaWriteError),
    /// Schema-directed deserialization of a stored value failed.
    #[error(transparent)]
    SchemaRead(#[from] facet_git_tree::SchemaReadError),
    /// A schema document is incompatible with the schema definition supported by this binary.
    #[error(transparent)]
    SchemaPin(#[from] facet_git_tree::SchemaPinError),
    /// A migration document is incompatible with the migration definition supported by this binary.
    #[error(transparent)]
    MigrationPin(#[from] facet_git_tree::MigrationPinError),
    /// Applying a migration to a value failed, with the offending path in the
    /// message.
    #[error(transparent)]
    Migration(#[from] facet_git_tree::MigrationError),
    /// A value is bound to a schema that is not part of the kind's published history.
    #[error("schema tree {schema_tree} is not in the published history of kind {kind}")]
    SchemaNotInHistory {
        /// The kind whose history was searched.
        kind: RefSegment,
        /// The schema tree bound into the value's own commit.
        schema_tree: ObjectId,
    },
    /// The selected target history contains no schema commits.
    #[error("selected target schema history for kind {kind} is empty")]
    TargetHistoryEmpty {
        /// The kind whose target history was empty.
        kind: RefSegment,
    },
    /// The selected target document does not match the selected history tip.
    #[error(
        "selected target schema for kind {kind} does not match schema commit {commit} (tree {schema_tree})"
    )]
    TargetSchemaMismatch {
        /// The kind whose target was invalid.
        kind: RefSegment,
        /// The selected target schema commit.
        commit: ObjectId,
        /// The tree stored by the selected target commit.
        schema_tree: ObjectId,
    },
    /// The source schema is not present in the explicitly selected target history.
    #[error("schema tree {schema_tree} is not in the selected target history of kind {kind}")]
    TargetSchemaNotInHistory {
        /// The kind whose selected history was searched.
        kind: RefSegment,
        /// The schema tree bound into the value.
        schema_tree: ObjectId,
    },
    /// A schema update has no migration from its predecessor, so older values cannot be updated.
    #[error("schema commit {commit} of kind {kind} has no update from its predecessor")]
    MigrationMissing {
        /// The kind whose schema history holds the gap.
        kind: RefSegment,
        /// The schema commit lacking the update information.
        commit: ObjectId,
    },
    /// The configured [`Signer`](gix_refstore::Signer) could not produce
    /// signature bytes, so nothing was written.
    #[error("signing the commit failed")]
    Signer(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),

    /// Computing a kind fingerprint failed, for example because SHA-1
    /// collision detection rejected the digest.
    #[error("could not compute kind fingerprint")]
    Fingerprint(#[source] gix::hash::hasher::Error),
    /// A backend failure from the ref store or object store.
    #[error(transparent)]
    Backend(Box<dyn std::error::Error + Send + Sync + 'static>),
}

impl Error {
    /// Collapse a backend error into [`Error::Backend`], preserving it as the
    /// source.
    pub(crate) fn backend<E>(err: E) -> Self
    where
        E: Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    {
        Error::Backend(err.into())
    }
}
