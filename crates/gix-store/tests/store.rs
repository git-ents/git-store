//! End-to-end [`Store`] behavior against real (temp) `gix` repositories.

use std::collections::HashSet;

use facet::Facet;
use facet_git_tree::{DeserializeError, SchemaDoc, SchemaVersionError, schema_of};
use facet_value::value;
use gix_store::Store;
use test_support::init_repo;

/// Rewrite `tree`'s top-level entries via `f`, write the result, and return
/// its object id. Shared by the `version`-marker tests below, which hand-edit
/// a normally-published schema tree to look like a document written by a
/// different (older or newer) binary.
fn rewrite_tree(
    repo: &gix::Repository,
    tree: gix::ObjectId,
    mut f: impl FnMut(&mut Vec<gix::objs::tree::Entry>),
) -> gix::ObjectId {
    let mut entries: Vec<_> = repo
        .find_tree(tree)
        .unwrap()
        .iter()
        .map(|e| {
            let e = e.unwrap().inner;
            gix::objs::tree::Entry {
                mode: e.mode,
                filename: e.filename.to_owned(),
                oid: e.oid.to_owned(),
            }
        })
        .collect();
    f(&mut entries);
    repo.write_object(gix::objs::Tree { entries })
        .unwrap()
        .detach()
}

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

// --- schema version marker (issue d4f8aaaf) ---

/// [`Store::put_schema`] refuses a document that declares a `version` above
/// what this binary writes, rather than silently downgrading it — and
/// publishes nothing when it does.
#[test]
fn put_schema_rejects_a_document_declaring_a_future_version() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let repo = gix::open(dir.path()).unwrap();
    let store = Store::open(&repo);

    let mut doc = schema_of::<Counter>().unwrap();
    doc.version = SchemaDoc::CURRENT_VERSION + 1;

    let err = store.put_schema("counter", &doc).unwrap_err();
    assert!(
        matches!(
            &err,
            gix_store::Error::SchemaVersionUnsupported { kind, found, supported }
                if kind == "counter"
                    && *found == SchemaDoc::CURRENT_VERSION + 1
                    && *supported == SchemaDoc::CURRENT_VERSION
        ),
        "{err:?}"
    );
    assert_eq!(
        store.schema("counter").unwrap(),
        None,
        "a rejected put_schema must not publish anything"
    );
}

/// [`Store::put_schema`] always stamps [`SchemaDoc::CURRENT_VERSION`] on what
/// it publishes, regardless of whatever placeholder the caller's document
/// carried — the version of a schema this binary writes is a property of the
/// binary, not something a caller can under- or over-state.
#[test]
fn put_schema_stamps_the_current_version_regardless_of_the_callers_placeholder() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let repo = gix::open(dir.path()).unwrap();
    let store = Store::open(&repo);

    let mut doc = schema_of::<Counter>().unwrap();
    doc.version = 0;
    store.put_schema("counter", &doc).unwrap();

    let stored = store.schema("counter").unwrap().unwrap();
    assert_eq!(stored.version, SchemaDoc::CURRENT_VERSION);
}

/// A stored schema tree with no `version` entry at all — the shape every
/// schema published before the field existed has — is refused as
/// [`SchemaVersionError::Missing`], naming re-storing as the remedy, not
/// silently treated as version 0 or 1.
#[test]
fn schema_with_no_version_entry_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let repo = gix::open(dir.path()).unwrap();
    let store = Store::open(&repo);

    let good_tree =
        facet_git_tree::serialize_into(&schema_of::<Counter>().unwrap(), &repo.objects).unwrap();
    let stripped = rewrite_tree(&repo, good_tree, |entries| {
        entries.retain(|e| e.filename != "version");
    });
    repo.commit(
        "refs/schema/counter",
        "schema counter\n",
        stripped,
        None::<gix::ObjectId>,
    )
    .unwrap();

    let err = store.schema("counter").unwrap_err();
    assert!(
        matches!(
            &err,
            gix_store::Error::SchemaVersion(SchemaVersionError::Missing(oid)) if *oid == stripped
        ),
        "{err:?}"
    );
}

