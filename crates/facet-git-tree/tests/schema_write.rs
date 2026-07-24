//! Integration tests for schema-directed serialization
//! (`serialize_value_with_schema`, `value` feature).
//!
//! Covers spec requirement:
//!   serialization.schema-directed
//!     — a schema document encodes a dynamic `Value` to the same objects the
//!       equivalent typed value produces (byte-identical, thus same oid);
//!       encoding is validation, failing with the offending path; the accepted
//!       set is the image of the schema-driven read plus the JSON bridges
//!       (integer into a float node, string into a `Bytes` node).

use std::collections::HashMap;
use std::fmt::Debug;

use facet::Facet;
use facet_git_tree::{
    ObjectStore, RawTree, Schema, SchemaDoc, SchemaWriteError, deserialize_value_with_schema,
    schema_of, serialize, serialize_into, serialize_value_with_schema,
};
use facet_value::{VObject, Value, value};

mod common;
use common::{Event, Person, Point, TreeNode, WithArray, WithMap, WithOptional, WithVec};

/// The typed encoding's root oid: the ground truth every schema-directed write
/// must reproduce.
fn typed_root<T: for<'a> Facet<'a>>(value: &T) -> facet_git_tree::ObjectId {
    serialize(value).expect("typed serialize").0
}

/// Every value the schema-driven *read* can produce must re-encode to the exact
/// object it was read from — the round-trip property at the heart of the
/// accepted-set contract.
fn assert_reencodes<T>(value: T)
where
    T: for<'a> Facet<'a> + PartialEq + Debug,
{
    let (root, store) = serialize(&value).expect("typed serialize");
    let doc = schema_of::<T>().expect("schema_of");
    let read = deserialize_value_with_schema(&root, &doc, &store).expect("read");
    let out = ObjectStore::default();
    let written = serialize_value_with_schema(&read, &doc, &out).expect("schema write");
    assert_eq!(
        written, root,
        "re-encoding the read image of {value:?} must reproduce the typed oid"
    );
}

// --- the read image re-encodes identically ---

#[test]
fn reencodes_scalars_structs_and_collections() {
    assert_reencodes(Person {
        name: "Alice".into(),
        age: 30,
        active: true,
    });
    assert_reencodes(Point { x: 1.5, y: -2.25 });
    assert_reencodes(WithVec {
        items: vec![1, 2, 3],
    });
    assert_reencodes(WithArray {
        values: [10, 20, 30, 40],
    });
    assert_reencodes(WithOptional { maybe: Some(7) });
    assert_reencodes(WithOptional { maybe: None });
    assert_reencodes(TreeNode {
        value: 1,
        children: vec![TreeNode {
            value: 2,
            children: vec![],
        }],
    });
}

#[test]
fn reencodes_every_enum_variant() {
    assert_reencodes(Event::Ping);
    assert_reencodes(Event::Message("hi".into()));
    assert_reencodes(Event::Move(1, -2));
    assert_reencodes(Event::Login {
        user: "bob".into(),
        ok: true,
    });
}

#[test]
fn reencodes_scalar_key_map() {
    let mut table = HashMap::new();
    table.insert("greeting".to_string(), "hello".to_string());
    assert_reencodes(WithMap { table });
}

#[test]
fn reencodes_composite_key_map() {
    let mut table: HashMap<(u8, u8), String> = HashMap::new();
    table.insert((3, 4), "cell".into());
    table.insert((1, 2), "other".into());
    assert_reencodes(table);
}

#[test]
fn reencodes_128_bit_extremes() {
    #[derive(Debug, Facet, PartialEq)]
    struct Big {
        a: u128,
        b: i128,
    }
    assert_reencodes(Big {
        a: u128::MAX,
        b: i128::MIN,
    });
}

#[test]
fn reencodes_byte_sequence() {
    #[derive(Debug, Facet, PartialEq)]
    struct WithBytes {
        raw: Vec<u8>,
    }
    assert_reencodes(WithBytes {
        raw: vec![0, 1, 2, 255],
    });
}

// --- the JSON bridges (values outside the read image) ---

