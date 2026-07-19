//! Integration tests for dynamic value serialization.
//!
//! Covers spec requirement:
//!   serialization.design.dynamic
//!     — a dynamic value serializes as its runtime kind's typed encoding:
//!       strings and bytes as raw blobs, booleans and numbers as their textual
//!       form, arrays as ordinal-keyed trees, objects as sorted name-keyed
//!       trees with validated keys, and null as the empty tree.

use facet_git_tree::{SerializeError, serialize};
use facet_value::{VDateTime, VObject, Value, value};

mod common;
use common::{HELLO_BLOB_OID, WithValue, find_entry, tree_entries};

// --- scalar kinds ---

/// A dynamic string is its UTF-8 bytes verbatim — the same blob (and thus the
/// same OID) as the equivalent typed `String`.
#[test]
fn string_blob_matches_typed_string() -> anyhow::Result<()> {
    let (dyn_root, dyn_store) = serialize(&Value::from("hello"))?;
    let (typed_root, _) = serialize(&"hello".to_string())?;
    assert_eq!(dyn_root, typed_root);
    assert_eq!(dyn_root.as_bytes(), HELLO_BLOB_OID.as_slice());
    assert_eq!(dyn_store.get_blob(&dyn_root).expect("blob"), b"hello");
    Ok(())
}

/// A dynamic bool is its textual form, exactly as a typed `bool` would encode.
#[test]
fn bool_is_textual_blob() -> anyhow::Result<()> {
    let (root, store) = serialize(&Value::from(true))?;
    assert_eq!(store.get_blob(&root).expect("blob"), b"true");
    Ok(())
}

/// Dynamic integers are their decimal text, signed and unsigned alike —
/// including a u64 too large for i64, which exercises the unsigned read.
#[test]
fn numbers_are_decimal_blobs() -> anyhow::Result<()> {
    let (root, store) = serialize(&Value::from(42i64))?;
    assert_eq!(store.get_blob(&root).expect("blob"), b"42");

    let (root, store) = serialize(&Value::from(-7i64))?;
    assert_eq!(store.get_blob(&root).expect("blob"), b"-7");

    let (root, store) = serialize(&Value::from(u64::MAX))?;
    assert_eq!(
        store.get_blob(&root).expect("blob"),
        b"18446744073709551615"
    );
    Ok(())
}

/// Null is the empty tree; an empty blob would collide with `""` and empty
/// bytes, which are far more common than null.
#[test]
fn null_is_empty_tree() -> anyhow::Result<()> {
    let (root, store) = serialize(&Value::NULL)?;
    assert!(tree_entries(&store, &root).is_empty());
    Ok(())
}

// --- containers ---

/// A dynamic array is an ordinal-keyed tree, exactly like a typed `Vec`.
#[test]
fn array_is_ordinal_tree() -> anyhow::Result<()> {
    let (root, store) = serialize(&value!(["a", "b", "c"]))?;
    let names: Vec<_> = tree_entries(&store, &root)
        .into_iter()
        .map(|e| e.filename.to_string())
        .collect();
    assert_eq!(names, ["0000", "0001", "0002"]);
    let first = find_entry(&store, &root, "0000");
    assert_eq!(store.get_blob(&first.oid).expect("blob"), b"a");
    Ok(())
}

/// A dynamic object is a name-keyed tree with entries in sorted order,
/// independent of insertion order.
#[test]
fn object_is_sorted_name_keyed_tree() -> anyhow::Result<()> {
    let mut obj = VObject::new();
    obj.insert("zeta", "z");
    obj.insert("alpha", "a");
    let (root, store) = serialize(&Value::from(obj))?;
    let names: Vec<_> = tree_entries(&store, &root)
        .into_iter()
        .map(|e| e.filename.to_string())
        .collect();
    assert_eq!(names, ["alpha", "zeta"]);
    let alpha = find_entry(&store, &root, "alpha");
    assert_eq!(store.get_blob(&alpha.oid).expect("blob"), b"a");
    Ok(())
}

/// An empty object is the empty tree (indistinguishable from null on disk —
/// a documented heuristic collision).
#[test]
fn empty_object_is_empty_tree() -> anyhow::Result<()> {
    let (root, store) = serialize(&Value::from(VObject::new()))?;
    assert!(tree_entries(&store, &root).is_empty());
    // The empty tree is one object: null and the empty object share an OID.
    let (null_root, _) = serialize(&Value::NULL)?;
    assert_eq!(root, null_root);
    Ok(())
}

