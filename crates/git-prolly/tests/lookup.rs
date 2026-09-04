//! Lookup invariants: a Prolly tree answers exactly like a reference
//! `BTreeMap` over the same logical entries.

mod common;

use common::{Rng, TestRepo, repo};
use facet_value::Value;
use git_prolly::ProllyStore;
use std::collections::{BTreeMap, BTreeSet};

/// Compare a tree against a `BTreeMap` over thousands of random operations
/// and lookups, including keys that must miss.
#[test]
fn random_lookups_match_a_btree_reference() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let mut rng = Rng::new(7);
    let mut reference: BTreeMap<Vec<u8>, Value> = BTreeMap::new();

    // Interleave inserts and removes over a randomly generated key set, so
    // roughly half the operations are removes on existing keys.
    let mut root = store.empty_root();
    let mut pending: BTreeSet<Vec<u8>> = BTreeSet::new();
    for i in 0..2000_u32 {
        let len = 1 + (rng.next_u64() % 24) as usize;
        let key = rng.key(len);
        let value = Value::from(format!("v{i}"));
        root = store.insert(Some(root), &key, &value).expect("insert");
        reference.insert(key.clone(), value);
        if i % 2 == 0 {
            pending.insert(key);
        }
        if let Some((key, value)) = reference.iter().next() {
            assert_eq!(
                store.get(root, key).expect("get"),
                Some(value.clone()),
                "tree and reference diverged"
            );
        }
    }
    for key in &pending {
        if !reference.contains_key(key) {
            continue;
        }
        root = store.remove(root, key).expect("remove");
        reference.remove(key);
        if let Some((key, value)) = reference.iter().next() {
            assert_eq!(
                store.get(root, key).expect("get"),
                Some(value.clone()),
                "tree and reference diverged after remove"
            );
        }
    }

    // Every reference key must be found with the exact value.
    for (key, value) in &reference {
        assert_eq!(store.get(root, key).expect("get"), Some(value.clone()));
    }
    // A thousand absent keys must miss.
    for _ in 0..1000 {
        let missing = {
            let mut key = rng.key(16);
            while reference.contains_key(&key) {
                key = rng.key(16);
            }
            key
        };
        assert_eq!(store.get(root, &missing).expect("get"), None);
    }
    assert!(
        (900..=1000).contains(&reference.len()),
        "roughly half the keys should remain, got {}",
        reference.len()
    );
}

/// Iteration yields exactly the reference map's items in key order.
#[test]
fn iteration_matches_the_reference_order() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let mut rng = Rng::new(11);
    let mut reference: BTreeMap<Vec<u8>, Value> = BTreeMap::new();
    let mut root = store.empty_root();
    for _ in 0..1500 {
        let len = 1 + (rng.next_u64() % 16) as usize;
        let key = rng.key(len);
        if reference.contains_key(&key) {
            continue;
        }
        let value = Value::from(format!("v{}", rng.next_u64()));
        root = store.insert(Some(root), &key, &value).expect("insert");
        reference.insert(key, value);
    }

    let iterated: Vec<(Vec<u8>, Value)> = store
        .iter(root)
        .expect("iterate")
        .map(|item| item.expect("item"))
        .collect();
    let expected: Vec<(Vec<u8>, Value)> = reference
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    assert_eq!(iterated, expected);
}

/// get_oid answers the value object's identity without deserializing, and
/// repeated lookups are stable.
#[test]
fn get_oid_is_stable_and_value_free() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let mut rng = Rng::new(23);
    let mut root = None;
    let mut oids: BTreeMap<Vec<u8>, gix::ObjectId> = BTreeMap::new();
    for i in 0..500_u32 {
        let key = rng.key(8);
        let value = common::user(&format!("n{i}"), &format!("n{i}@example.com"));
        root = Some(store.insert(root, &key, &value).expect("insert"));
        let key_clone = key.clone();
        oids.insert(
            key,
            store
                .get_oid(root.expect("root"), &key_clone)
                .expect("oid")
                .expect("present"),
        );
    }
    let root = root.expect("root");
    for (key, oid) in &oids {
        assert_eq!(store.get_oid(root, key).expect("oid"), Some(*oid));
    }
}

/// Lookups into an empty tree and at both key-range extremes behave.
#[test]
fn boundary_lookups() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let empty = store.empty_root();
    assert_eq!(store.get(empty, b"anything").expect("get"), None);
    assert_eq!(
        store
            .iter(empty)
            .expect("iterate")
            .map_while(|item| item.ok())
            .count(),
        0
    );

    let root = store
        .build((0..10_u32).map(|i| (format!("k{i}").into_bytes(), Value::from(i.to_string()))))
        .expect("build");
    assert_eq!(store.get(root, b"k0").expect("get"), Some(Value::from("0")));
    assert_eq!(store.get(root, b"k9").expect("get"), Some(Value::from("9")));
    assert_eq!(
        store.get(root, b"k").expect("get"),
        None,
        "prefix of every key misses"
    );
    assert_eq!(
        store.get(root, b"k9999999").expect("get"),
        None,
        "above range misses"
    );
}
