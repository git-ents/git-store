//! End-to-end [`Store`] behavior against [`MemoryRefStore`] and
//! [`facet_git_tree::ObjectStore`] — no filesystem, no temp-dir repository.
//! What genuinely needs a real `gix` repository (on-disk ref layout, a real
//! `git fetch`, cross-thread concurrency, `git ls-tree`-shaped plumbing
//! assertions) lives in `tests/repository.rs` instead.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::convert::Infallible;

use facet::Facet;
use facet_git_tree::{
    Constant, DeserializeError, GitObject, Hints, ObjectStore, SchemaPinError, Target, TreeEntry,
    schema_of,
};
use facet_value::value;
use gix_refstore::RefEdit;
use gix_store::{
    ApplyError, Committer, Entry, Error, MemoryRefStore, ObjectId, RefName, RefPath, RefPrefix,
    RefSegment, RefStore, Store, Subtree, entity_name, entity_name_under,
};

fn seg(s: &str) -> RefSegment {
    RefSegment::new(s).unwrap()
}

fn entity(s: &str) -> RefPath {
    RefPath::new(s).unwrap()
}

fn store() -> Store<MemoryRefStore, ObjectStore> {
    Store::new(MemoryRefStore::new(), ObjectStore::default())
}

/// Rewrite `tree`'s top-level entries via `f`, write the result, and return
/// its object id. Shared by the schema-schema pin tests below, which
/// hand-edit a normally-published schema tree to look like a document
/// written against a schema-schema this binary does not recognize, or with
/// no pin at all.
fn rewrite_tree(
    objects: &ObjectStore,
    tree: ObjectId,
    mut f: impl FnMut(&mut Vec<TreeEntry>),
) -> ObjectId {
    let mut entries = objects.get_tree(&tree).expect("tree present");
    f(&mut entries);
    gix::objs::Write::write(objects, &gix::objs::Tree { entries }).expect("write tree")
}

fn write_blob(objects: &ObjectStore, data: &[u8]) -> ObjectId {
    gix::objs::Write::write(
        objects,
        &gix::objs::Blob {
            data: data.to_vec(),
        },
    )
    .expect("write blob")
}

/// Write a commit directly against the store's own object database and point
/// `name` at it — a CAS `Create` when `parent` is `None`, an `Update` from
/// `parent` otherwise — bypassing `Kind` entirely to build fixtures the
/// normal write path cannot produce (pre-binding shapes, corrupted pins,
/// severed subtrees).
fn commit_ref(
    store: &Store<MemoryRefStore, ObjectStore>,
    name: &str,
    message: &str,
    tree: ObjectId,
    parent: Option<ObjectId>,
) -> ObjectId {
    let signature = store.refs().signature().expect("fixed signature");
    let commit = gix::objs::Commit {
        tree,
        parents: parent.into_iter().collect(),
        author: signature.clone(),
        committer: signature,
        encoding: None,
        message: message.into(),
        extra_headers: Vec::new(),
    };
    let commit = gix::objs::Write::write(store.objects(), &commit).expect("write commit");
    let name = RefName::new(name).expect("valid ref name");
    let edit = match parent {
        Some(expected) => RefEdit::Update {
            name,
            expected,
            new: commit,
        },
        None => RefEdit::Create { name, new: commit },
    };
    store.refs().apply(edit).expect("ref write");
    commit
}

#[derive(Facet, Debug, Clone, PartialEq)]
struct Recipe {
    title: String,
    serves: u32,
    steps: Vec<String>,
}

#[derive(Facet, Debug, Clone, PartialEq)]
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
    let store = store();

    let doc = schema_of::<Recipe>().unwrap();
    store.dynamic(seg("recipe")).schema().put(&doc).unwrap();
    assert_eq!(
        store
            .dynamic(seg("recipe"))
            .schema()
            .get()
            .unwrap()
            .as_ref(),
        Some(&doc)
    );

    let carbonara = value!({ "title": "Carbonara", "serves": 4, "steps": ["boil", "fry"] });
    store
        .dynamic(seg("recipe"))
        .put(&entity("carbonara"), &carbonara)
        .unwrap();

    assert_eq!(
        store
            .dynamic(seg("recipe"))
            .get(&entity("carbonara"))
            .unwrap(),
        Some(carbonara)
    );
    assert_eq!(
        store
            .dynamic(seg("recipe"))
            .get(&entity("missing"))
            .unwrap(),
        None
    );
    assert_eq!(
        store.dynamic(seg("recipe")).list().unwrap(),
        vec![entity("carbonara")]
    );
    assert_eq!(store.kinds().unwrap(), vec![seg("recipe")]);
}

/// An entity name may nest, so an identity that is naturally composite —
/// `<target>/<id>` — groups under its first segment without minting a kind,
/// and with it a published schema, per group.
#[test]
fn entities_nest_under_a_single_kind_and_schema() {
    let store = store();
    let note = store.dynamic(seg("note"));
    note.schema().put(&schema_of::<Counter>().unwrap()).unwrap();

    for name in ["dead/two", "dead/one", "beef/one"] {
        note.put(&entity(name), &value!({ "n": 1 })).unwrap();
    }

    assert_eq!(
        note.list().unwrap(),
        ["beef/one", "dead/one", "dead/two"].map(entity)
    );
    assert_eq!(
        note.list_under(&entity("dead")).unwrap(),
        ["dead/one", "dead/two"].map(entity),
        "one group's entities, named in full, without scanning the others"
    );
    assert_eq!(
        note.list_under(&entity("beef")).unwrap(),
        ["beef/one"].map(entity)
    );
    assert!(
        note.list_under(&entity("bee")).unwrap().is_empty(),
        "a shared text prefix across a segment boundary is not a group"
    );
    assert_eq!(
        note.reference(&entity("dead/one")).as_str(),
        "refs/store/note/dead/one"
    );
    assert_eq!(
        note.get(&entity("dead/one")).unwrap(),
        Some(value!({ "n": 1 }))
    );
    assert!(note.remove(&entity("dead/one")).unwrap());
    assert_eq!(
        store.kinds().unwrap(),
        vec![seg("note")],
        "three groups, one kind, one schema"
    );
}

