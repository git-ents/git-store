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
    /// A data commit has no `Schema:` trailer, so its tree cannot be read back.
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
