//! The identity normal form: a frozen mini-codec over a closed primitive
//! universe, plus the check that decides whether a schema subtree lives in
//! that universe.
//!
//! Identity- and key-bearing subtrees (an anchor id, an action key) are hashed
//! through this mapping rather than through the general codec, so that codec
//! stays free to evolve: a grammar or encoder change must never move an
//! identity. Only *this* mapping is frozen. It lives in `facet-git-tree`
//! because that crate owns the pure codec, and the normal form is a second,
//! deliberately smaller codec over the same [`Node`] universe — not a storage
//! or authority concern.
//!
//! # The universe
//!
//! [`NormalForm`] is closed by construction: there is no variant that can hold
//! an enum tag, a dynamic facet value, a `RawTree`, an `Option`, or anything
//! else schema-rich, so an out-of-universe value is unrepresentable rather
//! than merely rejected. [`Key`] is closed the same way, over scalars only.
//!
//! Platform-width integers (`isize`/`usize`) are deliberately absent: their
//! width is a property of the machine that captured the value, and a frozen
//! mapping cannot depend on that. Floats are present, encoded verbatim from
//! their IEEE-754 bits with no canonicalization, so `-0.0` and `0.0` — and two
//! NaNs with different payloads — are distinct identities; identity coordinates
//! are better expressed without them.
//!
//! # The frozen mapping
//!
//! Every leaf is a blob holding exactly the bytes below — no trailing newline,
//! unlike the general codec, whose newline is a readability affordance of an
//! evolving format. Every composite is a tree; git's own name-sorted entries
//! supply the ordering, and no presence marker is used, so an empty list,
//! struct, or map is the literal empty tree.
//!
//! | value | git object | bytes |
//! |---|---|---|
//! | [`Bool`](NormalForm::Bool) | blob | one byte, `0x01` for true, `0x00` for false |
//! | [`I8`](NormalForm::I8)…[`I128`](NormalForm::I128) | blob | two's complement, big-endian, 1/2/4/8/16 bytes |
//! | [`U8`](NormalForm::U8)…[`U128`](NormalForm::U128) | blob | unsigned big-endian, 1/2/4/8/16 bytes |
//! | [`F32`](NormalForm::F32)/[`F64`](NormalForm::F64) | blob | IEEE-754 bits, big-endian, 4/8 bytes |
//! | [`Char`](NormalForm::Char) | blob | the character's UTF-8 encoding, 1–4 bytes |
//! | [`Str`](NormalForm::Str) | blob | the string's UTF-8 bytes, verbatim |
//! | [`Bytes`](NormalForm::Bytes) | blob | the bytes, verbatim |
//! | [`Hash`](NormalForm::Hash) | blob | the object id's raw hash bytes (20 for SHA-1) |
//! | [`List`](NormalForm::List) | tree | one entry per element, named by its zero-based index as exactly eight zero-padded ASCII decimal digits (`00000000`) |
//! | [`Struct`](NormalForm::Struct) | tree | one entry per field, named by the field name verbatim |
//! | [`Map`](NormalForm::Map) | tree | one entry per pair, named by the key's name form below |
//!
//! Tree entry modes are `40000` for a tree child and `100644` for a blob
//! child; no other mode occurs.
//!
//! A [`Key`]'s name form is:
//!
//! | key | name |
//! |---|---|
//! | [`Bool`](Key::Bool) | `true` or `false` |
//! | [`I8`](Key::I8)…[`U128`](Key::U128) | ASCII decimal, `-` prefixed when negative, no leading zeros |
//! | [`Char`](Key::Char) | the character's UTF-8 encoding |
//! | [`Str`](Key::Str) | the string's UTF-8 bytes, verbatim |
//! | [`Bytes`](Key::Bytes)/[`Hash`](Key::Hash) | lowercase hex of the bytes |
//!
//! A name must be non-empty and hold neither `/` nor NUL, since it is a git
//! path segment; [`NormalFormError::InvalidKey`] reports one that is not. Two
//! keys of *different* variants can share a name (`Key::Str("true")` and
//! `Key::Bool(true)`), which is unambiguous in practice because a map's key
//! type is fixed by its schema. The mapping is untagged for the same reason:
//! the hash identifies a value under a known shape, exactly as a git tree
//! identifies content under a known layout.
//!
//! # Marking a subtree
//!
//! `#[facet(facet_git_tree::identity_key)]` on a field or a type marks its
//! subtree (see [`crate::attr`]). [`schema_of`](crate::schema_of) compiles the mark into the schema
//! document as a definition whose name carries the reserved
//! [`IDENTITY_DEF_PREFIX`], referenced by an ordinary [`Node::Ref`] — which
//! adds no tree level, so a marked type's encoding is byte-identical to an
//! unmarked one's. [`identity_subtrees`] recovers the marked nodes from a
//! schema document, and [`check_identity_subtrees`] checks every one of them,
//! which is what a schema registration refuses on.