// --- key validation ---

/// Object keys become tree entry names (path segments), so a key containing
/// `/` is rejected, exactly as for typed map keys.
#[test]
fn object_key_with_slash_is_rejected() {
    let mut obj = VObject::new();
    obj.insert("bad/key", "x");
    let err = serialize(&Value::from(obj)).unwrap_err();
    assert!(matches!(err, SerializeError::Key(_)), "got {err:?}");
}

// --- composition with typed values ---

/// A `Value` field inside a typed struct encodes exactly as the standalone
/// dynamic value would: the field entry's OID equals the standalone root.
#[test]
fn value_nested_in_typed_struct() -> anyhow::Result<()> {
    let meta = value!({ "k": "v" });
    let (root, store) = serialize(&WithValue { meta: meta.clone() })?;
    let entry = find_entry(&store, &root, "meta");
    let (standalone_root, _) = serialize(&meta)?;
    assert_eq!(entry.oid, standalone_root);
    let k = find_entry(&store, &entry.oid, "k");
    assert_eq!(store.get_blob(&k.oid).expect("blob"), b"v");
    Ok(())
}

// --- floats beyond 64/128 bits (`value` feature) ---

/// A finite float whose magnitude exceeds both `i128::MAX` and `u128::MAX`
/// still serializes: the `value`-feature fast path can tell — via
/// `VNumber::to_i128`/`to_u128` both returning `None` — that the value is
/// genuinely float-backed rather than the lossy image of an out-of-range
/// integer, so it renders through the same float encoding a typed `f64`
/// would use, producing an identical blob and OID.
#[cfg(feature = "value")]
#[test]
fn large_finite_float_matches_typed_f64() -> anyhow::Result<()> {
    let (dyn_root, dyn_store) = serialize(&Value::from(1e40_f64))?;
    let (typed_root, typed_store) = serialize(&1e40_f64)?;
    assert_eq!(dyn_root, typed_root);
    assert_eq!(
        dyn_store.get_blob(&dyn_root).expect("blob"),
        typed_store.get_blob(&typed_root).expect("blob"),
    );
    Ok(())
}

/// A whole-valued float that fits `u128` (unlike `1e40` above) exercises the
/// same fast path through its other branch — `to_u128` succeeds — and must
/// still match the typed encoding exactly.
#[cfg(feature = "value")]
#[test]
fn avogadro_float_matches_typed_f64() -> anyhow::Result<()> {
    let (dyn_root, dyn_store) = serialize(&Value::from(6.022e23_f64))?;
    let (typed_root, typed_store) = serialize(&6.022e23_f64)?;
    assert_eq!(dyn_root, typed_root);
    assert_eq!(
        dyn_store.get_blob(&dyn_root).expect("blob"),
        typed_store.get_blob(&typed_root).expect("blob"),
    );
    Ok(())
}

// --- datetime ---

/// A negative (BCE) year renders as `-` followed by its *magnitude*
/// zero-padded to four digits. Zero-padding the whole signed value instead
/// (`format!("{year:04}", ...)` with `year: i32`) would count the sign
/// character toward the width and under-pad the magnitude by one digit.
#[test]
fn datetime_negative_year_is_zero_padded_by_magnitude() -> anyhow::Result<()> {
    let dt = VDateTime::new_local_date(-5, 6, 15);
    let (root, store) = serialize(&Value::from(dt))?;
    assert_eq!(store.get_blob(&root).expect("blob"), b"-0005-06-15");
    Ok(())
}

// --- qname (`value` feature) ---

/// A `QName` with an empty-string namespace is indistinguishable from one
/// with no namespace at all: Clark notation reserves the empty-braces form
/// for "no namespace", so both constructions MUST serialize to the same
/// blob (and therefore the same OID).
#[cfg(feature = "value")]
#[test]
fn qname_empty_namespace_matches_no_namespace() -> anyhow::Result<()> {
    let with_empty_ns = facet_value::VQName::new("", "local");
    let no_ns = facet_value::VQName::new_local("local");
    let (root_a, _) = serialize(&Value::from(with_empty_ns))?;
    let (root_b, _) = serialize(&Value::from(no_ns))?;
    assert_eq!(root_a, root_b);
    Ok(())
}
