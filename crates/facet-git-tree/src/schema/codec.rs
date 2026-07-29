//! The codec fixture: a value exercising every construct this crate's codec
//! can encode, written into each pin tower's generation tree beside its own
//! schema-schema/migration-schema document.
//!
//! [`Fixture`]'s *schema* only captures which [`Node`](crate::schema::Node)
//! constructs exist — the shape language. It says nothing about how a
//! construct is actually spelled on disk: an `f64`'s text, `Bytes`' framing,
//! an empty `List`'s marker, `None`'s marker, and so on. A codec change that
//! touches only spelling leaves the schema-schema tree byte-identical, so a
//! generation pinned to it would not move — exactly the hole this fixture
//! closes. [`fixture`]'s *value*, encoded through the ordinary serializer
//! rather than hand-built, pins that spelling too: both the fixture's schema
//! and its value live inside the generation's own tree, so either changing
//! moves the generation id.
//!
//! `codec/schema/` is not itself pinned — it is inside the generation being
//! defined, and pinning it would recurse. It is identified by containment.
//!
//! Every value in [`fixture`] must encode identically on every target and
//! feature combination, since the generation id is a hash of them: two builds
//! of one version that disagreed here would reject each other's documents.
//! That rules out pointer-width-dependent extremes such as `isize::MIN`, and
//! is why `facet-value` is an unconditional dependency rather than gated
//! behind the `value` feature.

use std::collections::BTreeMap;

use facet::Facet;
use gix_object::{Tree, Write};

use crate::error::SerializeError;
use crate::schema::pin::decode_oid;
use crate::schema::{Schema, schema_of};
use crate::ser::serialize_into;
use crate::{EntryKind, EntryMode, ObjectId, RawTree, TreeEntry};

/// The tree entry name a generation's own tree carries its codec fixture
/// under.
pub(crate) const ENTRY: &str = "codec";

/// Git's well-known empty tree id (`git hash-object -t tree /dev/null`),
/// used as [`fixture`]'s [`RawTree`] target.
///
/// A tree's object id is a function of its entries' (mode, name, oid)
/// triples, not of whether the referenced objects exist, so this constant
/// alone is enough to keep [`fixture`] deterministic; [`codec_tree`] also
/// writes the empty tree explicitly, so the pointer resolves from the store
/// too, per [`RawTree`]'s contract.
const EMPTY_TREE_HEX: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// A named struct reached from [`Fixture`], giving [`Node::Ref`](crate::schema::Node::Ref)
/// an occurrence beyond the document's own root reference.
#[derive(Debug, Clone, PartialEq, Facet)]
pub struct FixtureNested {
    /// An arbitrary field, just to give the struct a body.
    pub tag: String,
}

/// A unit struct: the only source of `Node::Unit` — `()` itself has no
/// textual rendering and is refused by the scalar table (`schema::mod`).
#[derive(Debug, Clone, PartialEq, Facet)]
pub struct FixtureUnit;

/// An enum reaching all four `VariantKind` shapes.
///
/// A single instance only ever inhabits one variant, so [`Fixture`] carries
/// four fields of this type — one instantiated per shape — to exercise every
/// active-variant encoding in the fixture *value*, not just every shape in
/// its schema (which a single field would already cover, since a schema
/// describes the type's full variant set regardless of which one a given
/// value picks).
#[derive(Debug, Clone, PartialEq, Facet)]
#[repr(u8)]
pub enum FixtureEnum {
    /// [`VariantKind::Unit`](crate::schema::VariantKind::Unit).
    Unit,
    /// [`VariantKind::Newtype`](crate::schema::VariantKind::Newtype).
    Newtype(i32),
    /// [`VariantKind::Tuple`](crate::schema::VariantKind::Tuple).
    Tuple(i32, String),
    /// [`VariantKind::Struct`](crate::schema::VariantKind::Struct).
    Struct {
        /// An arbitrary field.
        a: i32,
        /// An arbitrary field.
        b: String,
    },
}

