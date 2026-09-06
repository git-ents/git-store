//! Errors produced by [`ProllyStore`](crate::ProllyStore) operations.

use gix::ObjectId;

/// An error from an insert, lookup, iteration, diff, or build operation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An underlying Git object-database operation failed.
    #[error("git object operation failed: {0}")]
    Git(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
    /// A referenced object is absent from the repository's object database.
    #[error("object {oid} not found in the repository")]
    ObjectNotFound { oid: ObjectId },
    /// A node expected to be a tree referenced a different object kind.
    #[error("object {oid} is not a tree")]
    NotATree { oid: ObjectId },
    /// A node expected to be a blob referenced a different object kind.
    #[error("object {oid} is not a blob")]
    NotABlob { oid: ObjectId },
    /// A value object is neither a tree nor a blob, so it cannot back a leaf entry.
    #[error("object {oid} has kind {kind}, which cannot back a leaf entry")]
    UnexpectedObjectKind { oid: ObjectId, kind: &'static str },
    /// A key could not be encoded or decoded.
    #[error("invalid key: {0}")]
    Key(#[from] KeyError),
    /// A configuration is not usable.
    #[error("invalid configuration: {0}")]
    Config(#[from] ConfigError),
    /// Facet tree serialization failed.
    #[error("facet tree serialization failed: {0}")]
    Serialize(#[source] facet_git_tree::SerializeError),
    /// Facet tree deserialization failed.
    #[error("facet tree deserialization failed: {0}")]
    Deserialize(#[source] facet_git_tree::DeserializeError),
    /// A batch build supplied the same logical key twice.
    #[error("duplicate key {}", String::from_utf8_lossy(.0))]
    DuplicateKey(Vec<u8>),
    /// A removal targeted a key the tree does not contain.
    #[error("key {} not found", String::from_utf8_lossy(.0))]
    KeyNotFound(Vec<u8>),
    /// A value could not be encoded to, or decoded from, its JSON text leaf.
    #[error("value JSON codec failed: {0}")]
    ValueJson(String),
    /// A structure descended through more levels than the format allows.
    ///
    /// Valid trees are bounded by construction; this bound keeps a hostile or
    /// corrupt object graph from exhausting the stack during a walk.
    #[error("tree rooted at {root} exceeds the maximum of {max} levels")]
    TooDeep { root: ObjectId, max: usize },
}

impl Error {
    /// Convert an error into the crate's backend error variant.
    pub(crate) fn git<E>(error: E) -> Self
    where
        E: Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    {
        Self::Git(error.into())
    }
}

/// A key could not be encoded or decoded.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    /// Empty keys are unrepresentable: Git tree entry names must be non-empty.
    #[error("empty keys are not representable as Git tree entry names")]
    Empty,
    /// The encoded key would exceed Git's tree-entry name limit.
    #[error(
        "key of {len} bytes encodes to {encoded} bytes, above the {max}-byte tree-entry name limit"
    )]
    TooLong {
        /// The raw key length in bytes.
        len: usize,
        /// The encoded length in bytes.
        encoded: usize,
        /// The maximum accepted encoded length.
        max: usize,
    },
    /// An encoded key is not a valid product of the codec.
    #[error("encoded key {0:?} is not valid for this codec")]
    InvalidEncoding(Vec<u8>),
    /// An encoded key has a length the codec can never have produced.
    #[error("encoded key has odd length {0}")]
    OddLength(usize),
}

/// A [`ProllyConfig`](crate::ProllyConfig) is not usable.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The chunk-boundary bit mask would exceed the 64-bit fingerprint.
    #[error("chunk bits {0} exceed the 31-bit fingerprint mask")]
    BitsTooLarge(u8),
    /// Chunks must be allowed at least one entry.
    #[error("min_entries {0} must be at least 1")]
    MinTooSmall(usize),
    /// The maximum chunk size may not undercut the minimum.
    #[error("max_entries {max} must be at least min_entries {min}")]
    MaxBelowMin {
        /// The configured maximum.
        max: usize,
        /// The configured minimum.
        min: usize,
    },
}