#[test]
fn unknown_kind_is_a_data_error() {
    let store = store();

    let err = store
        .dynamic(seg("ghost"))
        .put(&entity("x"), &value!({ "a": 1 }))
        .unwrap_err();
    assert!(matches!(err, Error::NoSchema { .. }), "{err:?}");
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

    let store = store();

    store
        .dynamic(seg("thing"))
        .schema()
        .put(&schema_of::<V1>().unwrap())
        .unwrap();
    let v1 = value!({ "name": "old" });
    let old_commit = store.dynamic(seg("thing")).put(&entity("a"), &v1).unwrap();

    // Evolve the kind: the schema ref moves forward, v1's tree stays reachable.
    store
        .dynamic(seg("thing"))
        .schema()
        .put(&schema_of::<V2>().unwrap())
        .unwrap();

    // The old commit reads back through its own `schema/` subtree.
    assert_eq!(store.dynamic(seg("thing")).get_at(old_commit).unwrap(), v1);
    // A new value conforming to v2 stores and reads under the evolved schema.
    let v2 = value!({ "name": "new", "rank": 1 });
    store.dynamic(seg("thing")).put(&entity("b"), &v2).unwrap();
    assert_eq!(
        store.dynamic(seg("thing")).get(&entity("b")).unwrap(),
        Some(v2)
    );
}

#[test]
fn ref_prefix_rejects_invalid_prefixes() {
    for bad in [
        "refs/../store",
        "/refs/store",
        "refs/store/",
        "refs//store",
        "",
    ] {
        assert!(RefPrefix::new(bad).is_err(), "{bad:?} should be rejected");
    }
}

#[test]
fn custom_layout_roundtrips_store_retrieve_and_history() {
    let store = Store::with_layout(
        MemoryRefStore::new(),
        ObjectStore::default(),
        gix_store::Layout {
            data: RefPrefix::new("refs/meta/rules").unwrap(),
            schema: RefPrefix::new("refs/meta/rules-schema").unwrap(),
        },
    );

    store
        .dynamic(seg("module"))
        .schema()
        .put(&schema_of::<Counter>().unwrap())
        .unwrap();
    store
        .dynamic(seg("module"))
        .put(&entity("a"), &value!({ "n": 1 }))
        .unwrap();
    store
        .dynamic(seg("module"))
        .put(&entity("a"), &value!({ "n": 2 }))
        .unwrap();

    assert_eq!(
        store.dynamic(seg("module")).get(&entity("a")).unwrap(),
        Some(value!({ "n": 2 }))
    );
    assert_eq!(
        store
            .dynamic(seg("module"))
            .history(&entity("a"))
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        store.dynamic(seg("module")).list().unwrap(),
        vec![entity("a")]
    );
    assert_eq!(store.kinds().unwrap(), vec![seg("module")]);

    // Refs actually landed under the custom namespace, not the default one.
    assert!(
        store
            .refs()
            .read(&RefName::new("refs/meta/rules-schema/module").unwrap())
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .refs()
            .read(&RefName::new("refs/meta/rules/module/a").unwrap())
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .refs()
            .read(&RefName::new("refs/store/module/a").unwrap())
            .unwrap()
            .is_none()
    );
}

/// [`gix_store::provenance::SchemaLabel`] (via [`Store::provenance`]): the
/// `Schema:` trailer is still written and still parses, but it is provenance
/// only, distinct from [`gix_store::Kind::get_at`], which never consults it.
#[test]
fn schema_provenance_reads_the_trailer_written_at_store_time() {
    let store = store();

    let schema_commit = store
        .dynamic(seg("counter"))
        .schema()
        .put(&schema_of::<Counter>().unwrap())
        .unwrap();
    let data_commit = store
        .dynamic(seg("counter"))
        .put(&entity("c"), &value!({ "n": 1 }))
        .unwrap();

    assert_eq!(
        store.provenance(data_commit).unwrap().recorded(),
        schema_commit
    );
}

