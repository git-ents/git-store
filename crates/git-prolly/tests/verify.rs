//! Verification invariants: every structural corruption is rejected, and
//! Prolly validity is checked on top of — never conflated with — Git validity.

#![expect(
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests index and panic deliberately"
)]

mod common;

use common::{TestRepo, repo};
use facet_value::Value;
use git_prolly::{HexKeyCodec, KeyCodec, ProllyConfig, ProllyStore, VerifyError};
use gix::ObjectId;
use gix::bstr::ByteSlice;
use gix::objs::Write as _;
use gix::objs::tree::{EntryKind, EntryMode};

/// The marker entry's reserved name and content (see `src/node.rs`).
const MARKER_NAME: &[u8] = b"!";
const MARKER_CONTENT: &[u8] = b"git-prolly:1:internal\n";

/// One raw tree entry: mode, name, and target object id.
type RawEntry = (EntryMode, Vec<u8>, ObjectId);

fn owned_entries(repo: &gix::Repository, oid: ObjectId) -> Vec<RawEntry> {
    let tree = repo.find_tree(oid).expect("tree exists");
    tree.decode()
        .expect("decode")
        .entries
        .iter()
        .map(|entry| (entry.mode, entry.filename.to_vec(), entry.oid.to_owned()))
        .collect()
}

/// Write a tree whose contents deliberately violate Prolly invariants.
///
/// gitoxide's tree serializer refuses unsorted or malformed entries, so the
/// corruption harness encodes the tree bytes by hand and stores them through
/// the object database's known-id path, which performs no validation.
fn write_raw_tree(repo: &gix::Repository, entries: &[RawEntry]) -> ObjectId {
    let mut buf = Vec::new();
    for (mode, name, oid) in entries {
        // Git writes tree modes as "40000" (no leading zero) and everything
        // else as its plain octal value.
        buf.extend_from_slice(format!("{:o}", mode.value()).as_bytes());
        buf.push(b' ');
        buf.extend_from_slice(name);
        buf.push(0);
        buf.extend_from_slice(oid.as_bytes());
    }
    let oid = gix::objs::compute_hash(repo.object_hash(), gix::objs::Kind::Tree, &buf)
        .expect("hash corrupt tree");
    repo.write_buf_with_known_id(gix::objs::Kind::Tree, &buf, oid)
        .expect("write corrupt tree")
}

fn encode(codec: &HexKeyCodec, key: &[u8]) -> Vec<u8> {
    codec.encode(key).expect("encode key")
}

/// Valid trees verify: empty, single-leaf, and multi-level.
#[test]
fn valid_trees_verify() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    assert!(
        store.verify(store.empty_root()).is_ok(),
        "empty root verifies"
    );

    let small = store
        .build((0..10_u32).map(|i| (format!("k{i}").into_bytes(), Value::from("v"))))
        .expect("build small");
    assert!(store.verify(small).is_ok(), "single-leaf root verifies");

    let large = store
        .build((0..500_u32).map(|i| (format!("k{i:05}").into_bytes(), Value::from("v"))))
        .expect("build large");
    assert!(store.verify(large).is_ok(), "multi-level root verifies");
}

/// A leaf whose entries are out of order is rejected.
#[test]
fn unordered_leaf_is_rejected() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let root = store
        .build((0..10_u32).map(|i| (format!("k{i}").into_bytes(), Value::from("v"))))
        .expect("build");
    let mut entries = owned_entries(&repo, root);
    entries.swap(0, 1);
    let corrupt = write_raw_tree(&repo, &entries);
    // Depending on the chunking, the root is a leaf (the swap is an ordering
    // violation) or internal (the swap displaces the marker, a key-decoding
    // violation); either way the corruption is rejected.
    assert!(
        matches!(
            store.verify(corrupt),
            Err(VerifyError::Unordered { .. }
                | VerifyError::NonCanonical { .. }
                | VerifyError::BadKey { .. })
        ),
        "swapped entries must be rejected"
    );
}

/// An internal node whose separator does not match its child's first key is
/// rejected.
#[test]
fn bad_separator_is_rejected() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let codec = HexKeyCodec;
    let root = store
        .build((0..300_u32).map(|i| (format!("k{i:05}").into_bytes(), Value::from("v"))))
        .expect("build");
    let mut entries = owned_entries(&repo, root);
    assert!(
        entries.len() >= 2,
        "the root must be internal for this test"
    );
    // Corrupt the first child's separator to a key that is not its first key.
    entries[1].1 = encode(&codec, b"k000009");
    let corrupt = write_raw_tree(&repo, &entries);
    match store.verify(corrupt) {
        Err(VerifyError::BadSeparator { .. } | VerifyError::NonCanonical { .. }) => {}
        other => panic!("expected separator rejection, got {other:?}"),
    }
}

