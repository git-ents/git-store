//! Integration tests for leaf value normalization on write.
//!
//! Covers spec requirement:
//!   serialization.design.normalization.leaves
//!     — negative zero must be normalized to positive zero
//!     — NaN must be normalized to the unquoted string "nan"

use facet::Facet;
use facet_git_tree::{EntryKind, serialize};

#[derive(Debug, Facet)]
struct WithF32 {
    v: f32,
}

#[derive(Debug, Facet)]
struct WithF64 {
    v: f64,
}

/// Retrieve the blob bytes for the single-field wrapper struct's `v` entry.
fn serialize_scalar_f64(value: f64) -> Vec<u8> {
    let (root_id, store) = serialize(&WithF64 { v: value }).expect("serialize should succeed");
    let entries = store.get_tree(&root_id).expect("root must be a tree");
    let v_entry = entries
        .iter()
        .find(|e| e.filename == "v")
        .expect("struct must have field `v`");
    assert_eq!(
        v_entry.mode.kind(),
        EntryKind::Blob,
        "scalar `v` must serialize to a blob"
    );
    store
        .get_blob(&v_entry.oid)
        .expect("blob must be in store")
        .to_vec()
}

fn serialize_scalar_f32(value: f32) -> Vec<u8> {
    let (root_id, store) = serialize(&WithF32 { v: value }).expect("serialize should succeed");
    let entries = store.get_tree(&root_id).expect("root must be a tree");
    let v_entry = entries
        .iter()
        .find(|e| e.filename == "v")
        .expect("struct must have field `v`");
    store
        .get_blob(&v_entry.oid)
        .expect("blob must be in store")
        .to_vec()
}

// --- negative zero normalization ---

/// f64 negative zero and positive zero produce identical blob content.
#[test]

fn f64_negative_zero_normalized_to_positive_zero() {
    let pos_zero = serialize_scalar_f64(0.0_f64);
    let neg_zero = serialize_scalar_f64(-0.0_f64);
    assert_eq!(
        pos_zero, neg_zero,
        "f64 -0.0 must serialize identically to +0.0"
    );
}

/// f32 negative zero and positive zero produce identical blob content.
#[test]

fn f32_negative_zero_normalized_to_positive_zero() {
    let pos_zero = serialize_scalar_f32(0.0_f32);
    let neg_zero = serialize_scalar_f32(-0.0_f32);
    assert_eq!(
        pos_zero, neg_zero,
        "f32 -0.0 must serialize identically to +0.0"
    );
}

/// Two structs that differ only in the sign of zero produce the same root object ID.
///
/// This verifies that normalization happens before hashing, ensuring that
/// positive and negative zero are structurally equal.
#[test]

fn negative_zero_structural_equality() {
    let (id_pos, _) = serialize(&WithF64 { v: 0.0_f64 }).expect("serialize should succeed");
    let (id_neg, _) = serialize(&WithF64 { v: -0.0_f64 }).expect("serialize should succeed");
    assert_eq!(
        id_pos, id_neg,
        "structs with +0.0 and -0.0 must have identical root object IDs"
    );
}

// --- NaN normalization ---

/// f64 NaN serializes to the unquoted string literal "nan".
#[test]

fn f64_nan_normalized_to_string_nan() {
    let bytes = serialize_scalar_f64(f64::NAN);
    assert_eq!(
        bytes, b"nan\n",
        "f64 NaN must serialize to the unquoted string `nan`"
    );
}

/// f32 NaN serializes to the unquoted string literal "nan".
#[test]

fn f32_nan_normalized_to_string_nan() {
    let bytes = serialize_scalar_f32(f32::NAN);
    assert_eq!(
        bytes, b"nan\n",
        "f32 NaN must serialize to the unquoted string `nan`"
    );
}

/// All NaN bit patterns serialize to the same "nan" blob, not to distinct values.
#[test]

fn different_nan_payloads_normalize_identically() {
    // Construct two different NaN payloads via bit manipulation.
    let nan1 = f64::from_bits(0x7FF8_0000_0000_0001);
    let nan2 = f64::from_bits(0x7FF0_0000_0000_0001);
    assert!(nan1.is_nan());
    assert!(nan2.is_nan());

    let bytes1 = serialize_scalar_f64(nan1);
    let bytes2 = serialize_scalar_f64(nan2);
    assert_eq!(
        bytes1, bytes2,
        "all NaN payloads must normalize to the same representation"
    );
    assert_eq!(bytes1, b"nan\n");
}
