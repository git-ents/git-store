//! Error types, one per operation: [`KeyError`] for key validation,
//! [`SerializeError`] for the write side, and [`DeserializeError`] for the
//! read side.

use gix_hash::ObjectId;

/// A user-supplied key cannot be used as a Git tree entry name.
///
/// Tree entry names double as path segments, so a key may not contain the
/// path separator `/`. Returned by [`check_key`](crate::check_key) and carried
/// by [`SerializeError::Key`] when serialization rejects a dynamic (map) key.
#[derive(Debug, thiserror::Error)]
#[error("invalid key {key:?}: must not contain '/'")]
pub struct KeyError {
    /// The offending key.
    pub key: String,
}

/// An error produced by serialization ([`serialize`](crate::serialize) and
/// friends).
#[derive(Debug, thiserror::Error)]
pub enum SerializeError {
    /// A facet key cannot be represented as a Git tree entry name.
    #[error(transparent)]
    Key(#[from] KeyError),
    /// An error from the underlying `gix` object backend.
    ///
    /// Wraps the backend's own error (from [`Write`](gix_object::Write)) as
    /// the source rather than flattening it into a string.
    #[error("git object backend error")]
    Backend(#[source] gix_object::write::Error),
    /// A `facet` reflection operation failed.
    ///
    /// `facet`'s reflection errors borrow from the reflected shape and are not
    /// `'static`-friendly, so they are collapsed to text at this boundary.
    #[error("reflection error: {0}")]
    Reflect(String),
    /// A map key's textual form is not valid UTF-8, so it cannot become a Git
    /// tree entry name.
    #[error("map key is not valid UTF-8")]
    NonUtf8MapKey,
    /// The value contains a type this encoding does not support.
    ///
    /// Holds the type identifier of the unsupported shape.
    #[error("unsupported type for serialization: {0}")]
    Unsupported(&'static str),
    /// The value contains a scalar type this encoding does not support.
    ///
    /// Holds the type identifier of the unsupported scalar.
    #[error("unsupported scalar type: {0}")]
    UnsupportedScalar(&'static str),
}

/// An error produced by deserialization ([`deserialize`](crate::deserialize)
/// and friends).
#[derive(Debug, thiserror::Error)]
pub enum DeserializeError {
    /// A referenced object was not present in its backing store.
    #[error("object {0} not found")]
    NotFound(ObjectId),
    /// An object was expected to be a tree but was of another kind.
    #[error("object {0} is not a tree")]
    NotATree(ObjectId),
    /// An object was expected to be a blob (a scalar leaf) but was of another
    /// kind.
    #[error("object {0} is not a blob")]
    NotABlob(ObjectId),
    /// A tree entry name (path segment) is not valid UTF-8.
    ///
    /// Holds the lossily-decoded name for diagnostics. Write-side names are
    /// always UTF-8, so this can only arise from an externally-produced tree.
    #[error("tree entry name {0:?} is not valid UTF-8")]
    NonUtf8Name(String),
    /// A scalar blob's contents are not valid UTF-8, so no scalar can be
    /// parsed from them.
    #[error("blob {0} is not valid UTF-8")]
    NonUtf8Blob(ObjectId),
    /// Deserialization exceeded the maximum supported nesting depth.
    ///
    /// A guard against unbounded recursion — and thus stack overflow — when
    /// reading a deeply nested, possibly externally-produced tree. The bundled
    /// encoder never approaches this depth for ordinary values.
    #[error("maximum nesting depth ({0}) exceeded while deserializing")]
    MaxDepth(usize),
    /// A sequence entry name is not a valid decimal ordinal.
    ///
    /// Sequence (`Vec`/array) entries are named by their zero-based decimal
    /// index on write, so a non-numeric name can only arise from an
    /// externally-produced tree.
    #[error("invalid sequence ordinal {0:?}")]
    InvalidOrdinal(String),
    /// An error from the underlying `gix` object backend.
    ///
    /// Wraps the backend's own error (from [`Find`](gix_object::Find)) as the
    /// source rather than flattening it into a string.
    #[error("git object backend error")]
    Backend(#[source] gix_object::find::Error),
    /// A stored tree object's bytes could not be decoded as a Git tree.
    #[error("failed to decode tree {oid}")]
    Decode {
        /// The id of the undecodable object.
        oid: ObjectId,
        /// The underlying `gix` decode error.
        #[source]
        source: gix_object::decode::Error,
    },
    /// A `facet` reflection operation failed.
    ///
    /// `facet`'s reflection errors borrow from the reflected shape and are not
    /// `'static`-friendly, so they are collapsed to text at this boundary.
    #[error("reflection error: {0}")]
    Reflect(String),
    /// A scalar blob's text failed to parse as the target type.
    #[error("cannot parse {text:?} as {shape}: {reason}")]
    Parse {
        /// The type identifier of the target scalar shape.
        shape: &'static str,
        /// The text that failed to parse.
        text: String,
        /// The parse failure, collapsed to text (`facet`'s reflection errors
        /// are not `'static`-friendly).
        reason: String,
    },
    /// An `Option` tree holds more than the single `some` entry.
    ///
    /// `Some` is written as exactly one entry named `some` and `None` as an
    /// empty tree, so any other arity is a malformed (necessarily foreign)
    /// tree.
    #[error("malformed Option tree: expected a single \"some\" entry, found {found} entries")]
    MalformedOption {
        /// How many entries the tree actually holds.
        found: usize,
    },
    /// An `Option` tree's single entry is not named `some`.
    #[error("malformed Option tree: entry must be named \"some\", found {name:?}")]
    MislabeledOption {
        /// The entry name actually found.
        name: String,
    },
    /// An enum tree does not hold exactly one (variant-named) entry.
    #[error("malformed enum tree: expected exactly one entry, found {found}")]
    MalformedEnum {
        /// How many entries the tree actually holds.
        found: usize,
    },
    /// A composite-key map pair sub-tree is missing its `k` or `v` entry.
    #[error("map pair sub-tree missing {entry:?} entry")]
    MissingMapPairEntry {
        /// The missing entry name (`"k"` or `"v"`).
        entry: &'static str,
    },
    /// The target type is not supported by this encoding.
    ///
    /// Holds the type identifier of the unsupported shape.
    #[error("unsupported type for deserialization: {0}")]
    Unsupported(&'static str),
}