/// The critical case the whole `version` marker exists for: a schema tree
/// that both declares a version above this binary's, *and* contains a
/// `Schema` variant this binary has never heard of (simulated as a `root`
/// tagged `DateTime`, the same repro the issue names) — something a full
/// typed deserialize cannot get through. [`Store::schema`] must report
/// [`Error::SchemaVersionTooNew`] here, not the reflection error the corrupt
/// content would otherwise produce, which is only possible because
/// `read_schema` reads `version` out of band *before* attempting to
/// deserialize the rest of the document.
#[test]
fn future_version_is_refused_before_a_reflection_incompatible_read_is_attempted() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let repo = gix::open(dir.path()).unwrap();
    let store = Store::open(&repo);

    let good_tree =
        facet_git_tree::serialize_into(&schema_of::<Counter>().unwrap(), &repo.objects).unwrap();
    let future_version = repo.write_blob(b"2\n").unwrap().detach();
    let bogus_root = repo.write_blob(b"DateTime\n").unwrap().detach();
    let corrupt = rewrite_tree(&repo, good_tree, |entries| {
        for entry in entries.iter_mut() {
            if entry.filename == "version" {
                entry.oid = future_version;
            } else if entry.filename == "root" {
                entry.mode = gix::objs::tree::EntryKind::Blob.into();
                entry.oid = bogus_root;
            }
        }
    });
    repo.commit(
        "refs/schema/counter",
        "schema counter\n",
        corrupt,
        None::<gix::ObjectId>,
    )
    .unwrap();

    let err = store.schema("counter").unwrap_err();
    assert!(
        matches!(
            err,
            gix_store::Error::SchemaVersionTooNew { oid, found: 2, supported: 1 } if oid == corrupt
        ),
        "expected SchemaVersionTooNew (the out-of-band pre-read catching the future version \
         before a full deserialize is attempted), got {err:?} instead — a reflection error here \
         would mean the version check ran too late to matter"
    );
}

/// The negative control for the test above: the very same `DateTime`-tagged
/// root, but at the *current* version, is not caught by the version check
/// (there is nothing to catch) and instead fails during the full typed
/// deserialize with a reflection error — confirming the corrupted content
/// really would break an ordinary read, so the version check in the previous
/// test is not simply vacuous.
#[test]
fn an_unknown_variant_at_the_current_version_fails_as_a_reflection_error() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let repo = gix::open(dir.path()).unwrap();
    let store = Store::open(&repo);

    let good_tree =
        facet_git_tree::serialize_into(&schema_of::<Counter>().unwrap(), &repo.objects).unwrap();
    let bogus_root = repo.write_blob(b"DateTime\n").unwrap().detach();
    let corrupt = rewrite_tree(&repo, good_tree, |entries| {
        for entry in entries.iter_mut() {
            if entry.filename == "root" {
                entry.mode = gix::objs::tree::EntryKind::Blob.into();
                entry.oid = bogus_root;
            }
        }
    });
    repo.commit(
        "refs/schema/counter",
        "schema counter\n",
        corrupt,
        None::<gix::ObjectId>,
    )
    .unwrap();

    let err = store.schema("counter").unwrap_err();
    assert!(
        matches!(
            &err,
            gix_store::Error::Deserialize(DeserializeError::Reflect(msg))
                if msg.contains("DateTime")
        ),
        "{err:?}"
    );
}

/// `0` is not a version any writer emits — numbering starts at 1 — so a
/// stored `0` is refused rather than read as though it were a real document.
/// Accepting it would contradict the reasoning behind
/// [`SchemaVersionError::Missing`], which declines to assume a version for an
/// unversioned tree precisely because every number is a real one.
#[test]
fn a_stored_version_of_zero_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let repo = gix::open(dir.path()).unwrap();
    let store = Store::open(&repo);

    let good_tree =
        facet_git_tree::serialize_into(&schema_of::<Counter>().unwrap(), &repo.objects).unwrap();
    let zero = repo.write_blob(b"0\n").unwrap().detach();
    let corrupt = rewrite_tree(&repo, good_tree, |entries| {
        for entry in entries.iter_mut() {
            if entry.filename == "version" {
                entry.oid = zero;
            }
        }
    });
    repo.commit(
        "refs/schema/counter",
        "schema counter\n",
        corrupt,
        None::<gix::ObjectId>,
    )
    .unwrap();

    let err = store.schema("counter").unwrap_err();
    assert!(
        matches!(
            &err,
            gix_store::Error::SchemaVersion(SchemaVersionError::Invalid { tree, version: 0 })
                if *tree == corrupt
        ),
        "{err:?}"
    );
}