/// A commit whose tree is not the `{schema/, value/}` split — anything not
/// written by [`gix_store::Kind::put`]/[`gix_store::kind::Put::anonymous`],
/// including every commit predating subtree binding — is diagnosable instead
/// of collapsing through the catch-all `Error::Backend`.
#[test]
fn retrieve_at_reports_a_commit_that_is_not_subtree_bound() {
    let store = store();

    let empty_tree = gix::objs::Write::write(
        store.objects(),
        &gix::objs::Tree {
            entries: Vec::new(),
        },
    )
    .unwrap();
    let bogus = commit_ref(
        &store,
        "refs/store/x/y",
        "not written by Store",
        empty_tree,
        None,
    );

    let err = store.dynamic(seg("x")).get_at(bogus).unwrap_err();
    assert!(
        matches!(
            err,
            Error::NotSubtreeBound { commit, .. } if commit == bogus
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
    let store = store();

    // A kind whose own fields collide with the two names the split uses.
    let doc = facet_git_tree::schema_of::<Colliding>().unwrap();
    store.dynamic(seg("colliding")).schema().put(&doc).unwrap();
    let colliding = value!({ "value": "v", "schema": "s" });

    // The old format: commit the value's tree directly, with no wrapper.
    let value_tree = facet_git_tree::serialize_value_with_schema(&colliding, &doc, store.objects())
        .expect("serialize");
    let old = commit_ref(
        &store,
        "refs/store/colliding/old",
        "pre-binding shape\n\nSchema: 0000000000000000000000000000000000000000\n",
        value_tree,
        None,
    );

    let err = store.dynamic(seg("colliding")).get_at(old).unwrap_err();
    assert!(
        matches!(
            err,
            Error::NotSubtreeBound { commit, .. } if commit == old
        ),
        "a pre-binding commit must not be read as if bound: {err:?}"
    );

    // Stored properly, the same colliding value round-trips through the split.
    store
        .dynamic(seg("colliding"))
        .put(&entity("new"), &colliding)
        .expect("store colliding value");
    assert_eq!(
        store.dynamic(seg("colliding")).get(&entity("new")).unwrap(),
        Some(colliding)
    );
}

/// An incomplete transfer — the subtree entry is present but the object it
/// names is not — reports which half is absent and on which commit, rather
/// than a bare object-not-found. This is the failure class the whole binding
/// exists to make diagnosable, so it must stay diagnosable even when the
/// binding itself cannot save the read.
#[test]
fn retrieve_at_reports_a_subtree_object_that_is_not_present() {
    let store = store();

    store
        .dynamic(seg("counter"))
        .schema()
        .put(&schema_of::<Counter>().unwrap())
        .unwrap();
    let commit = store
        .dynamic(seg("counter"))
        .put(&entity("c"), &value!({ "n": 1 }))
        .unwrap();

    // The schema entry names a tree that was never written to this store.
    let absent = ObjectId::from_hex(b"0123456789012345678901234567890123456789").unwrap();
    let GitObject::Commit(commit_obj) = store.objects().get(&commit).unwrap() else {
        panic!("expected a commit");
    };
    let value_tree = store
        .objects()
        .get_tree(&commit_obj.tree)
        .unwrap()
        .into_iter()
        .find(|e| e.filename == "value")
        .unwrap()
        .oid;
    let mut entries = vec![
        TreeEntry {
            mode: gix::objs::tree::EntryKind::Tree.into(),
            filename: "value".into(),
            oid: value_tree,
        },
        TreeEntry {
            mode: gix::objs::tree::EntryKind::Tree.into(),
            filename: "schema".into(),
            oid: absent,
        },
    ];
    entries.sort();
    let root = gix::objs::Write::write(store.objects(), &gix::objs::Tree { entries }).unwrap();
    let severed = commit_ref(
        &store,
        "refs/store/counter/severed",
        "schema subtree not transferred",
        root,
        None,
    );

    let err = store.dynamic(seg("counter")).get_at(severed).unwrap_err();
    assert!(
        matches!(
            err,
            Error::SubtreeMissing { subtree: Subtree::Schema, oid, commit }
                if oid == absent && commit == severed
        ),
        "{err:?}"
    );
}

/// [`gix_store::kind::Put::anonymous`] binds the schema by subtree exactly as
/// [`gix_store::Kind::put`] does — its commit is written before any ref, so a
/// regression there would be invisible to the ref-based tests.
#[test]
fn store_anonymous_binds_the_schema_by_subtree() {
    let store = store();

    store
        .dynamic(seg("counter"))
        .schema()
        .put(&schema_of::<Counter>().unwrap())
        .unwrap();
    let schema_ref = RefName::new("refs/schema/counter").unwrap();
    let schema_commit = store.refs().read(&schema_ref).unwrap().unwrap();
    let GitObject::Commit(schema_commit_obj) = store.objects().get(&schema_commit).unwrap() else {
        panic!("expected a commit");
    };
    let schema_tree = schema_commit_obj.tree;

    let counter = value!({ "n": 7 });
    let commit = store
        .dynamic(seg("counter"))
        .write(&counter)
        .anonymous()
        .unwrap();

    // Bound to the very same schema tree object, not a copy of it.
    let GitObject::Commit(commit_obj) = store.objects().get(&commit).unwrap() else {
        panic!("expected a commit");
    };
    let root = store.objects().get_tree(&commit_obj.tree).unwrap();
    let bound_schema = root.iter().find(|e| e.filename == "schema").unwrap().oid;
    assert_eq!(
        bound_schema, schema_tree,
        "store_anonymous must share the schema tree, not duplicate it"
    );
    // And self-contained: readable with no consultation of refs/schema/*.
    assert_eq!(
        store.dynamic(seg("counter")).get_at(commit).unwrap(),
        counter
    );
    assert_eq!(
        store
            .dynamic(seg("counter"))
            .get(&entity_name(commit))
            .unwrap(),
        Some(counter)
    );
}

#[test]
fn delete_removes_an_entity() {
    let store = store();

    store
        .dynamic(seg("counter"))
        .schema()
        .put(&schema_of::<Counter>().unwrap())
        .unwrap();
    store
        .dynamic(seg("counter"))
        .put(&entity("c"), &value!({ "n": 1 }))
        .unwrap();

    assert!(store.dynamic(seg("counter")).remove(&entity("c")).unwrap());
    assert_eq!(
        store.dynamic(seg("counter")).get(&entity("c")).unwrap(),
        None
    );
    assert!(!store.dynamic(seg("counter")).remove(&entity("c")).unwrap());
}

// --- typed API ---

#[test]
fn typed_kind_publish_put_get_roundtrips_a_real_value() {
    let store = store();
    let recipe = store.kind::<Recipe>(seg("recipe"));
    recipe.publish().unwrap();

    let carbonara = Recipe {
        title: "Carbonara".into(),
        serves: 4,
        steps: vec!["boil".into(), "fry".into()],
    };
    recipe.put(&entity("carbonara"), &carbonara).unwrap();

    assert_eq!(recipe.get(&entity("carbonara")).unwrap(), Some(carbonara));
}

/// The native `Typed` encoding and the schema-directed `Dynamic` one are
/// documented to be byte-identical, so a value written through one handle
/// must read back through the other over the very same refs — and, written
/// independently with identical content, the two must produce the same
/// commit, not merely mutually readable ones.
#[test]
fn typed_and_dynamic_kinds_interoperate_over_the_same_refs() {
    let store = store();
    store.kind::<Recipe>(seg("recipe")).publish().unwrap();

    let carbonara = Recipe {
        title: "Carbonara".into(),
        serves: 4,
        steps: vec!["boil".into()],
    };
    let carbonara_value = value!({ "title": "Carbonara", "serves": 4, "steps": ["boil"] });

    // Written typed, read dynamic.
    store
        .kind::<Recipe>(seg("recipe"))
        .put(&entity("a"), &carbonara)
        .unwrap();
    assert_eq!(
        store.dynamic(seg("recipe")).get(&entity("a")).unwrap(),
        Some(carbonara_value.clone())
    );

    // Written dynamic, read typed.
    store
        .dynamic(seg("recipe"))
        .put(&entity("b"), &carbonara_value)
        .unwrap();
    assert_eq!(
        store
            .kind::<Recipe>(seg("recipe"))
            .get(&entity("b"))
            .unwrap(),
        Some(carbonara.clone())
    );

    // The same content, written independently through both encodings with no
    // shared parent, lands on the same commit.
    let typed_commit = store
        .kind::<Recipe>(seg("recipe"))
        .write(&carbonara)
        .anonymous()
        .unwrap();
    let dynamic_commit = store
        .dynamic(seg("recipe"))
        .write(&carbonara_value)
        .anonymous()
        .unwrap();
    assert_eq!(
        typed_commit, dynamic_commit,
        "typed and schema-directed encodings should be byte-identical"
    );
}

/// A typed `put` whose `T` no longer matches the published schema fails with
/// the schema-directed error, not a bare backend error — [`Encoding::write`]
/// for `Typed<T>` validates against the published document even though it
/// serializes natively.
#[test]
fn typed_put_against_a_mismatched_published_schema_fails_as_a_schema_read_error() {
    let store = store();
    store
        .dynamic(seg("thing"))
        .schema()
        .put(&schema_of::<Recipe>().unwrap())
        .unwrap();

    let err = store
        .kind::<Counter>(seg("thing"))
        .put(&entity("a"), &Counter { n: 1 })
        .unwrap_err();
    assert!(matches!(err, Error::SchemaRead(_)), "{err:?}");
}

#[test]
fn put_message_sets_the_commit_summary_and_keeps_the_schema_trailer() {
    let store = store();
    let schema_commit = store
        .dynamic(seg("counter"))
        .schema()
        .put(&schema_of::<Counter>().unwrap())
        .unwrap();
    let commit = store
        .dynamic(seg("counter"))
        .write(&value!({ "n": 1 }))
        .message("bump the counter")
        .at(&entity("c"))
        .unwrap();

    let GitObject::Commit(commit_obj) = store.objects().get(&commit).unwrap() else {
        panic!("expected a commit");
    };
    let message = String::from_utf8_lossy(&commit_obj.message).into_owned();
    assert_eq!(
        message,
        format!("bump the counter\n\nSchema: {schema_commit}\n")
    );
}

/// [`gix_store::Kind::get_entry`]/[`gix_store::Kind::get_entry_at`]: the
/// returned [`Entry`] names the exact commit the value came from — the same
/// one [`gix_store::Kind::history`] reports as the tip — and carries that
/// commit's summary, both for a default message and for one set through
/// [`gix_store::kind::Put::message`].
#[test]
fn get_entry_carries_the_commit_and_message_the_value_was_written_with() {
    let store = store();
    store
        .dynamic(seg("counter"))
        .schema()
        .put(&schema_of::<Counter>().unwrap())
        .unwrap();

    let first = store
        .dynamic(seg("counter"))
        .put(&entity("c"), &value!({ "n": 1 }))
        .unwrap();
    let second = store
        .dynamic(seg("counter"))
        .write(&value!({ "n": 2 }))
        .message("bump the counter")
        .at(&entity("c"))
        .unwrap();

    let Entry {
        value,
        commit,
        message,
    } = store
        .dynamic(seg("counter"))
        .get_entry(&entity("c"))
        .unwrap()
        .expect("entity was written");
    assert_eq!(value, value!({ "n": 2 }));
    assert_eq!(commit, second);
    assert_eq!(message, "bump the counter");
    assert_eq!(
        store.dynamic(seg("counter")).history(&entity("c")).unwrap()[0],
        commit,
        "get_entry's commit must be the same tip history() reports"
    );

    // The earlier commit reads back through get_entry_at with its own
    // default-summary message, distinct from the tip's.
    let earlier = store.dynamic(seg("counter")).get_entry_at(first).unwrap();
    assert_eq!(earlier.value, value!({ "n": 1 }));
    assert_eq!(earlier.commit, first);
    assert_eq!(earlier.message, "store counter/c");
}

/// [`gix_store::kind::Put::anonymous`] derives an entity's name from its own
/// commit id, so writing identical content twice must not be treated as a
/// name collision — it lands on the same commit both times.
#[test]
fn anonymous_write_of_identical_content_twice_is_idempotent() {
    let store = store();
    store
        .dynamic(seg("counter"))
        .schema()
        .put(&schema_of::<Counter>().unwrap())
        .unwrap();

    let value = value!({ "n": 1 });
    let first = store
        .dynamic(seg("counter"))
        .write(&value)
        .anonymous()
        .unwrap();
    let second = store
        .dynamic(seg("counter"))
        .write(&value)
        .anonymous()
        .unwrap();
    assert_eq!(first, second);
}

/// An anonymous entity is named by its commit id *in full*, so two unrelated
/// values collide only when their commits do. A truncation would put the
/// entity namespace's collision odds far below the object database's own.
#[test]
fn anonymous_names_an_entity_by_its_whole_commit_id() {
    let store = store();
    store
        .dynamic(seg("counter"))
        .schema()
        .put(&schema_of::<Counter>().unwrap())
        .unwrap();

    let commit = store
        .dynamic(seg("counter"))
        .write(&value!({ "n": 1 }))
        .anonymous()
        .unwrap();

    let name = entity_name(commit);
    assert_eq!(name.to_string(), commit.to_string());
    assert_eq!(store.dynamic(seg("counter")).list().unwrap(), vec![name]);
}

/// [`gix_store::kind::Put::anonymous_under`] names the entity
/// `<group>/<commit-oid>`, so entities of one group list as a nested
/// `RefPath` and [`entity_name_under`] recovers the same name without string
/// surgery.
#[test]
fn anonymous_under_names_an_entity_by_group_and_whole_commit_id() {
    let store = store();
    store
        .dynamic(seg("counter"))
        .schema()
        .put(&schema_of::<Counter>().unwrap())
        .unwrap();

    let group = entity("batch-1");
    let commit = store
        .dynamic(seg("counter"))
        .write(&value!({ "n": 1 }))
        .anonymous_under(&group)
        .unwrap();

    let name = entity_name_under(&group, commit);
    assert_eq!(name.to_string(), format!("batch-1/{commit}"));
    assert_eq!(store.dynamic(seg("counter")).list().unwrap(), vec![name]);
}

// --- fault-injecting RefStore: retry paths ---

/// A scripted failure for [`FlakyRefStore::apply`].
enum Injection {
    /// Land this edit for real first, so the caller's own edit then fails
    /// against a genuine precondition mismatch on the backend — simulates a
    /// concurrent writer that actually won the race.
    Concurrent(RefEdit),
    /// Fail immediately with nothing written — contention indistinguishable
    /// from a real race by the caller, but the backend never changes.
    Phantom,
}

/// Wraps a [`MemoryRefStore`], scripting its first `apply` calls to fail —
/// exercises retry paths without a real concurrent writer.
struct FlakyRefStore {
    inner: MemoryRefStore,
    injections: RefCell<VecDeque<Injection>>,
}

impl FlakyRefStore {
    fn new() -> Self {
        Self {
            inner: MemoryRefStore::new(),
            injections: RefCell::new(VecDeque::new()),
        }
    }

    fn push_concurrent(&self, edit: RefEdit) {
        self.injections
            .borrow_mut()
            .push_back(Injection::Concurrent(edit));
    }

    fn push_phantom(&self) {
        self.injections.borrow_mut().push_back(Injection::Phantom);
    }
}

impl RefStore for FlakyRefStore {
    type Error = Infallible;

    fn read(&self, name: &RefName) -> Result<Option<ObjectId>, Self::Error> {
        self.inner.read(name)
    }

    fn prefixed(&self, prefix: &RefPrefix) -> Result<Vec<(RefName, ObjectId)>, Self::Error> {
        self.inner.prefixed(prefix)
    }

    fn apply_batch(&self, edits: Vec<RefEdit>) -> Result<(), ApplyError<Self::Error>> {
        match self.injections.borrow_mut().pop_front() {
            Some(Injection::Concurrent(winner)) => {
                self.inner
                    .apply(winner)
                    .expect("injected concurrent edit applies cleanly");
                self.inner.apply_batch(edits)
            }
            Some(Injection::Phantom) => {
                let edit = edits.first().expect("batch is not empty");
                Err(ApplyError::LostRace {
                    name: edit.name().clone(),
                    expected: edit.expectation(),
                })
            }
            None => self.inner.apply_batch(edits),
        }
    }
}

impl Committer for FlakyRefStore {
    type Error = Infallible;

    fn signature(&self) -> Result<gix::actor::Signature, Self::Error> {
        self.inner.signature()
    }
}

fn flaky_store() -> Store<FlakyRefStore, ObjectStore> {
    Store::new(FlakyRefStore::new(), ObjectStore::default())
}

/// [`Kind::update`]'s whole reason to exist: `rebuild` must see the entry
/// its own compare-and-swap actually lands over, not the one read before a
/// concurrent write landed — so forwarding state off the current value
/// (a running total, here) survives a real mid-write race intact.
#[test]
fn update_rebuilds_from_the_entry_the_retry_actually_commits_over() {
    let store = flaky_store();
    let counter = store.kind::<Counter>(seg("counter"));
    counter.publish().unwrap();

    let original = counter.put(&entity("c"), &Counter { n: 1 }).unwrap();

    // Build a legitimately schema-bound competing commit, parented on the
    // same tip, without ever letting it touch the "c" ref directly.
    let scratch = entity("scratch");
    store
        .refs()
        .apply(RefEdit::Create {
            name: counter.reference(&scratch),
            new: original,
        })
        .unwrap();
    let winner = counter.write(&Counter { n: 50 }).at(&scratch).unwrap();

    store.refs().push_concurrent(RefEdit::Update {
        name: counter.reference(&entity("c")),
        expected: original,
        new: winner,
    });

    let commit = counter
        .update(&entity("c"), |current| {
            let n = current.map_or(0, |entry| entry.value.n);
            (format!("bump to {}", n + 1), Counter { n: n + 1 })
        })
        .unwrap();

    assert_eq!(
        counter.get_at(commit).unwrap(),
        Counter { n: 51 },
        "rebuilt off the race winner's n=50, not the stale n=1 read before the race"
    );
    assert_eq!(
        counter.history(&entity("c")).unwrap(),
        vec![commit, winner, original]
    );
}

/// A caller whose forwarding needs an existing entry can refuse to commit at
/// all, rather than recreating an entity from nothing, and gets its own error
/// type back.
#[test]
fn try_update_lets_rebuild_refuse_an_absent_entry() {
    #[derive(Debug)]
    enum Refusal {
        Absent,
        Store(
            #[expect(dead_code, reason = "carried for Debug, never matched on")] gix_store::Error,
        ),
    }

    impl From<gix_store::Error> for Refusal {
        fn from(err: gix_store::Error) -> Self {
            Refusal::Store(err)
        }
    }

    let store = store();
    let counter = store.kind::<Counter>(seg("counter"));
    counter.publish().unwrap();

    let refused = counter.try_update::<Refusal>(&entity("missing"), |current| match current {
        Some(entry) => Ok((
            "bump".to_owned(),
            Counter {
                n: entry.value.n + 1,
            },
        )),
        None => Err(Refusal::Absent),
    });

    assert!(matches!(refused, Err(Refusal::Absent)));
    assert_eq!(
        counter.get(&entity("missing")).unwrap(),
        None,
        "a refused rebuild writes no ref"
    );
}

/// A [`Put::anonymous`] whose first `Create` loses to spurious backend
/// contention — nothing actually landed — retries the very same create
/// rather than reporting [`Error::NameTaken`]: the name is still free, so
/// there is nothing to have collided with.
#[test]
fn anonymous_retries_to_success_on_pure_contention() {
    let store = flaky_store();
    store
        .dynamic(seg("counter"))
        .schema()
        .put(&schema_of::<Counter>().unwrap())
        .unwrap();
    store.refs().push_phantom();

    let commit = store
        .dynamic(seg("counter"))
        .write(&value!({ "n": 1 }))
        .anonymous()
        .unwrap();
    assert_eq!(
        store.dynamic(seg("counter")).get_at(commit).unwrap(),
        value!({ "n": 1 })
    );
}

// --- schema-schema pin ---

/// A stored schema tree with no `schema` pin entry at all — the shape every
/// schema published before pinning existed has — is refused as
/// [`SchemaPinError::Unpinned`], naming re-publishing as the remedy, not
/// silently read as though it were the genesis generation (it is not: only a
/// known root generation's own tree id is entitled to that reading).
#[test]
fn schema_with_no_pin_entry_is_refused() {
    let store = store();

    let good_tree = schema_of::<Counter>()
        .unwrap()
        .write_pinned(store.objects())
        .unwrap();
    let stripped = rewrite_tree(store.objects(), good_tree, |entries| {
        entries.retain(|e| e.filename != "schema");
    });
    commit_ref(
        &store,
        "refs/schema/counter",
        "schema counter\n",
        stripped,
        None,
    );

    let err = store.dynamic(seg("counter")).schema().get().unwrap_err();
    assert!(
        matches!(
            &err,
            Error::SchemaPin(SchemaPinError::Unpinned(oid)) if *oid == stripped
        ),
        "{err:?}"
    );
}

/// The critical case the whole schema-schema pin exists for: a schema tree
/// that both pins a schema-schema this binary does not recognize, *and*
/// contains a `Node` variant this binary has never heard of (simulated as a
/// `root` tagged `DateTime`, the same repro the old version-marker issue
/// named) — something a full typed deserialize cannot get through.
/// [`gix_store::KindSchema::get`] must report the pin error here, not the
/// reflection error the corrupt content would otherwise produce, which is
/// only possible because [`facet_git_tree::Schema::read_pinned`] checks
/// the pin out of band *before* attempting to deserialize the rest of the
/// document.
#[test]
fn unrecognized_pin_is_refused_before_a_reflection_incompatible_read_is_attempted() {
    let store = store();

    let good_tree = schema_of::<Counter>()
        .unwrap()
        .write_pinned(store.objects())
        .unwrap();
    let bogus_pin = write_blob(store.objects(), b"not a schema-schema\n");
    let bogus_root = write_blob(store.objects(), b"DateTime\n");
    let corrupt = rewrite_tree(store.objects(), good_tree, |entries| {
        for entry in entries.iter_mut() {
            if entry.filename == "schema" {
                entry.oid = bogus_pin;
            } else if entry.filename == "root" {
                entry.mode = gix::objs::tree::EntryKind::Blob.into();
                entry.oid = bogus_root;
            }
        }
    });
    commit_ref(
        &store,
        "refs/schema/counter",
        "schema counter\n",
        corrupt,
        None,
    );

    let err = store.dynamic(seg("counter")).schema().get().unwrap_err();
    assert!(
        matches!(
            &err,
            Error::SchemaPin(SchemaPinError::Unrecognized { tree, pinned })
                if *tree == corrupt && *pinned == bogus_pin
        ),
        "expected SchemaPin(Unrecognized) (the out-of-band pin check catching the unrecognized \
         pin before a full deserialize is attempted), got {err:?} instead — a reflection error \
         here would mean the pin check ran too late to matter"
    );
}

/// The negative control for the test above: the very same `DateTime`-tagged
/// root, but with the pin left intact and recognized, is not caught by the
/// pin check (there is nothing to catch) and instead fails during the full
/// typed deserialize with a reflection error — confirming the corrupted
/// content really would break an ordinary read, so the pin check in the
/// previous test is not simply vacuous.
#[test]
fn an_unknown_variant_with_a_recognized_pin_fails_as_a_reflection_error() {
    let store = store();

    let good_tree = schema_of::<Counter>()
        .unwrap()
        .write_pinned(store.objects())
        .unwrap();
    let bogus_root = write_blob(store.objects(), b"DateTime\n");
    let corrupt = rewrite_tree(store.objects(), good_tree, |entries| {
        for entry in entries.iter_mut() {
            if entry.filename == "root" {
                entry.mode = gix::objs::tree::EntryKind::Blob.into();
                entry.oid = bogus_root;
            }
        }
    });
    commit_ref(
        &store,
        "refs/schema/counter",
        "schema counter\n",
        corrupt,
        None,
    );

    let err = store.dynamic(seg("counter")).schema().get().unwrap_err();
    assert!(
        matches!(
            &err,
            Error::SchemaPin(SchemaPinError::Deserialize(DeserializeError::Reflect(msg)))
                if msg.contains("DateTime")
        ),
        "{err:?}"
    );
}

/// Publishing over a schema tip pinned to a schema-schema this binary does
/// not recognize is refused. Declining to *read* such a schema while
/// overwriting it anyway would make the same unkeepable claim from the other
/// direction.
#[test]
fn put_schema_refuses_to_publish_over_a_tip_with_an_unrecognized_pin() {
    let store = store();

    let doc = schema_of::<Counter>().unwrap();
    let good_tree = doc.write_pinned(store.objects()).unwrap();
    let bogus_pin = write_blob(store.objects(), b"not a schema-schema\n");
    let ahead = rewrite_tree(store.objects(), good_tree, |entries| {
        for entry in entries.iter_mut() {
            if entry.filename == "schema" {
                entry.oid = bogus_pin;
            }
        }
    });
    commit_ref(
        &store,
        "refs/schema/counter",
        "schema counter\n",
        ahead,
        None,
    );

    let err = store
        .dynamic(seg("counter"))
        .schema()
        .put(&doc)
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::SchemaPin(SchemaPinError::Unrecognized { tree, pinned })
                if tree == ahead && pinned == bogus_pin
        ),
        "expected publishing over an unrecognized-pin tip to be refused, got {err:?}"
    );
}

