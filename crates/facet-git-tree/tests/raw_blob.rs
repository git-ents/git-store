//! Integration tests for [`RawBlob`], the raw-passthrough blob field.
//!
//! A `RawBlob` preserves an already-written Git blob object id as a blob entry
//! in a typed tree and rejects references to objects of another kind when read.

use facet::Facet;
use facet_git_tree::{
    DeserializeError, EntryKind, ObjectStore, RawBlob, deserialize, serialize_into,
};
use gix_object::{Kind, Write as _};

mod common;
use common::get_tree_entry_mode;

#[derive(Debug, Facet, PartialEq)]
struct WithRawBlob {
    content: RawBlob,
    label: String,
}

#[test]
fn raw_blob_passes_through_unchanged_as_blob_entry() {
    let store = ObjectStore::default();
    let blob_oid = store
        .write_buf(Kind::Blob, b"already written\0binary")
        .expect("blob");

    let value = WithRawBlob {
        content: RawBlob::new(blob_oid),
        label: "tool".to_owned(),
    };
    let root = serialize_into(&value, &store).expect("serialize");

    let (kind, oid) = get_tree_entry_mode(&store, &root, "content");
    assert_eq!(kind, EntryKind::Blob);
    assert_eq!(oid, blob_oid);
}

#[test]
fn raw_blob_roundtrips() {
    let store = ObjectStore::default();
    let blob_oid = store.write_buf(Kind::Blob, b"payload").expect("blob");

    let value = WithRawBlob {
        content: RawBlob::new(blob_oid),
        label: "tool".to_owned(),
    };
    let root = serialize_into(&value, &store).expect("serialize");
    let decoded: WithRawBlob = deserialize(&root, &store).expect("deserialize");

    assert_eq!(decoded, value);
    assert_eq!(decoded.content.oid(), blob_oid);
}

#[test]
fn raw_blob_over_a_tree_is_not_a_blob() {
    let store = ObjectStore::default();
    let tree_oid = store
        .write(&gix_object::Tree { entries: vec![] })
        .expect("tree");

    let value = WithRawBlob {
        content: RawBlob::new(tree_oid),
        label: "tool".to_owned(),
    };
    let root = serialize_into(&value, &store).expect("serialize");
    let result: Result<WithRawBlob, _> = deserialize(&root, &store);

    assert!(
        matches!(result, Err(DeserializeError::NotABlob(oid)) if oid == tree_oid),
        "RawBlob over a tree must be NotABlob, got {result:?}"
    );
}
