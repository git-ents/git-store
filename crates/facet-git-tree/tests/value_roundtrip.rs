//! Integration tests for dynamic value roundtrip behavior.
//!
//! Covers spec requirements:
//!   serialization.design.dynamic
//!     — the dynamic write mapping (blobs, ordinal trees, name-keyed trees).
//!   deserialization.dynamic.heuristic
//!     — the faithful subset (Strings, non-UTF-8 Bytes, non-empty Arrays and
//!       non-ordinal-keyed Objects thereof) roundtrips exactly; everything
//!       else degrades along the documented lossy mapping.

use facet_git_tree::{deserialize, serialize};
use facet_value::{VArray, VObject, Value, value};
use proptest::prelude::*;

mod common;
use common::roundtrip;

// --- the faithful subset ---

/// Strings roundtrip exactly: a UTF-8 blob always reads back as a String.
#[test]
fn string_roundtrips() {
    assert_eq!(roundtrip(Value::from("hello")), Value::from("hello"));
    assert_eq!(roundtrip(Value::from("")), Value::from(""));
}

/// Non-UTF-8 bytes roundtrip exactly: the blob cannot be read as a String, so
/// it comes back as Bytes verbatim.
#[test]
fn non_utf8_bytes_roundtrip() {
    let v = Value::from(vec![0xffu8, 0x00, 0x61]);
    assert_eq!(roundtrip(v.clone()), v);
}

/// A non-empty array of strings roundtrips exactly.
#[test]
fn array_of_strings_roundtrips() {
    let v = value!(["a", "b", "c"]);
    assert_eq!(roundtrip(v.clone()), v);
}

/// An object of strings (with non-ordinal keys) roundtrips exactly.
#[test]
fn object_of_strings_roundtrips() {
    let v = value!({ "alpha": "a", "beta": "b" });
    assert_eq!(roundtrip(v.clone()), v);
}

// --- property: the faithful subset always roundtrips ---

/// A strategy over the faithful subset: strings, guaranteed-non-UTF-8 bytes,
/// non-empty arrays, and objects with non-ordinal keys, nested a few levels.
fn faithful_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        // Any string, including the empty one (a UTF-8 blob is a String).
        "[a-z]{0,8}".prop_map(Value::from),
        // A leading 0xff byte is invalid anywhere in UTF-8, so these bytes can
        // never be mistaken for a String.
        proptest::collection::vec(any::<u8>(), 0..8).prop_map(|mut b| {
            b.insert(0, 0xff);
            Value::from(b)
        }),
    ];
    leaf.prop_recursive(3, 16, 4, |inner| {
        prop_oneof![
            // Arrays must be non-empty: an empty array writes the empty tree,
            // which reads back as an empty Object (documented lossy).
            proptest::collection::vec(inner.clone(), 1..4).prop_map(|items| {
                let mut arr = VArray::new();
                for item in items {
                    arr.push(item);
                }
                Value::from(arr)
            }),
            // Alphabetic keys can never look like ordinals, so these objects
            // (including the empty one) always read back as Objects.
            proptest::collection::btree_map("[a-z]{1,6}", inner, 0..4).prop_map(|entries| {
                let mut obj = VObject::new();
                for (k, v) in entries {
                    obj.insert(k.as_str(), v);
                }
                Value::from(obj)
            }),
        ]
    })
}

proptest! {
    /// Every value in the faithful subset survives serialize → deserialize
    /// unchanged.
    #[test]
    fn faithful_subset_roundtrips(v in faithful_value()) {
        let (root, store) = serialize(&v).expect("serialize should succeed");
        let back: Value = deserialize(&root, &store).expect("deserialize should succeed");
        prop_assert_eq!(back, v);
    }
}

// --- the documented lossy mappings ---

/// A bool has no on-disk marker, so it comes back as its textual form.
#[test]
fn bool_degrades_to_string() {
    assert_eq!(roundtrip(Value::from(true)), Value::from("true"));
}

/// A number has no on-disk marker, so it comes back as its decimal text.
#[test]
fn number_degrades_to_string() {
    assert_eq!(roundtrip(Value::from(42i64)), Value::from("42"));
}

/// Null writes the empty tree, which reads back as an empty Object.
#[test]
fn null_degrades_to_empty_object() {
    assert_eq!(roundtrip(Value::NULL), value!({}));
}

// --- content addressing ---

/// Equal dynamic values produce equal root OIDs, independent of construction
/// (object insertion) order — the content-addressing property.
#[test]
fn equal_values_share_a_root_oid() -> anyhow::Result<()> {
    let a = value!({ "k": ["x", "y"], "b": "c" });
    let mut obj = VObject::new();
    obj.insert("b", "c");
    obj.insert("k", value!(["x", "y"]));
    let b = Value::from(obj);
    assert_eq!(a, b);

    let (root_a, _) = serialize(&a)?;
    let (root_b, _) = serialize(&b)?;
    assert_eq!(root_a, root_b);

    // And serializing the same value twice is trivially stable.
    let (root_a2, _) = serialize(&a)?;
    assert_eq!(root_a, root_a2);
    Ok(())
}

// --- content addressing: dynamic floats vs. typed (`value` feature) ---

/// A fractional dynamic float shares its root OID with the equivalent typed
/// `f64` — content addressing holds across the typed/dynamic boundary for an
/// ordinary (in-range) float, not just for the large-magnitude case below.
#[cfg(feature = "value")]
#[test]
fn fractional_float_oid_matches_typed() -> anyhow::Result<()> {
    let (dyn_root, _) = serialize(&Value::from(3.5_f64))?;
    let (typed_root, _) = serialize(&3.5_f64)?;
    assert_eq!(dyn_root, typed_root);
    Ok(())
}

/// A dynamic float beyond `u128::MAX` also shares its root OID with the
/// equivalent typed `f64`, now that the `value`-feature fast path renders it
/// as a float instead of refusing it as an ambiguous whole value.
#[cfg(feature = "value")]
#[test]
fn large_float_oid_matches_typed() -> anyhow::Result<()> {
    let (dyn_root, _) = serialize(&Value::from(1e40_f64))?;
    let (typed_root, _) = serialize(&1e40_f64)?;
    assert_eq!(dyn_root, typed_root);
    Ok(())
}
