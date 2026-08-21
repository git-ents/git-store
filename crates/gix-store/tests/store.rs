#![allow(deprecated)]
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
    Constant, DeserializeError, GitObject, Hints, Node, ObjectStore, SchemaPinError, StructField,
    Target, TreeEntry, schema_of,
};
use facet_value::value;
use gix::bstr::ByteSlice;
use gix_refstore::RefEdit;
use gix::objs::Write as _;
use gix_store::{
    ApplyError, At, Committer, Compat, DeleteResult, DocumentInspection, DocumentKind,
    DocumentShapeError, DocumentTree, EntityState, Entry, Error, Expectation, MemoryRefStore,
    ObjectId, RefName, RefPath, RefPrefix, RefSegment, RefStore, SchemaTree, Store, Subtree,
    TargetSchema, ValueTree, entity_id_name, entity_name, entity_name_under,
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

fn bound_document(
    store: &Store<MemoryRefStore, ObjectStore>,
    value: &facet_value::Value,
    doc: &facet_git_tree::Schema,
) -> ObjectId {
    let schema_tree = doc.write_pinned(store.objects()).expect("write schema");
    let value_tree = facet_git_tree::serialize_value_with_schema(value, doc, store.objects())
        .expect("write value");
    let mut entries = vec![
        TreeEntry {
            mode: gix::objs::tree::EntryKind::Tree.into(),
            filename: "schema".into(),
            oid: schema_tree,
        },
        TreeEntry {
            mode: gix::objs::tree::EntryKind::Tree.into(),
            filename: "value".into(),
            oid: value_tree,
        },
    ];
    entries.sort();
    gix::objs::Write::write(store.objects(), &gix::objs::Tree { entries }).expect("write document")
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

#[derive(Facet)]
struct CounterWithLabel {
    n: u32,
    label: String,
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
fn embedded_document_decodes_without_a_kind_or_publication_ref() {
    let store = store();
    let doc = schema_of::<Counter>().unwrap().with_kind("orphan").unwrap();
    let expected = value!({ "n": 7 });
    let tree = bound_document(&store, &expected, &doc);

    assert_eq!(store.decode(tree).unwrap(), expected);
    assert_eq!(gix_store::decode(tree, store.objects()).unwrap(), expected);
}

#[test]
fn explicit_schema_value_codecs_round_trip_without_a_kind_handle() {
    let store = store();
    let doc = schema_of::<Counter>()
        .unwrap()
        .with_kind("counter")
        .unwrap();
    let schema_tree = doc.write_pinned(store.objects()).unwrap();
    let expected = value!({ "n": 7 });

    let value_tree = store
        .encode_value(&expected, SchemaTree::from(schema_tree))
        .unwrap();
    assert_eq!(
        store
            .decode_value(value_tree, SchemaTree::from(schema_tree))
            .unwrap(),
        expected
    );
    assert!(matches!(
        store.encode_value(&value!({ "n": "wrong" }), SchemaTree::from(schema_tree)),
        Err(Error::SchemaWrite(_))
    ));
}

#[test]
fn binding_and_inspection_describe_a_prepared_document() {
    let store = store();
    let doc = schema_of::<Counter>()
        .unwrap()
        .with_kind("counter")
        .unwrap();
    let schema_tree = doc.write_pinned(store.objects()).unwrap();
    let value_tree = store
        .encode_value(&value!({ "n": 7 }), SchemaTree::from(schema_tree))
        .unwrap();
    let prepared = store
        .bind_document(value_tree, SchemaTree::from(schema_tree))
        .unwrap();
    assert!(store.kinds().unwrap().is_empty());

    assert_eq!(
        store.inspect_document(prepared.document_tree()).unwrap(),
        DocumentInspection::Bound(prepared)
    );
    assert_eq!(
        store
            .inspect_document(DocumentTree::from(value_tree.object_id()))
            .unwrap()
            .kind(),
        DocumentKind::LegacyValueRoot
    );
}

#[test]
fn inspection_reports_malformed_envelopes_without_guessing() {
    let store = store();
    let value_tree = store
        .encode_value(
            &value!({ "n": 7 }),
            SchemaTree::from(
                schema_of::<Counter>()
                    .unwrap()
                    .write_pinned(store.objects())
                    .unwrap(),
            ),
        )
        .unwrap();
    let malformed = gix::objs::Write::write(
        store.objects(),
        &gix::objs::Tree {
            entries: vec![TreeEntry {
                mode: gix::objs::tree::EntryKind::Tree.into(),
                filename: "value".into(),
                oid: value_tree.object_id(),
            }],
        },
    )
    .unwrap();

    let DocumentInspection::Malformed { found, reason, .. } = store
        .inspect_document(DocumentTree::from(malformed))
        .unwrap()
    else {
        panic!("expected malformed document metadata");
    };
    assert_eq!(found, vec!["value"]);
    assert!(matches!(
        reason,
        DocumentShapeError::UnexpectedEntries { found } if found == vec!["value"]
    ));
}

#[test]
fn schema_snapshots_are_owned_and_independent_of_publication_progress() {
    let store = store();
    let schema = store.dynamic(seg("counter")).schema();
    let first = schema_of::<Counter>().unwrap();
    let first_published = first.clone().with_kind("counter").unwrap();
    let first_commit = schema.put(&first).unwrap();
    let first_snapshot = schema.current_snapshot().unwrap();

    let second = schema_of::<CounterWithLabel>().unwrap();
    let second_published = second.clone().with_kind("counter").unwrap();
    let second_commit = schema.put(&second).unwrap();
    let current = schema.current_snapshot().unwrap();
    let historical = schema.snapshot_at(first_commit).unwrap();

    assert_eq!(first_snapshot.commit(), first_commit);
    assert_eq!(first_snapshot.schema, first_published);
    assert_eq!(historical.commit(), first_commit);
    assert_eq!(historical.schema_tree(), first_snapshot.schema_tree());
    assert_eq!(current.commit(), second_commit);
    assert_eq!(current.schema, second_published);
    assert_ne!(current.schema_tree(), first_snapshot.schema_tree());
    assert_eq!(historical.schema, first_snapshot.schema);
}

#[test]
fn store_retrieve_list_and_schema_roundtrip() {
    let store = store();

    let doc = schema_of::<Recipe>().unwrap();
    let expected = doc.clone().with_kind("recipe").unwrap();
    store.dynamic(seg("recipe")).schema().put(&doc).unwrap();
    assert_eq!(
        store
            .dynamic(seg("recipe"))
            .schema()
            .get()
            .unwrap()
            .as_ref(),
        Some(&expected)
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
    // A new value conforming to v2 stores and reads under the evolved schema.
    let v2 = value!({ "name": "new", "rank": 1 });
    store.dynamic(seg("thing")).put(&entity("b"), &v2).unwrap();
    assert_eq!(
        store.dynamic(seg("thing")).get(&entity("b")).unwrap(),
        Some(v2)
    );

    let current_schema_ref = RefName::new("refs/schema/thing").unwrap();
    let current_schema_commit = store
        .refs()
        .read(&current_schema_ref)
        .unwrap()
        .expect("the evolved schema ref should exist");
    store
        .refs()
        .apply(RefEdit::Delete {
            name: current_schema_ref,
            expected: current_schema_commit,
        })
        .unwrap();

    // The old commit reads back through its own `schema/` subtree, even when
    // the current schema publication ref is unavailable.
    assert_eq!(store.dynamic(seg("thing")).schema().get().unwrap(), None);
    assert_eq!(store.dynamic(seg("thing")).get_at(old_commit).unwrap(), v1);

    let document_tree = match store.objects().get(&old_commit).unwrap() {
        GitObject::Commit(commit) => commit.tree,
        other => panic!("expected a commit, got {other:?}"),
    };
    assert_eq!(store.decode(document_tree).unwrap(), v1);
}

#[test]
fn explicit_target_migration_uses_captured_schema_history() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    let old = kind.put(&entity("old"), &value!({ "n": 1 })).unwrap();

    let mut evolved = schema_of::<Counter>().unwrap();
    let Node::Struct(fields) = evolved.defs.get_mut("Counter").unwrap() else {
        panic!("Counter schema should have a struct definition");
    };
    fields.insert(
        "label".into(),
        StructField {
            node: Node::String,
            has_default: true,
        },
    );
    let hints = Hints::new().defaulted(
        Target::Def("Counter".into()),
        "label",
        Constant::Text("migrated".into()),
    );
    kind.schema().write(&evolved, &hints).unwrap();

    let target = TargetSchema::new(
        kind.schema().get().unwrap().unwrap(),
        kind.schema().history().unwrap(),
    );
    assert_eq!(
        kind.get_at_migrated_to(old, &target).unwrap(),
        value!({ "n": 1, "label": "migrated" })
    );

    // The explicit target is self-contained. The live schema ref may disappear
    // after a caller has captured the target and its history.
    let schema_ref = RefName::new("refs/schema/counter").unwrap();
    let schema_tip = store.refs().read(&schema_ref).unwrap().unwrap();
    store
        .refs()
        .apply(RefEdit::Delete {
            name: schema_ref,
            expected: schema_tip,
        })
        .unwrap();
    assert_eq!(
        kind.get_at_migrated_to(old, &target).unwrap(),
        value!({ "n": 1, "label": "migrated" })
    );

    let unavailable = TargetSchema::new(target.schema().clone(), vec![target.history()[0]]);
    let err = kind.get_at_migrated_to(old, &unavailable).unwrap_err();
    assert!(
        matches!(err, Error::TargetSchemaNotInHistory { .. }),
        "expected an explicit target-history error, got {err:?}"
    );
}

#[test]
fn read_and_read_as_cover_every_address_including_entity_id() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    let id = kind.put_entity(&value!({ "n": 1 })).unwrap();
    let commit = match kind.read(id).unwrap() {
        EntityState::Present(entry) => entry.commit,
        other => panic!("expected the freshly published entity to be present, got {other:?}"),
    };
    let document_tree = match store.objects().get(&commit).unwrap() {
        GitObject::Commit(commit) => commit.tree,
        other => panic!("expected a commit, got {other:?}"),
    };
    let name = entity("alias");
    kind.put_with_alias(&name, &value!({ "n": 1 })).unwrap();

    let value = value!({ "n": 1 });
    // Every address axis reaches the same document: a caller-chosen alias,
    // the content-derived entity id, the publication commit, and the bare
    // document tree addressed without any ref.
    assert_eq!(
        kind.read(At::Name(name.clone())).unwrap().value(),
        Some(value.clone())
    );
    assert_eq!(
        kind.read(At::Entity(id)).unwrap().value(),
        Some(value.clone())
    );
    assert_eq!(
        kind.read(At::Commit(commit)).unwrap().value(),
        Some(value.clone())
    );
    assert_eq!(
        kind.read(At::Tree(document_tree)).unwrap().value(),
        Some(value.clone())
    );

    let mut evolved = schema_of::<Counter>().unwrap();
    let Node::Struct(fields) = evolved.defs.get_mut("Counter").unwrap() else {
        panic!("Counter schema should have a struct definition");
    };
    fields.insert(
        "label".into(),
        StructField {
            node: Node::String,
            has_default: true,
        },
    );
    let hints = Hints::new().defaulted(
        Target::Def("Counter".into()),
        "label",
        Constant::Text("migrated".into()),
    );
    kind.schema().write(&evolved, &hints).unwrap();
    let target = TargetSchema::new(
        kind.schema().get().unwrap().unwrap(),
        kind.schema().history().unwrap(),
    );
    let migrated = value!({ "n": 1, "label": "migrated" });

    // The migration axis is available for every address `read` accepts,
    // including `At::Entity` — previously there was no migration-aware read
    // addressed by `EntityId` at all.
    assert_eq!(
        kind.read_as(At::Name(name), &target).unwrap().value(),
        Some(migrated.clone())
    );
    assert_eq!(
        kind.read_as(At::Entity(id), &target).unwrap().value(),
        Some(migrated.clone())
    );
    assert_eq!(
        kind.read_as(At::Commit(commit), &target).unwrap().value(),
        Some(migrated.clone())
    );
    assert_eq!(
        kind.read_as(At::Tree(document_tree), &target)
            .unwrap()
            .value(),
        Some(migrated)
    );
}

#[test]
fn tombstone_migrated_read_bypasses_an_unavailable_target() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    let id = kind.put_entity(&value!({ "n": 1 })).unwrap();
    let tombstone = match kind.delete_entity(id).unwrap() {
        DeleteResult::Deleted(entry) => entry.commit,
        other => panic!("expected tombstone, got {other:?}"),
    };

    let target = TargetSchema::new(schema_of::<Counter>().unwrap(), Vec::new());
    let schema_ref = RefName::new("refs/schema/counter").unwrap();
    let schema_tip = store.refs().read(&schema_ref).unwrap().unwrap();
    store
        .refs()
        .apply(RefEdit::Delete {
            name: schema_ref,
            expected: schema_tip,
        })
        .unwrap();

    assert!(matches!(
        kind.read_at_migrated_to(tombstone, &target).unwrap(),
        EntityState::Deleted(entry) if entry.commit == tombstone
    ));
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

#[test]
fn new_schema_and_data_commits_have_no_schema_or_provenance_trailers() {
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

    for commit in [schema_commit, data_commit] {
        let gix::objs::Object::Commit(commit) = store.objects().get(&commit).unwrap() else {
            panic!("not a commit");
        };
        assert!(
            commit
                .message
                .as_bytes()
                .split(|&byte| byte == b'\n')
                .all(|line| {
                    !line.starts_with(b"Schema:")
                        && !line.starts_with(b"Schema-Version:")
                        && !line.starts_with(b"Ents-Ref:")
                })
        );
        assert!(commit.extra_headers.is_empty());
    }
}

#[test]
fn legacy_schema_and_provenance_trailers_are_ignored() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    let value = value!({ "n": 1 });
    let original = kind.put(&entity("original"), &value).unwrap();
    let gix::objs::Object::Commit(commit) = store.objects().get(&original).unwrap() else {
        panic!("not a commit");
    };

    let legacy = commit_ref(
        &store,
        "refs/store/counter/legacy",
        "legacy\n\nSchema: not-an-object-id\nSchema-Version: conflicting\nEnts-Ref: refs/schema/other\n",
        commit.tree,
        None,
    );
    assert_eq!(kind.get_at(legacy).unwrap(), value);
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
fn listing_reports_every_name_including_content_derived_ones() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();

    let by_content = kind.put_entity(&value!({ "n": 1 })).unwrap();
    let by_name = entity("legacy/named");
    kind.put(&by_name, &value!({ "n": 2 })).unwrap();

    let names = kind.list().unwrap();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&entity_id_name(by_content)));
    assert!(names.contains(&by_name));
}
#[test]
fn delete_name_tombstones_the_name_and_retains_its_history() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    let name = entity("legacy/history");
    kind.put(&name, &value!({ "n": 1 })).unwrap();
    let first_commit = kind.get_entry(&name).unwrap().unwrap().commit;
    kind.put(&name, &value!({ "n": 2 })).unwrap();

    assert_eq!(kind.list().unwrap(), vec![name.clone()]);

    let tombstone_commit = match kind.delete_name(&name).unwrap() {
        DeleteResult::Deleted(entry) => entry.commit,
        other => panic!("expected deletion, got {other:?}"),
    };
    assert!(matches!(
        kind.read(name.clone()).unwrap(),
        EntityState::Deleted(entry) if entry.commit == tombstone_commit
    ));
    assert!(kind.list().unwrap().is_empty());
    // Deletion publishes over the name rather than pruning it, so every
    // earlier publication stays reachable.
    assert_eq!(kind.get_at(first_commit).unwrap(), value!({ "n": 1 }));
    assert_eq!(kind.history(&name).unwrap().len(), 3);

    match kind.delete_name(&name).unwrap() {
        DeleteResult::AlreadyDeleted(entry) => assert_eq!(entry.commit, tombstone_commit),
        other => panic!("repeated deletion must be idempotent, got {other:?}"),
    }
}
#[test]
fn remove_prunes_a_name_without_a_tombstone() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    let name = entity("legacy/only");
    kind.put(&name, &value!({ "n": 1 })).unwrap();

    assert!(kind.remove(&name).unwrap());
    assert!(kind.list().unwrap().is_empty());
    assert!(kind.list_entries().unwrap().is_empty());
    assert!(matches!(
        kind.read(name.clone()).unwrap(),
        EntityState::Absent
    ));
    assert!(!kind.remove(&name).unwrap());
    assert!(matches!(
        kind.delete_name(&name).unwrap(),
        DeleteResult::Absent
    ));
}
#[test]
fn deleting_one_name_leaves_other_names_for_the_same_content_alone() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    let value = value!({ "n": 1 });
    let first = entity("legacy/current");
    let second = entity("legacy/other");
    kind.put(&first, &value).unwrap();
    kind.put(&second, &value).unwrap();

    let tombstone_commit = match kind.delete_name(&first).unwrap() {
        DeleteResult::Deleted(entry) => entry.commit,
        other => panic!("expected deletion, got {other:?}"),
    };
    assert!(matches!(
        kind.read(first.clone()).unwrap(),
        EntityState::Deleted(entry) if entry.commit == tombstone_commit
    ));
    // Names are independent: what one name means is not what another means,
    // even when both currently address identical content.
    assert_eq!(kind.get(&second).unwrap(), Some(value));
    assert_eq!(kind.list().unwrap(), vec![second]);
}
#[test]
fn delete_name_tombstones_a_rewound_name_from_its_current_target() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    let name = entity("legacy/history");
    kind.put(&name, &value!({ "n": 1 })).unwrap();
    let first_commit = kind.get_entry(&name).unwrap().unwrap().commit;
    kind.put(&name, &value!({ "n": 2 })).unwrap();
    let second_commit = kind.get_entry(&name).unwrap().unwrap().commit;

    // Simulate a fetched or rewound ref that still names the historical
    // publication.
    store
        .refs()
        .apply(RefEdit::Update {
            name: kind.reference(&name),
            expected: second_commit,
            new: first_commit,
        })
        .unwrap();

    let tombstone_commit = match kind.delete_name(&name).unwrap() {
        DeleteResult::Deleted(entry) => entry.commit,
        other => panic!("expected deletion, got {other:?}"),
    };
    assert!(matches!(
        kind.read(name.clone()).unwrap(),
        EntityState::Deleted(entry) if entry.commit == tombstone_commit
    ));
    // The tombstone records the identity that the name actually pointed at.
    match kind.read(name.clone()).unwrap() {
        EntityState::Deleted(entry) => assert_eq!(
            entry.tombstone.entity_id(),
            Some(kind.compile_entity(&value!({ "n": 1 })).unwrap())
        ),
        other => panic!("expected a tombstone, got {other:?}"),
    }
}
#[test]
fn ordinary_reads_and_canonical_recognition_reject_cross_kind_documents() {
    let store = store();
    let counter = store.dynamic(seg("counter"));
    let foreign = store.dynamic(seg("foreign"));
    counter
        .schema()
        .put(&schema_of::<Counter>().unwrap())
        .unwrap();
    foreign
        .schema()
        .put(&schema_of::<Counter>().unwrap())
        .unwrap();
    let foreign_id = foreign.put_entity(&value!({ "n": 1 })).unwrap();
    let foreign_commit = foreign
        .get_entry_entity(foreign_id)
        .unwrap()
        .unwrap()
        .commit;

    let alias = RefName::new("refs/store/counter/out-of-band").unwrap();
    store
        .refs()
        .apply(RefEdit::Create {
            name: alias,
            new: foreign_commit,
        })
        .unwrap();
    assert!(matches!(
        counter.get(&entity("out-of-band")),
        Err(Error::KindMismatch { .. })
    ));

    assert!(counter.list().unwrap().contains(&entity("out-of-band")));
}

