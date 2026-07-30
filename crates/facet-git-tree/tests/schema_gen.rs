//! Integration tests for schema generation (`schema_of`).
//!
//! Covers spec requirements:
//!   schema.generation
//!     — the shape → schema converter mirrors the encoder's dispatch order
//!       (transparency collapse, RawTree, dynamic values, scalars, byte
//!       sequences, composites), deduplicates named user types into `defs`
//!       with deterministic `_2`-suffixed names, and breaks recursion with
//!       `Node::Ref`.
//!   schema.representation
//!     — the `Node`/`Schema` shapes asserted here are the public,
//!       semver-major on-disk contract.

use std::collections::BTreeMap;

use facet::Facet;
use facet_git_tree::{Node, RawTree, Schema, SchemaError, StructField, VariantKind, schema_of};

mod common;
use common::{
    Event, Nested, Person, Point, TreeNode, WithArray, WithDefault, WithMap, WithOptional,
    WithValue, WithVec,
};

// --- helpers ---

/// [`Node::Struct`] fields, all non-defaulted — the common case in this file.
fn fields(pairs: Vec<(&str, Node)>) -> BTreeMap<String, StructField> {
    pairs
        .into_iter()
        .map(|(k, v)| {
            (
                k.into(),
                StructField {
                    node: v,
                    has_default: false,
                },
            )
        })
        .collect()
}

/// A struct enum variant's fields, which carry no default marker.
fn variant_fields(pairs: Vec<(&str, Node)>) -> BTreeMap<String, Node> {
    pairs.into_iter().map(|(k, v)| (k.into(), v)).collect()
}

fn variants(pairs: Vec<(&str, VariantKind)>) -> BTreeMap<String, VariantKind> {
    pairs.into_iter().map(|(k, v)| (k.into(), v)).collect()
}

fn re(name: &str) -> Node {
    Node::Ref(name.into())
}

fn doc(root: Node, defs: Vec<(&str, Node)>) -> Schema {
    Schema {
        root,
        defs: defs.into_iter().map(|(k, v)| (k.into(), v)).collect(),
    }
}

fn point_def() -> Node {
    Node::Struct(fields(vec![("x", Node::F64), ("y", Node::F64)]))
}

// --- named structs enter defs, referenced by Ref ---

/// A named struct becomes a `Ref` root with its body in `defs`.
#[test]
fn point_schema() -> anyhow::Result<()> {
    assert_eq!(
        schema_of::<Point>()?,
        doc(re("Point"), vec![("Point", point_def())])
    );
    Ok(())
}

/// Scalar fields map through the per-width scalar table, in declaration order.
#[test]
fn person_schema() -> anyhow::Result<()> {
    assert_eq!(
        schema_of::<Person>()?,
        doc(
            re("Person"),
            vec![(
                "Person",
                Node::Struct(fields(vec![
                    ("name", Node::String),
                    ("age", Node::U32),
                    ("active", Node::Bool),
                ]))
            )]
        )
    );
    Ok(())
}

/// `#[facet(default)]` on a field sets `StructField::has_default`, the
/// field-level default-presence marker; every other field's marker is unset.
#[test]
fn defaulted_field_schema_marks_has_default() -> anyhow::Result<()> {
    assert_eq!(
        schema_of::<WithDefault>()?,
        doc(
            re("WithDefault"),
            vec![(
                "WithDefault",
                Node::Struct(BTreeMap::from([
                    (
                        "label".to_owned(),
                        StructField {
                            node: Node::String,
                            has_default: false,
                        }
                    ),
                    (
                        "count".to_owned(),
                        StructField {
                            node: Node::U32,
                            has_default: true,
                        }
                    ),
                ]))
            )]
        )
    );
    Ok(())
}

/// A struct-valued field is itself deduplicated into `defs`.
#[test]
fn nested_schema() -> anyhow::Result<()> {
    assert_eq!(
        schema_of::<Nested>()?,
        doc(
            re("Nested"),
            vec![
                (
                    "Nested",
                    Node::Struct(fields(vec![
                        ("location", re("Point")),
                        ("label", Node::String),
                    ]))
                ),
                ("Point", point_def()),
            ]
        )
    );
    Ok(())
}

