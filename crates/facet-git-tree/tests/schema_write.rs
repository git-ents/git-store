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
    EntryKind, FieldSchema, ObjectStore, RawTree, Schema, SchemaDoc, SchemaWriteError, VariantKind,
    VariantSchema, deserialize_value_with_schema, schema_of, serialize, serialize_into,
    serialize_value_with_schema,
};
use facet_value::{VObject, Value, value};

mod common;
use common::{
    Event, Person, Point, TreeNode, WithArray, WithMap, WithOptional, WithVec, find_entry,
};

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

// --- regression: unit-variant / empty-collection visibility (issue 8d109650) ---

/// The `git-store` `task.json` repro: a struct with a `priority: Priority`
/// field, `Priority` a plain enum of unit variants (`Low`, `Medium`, `High`).
/// Storing a task and then flipping `priority` from `Low` to `High` must
/// change the `priority` entry's own *blob content* — the property that
/// makes `git diff`/`git log --stat` non-empty and `priority` appear in
/// `git ls-tree -r`, none of which held before this fix (the variant name
/// lived only in a tree-entry name, and both `Low` and `High` resolved to the
/// same empty-tree payload).
#[test]
fn priority_field_change_is_a_visible_blob_diff() {
    let priority_variants = vec![
        VariantSchema {
            name: "Low".into(),
            kind: VariantKind::Unit,
        },
        VariantSchema {
            name: "Medium".into(),
            kind: VariantKind::Unit,
        },
        VariantSchema {
            name: "High".into(),
            kind: VariantKind::Unit,
        },
    ];
    let doc = SchemaDoc {
        root: Schema::Struct(vec![FieldSchema {
            name: "priority".into(),
            schema: Schema::Enum(priority_variants),
        }]),
        defs: Default::default(),
    };

    let low_store = ObjectStore::default();
    let low_root =
        serialize_value_with_schema(&value!({ "priority": { "Low": null } }), &doc, &low_store)
            .expect("serialize Low");
    let high_store = ObjectStore::default();
    let high_root =
        serialize_value_with_schema(&value!({ "priority": { "High": null } }), &doc, &high_store)
            .expect("serialize High");

    assert_ne!(
        low_root, high_root,
        "changing priority must change the task's root id"
    );

    let low_entry = find_entry(&low_store, &low_root, "priority");
    let high_entry = find_entry(&high_store, &high_root, "priority");
    assert_eq!(
        low_entry.mode.kind(),
        EntryKind::Blob,
        "a unit-variant field must be a blob entry, not a tree — this is what makes it \
         visible to `git ls-tree -r`"
    );
    assert_eq!(high_entry.mode.kind(), EntryKind::Blob);
    assert_ne!(
        low_entry.oid, high_entry.oid,
        "the `priority` entry's oid must differ — this is what makes `git diff` non-empty"
    );
    assert_eq!(low_store.get_blob(&low_entry.oid).expect("blob"), b"Low\n");
    assert_eq!(
        high_store.get_blob(&high_entry.oid).expect("blob"),
        b"High\n"
    );
}

/// An empty collection (here, an empty `tags: List<String>`) writes the
/// presence-marker tree, so `tags` appears as a real (marker) entry in
/// `git ls-tree -r` rather than vanishing as an empty, unlisted directory.
#[test]
fn empty_tags_list_is_visible_as_a_marker_entry() {
    let doc = SchemaDoc {
        root: Schema::Struct(vec![FieldSchema {
            name: "tags".into(),
            schema: Schema::List(Box::new(Schema::String)),
        }]),
        defs: Default::default(),
    };
    let store = ObjectStore::default();
    let root = serialize_value_with_schema(&value!({ "tags": [] }), &doc, &store)
        .expect("serialize empty tags");
    let tags_entry = find_entry(&store, &root, "tags");
    assert_eq!(tags_entry.mode.kind(), EntryKind::Tree);
    let marker = find_entry(&store, &tags_entry.oid, "_");
    assert_eq!(
        marker.mode.kind(),
        EntryKind::Blob,
        "an empty List must hold a visible marker entry, not be a bare empty tree"
    );

    // And it still reads back as an empty list.
    let v = deserialize_value_with_schema(&root, &doc, &store).expect("read");
    assert_eq!(v, value!({ "tags": [] }));
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

// --- the presence marker does not swallow fixed-arity or forged shapes ---

/// A zero-element `Schema::Tuple` MUST encode as the literal empty tree, not
/// the presence marker.
///
/// A tuple's arity is fixed by the schema, so an empty one encodes identically
/// for every value and there is nothing for a diff to show — the same reason a
/// unit struct goes unmarked. Marking it broke both directions at once: the
/// written tree no longer matched the typed encoder's oid (violating the
/// byte-identity contract this module exists to enforce), and it could not be
/// read back at all, because the tuple read length-checks the entries it finds
/// before any marker could be stripped. That made `put` succeed and `get` fail
/// permanently on the same data.
#[test]
fn an_empty_tuple_is_not_markered_and_reads_back() {
    let doc = SchemaDoc {
        root: Schema::Struct(vec![FieldSchema {
            name: "nothing".into(),
            schema: Schema::Tuple(vec![]),
        }]),
        defs: Default::default(),
    };
    let store = ObjectStore::default();
    let v = value!({ "nothing": [] });

    let root = serialize_value_with_schema(&v, &doc, &store).expect("write an empty tuple");
    let back = deserialize_value_with_schema(&root, &doc, &store)
        .expect("an empty tuple written by the schema writer must read back");
    assert_eq!(back, v);
}

/// The same, against the typed encoder: a zero-field tuple struct must produce
/// the identical object id through both paths.
#[test]
fn zero_field_tuple_struct_matches_typed() {
    #[derive(Facet, PartialEq, Debug)]
    struct Zero();

    #[derive(Facet, PartialEq, Debug)]
    struct HasZero {
        z: Zero,
    }

    assert_reencodes(HasZero { z: Zero() });
}

/// A schema-declared field named exactly the reserved presence marker MUST be
/// rejected at write time.
///
/// A `SchemaDoc` is data — `git store schema put` ingests one from hand-written
/// JSON — so a field name is untrusted input, and the `#[derive(Facet)]`
/// guarantee that a field cannot be named a bare `_` does not hold for it.
/// Left unchecked, such a field encoded to a tree byte-identical to the one
/// meaning "empty", and read back as an empty collection: a silent, lossy
/// round-trip rather than a loud failure.
#[test]
fn a_schema_field_named_as_the_presence_marker_is_rejected() {
    let doc = SchemaDoc {
        root: Schema::Struct(vec![FieldSchema {
            name: "_".into(),
            schema: Schema::String,
        }]),
        defs: Default::default(),
    };
    let store = ObjectStore::default();
    let err = serialize_value_with_schema(&value!({ "_": "" }), &doc, &store).unwrap_err();
    assert!(
        matches!(&err, SchemaWriteError::Serialize(_)),
        "a field named as the reserved marker must be rejected, got {err:?}"
    );
}