#[test]
fn ordinary_reads_reject_cross_kind_tombstones() {
    let store = store();
    let counter = store.dynamic(seg("counter"));
    let foreign = store.dynamic(seg("foreign"));
    counter
        .schema()
        .put(&schema_of::<Counter>().unwrap())
        .unwrap();
    foreign
        .schema()
        .put(&schema_of::<Counter>().unwrap())
        .unwrap();
    let foreign_id = foreign.put_entity(&value!({ "n": 1 })).unwrap();
    let tombstone = match foreign.delete_entity(foreign_id).unwrap() {
        DeleteResult::Deleted(entry) => entry.commit,
        other => panic!("expected foreign tombstone, got {other:?}"),
    };
    store
        .refs()
        .apply(RefEdit::Create {
            name: RefName::new("refs/store/counter/foreign-tombstone").unwrap(),
            new: tombstone,
        })
        .unwrap();

    assert!(matches!(
        counter.read(entity("foreign-tombstone")),
        Err(Error::KindMismatch { .. })
    ));
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

#[test]
fn typed_delete_is_distinct_idempotent_and_restorable() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    let value = value!({ "n": 1 });
    let id = kind.put_entity(&value).unwrap();
    let live_commit = kind.get_entry_entity(id).unwrap().unwrap().commit;

    let deleted = kind.delete_entity(id).unwrap();
    let tombstone_commit = match &deleted {
        DeleteResult::Deleted(entry) => entry.commit,
        other => panic!("expected a new tombstone, got {other:?}"),
    };
    assert_ne!(tombstone_commit, live_commit);
    match kind.read_entity(id).unwrap() {
        EntityState::Deleted(entry) => {
            assert_eq!(entry.commit, tombstone_commit);
            assert_eq!(entry.tombstone.entity_id(), Some(id));
            assert_eq!(entry.tombstone.kind, "counter");
        }
        other => panic!("expected deleted state, got {other:?}"),
    }
    assert_eq!(kind.get_entity(id).unwrap(), None);
    assert!(kind.list().unwrap().is_empty());
    assert_eq!(
        kind.list_entries().unwrap(),
        vec![(entity_id_name(id), tombstone_commit)]
    );

    // The migrated reader classifies the embedded tombstone before looking
    // for schema history, so deleting the schema ref does not affect it.
    let schema_ref = RefName::new("refs/schema/counter").unwrap();
    let schema_tip = store.refs().read(&schema_ref).unwrap().unwrap();
    store
        .refs()
        .apply(RefEdit::Delete {
            name: schema_ref,
            expected: schema_tip,
        })
        .unwrap();
    assert!(matches!(
        kind.read_migrated(&entity_id_name(id)).unwrap(),
        EntityState::Deleted(_)
    ));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();

    match kind.delete_entity(id).unwrap() {
        DeleteResult::AlreadyDeleted(entry) => assert_eq!(entry.commit, tombstone_commit),
        other => panic!("repeated delete must be idempotent, got {other:?}"),
    }

    // The same complete document identity can be explicitly restored. A new
    // publication commit replaces the tombstone and retains its history.
    assert_eq!(kind.put_entity(&value).unwrap(), id);
    match kind.read_entity(id).unwrap() {
        EntityState::Present(entry) => {
            assert_eq!(entry.value, value);
            assert_ne!(entry.commit, tombstone_commit);
        }
        other => panic!("expected restored value, got {other:?}"),
    }
}

