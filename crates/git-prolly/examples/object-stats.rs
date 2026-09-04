//! Object-graph statistics for a Prolly tree built in a temporary repository.
//!
//! Complements `benches/prolly.rs` (wall-clock) with the structural metrics
//! the design brief asks for: number of Git objects, total object bytes, tree
//! count, lookup depth, objects read per lookup, and objects rewritten per
//! mutation.
//!
//! Usage: `cargo run -p git-prolly --example object-stats -- [entry_count]`

#![expect(
    clippy::indexing_slicing,
    clippy::unwrap_in_result,
    reason = "the example is a demonstration harness"
)]

use std::collections::HashSet;

use facet_value::{VObject, Value};
use git_prolly::{KeyCodec, ProllyStore};
use gix::bstr::ByteSlice;

fn user(i: usize) -> Value {
    let mut object = VObject::new();
    object.insert("name", Value::from(format!("user-{i}")));
    object.insert("email", Value::from(format!("user-{i}@example.com")));
    object.insert("active", Value::TRUE);
    Value::from(object)
}

fn entries(n: u32) -> Vec<(Vec<u8>, Value)> {
    (0..n)
        .map(|i| (format!("key-{i:08}").into_bytes(), user(i as usize)))
        .collect()
}

/// Walk every node and value object reachable from `root`.
fn graph(repo: &gix::Repository, root: gix::ObjectId) -> (HashSet<gix::ObjectId>, usize, usize) {
    let mut seen = HashSet::new();
    let mut trees = 0;
    let mut bytes = 0;
    let mut stack = vec![root];
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        trees += 1;
        let tree = repo.find_tree(oid).expect("node exists");
        bytes += tree.data.len();
        for entry in tree.decode().expect("decode").entries {
            if entry.mode.kind() == gix::objs::tree::EntryKind::Tree {
                stack.push(entry.oid.to_owned());
            } else if seen.insert(entry.oid.to_owned()) {
                bytes += repo.find_blob(entry.oid).expect("value exists").data.len();
            }
        }
    }
    (seen, trees, bytes)
}

/// The number of nodes a lookup of `key` descends through, root to leaf.
fn lookup_depth(
    repo: &gix::Repository,
    root: gix::ObjectId,
    key: &[u8],
) -> Result<usize, git_prolly::Error> {
    let store = ProllyStore::open(repo);
    let mut node = root;
    let mut depth = 0;
    loop {
        depth += 1;
        let entries = store.repo().find_tree(node).expect("node exists");
        let decoded = entries.decode().expect("decode");
        let is_internal = decoded
            .entries
            .first()
            .is_some_and(|entry| entry.filename.as_bytes() == b"!");
        if !is_internal {
            return Ok(depth);
        }
        let encoded = git_prolly::HexKeyCodec.encode(key).expect("encode");
        let children = &decoded.entries[1..];
        let index =
            children.partition_point(|entry| entry.filename.as_bytes() <= encoded.as_slice());
        node = children[index - 1].oid.to_owned();
    }
}

fn main() {
    let count: u32 = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(10_000);
    let dir = tempfile::TempDir::new().expect("temp dir");
    let repo = gix::init(dir.path()).expect("init");
    let store = ProllyStore::open(&repo);

    let data = entries(count);
    let root = store.build(data.iter().cloned()).expect("build");

    let (objects, trees, bytes) = graph(&repo, root);
    let levels = {
        let mut depth = 1;
        let mut node = root;
        loop {
            let tree = repo.find_tree(node).expect("node");
            let entries = tree.decode().expect("decode");
            let internal = entries
                .entries
                .first()
                .is_some_and(|entry| entry.filename.as_bytes() == b"!");
            if !internal {
                break;
            }
            node = entries.entries[1].oid.to_owned();
            depth += 1;
        }
        depth
    };
    let depths: Vec<usize> = data
        .iter()
        .take(100)
        .step_by(7)
        .map(|(key, _)| lookup_depth(&repo, root, key).expect("depth"))
        .collect();
    let avg_depth = depths.iter().sum::<usize>() as f64 / depths.len() as f64;

    // Objects rewritten by one mutation: nodes present after the insert that
    // were not present before.
    let (pre_objects, _, _) = graph(&repo, root);
    let new_root = store
        .insert(Some(root), b"zzz-new", &user(count as usize))
        .expect("insert");
    let (post_objects, _, _) = graph(&repo, new_root);
    let rewritten = post_objects.difference(&pre_objects).count();

    println!("git-prolly object stats for {count} entries");
    println!("  root oid:                    {root}");
    println!("  git objects:                 {}", objects.len());
    println!("  tree nodes:                  {trees}");
    println!("  total object bytes:          {bytes}");
    println!("  levels:                      {levels}");
    println!("  avg lookup depth:            {avg_depth:.2}");
    println!("  objects read per lookup:     {avg_depth:.2} nodes + 1 value");
    println!("  objects added by one insert: {rewritten}");
}