/// Publishing over a schema tip whose version this binary cannot read is
/// refused. Declining to *read* a future schema while overwriting it anyway
/// would make the same unkeepable claim from the other direction.
#[test]
fn put_schema_refuses_to_publish_over_a_future_version_tip() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let repo = gix::open(dir.path()).unwrap();
    let store = Store::open(&repo);

    let doc = schema_of::<Counter>().unwrap();
    let good_tree = facet_git_tree::serialize_into(&doc, &repo.objects).unwrap();
    let future_version = repo.write_blob(b"2\n").unwrap().detach();
    let ahead = rewrite_tree(&repo, good_tree, |entries| {
        for entry in entries.iter_mut() {
            if entry.filename == "version" {
                entry.oid = future_version;
            }
        }
    });
    repo.commit(
        "refs/schema/counter",
        "schema counter\n",
        ahead,
        None::<gix::ObjectId>,
    )
    .unwrap();

    let err = store.put_schema("counter", &doc).unwrap_err();
    assert!(
        matches!(
            err,
            gix_store::Error::SchemaVersionTooNew { oid, found: 2, supported: 1 } if oid == ahead
        ),
        "expected publishing over an unreadable future schema to be refused, got {err:?}"
    );
}

/// The counterpart to the test above, and the reason it checks the tip's
/// version rather than merely that it *has* one: a tip that predates
/// versioning stays overwritable. Republishing is the remedy
/// [`SchemaVersionError::Missing`] names, so refusing here would strand every
/// pre-versioning repository with no way to migrate.
#[test]
fn put_schema_still_republishes_over_a_pre_versioning_tip() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let repo = gix::open(dir.path()).unwrap();
    let store = Store::open(&repo);

    let doc = schema_of::<Counter>().unwrap();
    let good_tree = facet_git_tree::serialize_into(&doc, &repo.objects).unwrap();
    let stripped = rewrite_tree(&repo, good_tree, |entries| {
        entries.retain(|e| e.filename != "version");
    });
    repo.commit(
        "refs/schema/counter",
        "schema counter\n",
        stripped,
        None::<gix::ObjectId>,
    )
    .unwrap();
    assert!(
        store.schema("counter").is_err(),
        "fixture should start unreadable, or this proves nothing"
    );

    store.put_schema("counter", &doc).unwrap();
    assert_eq!(store.schema("counter").unwrap().as_ref(), Some(&doc));
}

/// The *data* read path is version-gated too, not only [`Store::schema`].
///
/// Subtree binding puts the schema inside every data commit (`{schema/,
/// value/}`) precisely so a fetched value stays readable where its kind was
/// never published — which makes the data path the one that most needs this
/// gate, and the one a reader in another repository actually travels.
/// `retrieve_at` resolves `schema/` through the same version-checked
/// `read_schema` as everything else; without this test, a refactor inlining a
/// plain deserialize there would drop the gate on exactly that path and leave
/// the rest of the suite green.
#[test]
fn a_future_version_in_a_data_commits_schema_subtree_is_refused_on_retrieve() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let repo = gix::open(dir.path()).unwrap();
    let store = Store::open(&repo);

    store
        .put_schema("recipe", &schema_of::<Recipe>().unwrap())
        .unwrap();
    let carbonara = value!({ "title": "Carbonara", "serves": 4, "steps": ["boil"] });
    store
        .store("recipe", "carbonara", &carbonara, None)
        .unwrap();
    // Reading works before the rewrite, so the failure below is the version
    // gate rather than a fixture that never read in the first place.
    assert_eq!(
        store.retrieve("recipe", "carbonara").unwrap(),
        Some(carbonara)
    );

    let tip = repo
        .find_reference("refs/store/recipe/carbonara")
        .unwrap()
        .id()
        .detach();
    let root = repo.find_commit(tip).unwrap().tree_id().unwrap().detach();
    let root_tree = repo.find_tree(root).unwrap();
    let schema_tree = root_tree.find_entry("schema").unwrap().object_id();

    // Bump only the `version` blob inside the bound schema subtree: the
    // document stays perfectly decodable, so nothing but the gate can reject
    // it.
    let future_version = repo.write_blob(b"2\n").unwrap().detach();
    let corrupt_schema = rewrite_tree(&repo, schema_tree, |entries| {
        for entry in entries.iter_mut() {
            if entry.filename == "version" {
                entry.oid = future_version;
            }
        }
    });
    let corrupt_root = rewrite_tree(&repo, root, |entries| {
        for entry in entries.iter_mut() {
            if entry.filename == "schema" {
                entry.oid = corrupt_schema;
            }
        }
    });
    repo.commit(
        "refs/store/recipe/carbonara",
        "store recipe/carbonara\n",
        corrupt_root,
        Some(tip),
    )
    .unwrap();

    let err = store.retrieve("recipe", "carbonara").unwrap_err();
    assert!(
        matches!(
            err,
            gix_store::Error::SchemaVersionTooNew { oid, found: 2, supported: 1 }
                if oid == corrupt_schema
        ),
        "expected the data read path to refuse a future-version bound schema, got {err:?} — a \
         successful read here would mean a value fetched from a newer writer is decoded against \
         a schema this binary cannot vouch for"
    );
}