#[test]
fn republishing_over_a_tombstone_restores_the_name_and_materialized_views() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    let name = entity("legacy/counter");
    let value = value!({ "n": 1 });
    let id = kind.put_with_alias(&name, &value).unwrap();

    let tombstone_commit = match kind.delete_name(&name).unwrap() {
        DeleteResult::Deleted(entry) => entry.commit,
        other => panic!("expected a new tombstone, got {other:?}"),
    };
    assert!(kind.list().unwrap().is_empty());
    assert!(kind.entries().unwrap().is_empty());

    assert_eq!(kind.put_with_alias(&name, &value).unwrap(), id);
    let restored_commit = match kind.read(name.clone()).unwrap() {
        EntityState::Present(entry) => {
            assert_eq!(entry.value, value);
            entry.commit
        }
        other => panic!("expected the name to be restored, got {other:?}"),
    };
    assert_ne!(restored_commit, tombstone_commit);
    assert_eq!(kind.list().unwrap(), vec![name.clone()]);
    assert_eq!(
        kind.list_entries().unwrap(),
        vec![(name.clone(), restored_commit)]
    );
    let entries = kind.entries().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].0, name);
    assert_eq!(entries[0].1.commit, restored_commit);
    assert_eq!(entries[0].1.value, value);
}
#[test]
fn hard_deleted_canonical_refs_remain_absent_not_deleted() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    let id = kind.put_entity(&value!({ "n": 1 })).unwrap();
    assert!(kind.remove(&entity_id_name(id)).unwrap());
    assert!(matches!(kind.read_entity(id).unwrap(), EntityState::Absent));
    assert!(matches!(
        kind.delete_entity(id).unwrap(),
        DeleteResult::Absent
    ));
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
fn put_message_sets_the_commit_summary_without_schema_trailer() {
    let store = store();
    store
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
    assert_eq!(message, "bump the counter\n");
}