/// An integer supplied for an `f64` field is the canonical width-selection
/// bridge: JSON `1` must encode as the typed `f64` `1.0` would.
#[test]
fn integer_into_float_field_matches_typed() {
    let doc = schema_of::<Point>().unwrap();
    let store = ObjectStore::default();
    let written = serialize_value_with_schema(&value!({ "x": 1, "y": 2 }), &doc, &store).unwrap();
    assert_eq!(written, typed_root(&Point { x: 1.0, y: 2.0 }));
}

/// A string supplied for a `Bytes` field encodes as its UTF-8 bytes — the same
/// blob a typed `Vec<u8>` of those bytes produces.
#[test]
fn string_into_bytes_field_matches_typed() {
    #[derive(Facet)]
    struct WithBytes {
        raw: Vec<u8>,
    }
    let doc = schema_of::<WithBytes>().unwrap();
    let store = ObjectStore::default();
    let written = serialize_value_with_schema(&value!({ "raw": "hello" }), &doc, &store).unwrap();
    assert_eq!(
        written,
        typed_root(&WithBytes {
            raw: b"hello".to_vec()
        })
    );
}

/// A bare `Some` payload (an unwrapped value, as JSON yields) is normalized
/// into the `some`-wrapped tree the typed `Option` writes — the reason this
/// function exists at all.
#[test]
fn bare_option_payload_is_wrapped_like_typed() {
    let doc = schema_of::<WithOptional>().unwrap();
    let store = ObjectStore::default();

    let some = serialize_value_with_schema(&value!({ "maybe": 5 }), &doc, &store).unwrap();
    assert_eq!(some, typed_root(&WithOptional { maybe: Some(5) }));

    let none = serialize_value_with_schema(&value!({ "maybe": null }), &doc, &store).unwrap();
    assert_eq!(none, typed_root(&WithOptional { maybe: None }));
}

/// A `RawTree` node accepts the referenced object id as hex and embeds it as a
/// tree reference, writing no object — matching the typed `RawTree` encoding.
#[test]
fn raw_tree_reference_matches_typed() {
    #[derive(Facet)]
    struct WithRaw {
        raw: RawTree,
    }
    // A pre-written subtree both encodings will reference.
    let (inner, store) = serialize(&Point { x: 1.0, y: 2.0 }).unwrap();
    let typed = serialize_into(
        &WithRaw {
            raw: RawTree::new(inner),
        },
        &store,
    )
    .unwrap();

    let doc = schema_of::<WithRaw>().unwrap();
    let written =
        serialize_value_with_schema(&value!({ "raw": (inner.to_string()) }), &doc, &store).unwrap();
    assert_eq!(written, typed);
}

/// A `Schema::Dynamic` node delegates to the ordinary dynamic encoder, so it
/// matches a plain `serialize` of the same value.
#[test]
fn dynamic_node_matches_plain_serialize() {
    let doc = SchemaDoc {
        root: Schema::Dynamic,
        defs: Default::default(),
    };
    let v = value!({ "a": [1, 2], "b": "x" });
    let store = ObjectStore::default();
    let written = serialize_value_with_schema(&v, &doc, &store).unwrap();
    assert_eq!(written, typed_root(&v));
}

// --- conformance failures name the path ---

#[test]
fn unknown_field_is_rejected_with_path() {
    let doc = schema_of::<Person>().unwrap();
    let store = ObjectStore::default();
    let err = serialize_value_with_schema(
        &value!({ "name": "a", "age": 1, "active": true, "extra": 1 }),
        &doc,
        &store,
    )
    .unwrap_err();
    assert!(
        matches!(&err, SchemaWriteError::UnknownField { field, .. } if field == "extra"),
        "expected UnknownField, got {err:?}"
    );
}

#[test]
fn out_of_range_integer_is_rejected() {
    // `age: u32`, given a value beyond u32::MAX.
    let doc = schema_of::<Person>().unwrap();
    let store = ObjectStore::default();
    let err = serialize_value_with_schema(
        &value!({ "name": "a", "age": 5000000000i64, "active": true }),
        &doc,
        &store,
    )
    .unwrap_err();
    assert!(
        matches!(&err, SchemaWriteError::NumberOutOfRange { schema: "U32", path, .. } if path == "$.age"),
        "expected NumberOutOfRange at $.age, got {err:?}"
    );
}