/// A child reference to a missing object is rejected.
#[test]
fn missing_child_is_rejected() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let root = store
        .build((0..300_u32).map(|i| (format!("k{i:05}").into_bytes(), Value::from("v"))))
        .expect("build");
    let mut entries = owned_entries(&repo, root);
    entries[1].2 = ObjectId::from_hex(b"abababababababababababababababababababab").expect("oid");
    let corrupt = write_raw_tree(&repo, &entries);
    assert!(
        store.verify(corrupt).is_err(),
        "missing child must be rejected"
    );
}

/// A tree built under one configuration does not verify under another: chunk
/// boundaries are part of the format, not an implementation detail.
#[test]
fn foreign_configuration_is_rejected() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let root = store
        .build((0..300_u32).map(|i| (format!("k{i:05}").into_bytes(), Value::from("v"))))
        .expect("build");
    let foreign = ProllyConfig {
        bits: 6,
        ..ProllyConfig::default()
    };
    let other = ProllyStore::with_config(&repo, foreign).expect("config");
    match other.verify(root) {
        Err(VerifyError::NonCanonical { .. }) => {}
        other => panic!("expected non-canonical rejection, got {other:?}"),
    }
}

/// Swapped leaf values (same keys, different value objects) are rejected.
#[test]
fn corrupted_leaf_contents_are_rejected() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let root = store
        .build((0..10_u32).map(|i| (format!("k{i}").into_bytes(), Value::from(format!("v{i}")))))
        .expect("build");
    let mut entries = owned_entries(&repo, root);
    entries.swap(2, 3);
    let corrupt = write_raw_tree(&repo, &entries);
    assert!(
        store.verify(corrupt).is_err(),
        "swapped values must be rejected"
    );
}

/// A leaf entry whose mode does not match the referenced object's kind is
/// rejected.
#[test]
fn wrong_mode_is_rejected() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let root = store
        .build((0..6_u32).map(|i| (format!("k{i}").into_bytes(), Value::from("text"))))
        .expect("build");
    let mut entries = owned_entries(&repo, root);
    entries[2].0 = EntryMode::from(EntryKind::Link);
    let corrupt = write_raw_tree(&repo, &entries);
    match store.verify(corrupt) {
        Err(VerifyError::WrongMode { .. }) => {}
        other => panic!("expected wrong-mode rejection, got {other:?}"),
    }
}

/// An internal node whose marker blob does not carry the format's content is
/// rejected.
#[test]
fn bad_marker_content_is_rejected() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let root = store
        .build((0..300_u32).map(|i| (format!("k{i:05}").into_bytes(), Value::from("v"))))
        .expect("build");
    let mut entries = owned_entries(&repo, root);
    entries[0].2 = repo
        .write_buf(gix::objs::Kind::Blob, b"someone else's blob\n")
        .expect("write wrong marker");
    let corrupt = write_raw_tree(&repo, &entries);
    match store.verify(corrupt) {
        Err(VerifyError::BadMarker { .. }) => {}
        other => panic!("expected bad-marker rejection, got {other:?}"),
    }
}

/// A marker entry that does not carry the marker blob mode is rejected.
///
/// (A marker stripped *entirely* is, by construction, indistinguishable from a
/// legitimate single-leaf tree whose keys happen to be the old separators —
/// the marker is what defines node roles, which is why its mode, name, and
/// content are pinned and the remaining checks are so tight.)
#[test]
fn marker_with_wrong_mode_is_rejected() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let root = store
        .build((0..300_u32).map(|i| (format!("k{i:05}").into_bytes(), Value::from("v"))))
        .expect("build");
    let mut entries = owned_entries(&repo, root);
    assert_eq!(entries[0].1.as_slice(), MARKER_NAME);
    entries[0].0 = EntryMode::from(EntryKind::Tree);
    let corrupt = write_raw_tree(&repo, &entries);
    match store.verify(corrupt) {
        Err(VerifyError::BadKey { .. } | VerifyError::NonCanonical { .. }) => {}
        other => panic!("expected rejection of a malformed marker, got {other:?}"),
    }
}

/// The marker blob's content is stable; a change would break identity.
#[test]
fn marker_content_is_the_documented_constant() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let root = store
        .build((0..300_u32).map(|i| (format!("k{i:05}").into_bytes(), Value::from("v"))))
        .expect("build");
    let entries = owned_entries(&repo, root);
    assert_eq!(entries[0].1.as_slice(), MARKER_NAME);
    let blob = repo.find_blob(entries[0].2).expect("marker blob");
    assert_eq!(blob.data.as_bytes(), MARKER_CONTENT);
}