use std::collections::BTreeMap;

use gix_object::{Kind, Write};

use crate::error::{NormalFormError, UniverseError};
use crate::schema::{Node, Schema};
use crate::store::ObjectStore;
use crate::{EntryKind, EntryMode, ObjectId, TreeEntry};

/// The reserved prefix of a schema definition name holding an identity- or
/// key-bearing subtree.
///
/// `:` cannot occur in a Rust type identifier, so a reserved name can never
/// collide with a name [`schema_of`](crate::schema_of) assigns to a user type.
pub const IDENTITY_DEF_PREFIX: &str = "identity:";

/// The maximum nesting depth [`check_universe`] walks before refusing.
///
/// A schema may be recursive, so the check is bounded rather than relying on
/// the graph being finite. The bound matches the codec's own
/// [`MAX_DEPTH`](crate::schema::Schema::from_shape) in spirit: a value nested
/// deeper could not be read back regardless.
const MAX_DEPTH: usize = 64;

/// How many elements a list may hold, given eight-digit ordinals.
const MAX_LIST_LEN: usize = 100_000_000;

/// A value in the identity normal form's closed universe.
///
/// Every variant maps to git bytes by the frozen mapping in the [module
/// docs](self). The type is the universe: no variant can hold an enum tag, a
/// dynamic value, an option, or a raw tree, so
/// [`hash_into`] needs no validation step.
#[derive(Debug, Clone, PartialEq)]
pub enum NormalForm {
    /// A boolean.
    Bool(bool),
    /// An 8-bit signed integer.
    I8(i8),
    /// A 16-bit signed integer.
    I16(i16),
    /// A 32-bit signed integer.
    I32(i32),
    /// A 64-bit signed integer.
    I64(i64),
    /// A 128-bit signed integer.
    I128(i128),
    /// An 8-bit unsigned integer.
    U8(u8),
    /// A 16-bit unsigned integer.
    U16(u16),
    /// A 32-bit unsigned integer.
    U32(u32),
    /// A 64-bit unsigned integer.
    U64(u64),
    /// A 128-bit unsigned integer.
    U128(u128),
    /// A 32-bit float, hashed from its bits with no canonicalization.
    F32(f32),
    /// A 64-bit float, hashed from its bits with no canonicalization.
    F64(f64),
    /// A character.
    Char(char),
    /// A string.
    Str(String),
    /// A byte string.
    Bytes(Vec<u8>),
    /// A git object id, hashed as its raw hash bytes.
    Hash(ObjectId),
    /// An ordered sequence.
    List(Vec<NormalForm>),
    /// A fixed-name-field composite: the shape a struct with named fields
    /// (`Action.key`'s `executor`/`inputs`/`params`) takes.
    ///
    /// First-class beside [`Map`](Self::Map) because named-field composites are
    /// the dominant identity shape and their keys come from the schema, not
    /// from the data: a `Struct`'s entry names are fixed by the type, so they
    /// need neither the key-name mapping nor its validation.
    Struct(BTreeMap<String, NormalForm>),
    /// A keyed map, whose keys come from the data.
    Map(BTreeMap<Key, NormalForm>),
}

/// A [`NormalForm::Map`] key: the scalar half of the universe, closed the same
/// way.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Key {
    /// A boolean.
    Bool(bool),
    /// An 8-bit signed integer.
    I8(i8),
    /// A 16-bit signed integer.
    I16(i16),
    /// A 32-bit signed integer.
    I32(i32),
    /// A 64-bit signed integer.
    I64(i64),
    /// A 128-bit signed integer.
    I128(i128),
    /// An 8-bit unsigned integer.
    U8(u8),
    /// A 16-bit unsigned integer.
    U16(u16),
    /// A 32-bit unsigned integer.
    U32(u32),
    /// A 64-bit unsigned integer.
    U64(u64),
    /// A 128-bit unsigned integer.
    U128(u128),
    /// A character.
    Char(char),
    /// A string.
    Str(String),
    /// A byte string, named in lowercase hex.
    Bytes(Vec<u8>),
    /// A git object id, named in lowercase hex.
    Hash(ObjectId),
}

