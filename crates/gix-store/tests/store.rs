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

/// A kind whose own top-level field names collide with the two the
/// `{schema/, value/}` split uses, so the split cannot be confused with the
/// value it wraps.
#[derive(Facet)]
struct Colliding {
    value: String,
    schema: String,
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
    store
        .store("recipe", "carbonara", &carbonara, None)
        .unwrap();

    assert_eq!(
        store.retrieve("recipe", "carbonara").unwrap(),
        Some(carbonara)
    );
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

    store
        .put_schema("thing", &schema_of::<V1>().unwrap())
        .unwrap();
    let v1 = value!({ "name": "old" });
    let old_commit = store.store("thing", "a", &v1, None).unwrap();

    // Evolve the kind: the schema ref moves forward, v1's tree stays reachable.
    store
        .put_schema("thing", &schema_of::<V2>().unwrap())
        .unwrap();

    // The old commit reads back through its own `schema/` subtree.
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
                    store
                        .store("counter", "c", &value!({ "n": (n) }), None)
                        .unwrap();
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
            v.as_object()
                .unwrap()
                .get("n")
                .unwrap()
                .as_number()
                .unwrap()
                .to_u128()
                .unwrap() as u32
        })
        .collect();
    let expected: HashSet<u32> = (0..(THREADS * WRITES) as u32).collect();
    assert_eq!(stored, expected);
}

#[test]
fn default_open_still_uses_refs_store_and_refs_schema() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let repo = gix::open(dir.path()).unwrap();
    let store = Store::open(&repo);

    store
        .put_schema("counter", &schema_of::<Counter>().unwrap())
        .unwrap();
    store
        .store("counter", "c", &value!({ "n": 1 }), None)
        .unwrap();

    assert!(
        repo.find_reference("refs/schema/counter").is_ok(),
        "schema ref should land under the default refs/schema prefix"
    );
    assert!(
        repo.find_reference("refs/store/counter/c").is_ok(),
        "data ref should land under the default refs/store prefix"
    );
}

#[test]
fn custom_prefixes_roundtrip_store_retrieve_and_history() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let repo = gix::open(dir.path()).unwrap();
    let store =
        Store::open_with_prefixes(&repo, "refs/meta/rules", "refs/meta/rules-schema").unwrap();

    store
        .put_schema("module", &schema_of::<Counter>().unwrap())
        .unwrap();
    store
        .store("module", "a", &value!({ "n": 1 }), None)
        .unwrap();
    store
        .store("module", "a", &value!({ "n": 2 }), None)
        .unwrap();

    assert_eq!(
        store.retrieve("module", "a").unwrap(),
        Some(value!({ "n": 2 }))
    );
    assert_eq!(store.history("module", "a").unwrap().len(), 2);
    assert_eq!(store.list("module").unwrap(), vec!["a".to_owned()]);
    assert_eq!(store.kinds().unwrap(), vec!["module".to_owned()]);

    // Refs actually landed under the custom namespace, not the default one.
    assert!(repo.find_reference("refs/meta/rules-schema/module").is_ok());
    assert!(repo.find_reference("refs/meta/rules/module/a").is_ok());
    assert!(
        repo.try_find_reference("refs/store/module/a")
            .unwrap()
            .is_none()
    );
}

#[test]
fn open_with_prefixes_rejects_invalid_prefixes() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let repo = gix::open(dir.path()).unwrap();

    let err = Store::open_with_prefixes(&repo, "refs/../store", "refs/schema")
        .err()
        .unwrap();
    assert!(
        matches!(err, gix_store::Error::InvalidName { .. }),
        "{err:?}"
    );

    let err = Store::open_with_prefixes(&repo, "/refs/store", "refs/schema")
        .err()
        .unwrap();
    assert!(
        matches!(err, gix_store::Error::InvalidName { .. }),
        "{err:?}"
    );

    let err = Store::open_with_prefixes(&repo, "refs/store/", "refs/schema")
        .err()
        .unwrap();
    assert!(
        matches!(err, gix_store::Error::InvalidName { .. }),
        "{err:?}"
    );

    let err = Store::open_with_prefixes(&repo, "refs//store", "refs/schema")
        .err()
        .unwrap();
    assert!(
        matches!(err, gix_store::Error::InvalidName { .. }),
        "{err:?}"
    );

    let err = Store::open_with_prefixes(&repo, "refs/store", "")
        .err()
        .unwrap();
    assert!(
        matches!(err, gix_store::Error::InvalidName { .. }),
        "{err:?}"
    );
}

