//! Determinism invariants: identical logical contents and configuration must
//! produce an identical root, however the entries were built.

#![expect(clippy::indexing_slicing, reason = "tests index deliberately")]

mod common;

use common::{Rng, TestRepo, repo};
use facet_value::Value;
use git_prolly::ProllyStore;

fn sample_map(n: u32) -> Vec<(Vec<u8>, Value)> {
    (0..n)
        .map(|i| {
            (
                format!("key-{i:06}").into_bytes(),
                Value::from(format!("v{i}")),
            )
        })
        .collect()
}

/// Sorted insertion, shuffled insertion, and batch construction all converge
/// on the same root.
#[test]
fn build_paths_converge_on_one_root() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let entries = sample_map(300);

    // A: batch construction from sorted entries.
    let batched = store.build(entries.iter().cloned()).expect("batch build");

    // B: one insert at a time in shuffled order.
    let mut shuffled = entries.clone();
    let mut rng = Rng::new(42);
    for index in (1..shuffled.len()).rev() {
        let swap = (rng.next_u64() as usize) % (index + 1);
        shuffled.swap(index, swap);
    }
    let mut inserted = None;
    for (key, value) in &shuffled {
        inserted = Some(store.insert(inserted, key, value).expect("insert"));
    }

    // C: batch construction from shuffled entries (the builder sorts).
    let shuffled_batch = store
        .build(shuffled.iter().cloned())
        .expect("shuffled batch");

    assert_eq!(
        batched,
        inserted.expect("non-empty"),
        "inserted root matches batch root"
    );
    assert_eq!(
        batched, shuffled_batch,
        "shuffled batch matches sorted batch"
    );
}

/// The same logical map in two different repositories yields the same root:
/// node identity is a pure function of content and configuration.
#[test]
fn roots_are_reproducible_across_repositories() {
    let TestRepo { _dir, repo } = repo();
    let first = ProllyStore::open(&repo)
        .build(sample_map(120))
        .expect("build");
    let TestRepo {
        _dir: _dir2,
        repo: repo2,
    } = common::repo();
    let second = ProllyStore::open(&repo2)
        .build(sample_map(120))
        .expect("build");
    assert_eq!(first, second);
}

/// Identical configuration, identical boundaries: the chunker's output is a
/// pure function of the entry sequence, checked here through whole-tree
/// identity after entry-preserving rebuilds.
#[test]
fn rebuild_of_identical_entries_is_identity() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let entries = sample_map(150);
    let root = store.build(entries.iter().cloned()).expect("build");

    // Inserting and then removing a key restores the exact original root.
    let with_extra = store
        .insert(Some(root), b"zzz-extra", &Value::from("extra"))
        .expect("insert");
    let restored = store.remove(with_extra, b"zzz-extra").expect("remove");
    assert_eq!(restored, root);

    // Replacing a value with the identical value is a no-op.
    let same_value = store
        .get(root, &entries[7].0)
        .expect("get")
        .expect("present");
    let unchanged = store
        .insert(Some(root), &entries[7].0, &same_value)
        .expect("insert");
    assert_eq!(unchanged, root);
}

/// A different configuration produces a different (still valid) tree: the
/// configuration participates in canonical identity.
#[test]
fn configuration_changes_identity() {
    let TestRepo { _dir, repo } = repo();
    let default_root = ProllyStore::open(&repo)
        .build(sample_map(80))
        .expect("build");
    let TestRepo {
        _dir: _dir2,
        repo: repo2,
    } = common::repo();
    let coarser = git_prolly::ProllyConfig {
        bits: 6,
        ..git_prolly::ProllyConfig::default()
    };
    let other_root = ProllyStore::with_config(&repo2, coarser)
        .expect("config")
        .build(sample_map(80))
        .expect("build");
    assert_ne!(default_root, other_root);
}
