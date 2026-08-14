//! Integration tests for deserialization error paths.
//!
//! These exercise the `DeserializeError` variants that only arise on read,
//! against missing, mistyped, or externally-produced (foreign) trees — the
//! robustness a git interop library must have:
//!   DeserializeError::NotFound         — a referenced object is absent from the store
//!   DeserializeError::NotATree         — an object expected to be a tree is another kind
//!   DeserializeError::NonUtf8Name      — a foreign tree has a non-UTF-8 entry name
//!   DeserializeError::InvalidOrdinal   — a sequence entry is not named by a decimal index
//!   DeserializeError::DuplicateOrdinal — two sequence entries name the same numeric index
//!   DeserializeError::MaxDepth         — a pathologically deep tree is rejected, not overflowed
//!   DeserializeError::MislabeledOption — an Option tree's single entry is not named "some"
//!   DeserializeError::MalformedOption  — a literal empty tree is no longer a valid `None`
//!   DeserializeError::UnitVariantIsTree    — a unit variant tagged with a tree, not a blob
//!   DeserializeError::VariantPayloadIsBlob — a non-unit variant tagged with a blob, not a tree
//!   DeserializeError::MissingLeafNewline   — a leaf blob is missing its mandatory trailing newline

use facet::Facet;
use facet_git_tree::{
    DeserializeError, EntryKind, EntryMode, ObjectId, ObjectStore, TreeEntry, deserialize,
    deserialize_legacy_leaves,
};
use gix_object::bstr::BString;
use gix_object::{Kind, Tree, Write};

mod common;
use common::Point;

#[derive(Debug, Facet, PartialEq)]
#[repr(u8)]
enum Shape {
    Unit,
    Circle { radius: f64 },
}

/// Write a tree of `(name, kind, oid)` entries and return its id.
fn write_tree(store: &ObjectStore, entries: &[(&str, EntryKind, ObjectId)]) -> ObjectId {
    let entries = entries
        .iter()
        .map(|(name, kind, oid)| TreeEntry {
            mode: EntryMode::from(*kind),
            filename: BString::from(*name),
            oid: *oid,
        })
        .collect();
    store.write(&Tree { entries }).expect("write tree")
}

/// Deserializing from a root id absent from the store yields `NotFound`.
#[test]

fn missing_root_object_is_not_found() {
    // An id produced in one store, queried in an empty one, is absent.
    let written = ObjectStore::default();
    let id = written.write_buf(Kind::Blob, b"hello").expect("write blob");

    let empty = ObjectStore::default();
    let result: Result<Point, _> = deserialize(&id, &empty);
    assert!(
        matches!(result, Err(DeserializeError::NotFound(_))),
        "absent root must be NotFound"
    );
}

/// Deserializing where the root id points to a blob yields `NotATree`.
#[test]

fn blob_root_is_not_a_tree() {
    let store = ObjectStore::default();
    let blob_id = store
        .write_buf(Kind::Blob, b"not a tree")
        .expect("write blob");

    let result: Result<Point, _> = deserialize(&blob_id, &store);
    assert!(
        matches!(result, Err(DeserializeError::NotATree(_))),
        "blob root must be NotATree"
    );
}

/// A foreign tree with a non-UTF-8 entry name is rejected as `NonUtf8Name`.
#[test]

fn non_utf8_entry_name_is_rejected() {
    let store = ObjectStore::default();
    let blob = store.write_buf(Kind::Blob, b"v").expect("write blob");

    // Only an externally-produced tree can contain a non-UTF-8 name, which is
    // exactly the foreign input read must tolerate (and reject cleanly).
    let tree = Tree {
        entries: vec![TreeEntry {
            mode: EntryMode::from(EntryKind::Blob),
            filename: BString::from(vec![0xff_u8, 0xfe]),
            oid: blob,
        }],
    };
    let tree_id = store.write(&tree).expect("write tree");

    let result: Result<Point, _> = deserialize(&tree_id, &store);
    assert!(
        matches!(result, Err(DeserializeError::NonUtf8Name(_))),
        "non-UTF-8 entry name must be NonUtf8Name"
    );
}

/// A foreign sequence tree with a non-numeric entry name is rejected rather than
/// silently misordered.
#[test]
fn non_numeric_sequence_ordinal_is_rejected() {
    let store = ObjectStore::default();
    let elem = store.write_buf(Kind::Blob, b"1").expect("write blob");
    // A Vec element must be named by its decimal index; "x" never is.
    let tree_id = write_tree(&store, &[("x", EntryKind::Blob, elem)]);

    let result: Result<Vec<i64>, _> = deserialize(&tree_id, &store);
    assert!(
        matches!(result, Err(DeserializeError::InvalidOrdinal(name)) if name == "x"),
        "non-numeric ordinal must be InvalidOrdinal"
    );
}

/// A foreign sequence tree with two entries naming the same numeric ordinal
/// (`"0"` and `"0000"`, distinct strings that parse to the same index) is
/// rejected rather than silently resolved by insertion or lexical order.
#[test]
fn duplicate_sequence_ordinal_is_rejected() {
    let store = ObjectStore::default();
    let a = store.write_buf(Kind::Blob, b"1").expect("write blob");
    let b = store.write_buf(Kind::Blob, b"2").expect("write blob");
    let tree_id = write_tree(
        &store,
        &[("0", EntryKind::Blob, a), ("0000", EntryKind::Blob, b)],
    );

    let result: Result<Vec<i64>, _> = deserialize(&tree_id, &store);
    assert!(
        matches!(result, Err(DeserializeError::DuplicateOrdinal(0))),
        "duplicate ordinal must be rejected, got {result:?}"
    );
}