// --- collections ---

/// A `Vec<T>` field is a `List` node.
#[test]
fn with_vec_schema() -> anyhow::Result<()> {
    assert_eq!(
        schema_of::<WithVec>()?,
        doc(
            re("WithVec"),
            vec![(
                "WithVec",
                Node::Struct(fields(vec![("items", Node::List(Box::new(Node::I64)))]))
            )]
        )
    );
    Ok(())
}

/// A `[T; N]` field is an `Array` node carrying its exact length.
#[test]
fn with_array_schema() -> anyhow::Result<()> {
    assert_eq!(
        schema_of::<WithArray>()?,
        doc(
            re("WithArray"),
            vec![(
                "WithArray",
                Node::Struct(fields(vec![(
                    "values",
                    Node::Array {
                        elem: Box::new(Node::I32),
                        len: 4
                    }
                )]))
            )]
        )
    );
    Ok(())
}

/// A map field carries both key and value schemas.
#[test]
fn with_map_schema() -> anyhow::Result<()> {
    assert_eq!(
        schema_of::<WithMap>()?,
        doc(
            re("WithMap"),
            vec![(
                "WithMap",
                Node::Struct(fields(vec![(
                    "table",
                    Node::Map {
                        key: Box::new(Node::String),
                        value: Box::new(Node::String)
                    }
                )]))
            )]
        )
    );
    Ok(())
}

/// An `Option<T>` field is an `Optional` node.
#[test]
fn with_optional_schema() -> anyhow::Result<()> {
    assert_eq!(
        schema_of::<WithOptional>()?,
        doc(
            re("WithOptional"),
            vec![(
                "WithOptional",
                Node::Struct(fields(vec![("maybe", Node::Optional(Box::new(Node::I32)))]))
            )]
        )
    );
    Ok(())
}

/// A `u8` sequence is `Bytes`, not a `List` — mirroring the blob encoding.
#[test]
fn byte_seq_schema_is_bytes() -> anyhow::Result<()> {
    assert_eq!(schema_of::<Vec<u8>>()?, doc(Node::Bytes, vec![]));
    Ok(())
}

// --- unit ---

/// `()` has no textual rendering — `scalar_bytes` has no `Display`/`FromStr`
/// path for it either — so a field of type `()` cannot be described by a
/// schema, mirroring the encoder's own refusal.
#[test]
fn unit_field_schema_errors() {
    #[derive(Facet)]
    struct WithUnit {
        u: (),
    }
    let err = schema_of::<WithUnit>().unwrap_err();
    assert!(
        matches!(err, SchemaError::UnsupportedScalar("()")),
        "expected UnsupportedScalar(\"()\"), got {err:?}"
    );
}

/// A unit struct still yields `Node::Unit`: unlike `()`, it has a real
/// (empty-tree) encoding to describe — it is simply reached through the
/// struct branch, not the scalar table `()` is refused from.
#[test]
fn unit_struct_schema_is_unit() -> anyhow::Result<()> {
    #[derive(Facet)]
    struct Marker;
    assert_eq!(
        schema_of::<Marker>()?,
        doc(re("Marker"), vec![("Marker", Node::Unit)])
    );
    Ok(())
}

// --- enums ---

/// Each variant's payload kind matches the encoder's classification: unit,
/// newtype (single-field tuple), tuple, and struct.
#[test]
fn enum_schema() -> anyhow::Result<()> {
    assert_eq!(
        schema_of::<Event>()?,
        doc(
            re("Event"),
            vec![(
                "Event",
                Node::Enum(variants(vec![
                    ("Ping", VariantKind::Unit),
                    ("Message", VariantKind::Newtype(Box::new(Node::String))),
                    ("Move", VariantKind::Tuple(vec![Node::I32, Node::I32])),
                    (
                        "Login",
                        VariantKind::Struct(variant_fields(vec![
                            ("user", Node::String),
                            ("ok", Node::Bool),
                        ]))
                    ),
                ]))
            )]
        )
    );
    Ok(())
}

// --- special leaves ---