#[test]
fn put_message_rejects_reserved_schema_and_provenance_trailers() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();

    let values = [value!({ "n": 0 }), value!({ "n": 1 }), value!({ "n": 2 })];
    for (index, trailer) in ["Schema:", "Schema-Version:", "Ents-Ref:"]
        .into_iter()
        .enumerate()
    {
        let name = entity(&format!("reserved-{index}"));
        let err = kind
            .write(&values[index])
            .message(format!("ordinary summary\n\n{trailer} legacy"))
            .at(&name)
            .expect_err("reserved trailer messages must not write a commit");
        match err {
            Error::ReservedTrailer { trailer: actual } => assert_eq!(actual, trailer),
            other => panic!("unexpected error: {other:?}"),
        }
        assert!(
            kind.history(&name).unwrap().is_empty(),
            "rejected message must not publish a ref"
        );
    }
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

/// [`gix_store::kind::Put::anonymous`] derives the canonical entity ref from
/// the complete document tree, so writing identical content twice is
/// idempotent even though the compatibility API returns a commit id.
/// Identity is derived from the bound frame, so the same content published
/// under two names has one id and two independent refs.
#[test]
fn content_identity_is_shared_across_names() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    let value = value!({ "n": 1 });

    let first = kind
        .write(&value)
        .message("first metadata")
        .with_alias(&entity("first"))
        .unwrap();
    let second = kind
        .write(&value)
        .message("different metadata")
        .with_alias(&entity("second"))
        .unwrap();

    assert_eq!(first, second, "the bound frame is the identity input");
    assert_eq!(
        kind.list().unwrap(),
        vec![entity("first"), entity("second")]
    );
    assert_eq!(kind.get(&entity("first")).unwrap(), Some(value.clone()));
    assert_eq!(kind.get(&entity("second")).unwrap(), Some(value));
}
#[test]
fn schema_changes_change_the_entity_id_even_for_the_same_value() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    let first = kind.put_entity(&value!({ "n": 1 })).unwrap();

    kind.schema()
        .put(&schema_of::<CounterWithLabel>().unwrap())
        .unwrap();
    let second = kind
        .put_entity(&value!({ "n": 1, "label": "new" }))
        .unwrap();

    assert_ne!(
        first, second,
        "the complete bound frame includes the schema"
    );
    assert!(
        kind.list_entries()
            .unwrap()
            .iter()
            .any(|(name, _)| name.to_string() == first.to_string())
    );
    assert!(
        kind.list_entries()
            .unwrap()
            .iter()
            .any(|(name, _)| name.to_string() == second.to_string())
    );
}