/// A foreign `Option` tree whose single entry is not named `some` is rejected
/// rather than read positionally.
#[test]
fn mislabeled_option_entry_is_rejected() {
    let store = ObjectStore::default();
    let inner = store.write_buf(Kind::Blob, b"5").expect("write blob");
    let tree_id = write_tree(&store, &[("nope", EntryKind::Blob, inner)]);

    let result: Result<Option<i32>, _> = deserialize(&tree_id, &store);
    assert!(
        matches!(&result, Err(DeserializeError::MislabeledOption { name }) if name == "nope"),
        "mislabeled Option entry must be rejected, got {result:?}"
    );
}

/// Per issue 8d109650, `None` is now written as the presence-marker tree, not
/// a literal empty tree; a literal empty tree is therefore foreign input and
/// rejected as `MalformedOption { found: 0 }` rather than accepted as `None`.
#[test]
fn literal_empty_tree_is_no_longer_a_valid_option() {
    let store = ObjectStore::default();
    let tree_id = write_tree(&store, &[]);

    let result: Result<Option<i32>, _> = deserialize(&tree_id, &store);
    assert!(
        matches!(result, Err(DeserializeError::MalformedOption { found: 0 })),
        "a literal empty tree must no longer decode as None, got {result:?}"
    );

    let legacy: Option<i32> = deserialize_legacy_leaves(&tree_id, &store).unwrap();
    assert_eq!(legacy, None);
}

/// A foreign tree tagging a *unit* variant (`Shape::Unit`) with a tree
/// instead of the required bare name-blob is rejected as `UnitVariantIsTree`.
#[test]
fn unit_variant_tagged_with_tree_is_rejected() {
    let store = ObjectStore::default();
    let payload = write_tree(&store, &[]);
    let tree_id = write_tree(&store, &[("Unit", EntryKind::Tree, payload)]);

    let result: Result<Shape, _> = deserialize(&tree_id, &store);
    assert!(
        matches!(&result, Err(DeserializeError::UnitVariantIsTree { variant }) if variant == "Unit"),
        "a unit variant tagged with a tree must be rejected, got {result:?}"
    );
}

/// A foreign object naming a *non-unit* variant (`Shape::Circle`) that is
/// itself a bare blob, rather than the required payload tree, is rejected as
/// `VariantPayloadIsBlob`.
#[test]
fn non_unit_variant_tagged_with_blob_is_rejected() {
    let store = ObjectStore::default();
    // A well-formed leaf blob (trailing newline included) so this exercises
    // the variant-shape mismatch specifically, not `MissingLeafNewline`.
    let blob_id = store
        .write_buf(Kind::Blob, b"Circle\n")
        .expect("write blob");

    let result: Result<Shape, _> = deserialize(&blob_id, &store);
    assert!(
        matches!(
            &result,
            Err(DeserializeError::VariantPayloadIsBlob { variant }) if variant == "Circle"
        ),
        "a non-unit variant tagged with a blob must be rejected, got {result:?}"
    );
}

/// A leaf blob missing its mandatory trailing newline — here, a unit
/// variant's name blob written without it — is rejected as
/// `MissingLeafNewline` rather than silently accepted as if the byte were
/// optional. Per `serialization.design.leaves.encoding`, the rule is
/// "exactly one, always present", not "at most one".
#[test]
fn leaf_blob_missing_trailing_newline_is_rejected() {
    let store = ObjectStore::default();
    let blob_id = store.write_buf(Kind::Blob, b"Unit").expect("write blob");

    let result: Result<Shape, _> = deserialize(&blob_id, &store);
    assert!(
        matches!(&result, Err(DeserializeError::MissingLeafNewline(oid)) if *oid == blob_id),
        "a leaf blob without its trailing newline must be MissingLeafNewline, got {result:?}"
    );
}

/// A self-referential type whose foreign tree nests past the depth guard is
/// rejected with `MaxDepth` instead of overflowing the stack.
#[test]
fn excessively_deep_tree_is_rejected() {
    #[derive(Debug, Facet)]
    struct DeepNode {
        children: Vec<DeepNode>,
    }

    let store = ObjectStore::default();
    // A leaf node: `{ children: [] }`.
    let empty_list = write_tree(&store, &[]);
    let mut node = write_tree(&store, &[("children", EntryKind::Tree, empty_list)]);
    // Each wrap adds a `struct → list → struct` layer (~2 levels of recursion);
    // 100 wraps is comfortably past the guard.
    for _ in 0..100 {
        let list = write_tree(&store, &[("0000", EntryKind::Tree, node)]);
        node = write_tree(&store, &[("children", EntryKind::Tree, list)]);
    }

    let result: Result<DeepNode, _> = deserialize(&node, &store);
    assert!(
        matches!(result, Err(DeserializeError::MaxDepth(_))),
        "deeply nested tree must be MaxDepth, got {result:?}"
    );
}