impl Key {
    /// The key's git tree entry name, per the frozen mapping.
    ///
    /// Fails with [`NormalFormError::InvalidKey`] when the name is not usable
    /// as a git path segment.
    pub fn name(&self) -> Result<String, NormalFormError> {
        let name = match self {
            Key::Bool(v) => v.to_string(),
            Key::I8(v) => v.to_string(),
            Key::I16(v) => v.to_string(),
            Key::I32(v) => v.to_string(),
            Key::I64(v) => v.to_string(),
            Key::I128(v) => v.to_string(),
            Key::U8(v) => v.to_string(),
            Key::U16(v) => v.to_string(),
            Key::U32(v) => v.to_string(),
            Key::U64(v) => v.to_string(),
            Key::U128(v) => v.to_string(),
            Key::Char(v) => v.to_string(),
            Key::Str(v) => v.clone(),
            Key::Bytes(v) => hex(v),
            Key::Hash(v) => v.to_string(),
        };
        if name.is_empty() || name.contains('/') || name.contains('\0') {
            return Err(NormalFormError::InvalidKey { key: name });
        }
        Ok(name)
    }
}

/// Lowercase hex of `bytes`.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Write `value`'s git objects into `store` and return the root object id: the
/// identity, or the key, of whatever the value describes.
///
/// The object is a genuine git object — the root of a real tree, or a blob for
/// a scalar value — so the returned id is fetchable, attestable, and
/// diffable like any other. `store` is the same backend contract the rest of
/// the crate takes: any `gix` [`Write`] sink, `?Sized` so a `&dyn Write` works.
pub fn hash_into<W: Write + ?Sized>(
    value: &NormalForm,
    store: &W,
) -> Result<ObjectId, NormalFormError> {
    write_node(value, store).map(|(oid, _kind)| oid)
}

/// [`hash_into`] against a fresh in-memory [`ObjectStore`], returned alongside
/// the root id so a caller can relay the objects onward.
pub fn hash(value: &NormalForm) -> Result<(ObjectId, ObjectStore), NormalFormError> {
    let store = ObjectStore::default();
    let root = hash_into(value, &store)?;
    Ok((root, store))
}

/// The ordinal name of list element `index`, per the frozen mapping.
fn ordinal(index: usize) -> String {
    format!("{index:08}")
}

fn write_node<W: Write + ?Sized>(
    value: &NormalForm,
    store: &W,
) -> Result<(ObjectId, EntryKind), NormalFormError> {
    match value {
        NormalForm::List(items) => {
            if items.len() >= MAX_LIST_LEN {
                return Err(NormalFormError::ListTooLong {
                    len: items.len(),
                    max: MAX_LIST_LEN - 1,
                });
            }
            let entries = items
                .iter()
                .enumerate()
                .map(|(i, item)| entry(ordinal(i), item, store))
                .collect::<Result<Vec<_>, _>>()?;
            write_tree(entries, store)
        }
        NormalForm::Struct(fields) => {
            let entries = fields
                .iter()
                .map(|(name, field)| entry(name.clone(), field, store))
                .collect::<Result<Vec<_>, _>>()?;
            write_tree(entries, store)
        }
        NormalForm::Map(pairs) => {
            let entries = pairs
                .iter()
                .map(|(key, item)| entry(key.name()?, item, store))
                .collect::<Result<Vec<_>, _>>()?;
            write_tree(entries, store)
        }
        scalar => {
            let oid = store
                .write_buf(Kind::Blob, &scalar_bytes(scalar))
                .map_err(NormalFormError::Backend)?;
            Ok((oid, EntryKind::Blob))
        }
    }
}

