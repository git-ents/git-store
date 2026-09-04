//! Git interoperability: a Prolly root is an ordinary Git object graph.
//! `git fsck` accepts it, Git's own tooling sees every object as reachable
//! from the root, packing preserves it, and a reopened repository reads it.

#![expect(clippy::panic, reason = "tests panic deliberately")]

mod common;

use common::{TestRepo, git, repo};
use facet_value::Value;
use git_prolly::ProllyStore;

fn sample(n: u32) -> Vec<(Vec<u8>, Value)> {
    (0..n)
        .map(|i| {
            let value = common::user(&format!("user-{i}"), &format!("user-{i}@example.com"));
            (format!("key-{i:06}").into_bytes(), value)
        })
        .collect()
}

/// `git fsck` accepts the full object graph of a Prolly root.
#[test]
fn fsck_accepts_a_prolly_object_graph() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let root = store.build(sample(400)).expect("build");
    let output = git(repo.git_dir().parent().expect("workdir"), &["fsck"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("broken links"),
        "fsck reported broken links: {stdout}"
    );
    let _ = root;
}

/// Every object of the Prolly tree is reachable from the root by Git's own
/// reachability machinery.
#[test]
fn all_objects_are_reachable_from_the_root() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let entries = sample(400);
    let root = store.build(entries.iter().cloned()).expect("build");
    let root_hex = root.to_hex().to_string();

    let output = git(
        repo.git_dir().parent().expect("workdir"),
        &["rev-list", "--objects", &root_hex],
    );
    let listed = String::from_utf8_lossy(&output.stdout);
    let reachable: Vec<&str> = listed
        .lines()
        .map(|line| line.split(' ').next().expect("oid"))
        .collect();
    assert!(!reachable.is_empty());

    // Every node and every value object in the tree appears in rev-list's
    // output, i.e. ordinary Git machinery sees the whole structure.
    let mut expected = std::collections::HashSet::new();
    let mut stack = vec![root];
    while let Some(oid) = stack.pop() {
        expected.insert(oid);
        let tree = repo.find_tree(oid).expect("tree");
        for entry in tree.decode().expect("decode").entries {
            if entry.mode.kind() == gix::objs::tree::EntryKind::Tree {
                stack.push(entry.oid.to_owned());
            } else {
                expected.insert(entry.oid.to_owned());
            }
        }
    }
    let reachable_set: std::collections::HashSet<&str> = reachable.into_iter().collect();
    for oid in &expected {
        let hex = oid.to_hex().to_string();
        assert!(
            reachable_set.contains(hex.as_str()),
            "object {hex} is not reachable via git rev-list"
        );
    }
}

/// Packing (`git repack -adf`) preserves the tree: after packing and
/// reopening the repository, lookups and verification still succeed.
#[test]
fn pack_reopen_and_lookup() {
    let TestRepo { _dir, repo } = repo();
    let workdir = repo.git_dir().parent().expect("workdir").to_path_buf();
    let store = ProllyStore::open(&repo);
    let entries = sample(500);
    let root = store.build(entries.iter().cloned()).expect("build");
    let root_hex = root.to_hex().to_string();

    git(&workdir, &["repack", "-adf"]);

    let reopened = gix::open(&workdir).expect("reopen repository");
    let store = ProllyStore::open(&reopened);
    assert_eq!(*store.config(), git_prolly::ProllyConfig::default());
    for (key, value) in &entries {
        assert_eq!(
            store.get(root, key).expect("get after repack"),
            Some(value.clone()),
            "lookup of {key:?} failed after packing"
        );
    }
    if let Err(error) = store.verify(root) {
        panic!("verification failed after packing: {error:?}");
    }

    // And Git still reports a healthy graph.
    let output = git(&workdir, &["fsck"]);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("broken links"));
    let _ = root_hex;
}

/// `git ls-tree` reads Prolly nodes as ordinary trees with ordinary entries.
#[test]
fn ls_tree_shows_prolly_entries() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let root = store.build(sample(20)).expect("build");
    let output = git(
        repo.git_dir().parent().expect("workdir"),
        &["ls-tree", &root.to_hex().to_string()],
    );
    let listing = String::from_utf8_lossy(&output.stdout);
    // Keys are hexadecimal: the first entry of a sorted map is key-000000.
    assert!(
        listing.contains("6b65792d303030303030"),
        "expected hex-encoded key-000000 in ls-tree output: {listing}"
    );
}