/// The counterpart to the test above, and the reason publishing checks the
/// tip's pin rather than merely that it *has* one: a tip that predates
/// pinning — or is otherwise unpinned — stays overwritable. Republishing is
/// the remedy [`SchemaPinError::Unpinned`] names, so refusing here would
/// strand every pre-pinning repository with no way to migrate.
#[test]
fn put_schema_still_republishes_over_an_unpinned_tip() {
    let store = store();

    let doc = schema_of::<Counter>().unwrap();
    let good_tree = doc.write_pinned(store.objects()).unwrap();
    let stripped = rewrite_tree(store.objects(), good_tree, |entries| {
        entries.retain(|e| e.filename != "schema");
    });
    commit_ref(
        &store,
        "refs/schema/counter",
        "schema counter\n",
        stripped,
        None,
    );
    assert!(
        store.dynamic(seg("counter")).schema().get().is_err(),
        "fixture should start unreadable, or this proves nothing"
    );

    store.dynamic(seg("counter")).schema().put(&doc).unwrap();
    assert_eq!(
        store
            .dynamic(seg("counter"))
            .schema()
            .get()
            .unwrap()
            .as_ref(),
        Some(&doc)
    );
}

/// The *data* read path is pin-gated too, not only [`gix_store::KindSchema::get`].
///
/// Subtree binding puts the schema inside every data commit (`{schema/,
/// value/}`) precisely so a fetched value stays readable where its kind was
/// never published — which makes the data path the one that most needs this
/// gate, and the one a reader in another repository actually travels.
/// `get_at` resolves `schema/` through the same pin-checked `read_pinned` as
/// everything else; without this test, a refactor inlining a plain
/// deserialize there would drop the gate on exactly that path and leave the
/// rest of the suite green.
#[test]
fn an_unrecognized_pin_in_a_data_commits_schema_subtree_is_refused_on_retrieve() {
    let store = store();

    store
        .dynamic(seg("recipe"))
        .schema()
        .put(&schema_of::<Recipe>().unwrap())
        .unwrap();
    let carbonara = value!({ "title": "Carbonara", "serves": 4, "steps": ["boil"] });
    store
        .dynamic(seg("recipe"))
        .put(&entity("carbonara"), &carbonara)
        .unwrap();
    // Reading works before the rewrite, so the failure below is the pin
    // gate rather than a fixture that never read in the first place.
    assert_eq!(
        store
            .dynamic(seg("recipe"))
            .get(&entity("carbonara"))
            .unwrap(),
        Some(carbonara)
    );

    let reference = store.dynamic(seg("recipe")).reference(&entity("carbonara"));
    let tip = store.refs().read(&reference).unwrap().unwrap();
    let GitObject::Commit(tip_obj) = store.objects().get(&tip).unwrap() else {
        panic!("expected a commit");
    };
    let root = tip_obj.tree;
    let root_entries = store.objects().get_tree(&root).unwrap();
    let schema_tree = root_entries
        .iter()
        .find(|e| e.filename == "schema")
        .unwrap()
        .oid;

    // Repoint only the pin entry inside the bound schema subtree: the
    // document stays perfectly decodable, so nothing but the gate can reject
    // it.
    let bogus_pin = write_blob(store.objects(), b"not a schema-schema\n");
    let corrupt_schema = rewrite_tree(store.objects(), schema_tree, |entries| {
        for entry in entries.iter_mut() {
            if entry.filename == "schema" {
                entry.oid = bogus_pin;
            }
        }
    });
    let corrupt_root = rewrite_tree(store.objects(), root, |entries| {
        for entry in entries.iter_mut() {
            if entry.filename == "schema" {
                entry.oid = corrupt_schema;
            }
        }
    });
    commit_ref(
        &store,
        reference.as_str(),
        "store recipe/carbonara\n",
        corrupt_root,
        Some(tip),
    );

    let err = store
        .dynamic(seg("recipe"))
        .get(&entity("carbonara"))
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::SchemaPin(SchemaPinError::Unrecognized { tree, pinned })
                if tree == corrupt_schema && pinned == bogus_pin
        ),
        "expected the data read path to refuse an unrecognized-pin bound schema, got {err:?} — a \
         successful read here would mean a value fetched from elsewhere is decoded against a \
         schema this binary cannot vouch for"
    );
}

