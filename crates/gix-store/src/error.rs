//! The single error type for every [`Store`](crate::Store) operation.

use facet_git_tree::ObjectId;

/// An error from a [`Store`](crate::Store) operation.
///
/// The `facet-git-tree` boundary errors are kept distinct — a value that
/// parses but does not conform reports the schema path — while the many small
/// `gix` error types are collapsed into [`Error::Git`], which keeps the
/// offending error as its source.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No schema is published for the kind, so nothing can be stored under it.
    #[error("no schema published for kind {kind:?}; run `git store schema put {kind}`")]
    NoSchema {
        /// The kind with no `refs/schema/<kind>`.
        kind: String,
    },
    /// A `<kind>` or `<name>` is not usable as a Git ref-name component.
    #[error("invalid {what} {value:?}: {reason}")]
    InvalidName {
        /// Which component was rejected (`"kind"` or `"name"`).
        what: &'static str,
        /// The offending value.
        value: String,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// A data commit has no `Schema:` trailer, so its provenance — which
    /// schema commit it was validated against at write time — cannot be
    /// recovered. Not a read failure: the trailer is provenance only, and
    /// [`Store::retrieve_at`](crate::Store::retrieve_at) reads the schema from
    /// the commit's own `schema/` subtree without consulting it.
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
    /// A data commit's tree is not the `{schema/, value/}` split that
    /// [`Store::store`](crate::Store::store) writes to keep the schema
    /// reachable from the data commit itself.
    ///
    /// Overwhelmingly this means the commit predates subtree schema binding,
    /// when the commit's tree *was* the value and the schema was named only by
    /// a `Schema:` trailer. Such a commit must be re-stored to be readable.
    /// It also covers a commit written by something other than this crate.
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
    /// A data commit's `value/` or `schema/` entry names an object that is not
    /// present in this repository.
    ///
    /// The entry is there, so the commit *is* subtree-bound; the object it
    /// points at is absent. That means an incomplete transfer — a filtered or
    /// partial clone with no live promisor, a hand-built bundle, or a damaged
    /// object store. Surfaced instead of letting the lookup collapse through
    /// [`Error::Git`], which would name only a bare oid.
    ///
    /// Only the subtree root is checked. Corruption deeper inside an otherwise
    /// present subtree still surfaces as [`Error::Git`].
    #[error("commit {commit} names a {subtree} subtree {oid} that is not present")]
    SchemaObjectMissing {
        /// Which half of the split: `"value"` or `"schema"`.
        subtree: &'static str,
        /// The absent object.
        oid: ObjectId,
        /// The data commit naming it.
        commit: ObjectId,
    },
    /// A write lost its compare-and-swap race too many times in a row.
    #[error("gave up updating {refname} after {attempts} contended attempts")]
    CasExhausted {
        /// The ref that stayed contended.
        refname: String,
        /// How many attempts were made before giving up.
        attempts: u32,
    },
    /// Typed serialization of a [`SchemaDoc`](facet_git_tree::SchemaDoc) failed.
    #[error(transparent)]
    Serialize(#[from] facet_git_tree::SerializeError),
    /// Schema-directed serialization of a value failed — the value did not
    /// conform to its schema, with the offending path in the message.
    #[error(transparent)]
    SchemaWrite(#[from] facet_git_tree::SchemaWriteError),
    /// Typed deserialization of a stored `SchemaDoc` failed.
    #[error(transparent)]
    Deserialize(#[from] facet_git_tree::DeserializeError),
    /// The out-of-band pre-read of a stored schema's `version` marker failed:
    /// the tree has no `version` entry at all (a pre-versioning document,
    /// which must be re-stored), or the entry does not parse as a version
    /// number.
    ///
    /// Reported by [`Store::read_schema`](crate::Store) *before* any attempt
    /// to deserialize the rest of the document — see
    /// [`SchemaDoc::read_stored_version`](facet_git_tree::SchemaDoc::read_stored_version)
    /// for why that ordering matters.
    #[error(transparent)]
    SchemaVersion(#[from] facet_git_tree::SchemaVersionError),
    /// A stored schema declares a `version` newer than this binary
    /// understands ([`SchemaDoc::CURRENT_VERSION`](facet_git_tree::SchemaDoc::CURRENT_VERSION)).
    ///
    /// Caught by the out-of-band pre-read, before a full deserialize is even
    /// attempted, so this fires instead of an opaque reflection error on a
    /// document containing a `Schema` variant this binary has never heard of.
    #[error(
        "schema tree {oid} declares version {found}, but this binary only understands up to \
         version {supported} — upgrade to read it"
    )]
    SchemaVersionTooNew {
        /// The schema tree that declared the unsupported version.
        oid: ObjectId,
        /// The version it declared.
        found: u32,
        /// The highest version this binary understands.
        supported: u32,
    },
    /// A caller asked to publish a schema declaring a `version` newer than
    /// this binary writes.
    ///
    /// [`Store::put_schema`](crate::Store) always stamps a published document
    /// with [`SchemaDoc::CURRENT_VERSION`](facet_git_tree::SchemaDoc::CURRENT_VERSION)
    /// once it accepts it, so this can only mean the caller explicitly
    /// declared a version this binary does not know how to write — publishing
    /// it anyway would claim a codec guarantee this binary cannot keep.
    #[error(
        "cannot publish schema for kind {kind:?}: it declares version {found}, but this binary \
         only writes up to version {supported}"
    )]
    SchemaVersionUnsupported {
        /// The kind the schema was published for.
        kind: String,
        /// The version the document declared.
        found: u32,
        /// The highest version this binary writes.
        supported: u32,
    },
    /// Schema-directed deserialization of a stored value failed.
    #[error(transparent)]
    SchemaRead(#[from] facet_git_tree::SchemaReadError),
    /// Any underlying `gix` failure — object, reference, or commit.
    #[error(transparent)]
    Git(Box<dyn std::error::Error + Send + Sync + 'static>),
}

impl Error {
    /// Collapse a `gix` error into [`Error::Git`], preserving it as the source.
    pub(crate) fn git<E>(err: E) -> Self
    where
        E: Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    {
        Error::Git(err.into())
    }
}