#[test]
fn republishing_identical_content_under_a_name_is_idempotent() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    let value = value!({ "n": 1 });
    let name = entity("one");

    kind.write(&value).message("one").at(&name).unwrap();
    let first_commit = kind.get_entry(&name).unwrap().unwrap().commit;
    kind.write(&value).message("two").at(&name).unwrap();
    let second_commit = kind.get_entry(&name).unwrap().unwrap().commit;

    // New metadata over unchanged content does not manufacture a commit.
    assert_eq!(first_commit, second_commit);
    assert_eq!(kind.history(&name).unwrap(), vec![first_commit]);
}
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

/// The compatibility `entity_name` helper still resolves the old anonymous
/// commit-named alias, while the canonical ref is based on the complete bound
/// document tree rather than the publication commit.
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

/// [`gix_store::kind::Put::anonymous_under`] retains the old grouped
/// commit-named alias, so compatibility reads and `entity_name_under` continue
/// to work while the canonical identity remains content-derived.
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

#[test]
fn delete_retries_over_a_concurrent_update_and_keeps_the_winner_in_history() {
    let store = flaky_store();
    let counter = store.kind::<Counter>(seg("counter"));
    counter.publish().unwrap();
    let id = counter.put_entity(&Counter { n: 1 }).unwrap();
    let original = counter.get_entry_entity(id).unwrap().unwrap().commit;

    let gix::objs::Object::Commit(original_object) = store.objects().get(&original).unwrap() else {
        panic!("expected a commit");
    };
    let signature = store.refs().signature().unwrap();
    let winner = gix::objs::Commit {
        tree: original_object.tree,
        parents: vec![original].into(),
        author: signature.clone(),
        committer: signature,
        encoding: None,
        message: "concurrent update\n".into(),
        extra_headers: Vec::new(),
    };
    let winner = gix::objs::Write::write(store.objects(), &winner).unwrap();
    store.refs().push_concurrent(RefEdit::Update {
        name: counter.entity_reference(id),
        expected: original,
        new: winner,
    });

    let tombstone = counter.delete_entity(id).unwrap();
    let deleted = match tombstone {
        DeleteResult::Deleted(entry) => entry.commit,
        other => panic!("expected deletion after retry, got {other:?}"),
    };
    assert_eq!(
        counter.history(&entity_id_name(id)).unwrap(),
        vec![deleted, winner, original]
    );
    assert!(matches!(
        counter.read_entity(id).unwrap(),
        EntityState::Deleted(_)
    ));
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
    let expected = doc.clone().with_kind("counter").unwrap();
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
        Some(&expected)
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
    assert!(matches!(err, Error::KindMismatch { .. }), "{err:?}");
}

