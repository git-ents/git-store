//! Integration tests for deserialization error paths.
//!
//! These exercise the `DeserializeError` variants that only arise on read,
//! against missing, mistyped, or externally-produced (foreign) trees — the
//! robustness a git interop library must have:
//!   DeserializeError::NotFound         — a referenced object is absent from the store
//!   DeserializeError::NotATree         — an object expected to be a tree is another kind
//!   DeserializeError::NonUtf8Name      — a foreign tree has a non-UTF-8 entry name
//!   DeserializeError::InvalidOrdinal   — a sequence entry is not named by a decimal index
//!   DeserializeError::MaxDepth         — a pathologically deep tree is rejected, not overflowed
//!   DeserializeError::MislabeledOption — an Option tree's single entry is not named "some"

use facet::Facet;
use facet_git_tree::{
    DeserializeError, EntryKind, EntryMode, ObjectId, ObjectStore, TreeEntry, deserialize,
};
use gix_object::bstr::BString;
use gix_object::{Kind, Tree, Write};

mod common;
use common::Point;

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