/// The blob bytes of a scalar value; composites are written by [`write_node`].
fn scalar_bytes(value: &NormalForm) -> Vec<u8> {
    match value {
        NormalForm::Bool(v) => vec![u8::from(*v)],
        NormalForm::I8(v) => v.to_be_bytes().to_vec(),
        NormalForm::I16(v) => v.to_be_bytes().to_vec(),
        NormalForm::I32(v) => v.to_be_bytes().to_vec(),
        NormalForm::I64(v) => v.to_be_bytes().to_vec(),
        NormalForm::I128(v) => v.to_be_bytes().to_vec(),
        NormalForm::U8(v) => v.to_be_bytes().to_vec(),
        NormalForm::U16(v) => v.to_be_bytes().to_vec(),
        NormalForm::U32(v) => v.to_be_bytes().to_vec(),
        NormalForm::U64(v) => v.to_be_bytes().to_vec(),
        NormalForm::U128(v) => v.to_be_bytes().to_vec(),
        NormalForm::F32(v) => v.to_bits().to_be_bytes().to_vec(),
        NormalForm::F64(v) => v.to_bits().to_be_bytes().to_vec(),
        NormalForm::Char(v) => v.to_string().into_bytes(),
        NormalForm::Str(v) => v.as_bytes().to_vec(),
        NormalForm::Bytes(v) => v.clone(),
        NormalForm::Hash(v) => v.as_bytes().to_vec(),
        NormalForm::List(_) | NormalForm::Struct(_) | NormalForm::Map(_) => {
            unreachable!("composites are written as trees")
        }
    }
}

fn entry<W: Write + ?Sized>(
    name: String,
    value: &NormalForm,
    store: &W,
) -> Result<TreeEntry, NormalFormError> {
    let (oid, kind) = write_node(value, store)?;
    Ok(TreeEntry {
        mode: EntryMode::from(kind),
        filename: name.into(),
        oid,
    })
}

fn write_tree<W: Write + ?Sized>(
    mut entries: Vec<TreeEntry>,
    store: &W,
) -> Result<(ObjectId, EntryKind), NormalFormError> {
    entries.sort();
    let oid = store
        .write(&gix_object::Tree { entries })
        .map_err(NormalFormError::Backend)?;
    Ok((oid, EntryKind::Tree))
}

/// Every identity- or key-bearing subtree of `schema`: its definition name and
/// the marked node.
///
/// A subtree is marked by `#[facet(identity::key)]` on the field or type it
/// describes, which [`schema_of`](crate::schema_of) compiles into a definition
/// named with the reserved [`IDENTITY_DEF_PREFIX`].
pub fn identity_subtrees(schema: &Schema) -> impl Iterator<Item = (&str, &Node)> {
    schema
        .defs
        .iter()
        .filter(|(name, _)| name.starts_with(IDENTITY_DEF_PREFIX))
        .map(|(name, node)| (name.as_str(), node))
}

/// Check every identity- or key-bearing subtree of `schema` against the normal
/// form's universe.
///
/// The gate a schema registration runs: a marked subtree that reaches an enum,
/// a dynamic value, an option, or any other excluded node makes the schema
/// unregisterable, because a value under it could never be given a stable
/// identity.
pub fn check_identity_subtrees(schema: &Schema) -> Result<(), UniverseError> {
    for (name, node) in identity_subtrees(schema) {
        check_universe_at(node, &schema.defs, name)?;
    }
    Ok(())
}

/// Whether `node`, resolved through `defs`, lies in the normal form's
/// universe.
///
/// Accepted: the scalar nodes the universe names, [`Node::Bytes`],
/// [`Node::Struct`], [`Node::Tuple`], [`Node::List`], [`Node::Array`],
/// [`Node::Map`] with a scalar or byte-string key, and [`Node::Ref`] to any of
/// those. Everything else is refused with the path at which it was found:
/// [`Node::Enum`] and [`Node::Dynamic`] because they are schema-rich,
/// [`Node::RawTree`] because it names a tree this mapping did not write,
/// [`Node::Optional`] because absence is not in the universe — an identity
/// coordinate that may be missing is a different identity, not the same one
/// with a hole — and [`Node::Unit`], [`Node::ISize`], and [`Node::USize`]
/// because they have no frozen encoding (see the [module docs](self)).
pub fn check_universe(node: &Node, defs: &BTreeMap<String, Node>) -> Result<(), UniverseError> {
    check_universe_at(node, defs, "")
}