/// The repro from issue `0b4a9b27`: a data ref fetched into a fresh
/// repository that has *no* `refs/schema/*` at all — not evolved past it,
/// never had it — must still read back. Before subtree binding, the schema
/// commit was only named in a `Schema:` trailer, unreachable from the data
/// commit, so a real `git fetch` of just the data ref left it looking present
/// (`git ls-tree` works) but unreadable (`Store::retrieve_at` fails looking
/// up an object nothing brought along). The fix makes the schema part of the
/// data commit's own tree, so ordinary tree reachability — which `git fetch`
/// already respects — carries it along for free.
#[test]
fn fetched_data_ref_reads_back_without_any_schema_ref() {
    #[derive(Facet)]
    struct Recipe {
        title: String,
        serves: u32,
    }

    let origin_dir = tempfile::tempdir().unwrap();
    init_repo(origin_dir.path());
    let origin = gix::open(origin_dir.path()).unwrap();
    let origin_store = Store::open(&origin);

    origin_store
        .put_schema("recipe", &schema_of::<Recipe>().unwrap())
        .unwrap();
    let carbonara = value!({ "title": "Carbonara", "serves": 4 });
    let commit = origin_store
        .store("recipe", "carbonara", &carbonara, None)
        .unwrap();

    // A fresh repository, never told about `recipe`'s schema at all.
    let consumer_dir = tempfile::tempdir().unwrap();
    init_repo(consumer_dir.path());

    // A real `git fetch` of exactly the data ref — nothing under
    // `refs/schema/*` — exactly as a refspec-scoped sync (a mirror, a partial
    // clone, `git push refs/store/*`) would do.
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(consumer_dir.path())
        .arg("fetch")
        .arg(origin_dir.path())
        .arg("refs/store/recipe/carbonara:refs/store/recipe/carbonara")
        .status()
        .expect("run git fetch");
    assert!(status.success(), "git fetch failed");

    let consumer = gix::open(consumer_dir.path()).unwrap();
    assert!(
        consumer
            .try_find_reference("refs/schema/recipe")
            .unwrap()
            .is_none(),
        "the repro requires no refs/schema/* to have been fetched"
    );

    let consumer_store = Store::open(&consumer);
    assert_eq!(consumer_store.retrieve_at(commit).unwrap(), carbonara);
    // The data ref itself was fetched, so the ordinary ref-based read works too.
    assert_eq!(
        consumer_store.retrieve("recipe", "carbonara").unwrap(),
        Some(carbonara)
    );
}

/// The `Schema:` trailer is still written and still parses, but it is
/// provenance only: [`Store::schema_provenance`] reads it back, distinct from
/// [`Store::retrieve_at`], which never consults it.
#[test]
fn schema_provenance_reads_the_trailer_written_at_store_time() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let repo = gix::open(dir.path()).unwrap();
    let store = Store::open(&repo);

    store
        .put_schema("counter", &schema_of::<Counter>().unwrap())
        .unwrap();
    let schema_commit = repo
        .find_reference("refs/schema/counter")
        .unwrap()
        .peel_to_id()
        .unwrap()
        .detach();
    let data_commit = store
        .store("counter", "c", &value!({ "n": 1 }), None)
        .unwrap();

    assert_eq!(store.schema_provenance(data_commit).unwrap(), schema_commit);
}

/// A commit whose tree is not the `{schema/, value/}` split — anything not
/// written by [`Store::store`]/[`Store::store_anonymous`], including every
/// commit predating subtree binding — is diagnosable instead of collapsing
/// through the catch-all `Error::Git`.
#[test]
fn retrieve_at_reports_a_commit_that_is_not_subtree_bound() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let repo = gix::open(dir.path()).unwrap();
    let store = Store::open(&repo);

    let empty_tree = repo.empty_tree().id().detach();
    let bogus = repo
        .commit(
            "refs/store/x/y",
            "not written by Store",
            empty_tree,
            None::<gix::ObjectId>,
        )
        .unwrap()
        .detach();

    let err = store.retrieve_at(bogus).unwrap_err();
    assert!(
        matches!(
            err,
            gix_store::Error::NotSubtreeBound { commit, .. } if commit == bogus
        ),
        "{err:?}"
    );
}

