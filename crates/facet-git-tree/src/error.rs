//! Error types, one per operation: [`KeyError`] for key validation,
//! [`SerializeError`] for the write side, [`DeserializeError`] for the
//! read side, [`SchemaError`] for schema generation, [`SchemaPinError`] for
//! the schema-schema pin, [`SchemaReadError`] for schema-driven reads, and
//! [`SchemaWriteError`] for schema-directed writes.

use gix_hash::ObjectId;

/// A user-supplied key cannot be used as a Git tree entry name.
///
/// Tree entry names double as path segments, so a key may not contain the
/// path separator `/`; nor may it equal the reserved presence-marker name
/// (`crate::marker::MARKER_KEY`, `"_"`) written in place of a literal empty
/// tree for `None`, `Null`, and an empty collection — a real entry named
/// exactly that would otherwise be indistinguishable, on read, from the
/// marker. Returned by [`check_key`](crate::check_key) and carried by
/// [`SerializeError::Key`] when serialization rejects a dynamic (map or
/// dynamic-object) key.
#[derive(Debug, thiserror::Error)]
#[error("invalid key {key:?}: must not contain '/' and must not equal the reserved marker \"_\"")]
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
    /// A dynamic value holds a number with no exact textual rendering.
    ///
    /// The generic dynamic-value vtable only surfaces 64-bit reads, so an
    /// integer beyond the 64-bit range can only be observed as a lossy `f64`
    /// approximation. Writing that approximation would silently change the
    /// value — and therefore its object id — so it is refused instead. The
    /// `value` cargo feature adds a `facet_value::Value` fast path that
    /// renders integers exactly at any width.
    #[error("dynamic number has no exact textual rendering")]
    UnrepresentableNumber,
    /// A dynamic value's runtime kind is not supported by this encoding.
    ///
    /// Holds the kind's name. Produced for kinds with no generic textual
    /// form (QName and UUID without the `value` feature) and for kinds this
    /// crate does not know (`DynValueKind` is `#[non_exhaustive]`); refusing
    /// them is preferred over guessing an encoding.
    #[error("unsupported dynamic value kind: {0}")]
    UnsupportedDynamicKind(String),
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
    /// A leaf blob's final byte is not `\n`.
    ///
    /// Every leaf blob (a scalar, a byte sequence, or a unit enum variant's
    /// name blob) MUST carry exactly one trailing newline
    /// (`serialization.design.leaves.encoding`); this is *not* "at most one",
    /// so a leaf blob missing that byte can only be a foreign or corrupt
    /// object, rejected here rather than accepted leniently. The presence
    /// marker (`crate::marker`) is a separate, structural object and is never
    /// checked against this rule.
    #[error(
        "leaf blob {0} is missing its mandatory trailing newline — it predates \
         the trailing-newline leaf encoding and must be re-stored"
    )]
    MissingLeafNewline(ObjectId),
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
    /// Two sequence entries name the same numeric ordinal (e.g. `"0"` and
    /// `"0000"`).
    ///
    /// Each element must occupy a distinct index; a foreign tree with two
    /// entries naming the same index leaves the element order ambiguous, so
    /// it is rejected rather than silently resolved by insertion or lexical
    /// order.
    #[error("duplicate sequence ordinal {0}: two entries name the same index")]
    DuplicateOrdinal(usize),
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
    /// An `Option` tree does not hold exactly the shape one of its two valid
    /// forms requires.
    ///
    /// `Some` is written as exactly one entry named `some`, and `None` as the
    /// marker tree (`crate::marker`) — never a literal empty tree — so any
    /// other arity (including a literal empty tree, with `found: 0`) is a
    /// malformed (necessarily foreign) tree.
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
    /// A non-unit enum variant's tag object is a tree but does not hold
    /// exactly one (variant-named) entry.
    #[error("malformed enum tree: expected exactly one entry, found {found}")]
    MalformedEnum {
        /// How many entries the tree actually holds.
        found: usize,
    },
    /// A unit enum variant's tag object was a tree instead of the blob its
    /// payload-free encoding requires.
    ///
    /// A unit variant tags with a bare blob holding the variant name (so it
    /// appears as ordinary content to git's blob-oriented diff and ls-tree
    /// tooling); a tree there can only come from a foreign encoder or a stale
    /// pre-blob-collapse object.
    #[error("enum variant {variant:?} is unit but its tag object is a tree, not a blob")]
    UnitVariantIsTree {
        /// The variant name.
        variant: String,
    },
    /// A non-unit enum variant's tag object was a blob instead of the tree
    /// its payload requires.
    #[error("enum variant {variant:?} has a payload and must be a tree, found a blob")]
    VariantPayloadIsBlob {
        /// The variant name.
        variant: String,
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

/// An error produced by schema generation ([`schema_of`](crate::schema_of) and
/// [`Schema::from_shape`](crate::Schema::from_shape)).
#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    /// The shape contains a scalar type this encoding does not support.
    ///
    /// Holds the type identifier of the unsupported scalar. Mirrors
    /// [`SerializeError::UnsupportedScalar`]: a shape that cannot be encoded
    /// cannot be described by a schema either.
    #[error("unsupported scalar type in schema: {0}")]
    UnsupportedScalar(&'static str),
    /// The shape contains a type this encoding does not support.
    ///
    /// Holds the type identifier of the unsupported shape. Mirrors
    /// [`SerializeError::Unsupported`].
    #[error("unsupported type in schema: {0}")]
    UnsupportedShape(&'static str),
    /// A smart pointer shape carries no pointee shape to collapse to.
    ///
    /// Holds the pointer type's identifier. Transparency collapse resolves a
    /// pointer to its pointee's schema; a pointer without one (an opaque
    /// pointer shape) has no schema.
    #[error("smart pointer {0} has no pointee shape")]
    MissingPointee(&'static str),
    /// Schema generation exceeded the maximum supported nesting depth.
    ///
    /// Mirrors [`DeserializeError::MaxDepth`]: data nested deeper than the
    /// limit could never be read back regardless, so describing it is
    /// refused rather than recursing unboundedly.
    #[error("maximum nesting depth ({0}) exceeded while generating schema")]
    MaxDepth(usize),
}

/// An error produced by the schema-schema pin
/// ([`Schema::write_pinned`](crate::Schema::write_pinned) and
/// [`Schema::read_pinned`](crate::Schema::read_pinned)/[`read_pin`](crate::Schema::read_pin)).
#[derive(Debug, thiserror::Error)]
pub enum SchemaPinError {
    /// The document pins a schema-schema this build does not speak.
    ///
    /// An oid pin gives equality only, never ordering, so this cannot
    /// distinguish an older generation from a newer one — it can only say
    /// which generations this build recognizes.
    #[error(
        "schema tree {tree} was written against schema-schema {pinned}, which this build does \
         not recognize; it speaks {}",
        crate::schema::pin::known_generations()
    )]
    Unrecognized {
        /// The schema tree carrying the unrecognized pin.
        tree: ObjectId,
        /// The pinned schema-schema tree id.
        pinned: ObjectId,
    },
    /// The document carries no `schema` pin entry, and is not itself a known
    /// schema-schema root.
    ///
    /// A truncated or hand-written document must be rejected here, not read
    /// as though it were the genesis generation.
    #[error("schema tree {0} carries no schema-schema pin and is not itself a known root")]
    Unpinned(ObjectId),
    /// Writing the document, or the schema-schema tree it pins, failed.
    #[error(transparent)]
    Serialize(#[from] SerializeError),
    /// Reading the pin entry, or the document itself, failed exactly as an
    /// ordinary typed deserialize would.
    #[error(transparent)]
    Deserialize(#[from] DeserializeError),
}

/// An error produced by the migration-schema pin
/// ([`Migration::write_pinned`](crate::Migration::write_pinned) and
/// [`Migration::read_pinned`](crate::Migration::read_pinned)/[`read_pin`](crate::Migration::read_pin)).
///
/// The migration tower is separate from the schema-schema tower, so this is a
/// separate error: a build may speak one generation of `Schema` and a
/// different generation of `Migration`.
#[derive(Debug, thiserror::Error)]
pub enum MigrationPinError {
    /// The migration pins a migration-schema this build does not speak.
    ///
    /// Refusing here is what stops an unrecognized operator from being
    /// silently skipped, which would not fail the read — it would produce a
    /// value that looks conformant and is not.
    #[error(
        "migration tree {tree} was written against migration-schema {pinned}, which this build \
         does not recognize; it speaks {}",
        crate::migration::pin::known_generations()
    )]
    Unrecognized {
        /// The migration tree carrying the unrecognized pin.
        tree: ObjectId,
        /// The pinned migration-schema tree id.
        pinned: ObjectId,
    },
    /// The migration carries no `schema` pin entry, and is not itself a known
    /// migration-schema root.
    #[error("migration tree {0} carries no migration-schema pin and is not itself a known root")]
    Unpinned(ObjectId),
    /// Writing the migration, or the migration-schema tree it pins, failed.
    #[error(transparent)]
    Serialize(#[from] SerializeError),
    /// Reading the pin entry, or the migration itself, failed exactly as an
    /// ordinary typed deserialize would.
    #[error(transparent)]
    Deserialize(#[from] DeserializeError),
}

/// An error produced by schema-driven deserialization
/// (`deserialize_value_with_schema` and `validate_with_schema`, available with
/// the `value` feature).
#[derive(Debug, thiserror::Error)]
pub enum SchemaReadError {
    /// The underlying tree walk failed exactly as a typed read would.
    ///
    /// Covers missing objects, malformed trees, depth exhaustion, and every
    /// other condition [`DeserializeError`] describes.
    #[error(transparent)]
    Deserialize(#[from] DeserializeError),
    /// A `Node::Ref` names a definition absent from the document's `defs`
    /// table.
    #[error("schema ref {0:?} has no definition in the document")]
    UnknownRef(String),
    /// An enum tree's variant name is not present in the schema.
    #[error("unknown enum variant {variant:?}; schema defines {expected:?}")]
    UnknownVariant {
        /// The variant name found in the tree.
        variant: String,
        /// The variant names the schema defines.
        expected: Vec<String>,
    },
    /// A fixed-length sequence's entry count does not match the schema.
    ///
    /// Produced for `Node::Array` (whose `len` is part of the schema) and
    /// `Node::Tuple` (whose element count is).
    #[error("sequence length mismatch: schema expects {expected} elements, tree holds {found}")]
    ArrayLenMismatch {
        /// The element count the schema requires.
        expected: usize,
        /// The entry count the tree actually holds.
        found: usize,
    },
    /// A scalar blob's text does not parse as the schema's scalar type.
    #[error("cannot parse {text:?} as schema scalar {schema}")]
    InvalidScalar {
        /// The name of the scalar schema node (e.g. `I8`, `Bool`).
        schema: &'static str,
        /// The text that failed to parse.
        text: String,
    },
    /// A tree that the schema requires to be empty (a `Unit` value or a unit
    /// enum variant payload) holds entries.
    #[error("malformed unit tree: expected no entries, found {found}")]
    MalformedUnit {
        /// How many entries the tree actually holds.
        found: usize,
    },
    /// A struct tree lacks an entry for a field the schema defines.
    ///
    /// An `Optional` field is still a present entry — the presence marker
    /// encodes `None` — so an absent entry always means the tree does not
    /// describe this schema, never that the field is simply unset.
    #[error("struct field {field:?} is missing from the tree")]
    MissingField {
        /// The field the schema defines and the tree omits.
        field: String,
    },
    /// A struct tree carries an entry the schema does not define.
    #[error("tree entry {entry:?} has no counterpart in the schema")]
    UnexpectedEntry {
        /// The entry name found in the tree.
        entry: String,
    },
}

/// An error produced by schema-directed serialization
/// (`serialize_value_with_schema`, available with the `value` feature).
///
/// The write-side mirror of [`SchemaReadError`]: it validates a dynamic value
/// against a schema *while* encoding it, so every variant beyond the backend
/// pass-through names the `path` in the value where the value diverged from
/// what the schema accepts. The accepted set is exactly the image of
/// [`deserialize_value_with_schema`](crate::deserialize_value_with_schema),
/// plus the deterministic bridges a JSON-authored value needs (an integer into
/// a float field; a string into a `Bytes` field).
#[derive(Debug, thiserror::Error)]
pub enum SchemaWriteError {
    /// The underlying object write, key validation, or `Dynamic`-node
    /// encoding failed exactly as an ordinary serialization would.
    #[error(transparent)]
    Serialize(#[from] SerializeError),
    /// The value's runtime kind does not match the schema node.
    #[error("at {path}: expected {expected}, found {found}")]
    Expected {
        /// The location within the value.
        path: String,
        /// The kind (or kinds) the schema node accepts.
        expected: &'static str,
        /// The value's actual runtime kind.
        found: &'static str,
    },
    /// A number does not fit the schema's integer type.
    #[error("at {path}: number {value} out of range for {schema}")]
    NumberOutOfRange {
        /// The location within the value.
        path: String,
        /// The integer schema node's name (e.g. `U8`).
        schema: &'static str,
        /// The offending number's textual form.
        value: String,
    },
    /// An integer has no exact representation in the schema's float type.
    ///
    /// The float-field bridge is lossless by design: an integer that cannot be
    /// represented exactly at the target width is refused rather than rounded,
    /// the same posture [`SerializeError::UnrepresentableNumber`] takes.
    #[error("at {path}: number has no exact {schema} representation")]
    UnrepresentableNumber {
        /// The location within the value.
        path: String,
        /// The float schema node's name (`F32` or `F64`).
        schema: &'static str,
    },
    /// An object holds a key the struct schema does not define.
    ///
    /// The accepted set is exactly the image of the schema-driven read, which
    /// only ever emits the schema's own fields, so an extra key is rejected
    /// rather than silently dropped.
    #[error("at {path}: unknown field {field:?}")]
    UnknownField {
        /// The location of the object.
        path: String,
        /// The offending key.
        field: String,
    },
    /// A fixed-length sequence (`Tuple` or `Array`) has the wrong element
    /// count.
    #[error("at {path}: expected {expected} elements, found {found}")]
    LengthMismatch {
        /// The location of the sequence.
        path: String,
        /// The element count the schema requires.
        expected: usize,
        /// The element count the value holds.
        found: usize,
    },
    /// An enum value is not a single-member tagged object.
    #[error("at {path}: enum must be a single-member object, found {found} members")]
    MalformedEnum {
        /// The location of the value.
        path: String,
        /// How many members the object holds.
        found: usize,
    },
    /// An enum variant name is not present in the schema.
    #[error("at {path}: unknown variant {variant:?}; schema defines {expected:?}")]
    UnknownVariant {
        /// The location of the value.
        path: String,
        /// The variant name found.
        variant: String,
        /// The variant names the schema defines.
        expected: Vec<String>,
    },
    /// A `RawTree` value is not a 40-character lowercase-hex object id.
    #[error("at {path}: invalid raw tree object id {text:?}")]
    InvalidRawTree {
        /// The location of the value.
        path: String,
        /// The offending text.
        text: String,
    },
    /// A `Ref` names a definition absent from the document's `defs` table.
    #[error("at {path}: schema ref {name:?} has no definition in the document")]
    UnknownRef {
        /// The location of the value.
        path: String,
        /// The undefined reference name.
        name: String,
    },
    /// Serialization exceeded the maximum supported nesting depth.
    ///
    /// Mirrors [`DeserializeError::MaxDepth`]: since every hop — including
    /// `Ref` resolution — counts against the limit, a `Ref`-to-`Ref` cycle in
    /// the schema fails here rather than recursing unboundedly.
    #[error("at {path}: maximum nesting depth ({depth}) exceeded while serializing")]
    MaxDepth {
        /// The location reached when the limit tripped.
        path: String,
        /// The limit that was exceeded.
        depth: usize,
    },
}

/// An error produced by read-time migration application
/// (`apply` and `apply_chain`, available with the `value` feature).
///
/// The migration-walk mirror of [`SchemaReadError`]/[`SchemaWriteError`]: it
/// walks an already-read `facet_value::Value` guided by the source
/// `Schema`, so every variant names the `path` at which the value
/// diverged from what that document describes.
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    /// The value does not match the source schema at `path`.
    #[error("at {path}: expected {expected}, found {found}")]
    Mismatch {
        /// The location within the value.
        path: String,
        /// The kind the schema node accepts.
        expected: &'static str,
        /// The value's actual runtime kind.
        found: &'static str,
    },
    /// A fixed-length sequence's element count does not match the source
    /// schema.
    ///
    /// Refused rather than truncated: an upcast that silently dropped
    /// elements would produce a value that conforms to the target schema and
    /// is not the value that was stored.
    #[error("at {path}: expected {expected} elements, found {found}")]
    LengthMismatch {
        /// The location within the value.
        path: String,
        /// The element count the source schema requires.
        expected: usize,
        /// The element count the value holds.
        found: usize,
    },
    /// A `Node::Ref` names a definition absent from the source document.
    #[error("at {path}: schema ref {name:?} has no definition in the source document")]
    UnknownRef {
        /// The location within the value.
        path: String,
        /// The undefined reference name.
        name: String,
    },
    /// Recursion exceeded the maximum supported nesting depth.
    ///
    /// Mirrors [`DeserializeError::MaxDepth`]: since every hop — including
    /// `Ref` resolution — counts against the limit, a `Ref`-to-`Ref` cycle in
    /// the source schema fails here rather than recursing unboundedly.
    #[error("at {path}: maximum nesting depth ({depth}) exceeded while applying a migration")]
    MaxDepth {
        /// The location reached when the limit tripped.
        path: String,
        /// The limit that was exceeded.
        depth: usize,
    },
}