/// A value exercising every [`Node`](crate::schema::Node) and
/// [`VariantKind`](crate::schema::VariantKind) construct this crate's codec
/// can encode.
///
/// `tests/codec_fixture.rs`'s `fixture_covers_every_construct` walks
/// [`schema_of::<Fixture>()`](schema_of) by reflection and asserts it
/// against the variant sets `schema_of::<Node>()` and
/// `schema_of::<VariantKind>()` themselves reflect, so an added `Node` or
/// `VariantKind` variant fails that test until this type is extended to
/// cover it.
#[derive(Debug, Clone, PartialEq, Facet)]
pub struct Fixture {
    /// `Node::Unit`.
    pub unit: FixtureUnit,
    /// `Node::Bool`.
    pub boolean: bool,
    /// `Node::Char`: a non-ASCII scalar.
    pub character: char,
    /// `Node::String`: holds a multi-byte character.
    pub text: String,
    /// `Node::I8`, at its negative extreme.
    pub i8: i8,
    /// `Node::I16`, at its negative extreme.
    pub i16: i16,
    /// `Node::I32`, at its negative extreme.
    pub i32: i32,
    /// `Node::I64`, at its negative extreme.
    pub i64: i64,
    /// `Node::I128`, at its negative extreme.
    pub i128: i128,
    /// `Node::ISize`. Held to 32-bit range: an integer is encoded as its
    /// decimal `Display` form, so `isize::MIN` would make the generation id
    /// depend on the target's pointer width.
    pub isize: isize,
    /// `Node::U8`, at its maximum.
    pub u8: u8,
    /// `Node::U16`, at its maximum.
    pub u16: u16,
    /// `Node::U32`, at its maximum.
    pub u32: u32,
    /// `Node::U64`, at its maximum.
    pub u64: u64,
    /// `Node::U128`, at its maximum.
    pub u128: u128,
    /// `Node::USize`. Held to 32-bit range, for the reason on `isize`.
    pub usize: usize,
    /// `Node::F32`: fractional, negative exponent.
    pub f32: f32,
    /// `Node::F64`: fractional, negative exponent.
    pub f64: f64,
    /// `Node::Bytes`: holds both `0x00` and `0xff`.
    pub bytes: Vec<u8>,
    /// `Node::Tuple`.
    pub tuple: (i32, String),
    /// `Node::List`, non-empty.
    pub list_full: Vec<i32>,
    /// `Node::List`, empty — the presence-marker encoding.
    pub list_empty: Vec<i32>,
    /// `Node::Array`.
    pub array: [i32; 3],
    /// `Node::Map`, non-empty.
    pub map_full: BTreeMap<String, i32>,
    /// `Node::Map`, empty — the presence-marker encoding.
    pub map_empty: BTreeMap<String, i32>,
    /// `Node::Optional`, `Some` — the "some" wrapper tree.
    pub opt_some: Option<i32>,
    /// `Node::Optional`, `None` — the presence-marker encoding.
    pub opt_none: Option<i32>,
    /// `Node::Enum`, active variant `VariantKind::Unit`.
    pub enum_unit: FixtureEnum,
    /// `Node::Enum`, active variant `VariantKind::Newtype`.
    pub enum_newtype: FixtureEnum,
    /// `Node::Enum`, active variant `VariantKind::Tuple`.
    pub enum_tuple: FixtureEnum,
    /// `Node::Enum`, active variant `VariantKind::Struct`.
    pub enum_struct: FixtureEnum,
    /// `Node::RawTree`.
    pub raw_tree: RawTree,
    /// `Node::Dynamic`.
    pub dynamic: facet_value::Value,
    /// `Node::Ref`, beyond the document's own root reference.
    pub nested: FixtureNested,
}

/// The fixed, deterministic value [`codec_tree`] encodes as the codec
/// fixture's `value/` entry.
pub fn fixture() -> Fixture {
    Fixture {
        unit: FixtureUnit,
        boolean: true,
        character: 'Ω',
        text: "codec-☃-fixture".to_string(),
        i8: i8::MIN,
        i16: i16::MIN,
        i32: i32::MIN,
        i64: i64::MIN,
        i128: i128::MIN,
        isize: i32::MIN as isize,
        u8: u8::MAX,
        u16: u16::MAX,
        u32: u32::MAX,
        u64: u64::MAX,
        u128: u128::MAX,
        usize: u32::MAX as usize,
        f32: -1.25e-3,
        f64: -2.5e-15,
        bytes: vec![0x00, 0x2a, 0xff],
        tuple: (42, "tuple-val".to_string()),
        list_full: vec![1, -2, 3],
        list_empty: Vec::new(),
        array: [1, -2, 3],
        map_full: BTreeMap::from([("a".to_string(), 1), ("b".to_string(), -2)]),
        map_empty: BTreeMap::new(),
        opt_some: Some(-7),
        opt_none: None,
        enum_unit: FixtureEnum::Unit,
        enum_newtype: FixtureEnum::Newtype(-99),
        enum_tuple: FixtureEnum::Tuple(1, "two".to_string()),
        enum_struct: FixtureEnum::Struct {
            a: 1,
            b: "struct-field".to_string(),
        },
        raw_tree: RawTree::new(decode_oid(EMPTY_TREE_HEX)),
        dynamic: facet_value::Value::from("dynamic-fixture-value"),
        nested: FixtureNested {
            tag: "nested".to_string(),
        },
    }
}

fn fixture_schema() -> Schema {
    schema_of::<Fixture>().expect("Fixture's own shape is always describable")
}

/// Write the codec fixture's schema and value into `store`, and return the
/// `{schema/, value/}` tree that wraps them.
///
/// Shared by both pin towers (`schema::pin` and `migration::pin`): they
/// splice the exact same fixture, so the returned tree is one shared,
/// content-addressed object regardless of which tower writes it first.
pub fn codec_tree<S: Write + ?Sized>(store: &S) -> Result<ObjectId, SerializeError> {
    // RawTree never writes its own target (`ser::serialize_node`'s RawTree
    // branch only reads `RawTree::oid()`), so the fixture's target is
    // written explicitly here.
    store
        .write(&Tree { entries: vec![] })
        .map_err(SerializeError::Backend)?;

    let schema_tree = serialize_into(&fixture_schema(), store)?;
    let value_tree = serialize_into(&fixture(), store)?;
    let mut entries = vec![
        TreeEntry {
            mode: EntryMode::from(EntryKind::Tree),
            filename: "schema".into(),
            oid: schema_tree,
        },
        TreeEntry {
            mode: EntryMode::from(EntryKind::Tree),
            filename: "value".into(),
            oid: value_tree,
        },
    ];
    entries.sort();
    store
        .write(&Tree { entries })
        .map_err(SerializeError::Backend)
}
