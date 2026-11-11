//! The single error type for every [`Store`](crate::Store) operation.

use facet_git_tree::ObjectId;
use gix_refstore::RefSegment;

/// An error from a [`Store`](crate::Store) operation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No schema is published for the kind, so publish one before storing values.
    #[error("no schema published for kind \"{kind}\"; publish a schema before storing values")]
    NoSchema {
        /// The kind with no published schema.
        kind: RefSegment,
    },
    /// The `.schema` document envelope refused a tree.
    ///
    /// The envelope codec and its errors live in `facet_git_tree::document`,
    /// which is a different type from this crate's own
    /// [`DocumentError`](crate::DocumentError) — the builder-and-classifier
    /// error over the same shapes. A read that reaches a non-envelope tree, or
    /// a defective envelope, surfaces the format crate's error here.
    #[error(transparent)]
    Document(#[from] facet_git_tree::DocumentError),
    /// A deletion named a ref whose tip is not a bound document this kind can
    /// tombstone, so no content-derived [`EntityId`](crate::EntityId) can be
    /// recorded for it.
    ///
    /// A tombstone records "the `EntityId` the ref addressed when it was
    /// deleted" (`docs/specification.adoc`, `plumbing.tombstone`), and an
    /// `EntityId` is the object id of a complete bound document tree — a
    /// value's own entries beside the reserved `.schema` entry. A pre-binding
    /// ("legacy") tip, whose tree is a bare value tree with no `.schema`, has
    /// no such id: deriving one from its value tree would name an object no
    /// bound document occupies, a phantom no reader could resolve. Deletion
    /// refuses rather than record it, and the wrapped `source` says why the tip
    /// did not resolve; the tip and its history stay exactly where they are.
    /// Re-publish the tip under the current envelope, or prune the ref outright
    /// with [`Kind::remove`](crate::Kind::remove), which writes no tombstone.
    #[error("commit {commit} is not a tombstonable bound document")]
    NotDeletable {
        /// The tip commit whose deletion was refused.
        commit: ObjectId,
        /// Why the tip did not resolve as a bound document: the shape or
        /// decode failure that left it without an [`EntityId`](crate::EntityId).
        #[source]
        source: Box<Error>,
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
    /// A canonical commit's author or committer identity carries bytes that
    /// would re-frame the commit header, so the written object would not mean
    /// what the caller declared.
    ///
    /// A newline in a name or email ends the identity's header line early and
    /// starts another, so it can fabricate a whole `committer`, `gpgsig`, or
    /// other header; an angle bracket shifts the `name <email>` boundaries a
    /// reader parses; an empty field names nobody; whitespace in an email is
    /// not part of an identity. Git's own signature writer refuses the same
    /// bytes; the canonical writer bypasses it by formatting the header by
    /// hand.
    #[error("canonical commit {field} is invalid: {reason}")]
    InvalidIdentity {
        /// Which identity field, for example `"author name"` or
        /// `"committer email"`.
        field: &'static str,
        /// Why it is invalid: empty, a control character, an angle bracket, or
        /// whitespace in an email.
        reason: &'static str,
    },
    /// A ref write kept losing its compare-and-swap to concurrent writers and
    /// exhausted its bounded retry budget, so it gave up rather than retrying
    /// forever. Distinguished from [`Error::Backend`] so a caller can tell
    /// "someone else keeps winning, try again later" from a genuine failure.
    #[error("ref store contended: lost the compare-and-swap {attempts} times in a row")]
    Contention {
        /// The number of compare-and-swap attempts made before giving up.
        attempts: u32,
    },
    /// [`Transaction::alias`](crate::Transaction::alias) was called when the
    /// last operation staged by the [`Transaction`](crate::Transaction) is not
    /// a [`publish`](crate::Transaction::publish), so there is no publication
    /// to redirect. `alias` must immediately follow the `publish` it renames.
    #[error("Transaction::alias must immediately follow Transaction::publish")]
    AliasWithoutPublish,
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
