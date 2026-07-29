//! Integration tests for schema-driven deserialization
//! (`deserialize_value_with_schema` / `validate_with_schema`, `value`
//! feature).
//!
//! Covers spec requirement:
//!   deserialization.schema-driven
//!     — a schema document recovers a faithful `Value` from a typed tree
//!       without the original `Facet` type: numbers as numbers, bools as
//!       bools, enums as single-member tagged objects, `RawTree` as the hex
//!       object id, `Dynamic` via the bare heuristic, and `Ref` through the
//!       document's `defs` table.

use std::collections::HashMap;
use std::sync::Arc;

use facet::Facet;
use facet_git_tree::{
    DeserializeError, EntryKind, EntryMode, Node, ObjectId, ObjectStore, RawTree, Schema,
    SchemaReadError, TreeEntry, deserialize, deserialize_value_with_schema, schema_of, serialize,
    serialize_into, validate_with_schema,
};
use facet_value::{VNumber, Value, value};
use gix_object::{Kind, Write as _};

mod common;
use common::{Event, Person, Point, TreeNode, WithMap, WithOptional};

// --- faithful scalars ---

/// A typed struct reads back with full fidelity: numbers are numbers and
/// bools are bools, not the strings the bare heuristic yields.
#[test]
fn person_reads_faithfully() -> anyhow::Result<()> {
    let person = Person {
        name: "Alice".into(),
        age: 30,
        active: true,
    };
    let (root, store) = serialize(&person)?;
    let doc = schema_of::<Person>()?;
    let v = deserialize_value_with_schema(&root, &doc, &store)?;
    assert_eq!(v, value!({ "name": "Alice", "age": 30, "active": true }));
    Ok(())
}

/// 128-bit extremes are preserved exactly, never routed through a float.
#[test]
fn extreme_128_bit_integers_are_exact() -> anyhow::Result<()> {
    #[derive(Facet)]
    struct Big {
        a: u128,
        b: i128,
    }
    let (root, store) = serialize(&Big {
        a: u128::MAX,
        b: i128::MIN,
    })?;
    let doc = schema_of::<Big>()?;
    let v = deserialize_value_with_schema(&root, &doc, &store)?;
    assert_eq!(
        v.as_object().unwrap().get("a").unwrap().as_number(),
        Some(&VNumber::from_u128(u128::MAX))
    );
    assert_eq!(
        v.as_object().unwrap().get("b").unwrap().as_number(),
        Some(&VNumber::from_i128(i128::MIN))
    );
    Ok(())
}

// --- Option ---

/// `None` reads as `NULL`; `Some` reads as the inner value directly.
#[test]
fn option_none_is_null_some_is_inner() -> anyhow::Result<()> {
    let doc = schema_of::<WithOptional>()?;

    let (root, store) = serialize(&WithOptional { maybe: None })?;
    let v = deserialize_value_with_schema(&root, &doc, &store)?;
    assert_eq!(v, value!({ "maybe": null }));

    let (root, store) = serialize(&WithOptional { maybe: Some(5) })?;
    let v = deserialize_value_with_schema(&root, &doc, &store)?;
    assert_eq!(v, value!({ "maybe": 5 }));
    Ok(())
}

// --- enums ---

/// Every variant kind reads as a single-member object tagged by the variant
/// name: unit → NULL, newtype → inner, tuple → array, struct → object.
#[test]
fn enum_variants_read_as_tagged_objects() -> anyhow::Result<()> {
    let doc = schema_of::<Event>()?;
    let read = |e: &Event| -> anyhow::Result<Value> {
        let (root, store) = serialize(e)?;
        Ok(deserialize_value_with_schema(&root, &doc, &store)?)
    };

    assert_eq!(read(&Event::Ping)?, value!({ "Ping": null }));
    assert_eq!(
        read(&Event::Message("hi".into()))?,
        value!({ "Message": "hi" })
    );
    assert_eq!(read(&Event::Move(1, -2))?, value!({ "Move": [1, (-2)] }));
    assert_eq!(
        read(&Event::Login {
            user: "bob".into(),
            ok: true
        })?,
        value!({ "Login": { "user": "bob", "ok": true } })
    );
    Ok(())
}

/// A variant name absent from the schema is an `UnknownVariant` error.
#[test]
fn unknown_variant_errors() -> anyhow::Result<()> {
    let (root, store) = serialize(&Event::Ping)?;
    // A schema for the same enum, but without the `Ping` variant.
    let doc = Schema {
        root: Node::Enum(Default::default()),
        defs: Default::default(),
    };
    let err = deserialize_value_with_schema(&root, &doc, &store).unwrap_err();
    assert!(
        matches!(&err, SchemaReadError::UnknownVariant { variant, .. } if variant == "Ping"),
        "expected UnknownVariant, got {err:?}"
    );
    Ok(())
}

// --- maps ---