#[test]
fn float_into_integer_field_is_rejected() {
    let doc = schema_of::<Person>().unwrap();
    let store = ObjectStore::default();
    let err = serialize_value_with_schema(
        &value!({ "name": "a", "age": 1.5, "active": true }),
        &doc,
        &store,
    )
    .unwrap_err();
    assert!(
        matches!(&err, SchemaWriteError::Expected { found: "float", path, .. } if path == "$.age"),
        "expected float rejection at $.age, got {err:?}"
    );
}

#[test]
fn integer_too_large_for_f64_is_refused() {
    // 2^53 + 1 has no exact f64 representation.
    let doc = SchemaDoc {
        root: Schema::F64,
        defs: Default::default(),
    };
    let store = ObjectStore::default();
    let err = serialize_value_with_schema(&value!(9007199254740993i64), &doc, &store).unwrap_err();
    assert!(
        matches!(
            err,
            SchemaWriteError::UnrepresentableNumber { schema: "F64", .. }
        ),
        "expected UnrepresentableNumber, got {err:?}"
    );
}

#[test]
fn type_mismatch_names_the_path() {
    let doc = schema_of::<Person>().unwrap();
    let store = ObjectStore::default();
    let err = serialize_value_with_schema(
        &value!({ "name": "a", "age": 1, "active": "yes" }),
        &doc,
        &store,
    )
    .unwrap_err();
    assert!(
        matches!(&err, SchemaWriteError::Expected { expected: "bool", found: "string", path } if path == "$.active"),
        "expected bool/string mismatch at $.active, got {err:?}"
    );
}

#[test]
fn tuple_length_mismatch_is_rejected() {
    // `Event::Move(i32, i32)` — a two-tuple; give it three.
    let doc = schema_of::<Event>().unwrap();
    let store = ObjectStore::default();
    let err =
        serialize_value_with_schema(&value!({ "Move": [1, 2, 3] }), &doc, &store).unwrap_err();
    assert!(
        matches!(
            &err,
            SchemaWriteError::LengthMismatch {
                expected: 2,
                found: 3,
                ..
            }
        ),
        "expected LengthMismatch, got {err:?}"
    );
}

#[test]
fn unknown_variant_is_rejected() {
    let doc = schema_of::<Event>().unwrap();
    let store = ObjectStore::default();
    let err = serialize_value_with_schema(&value!({ "Nope": null }), &doc, &store).unwrap_err();
    assert!(
        matches!(&err, SchemaWriteError::UnknownVariant { variant, .. } if variant == "Nope"),
        "expected UnknownVariant, got {err:?}"
    );
}

#[test]
fn multi_member_enum_object_is_rejected() {
    let doc = schema_of::<Event>().unwrap();
    let store = ObjectStore::default();
    let mut obj = VObject::new();
    obj.insert("Ping", Value::NULL);
    obj.insert("Message", "hi");
    let err = serialize_value_with_schema(&Value::from(obj), &doc, &store).unwrap_err();
    assert!(
        matches!(err, SchemaWriteError::MalformedEnum { found: 2, .. }),
        "expected MalformedEnum, got {err:?}"
    );
}

#[test]
fn unknown_ref_is_rejected() {
    let doc = SchemaDoc {
        root: Schema::Ref("missing".into()),
        defs: Default::default(),
    };
    let store = ObjectStore::default();
    let err = serialize_value_with_schema(&value!(1), &doc, &store).unwrap_err();
    assert!(
        matches!(&err, SchemaWriteError::UnknownRef { name, .. } if name == "missing"),
        "expected UnknownRef, got {err:?}"
    );
}

#[test]
fn ref_cycle_hits_the_depth_bound() {
    // A definition that refers only to itself: no value structure is ever
    // consumed, so only the depth bound stops the recursion.
    let mut defs = std::collections::BTreeMap::new();
    defs.insert("loop".to_string(), Schema::Ref("loop".into()));
    let doc = SchemaDoc {
        root: Schema::Ref("loop".into()),
        defs,
    };
    let store = ObjectStore::default();
    let err = serialize_value_with_schema(&value!(1), &doc, &store).unwrap_err();
    assert!(
        matches!(err, SchemaWriteError::MaxDepth { .. }),
        "expected MaxDepth, got {err:?}"
    );
}