/// The pre-binding shape specifically: a commit whose tree *is* the value, as
/// every commit written before this change looks. It must report
/// `NotSubtreeBound` — naming re-storing as the remedy — and not be mistaken
/// for a subtree-bound commit just because the value happens to carry
/// top-level fields called `value` and `schema`.
#[test]
fn a_pre_binding_commit_whose_value_has_value_and_schema_fields_is_not_mistaken_for_bound() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let repo = gix::open(dir.path()).unwrap();
    let store = Store::open(&repo);

    // A kind whose own fields collide with the two names the split uses.
    let doc = facet_git_tree::schema_of::<Colliding>().unwrap();
    store.put_schema("colliding", &doc).unwrap();
    let colliding = value!({ "value": "v", "schema": "s" });

    // The old format: commit the value's tree directly, with no wrapper.
    let value_tree = facet_git_tree::serialize_value_with_schema(&colliding, &doc, &repo.objects)
        .expect("serialize");
    let old = repo
        .commit(
            "refs/store/colliding/old",
            "pre-binding shape\n\nSchema: 0000000000000000000000000000000000000000\n",
            value_tree,
            None::<gix::ObjectId>,
        )
        .unwrap()
        .detach();

    let err = store.retrieve_at(old).unwrap_err();
    assert!(
        matches!(
            err,
            gix_store::Error::NotSubtreeBound { commit, .. } if commit == old
        ),
        "a pre-binding commit must not be read as if bound: {err:?}"
    );

    // Stored properly, the same colliding value round-trips through the split.
    store
        .store("colliding", "new", &colliding, None)
        .expect("store colliding value");
    assert_eq!(store.retrieve("colliding", "new").unwrap(), Some(colliding));
}

/// An incomplete transfer — the subtree entry is present but the object it
/// names is not — reports which half is absent and on which commit, rather
/// than a bare `gix` object-not-found. This is the failure class the whole
/// binding exists to make diagnosable, so it must stay diagnosable even when
/// the binding itself cannot save the read.
#[test]
fn retrieve_at_reports_a_subtree_object_that_is_not_present() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let repo = gix::open(dir.path()).unwrap();
    let store = Store::open(&repo);

    store
        .put_schema("counter", &schema_of::<Counter>().unwrap())
        .unwrap();
    let commit = store
        .store("counter", "c", &value!({ "n": 1 }), None)
        .unwrap();

    // The schema entry names a tree that was never written to this repository.
    let absent = gix::ObjectId::from_hex(b"0123456789012345678901234567890123456789").unwrap();
    let value_tree = repo
        .find_commit(commit)
        .unwrap()
        .tree()
        .unwrap()
        .find_entry("value")
        .unwrap()
        .object_id();
    let mut entries = vec![
        gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Tree.into(),
            filename: "value".into(),
            oid: value_tree,
        },
        gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Tree.into(),
            filename: "schema".into(),
            oid: absent,
        },
    ];
    entries.sort();
    let root = repo
        .write_object(gix::objs::Tree { entries })
        .unwrap()
        .detach();
    let severed = repo
        .commit(
            "refs/store/counter/severed",
            "schema subtree not transferred",
            root,
            None::<gix::ObjectId>,
        )
        .unwrap()
        .detach();

    let err = store.retrieve_at(severed).unwrap_err();
    assert!(
        matches!(
            err,
            gix_store::Error::SchemaObjectMissing { subtree: "schema", oid, commit }
                if oid == absent && commit == severed
        ),
        "{err:?}"
    );
}

/// [`Store::store_anonymous`] binds the schema by subtree exactly as
/// [`Store::store`] does — its commit is written before any ref, so a
/// regression there would be invisible to the ref-based tests.
#[test]
fn store_anonymous_binds_the_schema_by_subtree() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let repo = gix::open(dir.path()).unwrap();
    let store = Store::open(&repo);

    store
        .put_schema("counter", &schema_of::<Counter>().unwrap())
        .unwrap();
    let schema_tree = repo
        .find_reference("refs/schema/counter")
        .unwrap()
        .peel_to_id()
        .unwrap()
        .object()
        .unwrap()
        .into_commit()
        .tree_id()
        .unwrap()
        .detach();

    let counter = value!({ "n": 7 });
    let (name, commit) = store.store_anonymous("counter", &counter, None).unwrap();

    // Bound to the very same schema tree object, not a copy of it.
    let root = repo.find_commit(commit).unwrap().tree().unwrap();
    assert_eq!(
        root.find_entry("schema").unwrap().object_id(),
        schema_tree,
        "store_anonymous must share the schema tree, not duplicate it"
    );
    // And self-contained: readable with no consultation of refs/schema/*.
    assert_eq!(store.retrieve_at(commit).unwrap(), counter);
    assert_eq!(store.retrieve("counter", &name).unwrap(), Some(counter));
}

#[test]
fn delete_removes_an_entity() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let repo = gix::open(dir.path()).unwrap();
    let store = Store::open(&repo);

    store
        .put_schema("counter", &schema_of::<Counter>().unwrap())
        .unwrap();
    store
        .store("counter", "c", &value!({ "n": 1 }), None)
        .unwrap();

    assert!(store.delete("counter", "c").unwrap());
    assert_eq!(store.retrieve("counter", "c").unwrap(), None);
    assert!(!store.delete("counter", "c").unwrap());
}