/// [`check_universe`] with `root` naming the subtree in reported paths.
pub fn check_universe_at(
    node: &Node,
    defs: &BTreeMap<String, Node>,
    root: &str,
) -> Result<(), UniverseError> {
    walk(node, defs, root.to_owned(), 0)
}

fn walk(
    node: &Node,
    defs: &BTreeMap<String, Node>,
    path: String,
    depth: usize,
) -> Result<(), UniverseError> {
    if depth > MAX_DEPTH {
        return Err(UniverseError::MaxDepth {
            path,
            depth: MAX_DEPTH,
        });
    }
    let excluded = |found: &'static str| UniverseError::Excluded {
        path: path.clone(),
        found,
    };
    match node {
        Node::Bool
        | Node::Char
        | Node::String
        | Node::I8
        | Node::I16
        | Node::I32
        | Node::I64
        | Node::I128
        | Node::U8
        | Node::U16
        | Node::U32
        | Node::U64
        | Node::U128
        | Node::F32
        | Node::F64
        | Node::Bytes => Ok(()),
        Node::Struct(fields) => fields.iter().try_for_each(|(name, field)| {
            walk(&field.node, defs, format!("{path}.{name}"), depth + 1)
        }),
        Node::Tuple(elems) => elems
            .iter()
            .enumerate()
            .try_for_each(|(i, elem)| walk(elem, defs, format!("{path}.{i}"), depth + 1)),
        Node::List(elem) => walk(elem, defs, format!("{path}[]"), depth + 1),
        Node::Array { elem, .. } => walk(elem, defs, format!("{path}[]"), depth + 1),
        Node::Map { key, value } => {
            walk_key(key, defs, &path)?;
            walk(value, defs, format!("{path}{{}}"), depth + 1)
        }
        Node::Ref(name) => {
            let def = defs.get(name).ok_or_else(|| UniverseError::UnknownRef {
                path: path.clone(),
                name: name.clone(),
            })?;
            walk(def, defs, path, depth + 1)
        }
        Node::Unit => Err(excluded("Unit")),
        Node::ISize => Err(excluded("ISize")),
        Node::USize => Err(excluded("USize")),
        Node::Optional(_) => Err(excluded("Optional")),
        Node::Enum(_) => Err(excluded("Enum")),
        Node::RawTree => Err(excluded("RawTree")),
        Node::Dynamic => Err(excluded("Dynamic")),
    }
}

/// A map key must be a [`Key`] shape: the scalar universe, or a byte string.
fn walk_key(key: &Node, defs: &BTreeMap<String, Node>, path: &str) -> Result<(), UniverseError> {
    let key = match key {
        Node::Ref(name) => defs.get(name).ok_or_else(|| UniverseError::UnknownRef {
            path: format!("{path}<key>"),
            name: name.clone(),
        })?,
        other => other,
    };
    match key {
        Node::Bool
        | Node::Char
        | Node::String
        | Node::I8
        | Node::I16
        | Node::I32
        | Node::I64
        | Node::I128
        | Node::U8
        | Node::U16
        | Node::U32
        | Node::U64
        | Node::U128
        | Node::Bytes => Ok(()),
        other => Err(UniverseError::Excluded {
            path: format!("{path}<key>"),
            found: node_name(other),
        }),
    }
}

/// The [`Node`] variant's name, for an error to report.
fn node_name(node: &Node) -> &'static str {
    match node {
        Node::Unit => "Unit",
        Node::Bool => "Bool",
        Node::Char => "Char",
        Node::String => "String",
        Node::I8 => "I8",
        Node::I16 => "I16",
        Node::I32 => "I32",
        Node::I64 => "I64",
        Node::I128 => "I128",
        Node::ISize => "ISize",
        Node::U8 => "U8",
        Node::U16 => "U16",
        Node::U32 => "U32",
        Node::U64 => "U64",
        Node::U128 => "U128",
        Node::USize => "USize",
        Node::F32 => "F32",
        Node::F64 => "F64",
        Node::Bytes => "Bytes",
        Node::Struct(_) => "Struct",
        Node::Tuple(_) => "Tuple",
        Node::List(_) => "List",
        Node::Array { .. } => "Array",
        Node::Map { .. } => "Map",
        Node::Optional(_) => "Optional",
        Node::Enum(_) => "Enum",
        Node::RawTree => "RawTree",
        Node::Dynamic => "Dynamic",
        Node::Ref(_) => "Ref",
    }
}
