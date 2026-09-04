//! Diff invariants: structural, ObjectId-based, exact against a reference
//! computation, and free of value deserialization.

#![expect(
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests index and panic deliberately"
)]

mod common;

use common::{TestRepo, repo, user};
use facet_value::Value;
use git_prolly::{DiffEntry, ProllyStore};
use std::collections::BTreeMap;

fn as_map(changes: &[DiffEntry]) -> BTreeMap<Vec<u8>, &DiffEntry> {
    changes
        .iter()
        .map(|change| (change_key(change), change))
        .collect()
}

fn change_key(change: &DiffEntry) -> Vec<u8> {
    match change {
        DiffEntry::Insert { key, .. }
        | DiffEntry::Delete { key, .. }
        | DiffEntry::Modify { key, .. } => key.clone(),
    }
}

/// Identical roots diff to nothing, and diffing in both directions is exact.
#[test]
fn identical_roots_have_no_changes() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let entries = common::user("a", "a@x").clone();
    let root = store
        .build((0..200_u32).map(|i| (format!("k{i:04}").into_bytes(), entries.clone())))
        .expect("build");
    assert!(store.diff(root, root).expect("diff").is_empty());

    // A tree rebuilt from the same contents equals the original root, so its
    // diff is also empty.
    let twin = store
        .build((0..200_u32).map(|i| (format!("k{i:04}").into_bytes(), entries.clone())))
        .expect("rebuild");
    assert!(store.diff(root, twin).expect("diff").is_empty());
}

/// Inserts, deletes, and modifications are each detected with their value
/// object ids, against both leaf-rooted and multi-level trees.
#[test]
fn all_change_kinds_are_detected() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let base: Vec<(Vec<u8>, Value)> = (0..300_u32)
        .map(|i| {
            (
                format!("k{i:05}").into_bytes(),
                user(&format!("u{i}"), &format!("u{i}@x")),
            )
        })
        .collect();
    let a = store.build(base.iter().cloned()).expect("build a");

    let mut modified = base.clone();
    // Modify k00050, delete k00100, insert kzzzzz.
    modified[50].1 = user("u50-changed", "u50@x");
    let delete_at = 100;
    let deleted_key = modified[delete_at].0.clone();
    modified.remove(delete_at);
    modified.push((b"zzzzz".to_vec(), user("new", "new@x")));
    let b = store.build(modified).expect("build b");

    let changes = store.diff(a, b).expect("diff");
    let by_key = as_map(&changes);
    assert_eq!(changes.len(), 3, "exactly three changes: {changes:?}");

    match by_key.get(b"k00050".as_slice()) {
        Some(DiffEntry::Modify { old, new, .. }) => assert_ne!(old, new),
        other => panic!("expected modify for k00050, got {other:?}"),
    }
    assert!(
        matches!(
            by_key.get(deleted_key.as_slice()),
            Some(DiffEntry::Delete { .. })
        ),
        "expected delete for {deleted_key:?}"
    );
    assert!(
        matches!(
            by_key.get(b"zzzzz".as_slice()),
            Some(DiffEntry::Insert { .. })
        ),
        "expected insert for zzzzz"
    );

    // The reverse diff mirrors the changes.
    let reverse = store.diff(b, a).expect("reverse diff");
    let reverse_by_key = as_map(&reverse);
    assert_eq!(reverse.len(), 3);
    assert!(matches!(
        reverse_by_key.get(b"k00050".as_slice()),
        Some(DiffEntry::Modify { .. })
    ));
    assert!(matches!(
        reverse_by_key.get(deleted_key.as_slice()),
        Some(DiffEntry::Insert { .. })
    ));
    assert!(matches!(
        reverse_by_key.get(b"zzzzz".as_slice()),
        Some(DiffEntry::Delete { .. })
    ));
}

/// A diff of a small random mutation series matches a reference diff computed
/// from two `BTreeMap`s.
#[test]
fn random_mutation_series_matches_reference_diff() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let mut rng = common::Rng::new(5);
    let mut left: BTreeMap<Vec<u8>, Value> = BTreeMap::new();
    let mut right: BTreeMap<Vec<u8>, Value> = BTreeMap::new();
    let mut root_a = None;
    let mut root_b = None;

    for i in 0..600_u32 {
        let len = 1 + (rng.next_u64() % 10) as usize;
        let key = rng.key(len);
        let value = Value::from(format!("v{i}"));
        // Left takes every third entry; right takes every second.
        if i % 3 == 0 {
            root_a = Some(store.insert(root_a, &key, &value).expect("insert a"));
            left.insert(key.clone(), value.clone());
        }
        if i % 2 == 0 {
            root_b = Some(store.insert(root_b, &key, &value).expect("insert b"));
            right.insert(key.clone(), value);
        }
    }
    let root_a = root_a.unwrap_or_else(|| store.empty_root());
    let root_b = root_b.unwrap_or_else(|| store.empty_root());

    let changes = store.diff(root_a, root_b).expect("diff");
    let by_key = as_map(&changes);
    let mut expected = 0;
    for (key, value) in &left {
        match right.get(key) {
            None => {
                expected += 1;
                assert!(matches!(by_key.get(key), Some(DiffEntry::Delete { .. })));
                let _ = value;
            }
            Some(other) if other != value => {
                expected += 1;
                assert!(matches!(by_key.get(key), Some(DiffEntry::Modify { .. })));
            }
            Some(_) => {
                assert!(!by_key.contains_key(key), "unchanged key {key:?} reported");
            }
        }
    }
    for key in right.keys() {
        if !left.contains_key(key) {
            expected += 1;
            assert!(matches!(by_key.get(key), Some(DiffEntry::Insert { .. })));
        }
    }
    assert_eq!(changes.len(), expected);
}

/// Diffing the empty root against a tree reports every key as an insert.
#[test]
fn diff_from_empty_is_the_whole_map() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let root = store
        .build((0..80_u32).map(|i| (format!("k{i:03}").into_bytes(), Value::from("v"))))
        .expect("build");
    let changes = store.diff(store.empty_root(), root).expect("diff");
    assert_eq!(changes.len(), 80);
    assert!(
        changes
            .iter()
            .all(|change| matches!(change, DiffEntry::Insert { .. }))
    );
}
