//! Persistence invariants: trees are immutable, mutations preserve unchanged
//! subtrees' ObjectIds, and identical content deduplicates.

#![expect(clippy::indexing_slicing, reason = "tests index deliberately")]

mod common;

use common::{TestRepo, repo};
use facet_value::Value;
use git_prolly::ProllyStore;
use gix::ObjectId;
use gix::objs::tree::EntryKind;

fn sample(n: u32) -> Vec<(Vec<u8>, Value)> {
    (0..n)
        .map(|i| {
            (
                format!("key-{i:06}").into_bytes(),
                Value::from(format!("v{i}")),
            )
        })
        .collect()
}

/// Collect the ObjectIds of every node (leaf and internal) reachable from a
/// root, for subtree-reuse assertions.
fn node_ids(repo: &gix::Repository, root: ObjectId, out: &mut Vec<ObjectId>) {
    out.push(root);
    let tree = repo.find_tree(root).expect("tree exists");
    for entry in tree.decode().expect("decode").entries {
        if entry.mode.kind() == EntryKind::Tree {
            node_ids(repo, entry.oid.to_owned(), out);
        }
    }
}

/// Mutating a tree leaves the old tree fully intact and readable.
#[test]
fn mutation_preserves_the_old_tree() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let entries = sample(120);
    let root1 = store.build(entries.iter().cloned()).expect("build");
    let root2 = store
        .insert(Some(root1), b"zzz-new", &Value::from("new"))
        .expect("insert");
    assert_ne!(root1, root2);

    // The old root still reads as before the mutation.
    for (key, value) in &entries {
        assert_eq!(store.get(root1, key).expect("get"), Some(value.clone()));
    }
    assert_eq!(store.get(root1, b"zzz-new").expect("get"), None);
    // The new root contains everything plus the new key.
    assert_eq!(
        store.get(root2, b"zzz-new").expect("get"),
        Some(Value::from("new"))
    );
}

/// Inserting at the end of the key range leaves every earlier chunk's
/// ObjectId untouched: the only rewritten nodes are those on the path from
/// the root to the appended key's leaf.
#[test]
fn appends_reuse_unchanged_subtrees() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let entries = sample(400);
    let root1 = store.build(entries.iter().cloned()).expect("build");
    let root2 = store
        .insert(Some(root1), b"zzzzz-append", &Value::from("appended"))
        .expect("insert");

    let mut before = Vec::new();
    node_ids(&repo, root1, &mut before);
    let mut after = Vec::new();
    node_ids(&repo, root2, &mut after);

    // Walk root1 down the rightmost child at each level: that path is the
    // set of nodes an append at the end of the key range rewrites.
    let mut path = std::collections::HashSet::new();
    let mut cursor = root1;
    loop {
        path.insert(cursor);
        let tree = repo.find_tree(cursor).expect("tree exists");
        let last_child = tree
            .decode()
            .expect("decode")
            .entries
            .iter()
            .rev()
            .find(|entry| entry.mode.kind() == gix::objs::tree::EntryKind::Tree)
            .map(|entry| entry.oid.to_owned());
        match last_child {
            Some(child) => cursor = child,
            None => break,
        }
    }

    let rewritten: Vec<&ObjectId> = before
        .iter()
        .filter(|oid| !after.contains(*oid) && !path.contains(*oid))
        .collect();
    assert!(
        rewritten.is_empty(),
        "only the append's path may be rewritten: {rewritten:?}"
    );
    assert!(
        after.iter().filter(|oid| !before.contains(oid)).count() <= path.len(),
        "no more nodes appear than the path can account for"
    );
}

/// Removing every key returns the canonical empty root.
#[test]
fn emptying_returns_the_empty_tree() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let entries = sample(50);
    let mut root = store.build(entries.iter().cloned()).expect("build");
    for (key, _) in &entries {
        root = store.remove(root, key).expect("remove");
    }
    assert!(store.is_empty_root(root));
    assert!(store.verify(root).is_ok(), "the empty root verifies");
    let iterated = store
        .iter(root)
        .expect("iterate")
        .map_while(|item| item.ok())
        .count();
    assert_eq!(iterated, 0);
}

/// Identical values produce identical Git object graphs, and trees holding
/// the same values share those objects.
#[test]
fn identical_values_deduplicate() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let value = common::user("alice", "alice@example.com");

    let root_a = store.insert(None, b"a", &value).expect("insert a");
    let root_b = store.insert(None, b"b", &value).expect("insert b");
    let oid_a = store
        .get_oid(root_a, b"a")
        .expect("get_oid")
        .expect("present");
    let oid_b = store
        .get_oid(root_b, b"b")
        .expect("get_oid")
        .expect("present");
    assert_eq!(oid_a, oid_b, "identical values are the same object graph");

    // Re-inserting the same value under another key reuses the blob.
    let root_c = store.insert(None, b"c", &value).expect("insert c");
    assert_eq!(
        store
            .get_oid(root_c, b"c")
            .expect("get_oid")
            .expect("present"),
        oid_a,
        "identical values are the same object graph"
    );
}

/// A key removed and re-added with the same value converges back to the
/// original root.
#[test]
fn remove_then_reinsert_restores_identity() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let entries = sample(90);
    let root = store.build(entries.iter().cloned()).expect("build");
    let removed = store.remove(root, &entries[3].0).expect("remove");
    let restored = store
        .insert(Some(removed), &entries[3].0, &entries[3].1)
        .expect("reinsert");
    assert_eq!(restored, root);
}
