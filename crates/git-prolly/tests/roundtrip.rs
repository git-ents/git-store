//! Round-trip invariants: Rust values through facet-git-tree into Git objects
//! and back, and maps through Prolly roots and iteration back into maps.

mod common;

use common::{TestRepo, repo, user};
use facet_value::Value;
use git_prolly::ProllyStore;

/// A dynamic value round-trips through facet-git-tree's Git object graph.
#[test]
fn facet_value_round_trip() {
    let TestRepo { _dir, repo } = repo();
    let original = user("alice", "alice@example.com");
    let value_oid = facet_git_tree::serialize_into(&original, &repo).expect("serialize value");
    assert_eq!(
        repo.find_header(value_oid)
            .expect("value root exists")
            .kind(),
        gix::objs::Kind::Tree,
        "an object value's facet-git-tree root is a tree"
    );
    let decoded =
        facet_git_tree::deserialize::<Value>(&value_oid, &repo).expect("deserialize value");
    assert_eq!(decoded, original);
}

/// The tagged value root is structural even when its scalar payload is a leaf
/// blob; the Prolly leaf entry carries the root with the correct mode.
#[test]
fn scalar_values_are_structurally_tagged() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let root = store
        .insert(None, b"scalar", &Value::from("just a string"))
        .expect("insert");
    let value_oid = store
        .get_oid(root, b"scalar")
        .expect("get_oid")
        .expect("present");
    assert_eq!(
        repo.find_header(value_oid).expect("value exists").kind(),
        gix::objs::Kind::Tree
    );
    assert_eq!(
        store.get(root, b"scalar").expect("get"),
        Some(Value::from("just a string"))
    );
}

/// A map built into a Prolly tree iterates back as exactly that map.
#[test]
fn prolly_round_trip() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let entries: Vec<(Vec<u8>, Value)> = (0..100_u32)
        .map(|i| {
            (
                format!("key-{i:03}").into_bytes(),
                user(&format!("user-{i}"), &format!("user-{i}@example.com")),
            )
        })
        .collect();

    let root = store.build(entries.iter().cloned()).expect("build");
    let iterated: Vec<(Vec<u8>, Value)> = store
        .iter(root)
        .expect("iterate")
        .map(|item| item.expect("item"))
        .collect();
    assert_eq!(iterated.len(), entries.len());
    for (got, (key, value)) in iterated.iter().zip(&entries) {
        assert_eq!(got.0, key.clone());
        assert_eq!(&got.1, value);
    }
}

/// Every key built into the tree is readable, and missing keys are not.
#[test]
fn get_reads_every_entry() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let entries: Vec<(Vec<u8>, Value)> = (0..200_u32)
        .map(|i| {
            (
                format!("k-{i:04}").into_bytes(),
                Value::from(format!("v{i}")),
            )
        })
        .collect();
    let root = store.build(entries.iter().cloned()).expect("build");
    for (key, value) in &entries {
        assert_eq!(store.get(root, key).expect("get"), Some(value.clone()));
    }
    assert_eq!(store.get(root, b"missing").expect("get"), None);
    assert!(store.get(root, b"").is_err(), "empty keys are rejected");
}

/// Typed reads decode the value's Git object graph through Facet reflection.
#[test]
fn typed_get_as() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);

    #[derive(Debug, Clone, facet::Facet, PartialEq)]
    struct Person {
        name: String,
        email: String,
    }

    let read_back = |person: &Person| -> Value {
        let (oid, store) = facet_git_tree::serialize(person).expect("serialize typed value");
        facet_git_tree::deserialize::<Value>(&oid, &store).expect("read back as dynamic value")
    };

    let person = Person {
        name: "bob".to_owned(),
        email: "bob@example.com".to_owned(),
    };
    let root = store
        .insert(None, b"bob", &read_back(&person))
        .expect("insert");
    let decoded: Person = store
        .get_as::<Person>(root, b"bob")
        .expect("get_as")
        .expect("present");
    assert_eq!(decoded, person);

    let other = Person {
        name: "carol".to_owned(),
        email: "carol@example.com".to_owned(),
    };
    let root = store
        .insert(Some(root), b"carol", &read_back(&other))
        .expect("insert");
    let decoded: Person = store
        .get_as::<Person>(root, b"carol")
        .expect("get_as")
        .expect("present");
    assert_eq!(decoded, other);
    assert!(
        store
            .get_as::<Person>(root, b"missing")
            .expect("get_as")
            .is_none()
    );
}