/// The tree of the commit `id` names.
fn commit_tree(store: &Store<MemoryRefStore, ObjectStore>, id: ObjectId) -> ObjectId {
    let GitObject::Commit(commit) = store.objects().get(&id).unwrap() else {
        panic!("expected a commit");
    };
    commit.tree
}

#[test]
fn transaction_publishes_two_entities_across_kinds_atomically() {
    let store = store();
    let a = store.dynamic(seg("a"));
    let b = store.dynamic(seg("b"));
    a.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    b.schema().put(&schema_of::<Counter>().unwrap()).unwrap();

    let doc_a = DocumentTree::from(a.compile(&value!({ "n": 1 })).unwrap());
    let doc_b = DocumentTree::from(b.compile(&value!({ "n": 2 })).unwrap());

    let publications = store
        .transaction("batch publish")
        .publish(&seg("a"), doc_a, Expectation::Absent)
        .publish(&seg("b"), doc_b, Expectation::Absent)
        .commit()
        .unwrap();

    assert_eq!(publications.len(), 2);
    assert_eq!(a.read(publications[0].entity_id()).unwrap().value(), Some(value!({ "n": 1 })));
    assert_eq!(b.read(publications[1].entity_id()).unwrap().value(), Some(value!({ "n": 2 })));
    // The materialized index for each kind reflects the transaction too, not
    // just the entity ref itself.
    assert_eq!(a.list_entries().unwrap().len(), 1);
    assert_eq!(b.list_entries().unwrap().len(), 1);
}