/// A scalar-key map reads as an object keyed by the textual keys.
#[test]
fn scalar_key_map_reads_as_object() -> anyhow::Result<()> {
    let mut table = HashMap::new();
    table.insert("greeting".to_string(), "hello".to_string());
    let (root, store) = serialize(&WithMap { table })?;
    let doc = schema_of::<WithMap>()?;
    let v = deserialize_value_with_schema(&root, &doc, &store)?;
    assert_eq!(v, value!({ "table": { "greeting": "hello" } }));
    Ok(())
}

/// A composite-key map reads as an array of `{ "k": …, "v": … }` objects.
#[test]
fn composite_key_map_reads_as_pair_array() -> anyhow::Result<()> {
    let mut table: HashMap<(u8, u8), String> = HashMap::new();
    table.insert((3, 4), "cell".to_string());
    let (root, store) = serialize(&table)?;
    let doc = schema_of::<HashMap<(u8, u8), String>>()?;
    let v = deserialize_value_with_schema(&root, &doc, &store)?;
    assert_eq!(v, value!([{ "k": [3, 4], "v": "cell" }]));
    Ok(())
}

/// A smart-pointer scalar key (`Arc<str>`) is classified scalar by
/// `schema_of` — matching what the encoder actually writes after
/// transparency collapse (`collapse_shape`) — rather than composite, which is
/// what the raw, uncollapsed `Def::Pointer` key shape would otherwise suggest.
/// Before that collapse happened at the same altitude as the encoder, this
/// combination (composite bytes, scalar schema) made
/// `deserialize_value_with_schema` fail with `NotABlob`.
#[test]
fn arc_str_key_map_schema_is_scalar_and_reads() -> anyhow::Result<()> {
    let mut table: HashMap<Arc<str>, u32> = HashMap::new();
    table.insert(Arc::from("hi"), 1);

    let doc = schema_of::<HashMap<Arc<str>, u32>>()?;
    assert_eq!(
        doc.root,
        Node::Map {
            key: Box::new(Node::String),
            value: Box::new(Node::U32),
        },
        "an Arc<str> key must be schema'd as its collapsed scalar (String), not composite"
    );

    let (root, store) = serialize(&table)?;
    let v = deserialize_value_with_schema(&root, &doc, &store)?;
    assert_eq!(v, value!({ "hi": 1 }));
    Ok(())
}

// --- recursion via Ref ---

/// A recursive type reads through its `Ref` definition at every level.
#[test]
fn recursive_tree_reads_via_ref() -> anyhow::Result<()> {
    let tree = TreeNode {
        value: 1,
        children: vec![TreeNode {
            value: 2,
            children: vec![],
        }],
    };
    let (root, store) = serialize(&tree)?;
    let doc = schema_of::<TreeNode>()?;
    let v = deserialize_value_with_schema(&root, &doc, &store)?;
    assert_eq!(
        v,
        value!({ "value": 1, "children": [{ "value": 2, "children": [] }] })
    );
    Ok(())
}

/// A `Ref` naming no definition is an `UnknownRef` error.
#[test]
fn unknown_ref_errors() -> anyhow::Result<()> {
    let (root, store) = serialize(&42i32)?;
    let doc = Schema {
        root: Node::Ref("nope".into()),
        defs: Default::default(),
    };
    let err = deserialize_value_with_schema(&root, &doc, &store).unwrap_err();
    assert!(
        matches!(&err, SchemaReadError::UnknownRef(name) if name == "nope"),
        "expected UnknownRef, got {err:?}"
    );
    Ok(())
}

// --- RawTree and Dynamic ---

/// A `RawTree` node reads as the referenced object id in lowercase hex.
#[test]
fn raw_tree_reads_as_hex_string() -> anyhow::Result<()> {
    #[derive(Facet)]
    struct WithRaw {
        raw: RawTree,
    }
    let (inner, store) = serialize(&Point { x: 1.0, y: 2.0 })?;
    let root = serialize_into(
        &WithRaw {
            raw: RawTree::new(inner),
        },
        &store,
    )?;
    let doc = schema_of::<WithRaw>()?;
    let v = deserialize_value_with_schema(&root, &doc, &store)?;
    assert_eq!(v, value!({ "raw": (inner.to_string()) }));
    Ok(())
}

/// Wrap `oid` in an `Option`-style `{ "some": oid }` tree, `wraps` times,
/// returning the outermost object id — a synthetic, arbitrarily deep tree
/// without needing a matching arbitrarily-deep `Facet` type. Every wrapping
/// level after the first points at a tree (the previous wrapper); only the
/// innermost may point at a blob, per `leaf_is_blob`.
fn nest_some(store: &ObjectStore, oid: ObjectId, wraps: usize, leaf_is_blob: bool) -> ObjectId {
    let mut current = oid;
    let mut current_kind = if leaf_is_blob {
        EntryKind::Blob
    } else {
        EntryKind::Tree
    };
    for _ in 0..wraps {
        let entries = vec![TreeEntry {
            mode: EntryMode::from(current_kind),
            filename: "some".into(),
            oid: current,
        }];
        current = store
            .write(&gix_object::Tree { entries })
            .expect("write tree");
        current_kind = EntryKind::Tree;
    }
    current
}

