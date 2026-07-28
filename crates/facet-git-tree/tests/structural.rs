//! Integration tests for structural comparison.
//!
//! Covers spec requirement:
//!   structural.comparison.equality — same data produces the same root object ID

use facet::Facet;
use facet_git_tree::serialize;

mod common;
use common::{Config, HELLO_LEAF_BLOB_OID, Point, WithVec};

// --- structural.comparison.equality ---

/// Two identical values produce the same root object ID.
#[test]
fn identical_values_have_same_root_id() {
    let (id1, _) = serialize(&Point { x: 1.0, y: 2.0 }).expect("serialize ok");
    let (id2, _) = serialize(&Point { x: 1.0, y: 2.0 }).expect("serialize ok");
    assert_eq!(
        id1, id2,
        "identical values must produce the same root object ID"
    );
}

/// Values that differ in even one field produce distinct root object IDs.
#[test]
fn different_values_have_different_root_ids() {
    let (id1, _) = serialize(&Point { x: 1.0, y: 2.0 }).expect("serialize ok");
    let (id2, _) = serialize(&Point { x: 1.0, y: 3.0 }).expect("serialize ok");
    assert_ne!(
        id1, id2,
        "values that differ in a field must have distinct root object IDs"
    );
}

/// The root object ID is deterministic across separate serialize calls.
#[test]
fn serialization_is_deterministic() {
    let value = Config {
        name: "test".to_string(),
        value: 42,
    };
    let ids: Vec<_> = (0..5)
        .map(|_| serialize(&value).expect("serialize ok").0)
        .collect();
    let first = &ids[0];
    for id in &ids[1..] {
        assert_eq!(id, first, "each call must produce the same root id");
    }
}

/// Structural equality holds for nested structs.
#[test]
fn nested_identical_values_have_same_id() {
    #[derive(Debug, Facet)]
    struct Outer {
        inner: Point,
        tag: String,
    }

    let (id1, _) = serialize(&Outer {
        inner: Point { x: 1.0, y: 2.0 },
        tag: "a".to_string(),
    })
    .expect("serialize ok");
    let (id2, _) = serialize(&Outer {
        inner: Point { x: 1.0, y: 2.0 },
        tag: "a".to_string(),
    })
    .expect("serialize ok");
    assert_eq!(id1, id2);
}

/// Structural equality for Vec: same elements in same order → same ID.
#[test]
fn vec_equality() {
    let (id1, _) = serialize(&WithVec {
        items: vec![1, 2, 3],
    })
    .expect("serialize ok");
    let (id2, _) = serialize(&WithVec {
        items: vec![1, 2, 3],
    })
    .expect("serialize ok");
    assert_eq!(id1, id2);
}

/// Structural inequality for Vec: different order → different ID.
#[test]
fn vec_order_matters() {
    let (id1, _) = serialize(&WithVec {
        items: vec![1, 2, 3],
    })
    .expect("serialize ok");
    let (id2, _) = serialize(&WithVec {
        items: vec![3, 2, 1],
    })
    .expect("serialize ok");
    assert_ne!(id1, id2, "Vec order must affect the object ID");
}

/// A leaf blob's object ID equals the SHA-1 git would compute for the same blob.
///
/// The `name` field of this `Config` serializes to the blob `b"hello\n"` — a
/// leaf blob's mandatory trailing newline included, per
/// `serialization.design.leaves.encoding`. Git hashes blobs as
/// `sha1("blob 6\0hello\n")`; [`HELLO_LEAF_BLOB_OID`] is reproducible with
/// `printf 'hello\n' | git hash-object --stdin`. (Tree-level git compatibility
/// is pinned by `object_store::tree_oid_matches_git`.)
#[test]
fn leaf_blob_id_matches_git() {
    let (root_id, store) = serialize(&Config {
        name: "hello".to_string(),
        value: 0,
    })
    .expect("serialize ok");

    let entries = store.get_tree(&root_id).expect("root must be a tree");
    let name_entry = entries
        .iter()
        .find(|e| e.filename == "name")
        .expect("struct must have field `name`");

    assert_eq!(
        name_entry.oid.as_bytes(),
        HELLO_LEAF_BLOB_OID.as_slice(),
        "leaf blob id must match git's SHA-1 of `blob 6\\0hello\\n`"
    );
}
