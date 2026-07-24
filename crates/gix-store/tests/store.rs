//! End-to-end [`Store`] behaviour against real (temp) `gix` repositories.

use std::collections::HashSet;

use facet::Facet;
use facet_git_tree::schema_of;
use facet_value::value;
use gix_store::Store;
use test_support::init_repo;

#[derive(Facet)]
struct Recipe {
    title: String,
    serves: u32,
    steps: Vec<String>,
}

#[derive(Facet)]
struct Counter {
    n: u32,
}

#[test]
fn store_retrieve_list_and_schema_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let repo = gix::open(dir.path()).unwrap();
    let store = Store::open(&repo);

    let doc = schema_of::<Recipe>().unwrap();
    store.put_schema("recipe", &doc).unwrap();
    assert_eq!(store.schema("recipe").unwrap().as_ref(), Some(&doc));

    let carbonara = value!({ "title": "Carbonara", "serves": 4, "steps": ["boil", "fry"] });
    store.store("recipe", "carbonara", &carbonara, None).unwrap();

    assert_eq!(store.retrieve("recipe", "carbonara").unwrap(), Some(carbonara));
    assert_eq!(store.retrieve("recipe", "missing").unwrap(), None);
    assert_eq!(store.list("recipe").unwrap(), vec!["carbonara".to_owned()]);
    assert_eq!(store.kinds().unwrap(), vec!["recipe".to_owned()]);
}

#[test]
fn unknown_kind_is_a_data_error() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let repo = gix::open(dir.path()).unwrap();
    let store = Store::open(&repo);

    let err = store
        .store("ghost", "x", &value!({ "a": 1 }), None)
        .unwrap_err();
    assert!(matches!(err, gix_store::Error::NoSchema { .. }), "{err:?}");
}

#[test]
fn old_versions_stay_readable_after_schema_evolves() {
    #[derive(Facet)]
    struct V1 {
        name: String,
    }
    #[derive(Facet)]
    struct V2 {
        name: String,
        rank: u32,
    }

    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let repo = gix::open(dir.path()).unwrap();
    let store = Store::open(&repo);

    store.put_schema("thing", &schema_of::<V1>().unwrap()).unwrap();
    let v1 = value!({ "name": "old" });
    let old_commit = store.store("thing", "a", &v1, None).unwrap();

    // Evolve the kind: the schema ref moves forward, v1's tree stays reachable.
    store.put_schema("thing", &schema_of::<V2>().unwrap()).unwrap();

    // The old commit reads back through its own `Schema:` trailer.
    assert_eq!(store.retrieve_at(old_commit).unwrap(), v1);
    // A new value conforming to v2 stores and reads under the evolved schema.
    let v2 = value!({ "name": "new", "rank": 1 });
    store.store("thing", "b", &v2, None).unwrap();
    assert_eq!(store.retrieve("thing", "b").unwrap(), Some(v2));
}

#[test]
fn concurrent_writers_land_a_linear_history() {
    const THREADS: usize = 3;
    const WRITES: usize = 10;

    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    {
        let repo = gix::open(dir.path()).unwrap();
        Store::open(&repo)
            .put_schema("counter", &schema_of::<Counter>().unwrap())
            .unwrap();
    }

    let path = dir.path();
    std::thread::scope(|scope| {
        for t in 0..THREADS {
            scope.spawn(move || {
                // Each thread is its own writer: a separate repository handle
                // racing on the one ref, exactly as separate processes would.
                let repo = gix::open(path).unwrap();
                let store = Store::open(&repo);
                for i in 0..WRITES {
                    let n = (t * WRITES + i) as u32;
                    store.store("counter", "c", &value!({ "n": (n) }), None).unwrap();
                }
            });
        }
    });

    let repo = gix::open(path).unwrap();
    let store = Store::open(&repo);
    let history = store.history("counter", "c").unwrap();

    // Every write committed forward: none was lost to a race, and the chain is
    // linear (distinct commits, one per write).
    assert_eq!(history.len(), THREADS * WRITES);
    let distinct: HashSet<_> = history.iter().collect();
    assert_eq!(distinct.len(), history.len());

    // Every distinct value survives somewhere in the history.
    let stored: HashSet<u32> = history
        .iter()
        .map(|&id| {
            let v = store.retrieve_at(id).unwrap();
            v.as_object().unwrap().get("n").unwrap().as_number().unwrap().to_u128().unwrap() as u32
        })
        .collect();
    let expected: HashSet<u32> = (0..(THREADS * WRITES) as u32).collect();
    assert_eq!(stored, expected);
}

#[test]
fn delete_removes_an_entity() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let repo = gix::open(dir.path()).unwrap();
    let store = Store::open(&repo);

    store.put_schema("counter", &schema_of::<Counter>().unwrap()).unwrap();
    store.store("counter", "c", &value!({ "n": 1 }), None).unwrap();

    assert!(store.delete("counter", "c").unwrap());
    assert_eq!(store.retrieve("counter", "c").unwrap(), None);
    assert!(!store.delete("counter", "c").unwrap());
}
