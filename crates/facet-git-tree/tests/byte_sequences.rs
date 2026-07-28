//! Integration tests for byte-sequence encoding.
//!
//! Covers spec requirements:
//!   serialization.design.bytes — `u8` sequences are stored as a single blob
//!   deserialization.roundtrip  — deserialize(serialize(x)) must equal x

use facet::Facet;
use facet_git_tree::{EntryKind, serialize};
use rstest::rstest;

mod common;
use common::roundtrip;

#[derive(Debug, Facet, PartialEq)]
struct WithBytes {
    data: Vec<u8>,
}

#[derive(Debug, Facet, PartialEq)]
struct WithByteArray {
    hash: [u8; 4],
}

#[derive(Debug, Facet, PartialEq)]
struct TwoBuffers {
    a: Vec<u8>,
    b: Vec<u8>,
}

/// `Vec<u8>` roundtrips for a range of contents, including empty, embedded NUL
/// bytes, and the full byte range — none of which a textual encoding survives.
#[rstest]
#[case(vec![])]
#[case(vec![0])]
#[case(vec![0, 0, 0])]
#[case(b"\x7fELF\x02\x01\x01".to_vec())]
#[case((0u8..=255).collect())]
fn vec_u8_roundtrip(#[case] bytes: Vec<u8>) {
    assert_eq!(
        roundtrip(WithBytes {
            data: bytes.clone()
        }),
        WithBytes { data: bytes }
    );
}

/// A fixed-size `[u8; N]` array roundtrips through its blob encoding.
#[test]
fn byte_array_roundtrip() {
    assert_eq!(
        roundtrip(WithByteArray {
            hash: [0xde, 0xad, 0xbe, 0xef]
        }),
        WithByteArray {
            hash: [0xde, 0xad, 0xbe, 0xef]
        }
    );
}

/// A `Vec<u8>` field is stored as a single blob, not a per-byte tree, and that
/// blob holds the bytes verbatim followed by the mandatory trailing newline
/// every leaf blob carries (`serialization.design.leaves.encoding`).
#[test]
fn vec_u8_is_a_single_blob() {
    let bytes = b"\x00\x01\x02hello\xff".to_vec();
    let (root, store) = serialize(&WithBytes {
        data: bytes.clone(),
    })
    .expect("serialize");
    let (kind, oid) = common::get_tree_entry_mode(&store, &root, "data");
    assert_eq!(kind, EntryKind::Blob, "byte sequence must be a blob");
    let mut expected = bytes;
    expected.push(b'\n');
    assert_eq!(store.get_blob(&oid).expect("blob"), expected);
}

/// Two byte-identical buffers deduplicate to the same blob object.
#[test]
fn identical_buffers_dedup() {
    let (root, store) = serialize(&TwoBuffers {
        a: vec![1, 2, 3, 4],
        b: vec![1, 2, 3, 4],
    })
    .expect("serialize");
    let (_, a_oid) = common::get_tree_entry_mode(&store, &root, "a");
    let (_, b_oid) = common::get_tree_entry_mode(&store, &root, "b");
    assert_eq!(a_oid, b_oid, "equal buffers must share one blob");
}

/// An empty `Vec<u8>` roundtrips as empty (an empty blob), not as a missing field.
#[test]
fn empty_vec_u8_roundtrip() {
    assert_eq!(
        roundtrip(WithBytes { data: vec![] }),
        WithBytes { data: vec![] }
    );
}