/// Two generations of one type. Derivation pairs definitions by name, so the
/// type keeps its name across the edge exactly as a real evolution would.
mod v1 {
    use facet::Facet;
    #[derive(Facet)]
    pub struct Thing {
        pub name: String,
    }
}
mod v2 {
    use facet::Facet;
    #[derive(Facet)]
    pub struct Thing {
        pub name: String,
        pub rank: u32,
    }
}

/// `rank` is new on the target side, so the edge derives only with a default
/// to fill it: without the hint the added field has no image.
fn rank_defaulted() -> Hints {
    Hints::new().defaulted(Target::Def("Thing".into()), "rank", Constant::Integer(0))
}

/// A schema advance records the derived migration in the advancing commit's
/// own tree, and a value written under the predecessor upcasts through it.
#[test]
fn an_old_value_upcasts_to_the_current_schema() {
    let store = store();
    let thing = || store.dynamic(seg("thing"));

    thing()
        .schema()
        .put(&schema_of::<v1::Thing>().unwrap())
        .unwrap();
    thing()
        .put(&entity("a"), &value!({ "name": "old" }))
        .unwrap();

    let advance = thing()
        .schema()
        .write(&schema_of::<v2::Thing>().unwrap(), &rank_defaulted())
        .unwrap();

    // The migration travels in the schema commit's own tree.
    assert!(
        thing().schema().migration_at(advance).unwrap().is_some(),
        "the advancing commit must record its migration"
    );

    // Reading under the value's own bound schema is unchanged...
    assert_eq!(
        thing().get(&entity("a")).unwrap(),
        Some(value!({ "name": "old" }))
    );
    // ...and reading it as the current schema fills the added field.
    assert_eq!(
        thing().get_migrated(&entity("a")).unwrap(),
        Some(value!({ "name": "old", "rank": 0 }))
    );
}