/// A `Node::Dynamic` node hands off to the same recursion-depth budget the
/// surrounding schema-driven read is already spending from, rather than
/// resetting it to `0`. Neither half of this tree — 20 `Node::Optional`
/// levels, then 20 more levels the dynamic heuristic itself must walk —
/// exceeds `MAX_DEPTH` (32) alone, but their sum (40) does. Before the
/// hand-off carried the depth across, the inner heuristic read restarted at
/// depth `0` and this tree would have been read successfully instead of
/// rejected.
#[test]
fn dynamic_schema_node_shares_the_surrounding_depth_budget() -> anyhow::Result<()> {
    let store = ObjectStore::default();
    let leaf = store.write_buf(Kind::Blob, b"leaf").expect("write blob");
    // The dynamic heuristic classifies a non-ordinal-named tree as an Object
    // (see `deserialization.dynamic.heuristic`), so continuing the same
    // `{ "some": … }` shape past the schema/dynamic boundary still costs one
    // recursion level per wrap on the dynamic side, exactly as it does on the
    // `Node::Optional` side.
    let dynamic_part = nest_some(&store, leaf, 20, true);
    let root = nest_some(&store, dynamic_part, 20, false);

    let mut schema = Node::Dynamic;
    for _ in 0..20 {
        schema = Node::Optional(Box::new(schema));
    }
    let doc = Schema {
        root: schema,
        defs: Default::default(),
    };

    let err = deserialize_value_with_schema(&root, &doc, &store).unwrap_err();
    assert!(
        matches!(
            err,
            SchemaReadError::Deserialize(DeserializeError::MaxDepth(_))
        ),
        "expected MaxDepth from the combined schema + dynamic depth, got {err:?}"
    );
    Ok(())
}

/// A `Dynamic` node delegates to the bare heuristic: the same documented-lossy
/// reading a plain `deserialize::<Value>` produces.
#[test]
fn dynamic_delegates_to_heuristic() -> anyhow::Result<()> {
    let person = Person {
        name: "Alice".into(),
        age: 30,
        active: true,
    };
    let (root, store) = serialize(&person)?;
    let doc = Schema {
        root: Node::Dynamic,
        defs: Default::default(),
    };
    let via_schema = deserialize_value_with_schema(&root, &doc, &store)?;
    let via_heuristic: Value = deserialize(&root, &store)?;
    assert_eq!(via_schema, via_heuristic);
    // Lossy as documented: the number came back as its textual form.
    assert_eq!(
        via_schema,
        value!({ "name": "Alice", "age": "30", "active": "true" })
    );
    Ok(())
}

// --- validation ---

/// `validate_with_schema` succeeds on a conforming tree and fails on a
/// non-conforming one.
#[test]
fn validate_ok_and_err() -> anyhow::Result<()> {
    let (root, store) = serialize(&Person {
        name: "Alice".into(),
        age: 30,
        active: true,
    })?;
    validate_with_schema(&root, &schema_of::<Person>()?, &store)?;

    // A Person tree is not an empty tree, so a Unit schema must reject it.
    let unit = Schema {
        root: Node::Unit,
        defs: Default::default(),
    };
    let err = validate_with_schema(&root, &unit, &store).unwrap_err();
    assert!(
        matches!(err, SchemaReadError::MalformedUnit { found: 3 }),
        "expected MalformedUnit, got {err:?}"
    );
    Ok(())
}

/// A struct schema rejects a tree missing one of its fields, rather than
/// reading the field as absent.
#[test]
fn struct_read_requires_every_schema_field() -> anyhow::Result<()> {
    let (root, store) = serialize(&Point { x: 1.0, y: 2.0 })?;
    let mut doc = schema_of::<Point>()?;
    let Some(Node::Struct(fields)) = doc.defs.get_mut("Point") else {
        panic!("Point is a struct definition");
    };
    fields.insert("z".into(), Node::F64);

    let err = validate_with_schema(&root, &doc, &store).unwrap_err();
    assert!(
        matches!(&err, SchemaReadError::MissingField { field } if field == "z"),
        "expected MissingField, got {err:?}"
    );
    Ok(())
}

/// A struct schema rejects a tree carrying an entry it does not define, so a
/// tree that merely overlaps a schema does not pass as conforming.
#[test]
fn struct_read_rejects_entries_absent_from_the_schema() -> anyhow::Result<()> {
    let (root, store) = serialize(&Point { x: 1.0, y: 2.0 })?;
    let mut doc = schema_of::<Point>()?;
    let Some(Node::Struct(fields)) = doc.defs.get_mut("Point") else {
        panic!("Point is a struct definition");
    };
    fields.remove("y");

    let err = validate_with_schema(&root, &doc, &store).unwrap_err();
    assert!(
        matches!(&err, SchemaReadError::UnexpectedEntry { entry } if entry == "y"),
        "expected UnexpectedEntry, got {err:?}"
    );
    Ok(())
}