/// A `RawTree` field is the `RawTree` schema node.
#[test]
fn raw_tree_field_schema() -> anyhow::Result<()> {
    #[derive(Facet)]
    struct WithRaw {
        raw: RawTree,
    }
    assert_eq!(
        schema_of::<WithRaw>()?,
        doc(
            re("WithRaw"),
            vec![(
                "WithRaw",
                Node::Struct(fields(vec![("raw", Node::RawTree)]))
            )]
        )
    );
    Ok(())
}

/// A `facet_value::Value` field is the `Dynamic` schema node.
#[test]
fn value_field_schema_is_dynamic() -> anyhow::Result<()> {
    assert_eq!(
        schema_of::<WithValue>()?,
        doc(
            re("WithValue"),
            vec![(
                "WithValue",
                Node::Struct(fields(vec![("meta", Node::Dynamic)]))
            )]
        )
    );
    Ok(())
}

// --- recursion ---

/// A self-referential type resolves its own occurrences to `Ref`, breaking
/// the cycle through `defs`.
#[test]
fn recursive_schema_uses_ref() -> anyhow::Result<()> {
    assert_eq!(
        schema_of::<TreeNode>()?,
        doc(
            re("TreeNode"),
            vec![(
                "TreeNode",
                Node::Struct(fields(vec![
                    ("value", Node::I64),
                    ("children", Node::List(Box::new(re("TreeNode")))),
                ]))
            )]
        )
    );
    Ok(())
}

// --- name collisions ---

/// Distinct types sharing an identifier get deterministic `_2` suffixes in
/// pre-order.
#[test]
fn name_collision_disambiguated() -> anyhow::Result<()> {
    mod a {
        #[derive(facet::Facet)]
        pub struct Dup {
            pub x: i32,
        }
    }
    mod b {
        #[derive(facet::Facet)]
        pub struct Dup {
            pub y: u8,
        }
    }
    #[derive(Facet)]
    struct Both {
        first: a::Dup,
        second: b::Dup,
    }
    assert_eq!(
        schema_of::<Both>()?,
        doc(
            re("Both"),
            vec![
                (
                    "Both",
                    Node::Struct(fields(vec![("first", re("Dup")), ("second", re("Dup_2"))]))
                ),
                ("Dup", Node::Struct(fields(vec![("x", Node::I32)]))),
                ("Dup_2", Node::Struct(fields(vec![("y", Node::U8)]))),
            ]
        )
    );
    Ok(())
}

// --- transparency ---

/// A smart pointer collapses to its pointee's schema: `Box<T>` and `T` are
/// indistinguishable, exactly as they are indistinguishable on disk.
#[test]
fn box_schema_collapses() -> anyhow::Result<()> {
    assert_eq!(schema_of::<Box<Point>>()?, schema_of::<Point>()?);
    Ok(())
}

/// A transparent newtype collapses to its inner type's schema.
#[test]
fn transparent_newtype_schema_collapses() -> anyhow::Result<()> {
    #[derive(Facet)]
    #[facet(transparent)]
    struct Hex(String);
    assert_eq!(schema_of::<Hex>()?, doc(Node::String, vec![]));
    Ok(())
}

// --- depth guard ---

/// A shape nested deeper than the walker's bound is refused with
/// `SchemaError::MaxDepth` rather than recursing unboundedly — data written
/// that deep could never be read back by the depth-bounded deserializer
/// regardless of what schema described it.
///
/// Exercised through `from_shape_with_limit` with a small bound: a type
/// actually deeper than `MAX_DEPTH` (32) makes the compiler's recursive
/// `SHAPE` evaluation prohibitively expensive, and the guard's threading is
/// identical at every bound.
#[test]
fn excessively_nested_shape_schema_is_rejected() {
    type Nested = Option<Option<Option<Option<Option<i32>>>>>;
    let err = Schema::from_shape_with_limit(<Nested as facet::Facet>::SHAPE, 3).unwrap_err();
    assert!(
        matches!(err, SchemaError::MaxDepth(3)),
        "expected MaxDepth(3), got {err:?}"
    );
    // The same shape is comfortably within the real bound.
    assert!(schema_of::<Nested>().is_ok());
}
