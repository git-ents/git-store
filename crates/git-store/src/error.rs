//! The single error type for every [`Store`](crate::Store) operation.

use std::fmt;

use facet_git_tree::ObjectId;
use gix_refstore::{RefName, RefSegment};

/// One of a document's two logical halves: the schema entry or the value it
/// describes.
///
/// A document root stores the value's own entries directly, so the value half
/// has no entry of its own to name; [`Subtree::Value`] carries that half's
/// reserved name, which this format never writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subtree {
    /// The value half: the entries beside `.schema`.
    Value,
    /// The `.schema` entry: the unbound schema the value conforms to.
    Schema,
}

impl Subtree {
    /// The reserved tree entry name: `".schema"`, or `".value"` for the value
    /// half, which this format reserves but never writes.
    pub const fn as_str(self) -> &'static str {
        match self {
            Subtree::Value => ".value",
            Subtree::Schema => ".schema",
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
    /// A data commit's tree is not a bound document.
    #[error("commit {commit} has no `.schema` entry beside its value entries (found [{found}])")]
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
    /// A caller-supplied commit message contains a line that would recreate a
    /// reserved schema or provenance trailer.
    ///
    /// Trailers bind nothing: the normative schema is the document's embedded
    /// `.schema` entry, so a trailer line in a message is rejected rather than
    /// read as metadata.
    #[error(
        "commit message contains reserved trailer line {trailer:?}; git-store writes no schema or provenance trailers"
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
    /// The configured [`Signer`](gix_refstore::Signer) could not produce
    /// signature bytes, so nothing was written.
    #[error("signing the commit failed")]
    Signer(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),

    /// Computing a kind fingerprint failed, for example because SHA-1
    /// collision detection rejected the digest.
    #[error("could not compute kind fingerprint")]
    Fingerprint(#[source] gix::hash::hasher::Error),
    /// The repository uses an object format this build's plumbing cannot
    /// write.
    ///
    /// This build's `gix` is compiled without its `sha256` feature, so it
    /// writes SHA-1 only: a SHA-256 repository is refused, whether `gix`
    /// reports that format or silently reports it as SHA-1. The codec itself,
    /// and the `doctor` fixed point, cover both formats.
    #[error("repository object format {observed} is not supported by this build")]
    UnsupportedObjectFormat {
        /// The object hash algorithm actually observed, e.g. `"sha256"`.
        observed: String,
    },
    /// A document's schema root does not resolve to a struct.
    ///
    /// Only a struct root has named entries that can sit beside `.schema`, so
    /// any other resolved root describes a bare value and cannot be bound as
    /// a document.
    #[error(
        "schema root for kind \"{kind}\" is {node}, not a struct; only struct schemas form documents"
    )]
    DocumentRootNotStruct {
        /// The kind whose schema was rejected.
        kind: String,
        /// The keyword of the node the schema root resolved to, e.g. `list`.
        node: &'static str,
    },
    /// The schema-of-schemas fixed-point check failed.
    #[error(transparent)]
    Doctor(#[from] facet_git_tree::doctor::DoctorError),
    /// A canonical commit named the same parent more than once; `git fsck`
    /// rejects the result as a duplicateParent.
    #[error("commit names parent {parent} more than once")]
    DuplicateParent {
        /// The repeated parent.
        parent: ObjectId,
    },
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