/// Upcasting never rewrites: the stored value keeps its object id, so every
/// attestation bound to that id survives the schema advance.
#[test]
fn upcasting_leaves_the_stored_value_untouched() {
    let store = store();
    let thing = || store.dynamic(seg("thing"));

    thing()
        .schema()
        .put(&schema_of::<v1::Thing>().unwrap())
        .unwrap();
    let commit = thing()
        .put(&entity("a"), &value!({ "name": "old" }))
        .unwrap();
    let before = commit_tree(&store, commit);

    thing()
        .schema()
        .write(&schema_of::<v2::Thing>().unwrap(), &rank_defaulted())
        .unwrap();
    thing().get_migrated(&entity("a")).unwrap();

    assert_eq!(
        before,
        commit_tree(&store, commit),
        "upcasting must not rewrite the stored value"
    );
}

/// A value whose bound schema was never published under this kind has no
/// chain to the current schema, and says so.
#[test]
fn a_foreign_schema_tree_has_no_chain() {
    let store = store();
    store
        .dynamic(seg("thing"))
        .schema()
        .put(&schema_of::<v1::Thing>().unwrap())
        .unwrap();
    let commit = store
        .dynamic(seg("thing"))
        .put(&entity("a"), &value!({ "name": "old" }))
        .unwrap();

    // Republish an unrelated shape as a *different* kind, then ask that kind
    // to upcast a commit bound to the first kind's schema.
    store
        .dynamic(seg("other"))
        .schema()
        .put(&schema_of::<v2::Thing>().unwrap())
        .unwrap();

    let err = store
        .dynamic(seg("other"))
        .get_at_migrated(commit)
        .unwrap_err();
    assert!(matches!(err, Error::SchemaNotInHistory { .. }), "{err:?}");
}

/// The tree of the commit `id` names.
fn commit_tree(store: &Store<MemoryRefStore, ObjectStore>, id: ObjectId) -> ObjectId {
    let GitObject::Commit(commit) = store.objects().get(&id).unwrap() else {
        panic!("expected a commit");
    };
    commit.tree
}
