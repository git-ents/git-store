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
    /// No schema is published for the kind, so nothing can be stored under it.
    #[error("no schema published for kind \"{kind}\"; run `git store schema put {kind}`")]
    NoSchema {
        /// The kind with no published schema.
        kind: RefSegment,
    },
    /// A data commit's tree is not the `{schema/, value/}` split a write
    /// produces.
    #[error(
        "commit {commit} is not subtree-bound: its tree has [{found}], expected `schema` and \
         `value` — it predates subtree schema binding and must be re-stored"
    )]
    NotSubtreeBound {
        /// The data commit whose tree was not the expected split.
        commit: ObjectId,
        /// The entry names actually found, comma-separated, for diagnosis.
        found: String,
    },
    /// A data commit's `value/` or `schema/` entry names an object that is
    /// not present in this repository.
    #[error("commit {commit} names a {subtree} subtree {oid} that is not present")]
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
    /// A data commit has no `Schema:` trailer.
    #[error("commit {commit} is missing its Schema: trailer")]
    MissingTrailer {
        /// The commit that lacked the trailer.
        commit: ObjectId,
    },
    /// A data commit's `Schema:` trailer is not a valid object id.
    #[error("commit {commit} has an invalid Schema: trailer {text:?}")]
    InvalidTrailer {
        /// The commit carrying the malformed trailer.
        commit: ObjectId,
        /// The trailer text that failed to parse.
        text: String,
    },
    /// An anonymous entity's derived name collided with a live ref that does
    /// not already hold the commit just written.
    #[error("{name} already exists and points elsewhere")]
    NameTaken {
        /// The ref that was already taken.
        name: RefName,
    },
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
    /// The schema-schema pin failed: a document was pinned to (or, on
    /// publish, would overwrite a tip pinned to) a schema-schema this binary
    /// does not recognize, or a document carries no pin and is not itself a
    /// known root.
    #[error(transparent)]
    SchemaPin(#[from] facet_git_tree::SchemaPinError),
    /// The migration-schema pin failed, on the same terms as
    /// [`SchemaPin`](Self::SchemaPin) but for a stored migration document.
    #[error(transparent)]
    MigrationPin(#[from] facet_git_tree::MigrationPinError),
    /// Applying a migration to a value failed, with the offending path in the
    /// message.
    #[error(transparent)]
    Migration(#[from] facet_git_tree::MigrationError),
    /// A value's bound schema tree is not one this kind's schema ref ever
    /// published, so no chain of migrations reaches the current schema.
    #[error("schema tree {schema_tree} is not in the published history of kind {kind}")]
    SchemaNotInHistory {
        /// The kind whose history was searched.
        kind: RefSegment,
        /// The schema tree bound into the value's own commit.
        schema_tree: ObjectId,
    },
    /// A schema commit advanced over a predecessor without recording a
    /// migration, so values written under the predecessor cannot be upcast.
    #[error("schema commit {commit} of kind {kind} records no migration off its parent")]
    MigrationMissing {
        /// The kind whose schema history holds the gap.
        kind: RefSegment,
        /// The schema commit lacking a `migration` entry.
        commit: ObjectId,
    },
    /// The configured [`Signer`](gix_refstore::Signer) could not produce
    /// signature bytes, so nothing was written.
    #[error("signing the commit failed")]
    Signer(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),

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