#[test]
fn transaction_stale_expectation_leaves_every_staged_ref_untouched() {
    let store = store();
    let a = store.dynamic(seg("a"));
    let b = store.dynamic(seg("b"));
    a.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    b.schema().put(&schema_of::<Counter>().unwrap()).unwrap();

    // `a` already has a published entity; `b` does not yet have one.
    let existing = a.put_entity(&value!({ "n": 1 })).unwrap();
    let new_doc_b = DocumentTree::from(b.compile(&value!({ "n": 2 })).unwrap());
    // Republishing identical content at `existing` is a stale expectation:
    // `Expectation::Absent` no longer holds, since the ref already exists.
    let same_content_a = DocumentTree::from(a.compile(&value!({ "n": 1 })).unwrap());

    let err = store
        .transaction("batch publish")
        .publish(&seg("b"), new_doc_b, Expectation::Absent)
        .publish(&seg("a"), same_content_a, Expectation::Absent)
        .commit()
        .unwrap_err();
    assert!(matches!(err, Error::Backend(_)), "{err:?}");

    // Neither ref was touched: `b` is still absent, and `a` still names only
    // the entity that existed before the transaction was attempted.
    assert!(b.list_entries().unwrap().is_empty());
    assert_eq!(a.list_entries().unwrap().len(), 1);
    assert_eq!(a.read(existing).unwrap().value(), Some(value!({ "n": 1 })));
}

