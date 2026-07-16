//! Integration tests for [`RawTree`], the raw-passthrough tree field.
//!
//! Covers spec requirements:
//!   serialization.design.raw-tree — a `RawTree` field passes an already-written
//!                                   tree through unchanged, verified as a tree on read
//!   deserialization.roundtrip     — deserialize(serialize(x)) must equal x

use facet::Facet;
use facet_git_tree::{
    DeserializeError, EntryKind, ObjectStore, RawTree, deserialize, serialize_into,
};
use gix_object::{Kind, Write as _};

mod common;
use common::get_tree_entry_mode;

#[derive(Debug, Facet, PartialEq)]
struct WithRawTree {
    bin: RawTree,
    license: String,
}

/// A `RawTree` field's wrapped object id is written straight through as a tree
/// entry — the referenced subtree is not re-encoded or altered.
#[test]
fn raw_tree_passes_through_unchanged() {
    let store = ObjectStore::default();
    let file_oid = store.write_buf(Kind::Blob, b"#!/bin/sh\n").expect("blob");
    let bin_tree = gix_object::Tree {
        entries: vec![gix_object::tree::Entry {
            mode: gix_object::tree::EntryMode::from(EntryKind::Blob),
            filename: "run".into(),
            oid: file_oid,
        }],
    };
    let bin_oid = store.write(&bin_tree).expect("tree");

    let value = WithRawTree {
        bin: RawTree::new(bin_oid),
        license: "MIT".to_owned(),
    };
    let root = serialize_into(&value, &store).expect("serialize");

    let (kind, oid) = get_tree_entry_mode(&store, &root, "bin");
    assert_eq!(kind, EntryKind::Tree);
    assert_eq!(
        oid, bin_oid,
        "bin entry must be the pre-written tree, unchanged"
    );
}

/// `WithRawTree` roundtrips through the same backing store the `RawTree`
/// subtree was written into.
#[test]
fn raw_tree_roundtrips() {
    let store = ObjectStore::default();
    let file_oid = store.write_buf(Kind::Blob, b"binary").expect("blob");
    let bin_tree = gix_object::Tree {
        entries: vec![gix_object::tree::Entry {
            mode: gix_object::tree::EntryMode::from(EntryKind::Blob),
            filename: "tool".into(),
            oid: file_oid,
        }],
    };
    let bin_oid = store.write(&bin_tree).expect("tree");

    let value = WithRawTree {
        bin: RawTree::new(bin_oid),
        license: "Apache-2.0".to_owned(),
    };
    let root = serialize_into(&value, &store).expect("serialize");
    let decoded: WithRawTree = deserialize(&root, &store).expect("deserialize");

    assert_eq!(decoded, value);
    assert_eq!(decoded.bin.oid(), bin_oid);
}

/// A `RawTree` field pointing at a blob, rather than a tree, is rejected as
/// `NotATree` on read — the same guard ordinary tree decoding gets.
#[test]
fn raw_tree_over_a_blob_is_not_a_tree() {
    let store = ObjectStore::default();
    let blob_oid = store.write_buf(Kind::Blob, b"not a tree").expect("blob");

    let value = WithRawTree {
        bin: RawTree::new(blob_oid),
        license: "MIT".to_owned(),
    };
    let root = serialize_into(&value, &store).expect("serialize");
    let result: Result<WithRawTree, _> = deserialize(&root, &store);
    assert!(
        matches!(result, Err(DeserializeError::NotATree(oid)) if oid == blob_oid),
        "RawTree over a blob must be NotATree, got {result:?}"
    );
}