/// Rewrite a current-format object graph with the historical no-newline
/// blob spelling, reproducing pre-newline leaf framing in memory.
fn strip_leaf_newlines(store: &ObjectStore, oid: ObjectId) -> ObjectId {
    match store.get(&oid).expect("fixture object") {
        GitObject::Blob(blob) => {
            let bytes = blob.data.strip_suffix(b"\n").unwrap_or(&blob.data);
            store
                .write_buf(gix::objs::Kind::Blob, bytes)
                .expect("write legacy blob")
        }
        GitObject::Tree(tree) => {
            let entries = tree
                .entries
                .into_iter()
                .map(|entry| gix::objs::tree::Entry {
                    mode: entry.mode,
                    filename: entry.filename,
                    oid: strip_leaf_newlines(store, entry.oid),
                })
                .collect();
            store
                .write(&gix::objs::Tree { entries })
                .expect("write legacy tree")
        }
        other => panic!("unexpected fixture object: {other:?}"),
    }
}

#[test]
fn compat_strict_is_the_default_and_rejects_pre_newline_leaves() {
    let store = store();
    let schema_tree = schema_of::<Counter>()
        .unwrap()
        .write_pinned(store.objects())
        .unwrap();
    let ordinary = store
        .encode_value(&value!({ "n": 7 }), SchemaTree::from(schema_tree))
        .unwrap();
    let legacy_value_tree = strip_leaf_newlines(store.objects(), ordinary.object_id());

    let err = store
        .decode_value(ValueTree::from(legacy_value_tree), SchemaTree::from(schema_tree))
        .unwrap_err();
    assert!(matches!(err, Error::SchemaRead(_)), "{err:?}");
}

#[test]
fn compat_legacy_leaves_accepts_what_strict_rejects() {
    let store = store();
    let schema_tree = schema_of::<Counter>()
        .unwrap()
        .write_pinned(store.objects())
        .unwrap();
    let ordinary = store
        .encode_value(&value!({ "n": 7 }), SchemaTree::from(schema_tree))
        .unwrap();
    let legacy_value_tree = strip_leaf_newlines(store.objects(), ordinary.object_id());

    let store = store.with_compat(Compat::LegacyLeaves);
    assert_eq!(
        store
            .decode_value(ValueTree::from(legacy_value_tree), SchemaTree::from(schema_tree))
            .unwrap(),
        value!({ "n": 7 })
    );

    // An ordinary, current-format tree still decodes identically under
    // either setting.
    assert_eq!(
        store
            .decode_value(ordinary, SchemaTree::from(schema_tree))
            .unwrap(),
        value!({ "n": 7 })
    );
}
