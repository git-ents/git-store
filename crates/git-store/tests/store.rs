//! End-to-end [`Store`] behavior against [`MemoryRefStore`] and
//! [`facet_git_tree::ObjectStore`] — no filesystem, no temp-dir repository.
//! What genuinely needs a real `gix` repository (on-disk ref layout, a real
//! `git fetch`, cross-thread concurrency, `git ls-tree`-shaped plumbing
//! assertions) lives in `tests/repository.rs` instead.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::rc::Rc;

use facet::Facet;
use facet_git_tree::{
    GitObject, Node, ObjectStore, Oid, Schema, Sha1, TreeEntry, schema_of, serialize_into,
};
use facet_value::value;
use git_store::{
    ApplyError, At, Committer, DeleteResult, DocumentInspection, DocumentKind, DocumentShapeError,
    DocumentTree, EntityState, Entry, Error, Expectation, MemoryRefStore, ObjectId, PublishOptions,
    RefName, RefPath, RefPrefix, RefSegment, RefStore, SchemaTree, Store, Subtree,
    canonical_document_id, decode, entity_id_name, entity_name, entity_name_under,
};
use gix::bstr::ByteSlice;
use gix_refstore::RefEdit;

fn seg(s: &str) -> RefSegment {
    RefSegment::new(s).unwrap()
}

fn entity(s: &str) -> RefPath {
    RefPath::new(s).unwrap()
}

/// Re-tag a bare object id for the format-checked object-store accessors.
fn typed(id: ObjectId) -> Oid<Sha1> {
    Oid::new(id).expect("an id written by this store uses SHA-1")
}

fn store() -> Store<MemoryRefStore, ObjectStore> {
    Store::new(MemoryRefStore::new(), ObjectStore::default())
}

fn bound_document(
    store: &Store<MemoryRefStore, ObjectStore>,
    value: &facet_value::Value,
    doc: &facet_git_tree::Schema,
) -> ObjectId {
    let schema_tree = serialize_into(doc, store.objects())
        .expect("write schema")
        .into_raw();
    let value_tree = facet_git_tree::serialize_value_with_schema(
        value,
        &doc.checked().expect("well-formed schema"),
        store.objects(),
    )
    .expect("write value");
    // A bound document is the value's own entries beside the reserved
    // `.schema` entry.
    let mut entries = store
        .objects()
        .get_tree(&value_tree)
        .expect("read value tree")
        .expect("value tree is present");
    entries.push(TreeEntry {
        mode: gix::objs::tree::EntryKind::Tree.into(),
        filename: ".schema".into(),
        oid: schema_tree,
    });
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

/// A kind whose own top-level field names are `value` and `schema` — ordinary
/// names, since only the dotted `.schema`/`.value` are reserved — so an unbound
/// value tree cannot be confused with a bound document.
#[derive(Facet)]
struct Colliding {
    value: String,
    schema: String,
}

/// A user type whose name *and* structure are identical to
/// [`git_store::Tombstone`] — a plausible domain name in a forge. It must not
/// be reclassified as a deletion marker by schema coincidence (claim S3).
mod user_tombstone {
    use facet::Facet;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Facet)]
    #[repr(u8)]
    pub enum TombstoneState {
        Deleted,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Facet)]
    pub struct Tombstone {
        pub state: TombstoneState,
        pub kind: String,
        pub entity: String,
    }
}

#[test]
fn embedded_document_decodes_without_a_kind_or_publication_ref() {
    let store = store();
    let doc = schema_of::<Counter>().unwrap();
    let expected = value!({ "n": 7 });
    let tree = bound_document(&store, &expected, &doc);

    assert_eq!(store.decode(tree).unwrap(), expected);
    assert_eq!(git_store::decode(tree, store.objects()).unwrap(), expected);
}

#[test]
fn explicit_schema_value_codecs_round_trip_without_a_kind_handle() {
    let store = store();
    let doc = schema_of::<Counter>().unwrap();
    let schema_tree = serialize_into(&doc, store.objects()).unwrap().into_raw();
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
    let doc = schema_of::<Counter>().unwrap();
    let schema_tree = serialize_into(&doc, store.objects()).unwrap().into_raw();
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
    let doc = schema_of::<Counter>().unwrap();
    let schema_tree = serialize_into(&doc, store.objects()).unwrap().into_raw();
    let value_tree = store
        .encode_value(&value!({ "n": 7 }), SchemaTree::from(schema_tree))
        .unwrap();

    // No `.schema` entry: a legacy, unbound value root rather than malformed.
    assert_eq!(
        store
            .inspect_document(DocumentTree::from(value_tree.object_id()))
            .unwrap()
            .kind(),
        DocumentKind::LegacyValueRoot
    );

    // A `.schema` entry that is a blob, not a tree, is a shape error.
    let blob_schema = gix::objs::Write::write(
        store.objects(),
        &gix::objs::Tree {
            entries: vec![TreeEntry {
                mode: gix::objs::tree::EntryKind::Blob.into(),
                filename: ".schema".into(),
                oid: value_tree.object_id(),
            }],
        },
    )
    .unwrap();
    let DocumentInspection::Malformed { found, reason, .. } = store
        .inspect_document(DocumentTree::from(blob_schema))
        .unwrap()
    else {
        panic!("expected malformed document metadata");
    };
    assert_eq!(found, vec![".schema"]);
    assert_eq!(reason, DocumentShapeError::SchemaNotTree);

    // A `.schema`-only root has no value entries beside it. A struct root
    // always contributes at least one entry — the `none` marker when it is
    // empty — so this envelope cannot have been written by this format.
    let schema_only = gix::objs::Write::write(
        store.objects(),
        &gix::objs::Tree {
            entries: vec![TreeEntry {
                mode: gix::objs::tree::EntryKind::Tree.into(),
                filename: ".schema".into(),
                oid: schema_tree,
            }],
        },
    )
    .unwrap();
    let DocumentInspection::Malformed { found, reason, .. } = store
        .inspect_document(DocumentTree::from(schema_only))
        .unwrap()
    else {
        panic!("expected malformed document metadata");
    };
    assert_eq!(found, vec![".schema"]);
    assert_eq!(reason, DocumentShapeError::NoValueEntries);

    // A `.schema` entry naming an object the database lacks is malformed too,
    // and names the object it could not find.
    let absent = ObjectId::from_hex(b"0123456789012345678901234567890123456789").unwrap();
    let missing_schema = gix::objs::Write::write(
        store.objects(),
        &gix::objs::Tree {
            entries: vec![TreeEntry {
                mode: gix::objs::tree::EntryKind::Tree.into(),
                filename: ".schema".into(),
                oid: absent,
            }],
        },
    )
    .unwrap();
    let DocumentInspection::Malformed { reason, .. } = store
        .inspect_document(DocumentTree::from(missing_schema))
        .unwrap()
    else {
        panic!("expected malformed document metadata");
    };
    assert_eq!(reason, DocumentShapeError::SchemaMissing { oid: absent });
}

/// Claim D3: [`Store::inspect_document`] classified *every* tree without a
/// `.schema` entry as [`DocumentInspection::LegacyValueRoot`], so a schema
/// document tree (entries `kind`/`root`/`defs`), an empty tree, and a genuine
/// pre-binding value tree all looked alike — while the split-backed read path
/// ([`decode`]) rejects every one of them.
///
/// This pins what each shape is to each API. The classifier answers "does this
/// tree carry the envelope entry?"; the parser answers "is this a usable
/// envelope, and decode it". Only shapes that could plausibly be bare value
/// roots may be `LegacyValueRoot`; a tree that is provably not a value (an
/// empty root, which no encoding writes and no bare value produces) must not.
#[test]
fn classification_and_the_read_path_agree_on_every_root_shape() {
    use gix::objs::{Tree, Write, tree::EntryKind};

    let store = store();
    let doc = schema_of::<Counter>().unwrap();
    let schema_tree = serialize_into(&doc, store.objects()).unwrap().into_raw();

    // (a) A `.schema`-only root: an envelope with no value entries beside it.
    //     A struct contributes at least the `none` marker, so this format can
    //     never have written this shape.
    let schema_only = Write::write(
        store.objects(),
        &Tree {
            entries: vec![TreeEntry {
                mode: EntryKind::Tree.into(),
                filename: ".schema".into(),
                oid: schema_tree,
            }],
        },
    )
    .unwrap();

    // (b) A schema document tree: the bare `Schema` value (entries
    //     `kind`/`root`/`defs`) with no `.schema` of its own.
    let schema_document = schema_tree;

    // (c) An empty tree. `envelope.empty` writes no empty tree and a bare value
    //     always contributes the `none` marker, so an empty tree is neither a
    //     document nor a value.
    let empty = Write::write(store.objects(), &Tree { entries: vec![] }).unwrap();

    // (d) A genuine legacy value root: a bare value tree with real entries, the
    //     shape every pre-envelope commit carried.
    let legacy = facet_git_tree::serialize_value_with_schema(
        &value!({ "n": 7 }),
        &doc.checked().expect("well-formed schema"),
        store.objects(),
    )
    .unwrap()
    .into_raw();

    // (e) A bound document, for contrast.
    let bound = bound_document(&store, &value!({ "n": 7 }), &doc);

    let kind_of = |tree: ObjectId| {
        store
            .inspect_document(DocumentTree::from(tree))
            .unwrap()
            .kind()
    };
    // The split-backed read path refuses every non-envelope with the commit
    // being the tree itself (as `decode` passes `tree` for both root and
    // commit).
    let read_refuses = |tree: ObjectId| {
        matches!(
            decode(tree, store.objects()),
            Err(Error::NotSubtreeBound { commit, .. }) if commit == tree
        )
    };

    // (a) Envelope-like but with no value entries: malformed by the classifier,
    //     unbound by the parser.
    assert!(matches!(
        store
            .inspect_document(DocumentTree::from(schema_only))
            .unwrap(),
        DocumentInspection::Malformed {
            reason: DocumentShapeError::NoValueEntries,
            ..
        }
    ));
    assert!(read_refuses(schema_only));

    // (b) A schema document tree is neither an envelope nor a value. It reaches
    //     the classifier's no-envelope arm, which reports the *absence* of an
    //     envelope and does not claim the tree is a value; the parser rejects
    //     it.
    assert_eq!(kind_of(schema_document), DocumentKind::LegacyValueRoot);
    assert!(read_refuses(schema_document));

    // (c) An empty tree is neither an envelope nor a value, so the classifier
    //     must not offer it as a legacy value root.
    assert_eq!(
        kind_of(empty),
        DocumentKind::Malformed,
        "an empty tree is not a legacy value root"
    );
    assert!(read_refuses(empty));

    // (d) A genuine pre-binding value tree is still recognized as unbound...
    assert_eq!(kind_of(legacy), DocumentKind::LegacyValueRoot);
    // ...and the parser still refuses it: it has no envelope.
    assert!(read_refuses(legacy));

    // (e) A bound document is a document to both.
    assert_eq!(kind_of(bound), DocumentKind::Bound);
    assert!(decode(bound, store.objects()).is_ok());

    // The invariant tying the two together: for every shape here, the
    // classifier calls it a document exactly when the read path accepts it.
    // (`decode` layers schema validation over `envelope::split`; these shapes
    // fail the envelope check first, so the two coincide.)
    for (tree, is_document) in [
        (schema_only, false),
        (schema_document, false),
        (empty, false),
        (legacy, false),
        (bound, true),
    ] {
        assert_eq!(
            kind_of(tree) == DocumentKind::Bound,
            decode(tree, store.objects()).is_ok(),
            "classifier and parser disagree about {tree}"
        );
        assert_eq!(
            kind_of(tree) == DocumentKind::Bound,
            is_document,
            "unexpected document-ness for {tree}"
        );
    }
}

#[test]
fn schema_snapshots_are_owned_and_independent_of_publication_progress() {
    let store = store();
    let schema = store.dynamic(seg("counter")).schema();
    let first = schema_of::<Counter>().unwrap();
    let first_published = first.clone();
    let first_commit = schema.put(&first).unwrap();
    let first_snapshot = schema.current_snapshot().unwrap();

    let second = schema_of::<CounterWithLabel>().unwrap();
    let second_published = second.clone();
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

/// A schema whose resolved root is not a struct cannot describe a document:
/// the value's named fields have nowhere to sit beside `.schema`, so there is
/// no envelope to bind. Registration must refuse it, rather than publishing it
/// and then failing every later `put` — which would blame the value for a
/// defect in the schema (`docs/specification.adoc`, `envelope.schema-root`).
#[test]
fn a_non_struct_root_schema_is_refused_at_registration() {
    let store = store();
    let kind = store.dynamic(seg("counters"));
    let doc = schema_of::<Vec<Counter>>().unwrap();
    assert_eq!(
        doc.resolved_root().unwrap().keyword(),
        "list",
        "the fixture's root must not be a struct"
    );

    let err = kind.schema().put(&doc).unwrap_err();
    match err {
        Error::DocumentRootNotStruct { kind, node } => {
            assert_eq!(kind, "Vec", "the error names the refused schema");
            assert_eq!(node, "list");
        }
        other => panic!("expected DocumentRootNotStruct, got {other:?}"),
    }

    // The refusal is atomic: nothing was published, so the kind is simply
    // unconfigured and a later `put` reports the missing schema rather than a
    // value-shape failure.
    assert_eq!(kind.schema().get().unwrap(), None);
    let err = kind.put(&entity("x"), &value!([])).unwrap_err();
    assert!(matches!(err, Error::NoSchema { .. }), "{err:?}");

    // A struct-root schema still registers and puts, unaffected.
    let good = store.dynamic(seg("counter"));
    good.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    good.put(&entity("c"), &value!({ "n": 1 })).unwrap();
    assert_eq!(
        good.read(entity("c")).unwrap().value(),
        Some(value!({ "n": 1 }))
    );
}

/// A root `ref` that names no definition — a hand-authored schema's most
/// likely defect — can never resolve to a struct. `facet-git-tree` now rejects
/// it while building the `Schema`, so it is refused before any `Store` sees it:
/// no `put` can publish it and fail every later write.
#[test]
fn a_root_ref_that_cannot_resolve_is_refused_at_construction() {
    let err = Schema::new(
        "broken".to_owned(),
        Node::Ref("Missing".to_owned()),
        Default::default(),
    )
    .unwrap_err();
    match err {
        facet_git_tree::SchemaReadError::UnknownRef(name) => assert_eq!(name, "Missing"),
        other => panic!("expected a dangling ref, got {other:?}"),
    }
}

#[test]
fn store_retrieve_list_and_schema_roundtrip() {
    let store = store();

    let doc = schema_of::<Recipe>().unwrap();
    let expected = doc.clone();
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
            .read(entity("carbonara"))
            .unwrap()
            .value(),
        Some(carbonara)
    );
    assert_eq!(
        store
            .dynamic(seg("recipe"))
            .read(entity("missing"))
            .unwrap()
            .value(),
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
        note.read(entity("dead/one")).unwrap().value(),
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
        store
            .dynamic(seg("thing"))
            .read(entity("b"))
            .unwrap()
            .value(),
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

    // The old commit reads back through its own embedded `.schema` entry, even
    // when the current schema publication ref is unavailable.
    assert_eq!(store.dynamic(seg("thing")).schema().get().unwrap(), None);
    assert_eq!(
        store
            .dynamic(seg("thing"))
            .read(old_commit)
            .unwrap()
            .value()
            .unwrap(),
        v1
    );

    let document_tree = match store.objects().get(&typed(old_commit)).unwrap().unwrap() {
        GitObject::Commit(commit) => commit.tree,
        other => panic!("expected a commit, got {other:?}"),
    };
    assert_eq!(store.decode(document_tree).unwrap(), v1);
}

#[test]
fn read_addresses_include_entity_id() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    let id = kind.put_entity(&value!({ "n": 1 })).unwrap();
    let commit = match kind.read(id).unwrap() {
        EntityState::Present(entry) => entry.commit,
        other => panic!("expected the freshly published entity to be present, got {other:?}"),
    };
    let document_tree = match store.objects().get(&typed(commit)).unwrap().unwrap() {
        GitObject::Commit(commit) => commit.tree,
        other => panic!("expected a commit, got {other:?}"),
    };
    let name = entity("alias");
    kind.put_entity_with_alias(&name, &value!({ "n": 1 }))
        .unwrap();
    let expected = Some(value!({ "n": 1 }));

    assert_eq!(kind.read(At::Name(name)).unwrap().value(), expected);
    assert_eq!(kind.read(At::Entity(id)).unwrap().value(), expected);
    assert_eq!(kind.read(At::Commit(commit)).unwrap().value(), expected);
    assert_eq!(
        kind.read(At::Tree(document_tree)).unwrap().value(),
        expected
    );
}

/// Reading a named entity through an as-of view answers the override — a ref
/// pointed at a different publication — while the underlying store keeps its
/// real state.
#[test]
fn as_of_view_answers_overrides_without_touching_the_store() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    kind.put(&entity("c"), &value!({ "n": 1 })).unwrap();
    let elsewhere = kind.put(&entity("d"), &value!({ "n": 2 })).unwrap();

    // Read `c` as though it pointed at the publication under `d`.
    let view = store.as_of([(kind.reference(&entity("c")), Some(elsewhere))]);
    assert_eq!(
        view.dynamic(seg("counter"))
            .read(entity("c"))
            .unwrap()
            .value(),
        Some(value!({ "n": 2 }))
    );

    // The view's read left the real store's ref exactly where it was.
    assert_eq!(
        kind.read(entity("c")).unwrap().value(),
        Some(value!({ "n": 1 }))
    );
}

#[test]
fn tombstone_reads_without_the_schema_publication_ref() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    let id = kind.put_entity(&value!({ "n": 1 })).unwrap();
    let tombstone = match kind.delete_entity(id).unwrap() {
        DeleteResult::Deleted(entry) => entry.commit,
        other => panic!("expected tombstone, got {other:?}"),
    };

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
        kind.read(entity_id_name(id)).unwrap(),
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
        git_store::Layout {
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
        store
            .dynamic(seg("module"))
            .read(entity("a"))
            .unwrap()
            .value(),
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
        let gix::objs::Object::Commit(commit) =
            store.objects().get(&typed(commit)).unwrap().unwrap()
        else {
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
                        && !line.starts_with(b"Schema-commit:")
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
    let gix::objs::Object::Commit(commit) = store.objects().get(&typed(original)).unwrap().unwrap()
    else {
        panic!("not a commit");
    };

    let legacy = commit_ref(
        &store,
        "refs/store/counter/legacy",
        "legacy\n\nSchema: not-an-object-id\nSchema-Version: conflicting\nEnts-Ref: refs/schema/other\n",
        commit.tree,
        None,
    );
    assert_eq!(kind.read(legacy).unwrap().value().unwrap(), value);
}

/// A commit whose tree is not a `.schema`-bound document — anything not
/// written by [`git_store::Kind::put`]/[`git_store::kind::Put::by_commit`],
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

    let err = store.dynamic(seg("x")).read(bogus).unwrap_err();
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
/// for a bound commit just because the value happens to carry top-level fields
/// called `value` and `schema` (either of which would need a leading dot to be
/// the reserved envelope entry).
#[test]
fn a_pre_binding_commit_whose_value_has_value_and_schema_fields_is_not_mistaken_for_bound() {
    let store = store();

    // A kind whose own fields are named `value` and `schema`.
    let doc = facet_git_tree::schema_of::<Colliding>().unwrap();
    store.dynamic(seg("colliding")).schema().put(&doc).unwrap();
    let colliding = value!({ "value": "v", "schema": "s" });

    // The old format: commit the value's tree directly, with no wrapper.
    let value_tree = facet_git_tree::serialize_value_with_schema(
        &colliding,
        &doc.checked().expect("well-formed schema"),
        store.objects(),
    )
    .expect("serialize")
    .into_raw();
    let old = commit_ref(
        &store,
        "refs/store/colliding/old",
        "pre-binding shape\n\nSchema: 0000000000000000000000000000000000000000\n",
        value_tree,
        None,
    );

    let err = store.dynamic(seg("colliding")).read(old).unwrap_err();
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
        store
            .dynamic(seg("colliding"))
            .read(entity("new"))
            .unwrap()
            .value(),
        Some(colliding)
    );
}

/// An incomplete transfer — the `.schema` entry is present but the object it
/// names is not — reports which half is absent and on which commit, rather
/// than a bare object-not-found. This is the failure class the whole binding
/// exists to make diagnosable, so it must stay diagnosable even when the
/// binding itself cannot save the read.
#[test]
fn retrieve_at_reports_a_subtree_object_that_is_not_present() {
    let store = store();

    let doc = schema_of::<Counter>().unwrap();
    store.dynamic(seg("counter")).schema().put(&doc).unwrap();
    let commit = store
        .dynamic(seg("counter"))
        .put(&entity("c"), &value!({ "n": 1 }))
        .unwrap();

    // Take the just-written document root and point its `.schema` entry at a
    // tree that was never written to this store.
    let absent = ObjectId::from_hex(b"0123456789012345678901234567890123456789").unwrap();
    let GitObject::Commit(commit_obj) = store.objects().get(&typed(commit)).unwrap().unwrap()
    else {
        panic!("expected a commit");
    };
    let mut entries = store
        .objects()
        .get_tree(&typed(commit_obj.tree))
        .unwrap()
        .unwrap();
    for entry in &mut entries {
        if entry.filename == ".schema" {
            entry.oid = absent;
        }
    }
    entries.sort();
    let root = gix::objs::Write::write(store.objects(), &gix::objs::Tree { entries }).unwrap();
    let severed = commit_ref(
        &store,
        "refs/store/counter/severed",
        "schema subtree not transferred",
        root,
        None,
    );

    let err = store.dynamic(seg("counter")).read(severed).unwrap_err();
    assert!(
        matches!(
            err,
            Error::SubtreeMissing { subtree: Subtree::Schema, oid, commit }
                if oid == absent && commit == severed
        ),
        "{err:?}"
    );
}

/// [`git_store::kind::Put::by_commit`] binds the schema by subtree exactly as
/// [`git_store::Kind::put`] does — its commit is written before any ref, so a
/// regression there would be invisible to the ref-based tests.
#[test]
fn by_commit_binds_the_schema_by_subtree() {
    let store = store();

    store
        .dynamic(seg("counter"))
        .schema()
        .put(&schema_of::<Counter>().unwrap())
        .unwrap();
    let schema_ref = RefName::new("refs/schema/counter").unwrap();
    let schema_commit = store.refs().read(&schema_ref).unwrap().unwrap();
    let GitObject::Commit(schema_commit_obj) =
        store.objects().get(&typed(schema_commit)).unwrap().unwrap()
    else {
        panic!("expected a commit");
    };
    let schema_tree = schema_commit_obj.tree;

    let counter = value!({ "n": 7 });
    let commit = store
        .dynamic(seg("counter"))
        .write(&counter)
        .by_commit()
        .unwrap();

    // Bound to the very same schema tree object, not a copy of it.
    let GitObject::Commit(commit_obj) = store.objects().get(&typed(commit)).unwrap().unwrap()
    else {
        panic!("expected a commit");
    };
    let root = store
        .objects()
        .get_tree(&typed(commit_obj.tree))
        .unwrap()
        .unwrap();
    let bound_schema = root.iter().find(|e| e.filename == ".schema").unwrap().oid;
    assert_eq!(
        bound_schema, schema_tree,
        "by_commit must share the schema tree, not duplicate it"
    );
    // And self-contained: readable with no consultation of refs/schema/*.
    assert_eq!(
        store
            .dynamic(seg("counter"))
            .read(commit)
            .unwrap()
            .value()
            .unwrap(),
        counter
    );
    assert_eq!(
        store
            .dynamic(seg("counter"))
            .read(entity_name(commit))
            .unwrap()
            .value(),
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
    let first_commit = kind
        .read(&name)
        .map(|state| state.present())
        .unwrap()
        .unwrap()
        .commit;
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
    assert_eq!(
        kind.read(first_commit).unwrap().value().unwrap(),
        value!({ "n": 1 })
    );
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
    assert_eq!(kind.read(&second).unwrap().value(), Some(value));
    assert_eq!(kind.list().unwrap(), vec![second]);
}
#[test]
fn delete_name_tombstones_a_rewound_name_from_its_current_target() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    let name = entity("legacy/history");
    kind.put(&name, &value!({ "n": 1 })).unwrap();
    let first_commit = kind
        .read(&name)
        .map(|state| state.present())
        .unwrap()
        .unwrap()
        .commit;
    kind.put(&name, &value!({ "n": 2 })).unwrap();
    let second_commit = kind
        .read(&name)
        .map(|state| state.present())
        .unwrap()
        .unwrap()
        .commit;

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
        store
            .dynamic(seg("counter"))
            .read(entity("c"))
            .unwrap()
            .value(),
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
    let live_commit = kind
        .read(id)
        .map(|state| state.present())
        .unwrap()
        .unwrap()
        .commit;

    let deleted = kind.delete_entity(id).unwrap();
    let tombstone_commit = match &deleted {
        DeleteResult::Deleted(entry) => entry.commit,
        other => panic!("expected a new tombstone, got {other:?}"),
    };
    assert_ne!(tombstone_commit, live_commit);
    match kind.read(id).unwrap() {
        EntityState::Deleted(entry) => {
            assert_eq!(entry.commit, tombstone_commit);
            assert_eq!(entry.tombstone.entity_id(), Some(id));
            assert_eq!(entry.tombstone.kind, "counter");
        }
        other => panic!("expected deleted state, got {other:?}"),
    }
    assert_eq!(kind.read(id).map(|state| state.value()).unwrap(), None);
    assert!(kind.list().unwrap().is_empty());
    assert_eq!(
        kind.list_entries().unwrap(),
        vec![(entity_id_name(id), tombstone_commit)]
    );

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
        kind.read(entity_id_name(id)).unwrap(),
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
    match kind.read(id).unwrap() {
        EntityState::Present(entry) => {
            assert_eq!(entry.value, value);
            assert_ne!(entry.commit, tombstone_commit);
        }
        other => panic!("expected restored value, got {other:?}"),
    }
}

/// A pre-binding ("legacy") tip — a commit whose tree is a bare value tree with
/// no `.schema` envelope — has no bound-document id, so `delete_name` must
/// refuse with a typed error rather than record a tombstone. The value-tree oid
/// the tip *does* have is a phantom: no bound document occupies it, so a
/// tombstone that carried it would point at nothing.
#[test]
fn delete_name_refuses_a_legacy_tip_without_recording_a_phantom_id() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    let doc = schema_of::<Counter>().unwrap();
    kind.schema().put(&doc).unwrap();
    let value = value!({ "n": 1 });
    let value_tree = facet_git_tree::serialize_value_with_schema(
        &value,
        &doc.checked().expect("well-formed schema"),
        store.objects(),
    )
    .expect("write value tree")
    .into_raw();

    // The pre-binding shape: the commit's tree *is* the value, with no
    // envelope around it.
    let name = entity("legacy");
    let tip = commit_ref(
        &store,
        "refs/store/counter/legacy",
        "pre-binding value\n",
        value_tree,
        None,
    );

    let err = kind.delete_name(&name).unwrap_err();
    let Error::NotDeletable { commit, source } = err else {
        panic!("a legacy tip must refuse deletion, got {err:?}");
    };
    assert_eq!(commit, tip);
    assert!(
        matches!(*source, Error::NotSubtreeBound { commit, .. } if commit == tip),
        "the refusal must name the pre-binding shape: {source:?}"
    );

    // No tombstone was written and the tip is untouched: the ref still points
    // at the legacy commit, so its history is intact and a later read still
    // reports the pre-binding shape rather than a `Deleted` state.
    assert_eq!(
        store.refs().read(&kind.reference(&name)).unwrap(),
        Some(tip)
    );
    assert_eq!(kind.history(&name).unwrap(), vec![tip]);
    assert!(matches!(
        kind.read(name.clone()).unwrap_err(),
        Error::NotSubtreeBound { commit, .. } if commit == tip
    ));

    // The phantom this refusal prevents: the value-tree oid read back as an
    // entity id addresses no document, so it resolves `Absent`.
    let phantom = canonical_document_id(value_tree);
    assert!(matches!(
        kind.read(At::Entity(phantom)).unwrap(),
        EntityState::Absent
    ));
}

/// A bound-shaped root whose *value* does not decode is still not safe to
/// tombstone by guesswork: `delete_name` refuses with the same typed error
/// rather than clearing the name or recording an id it cannot stand behind.
#[test]
fn delete_name_refuses_a_bound_document_whose_value_does_not_decode() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    let doc = schema_of::<Counter>().unwrap();
    kind.schema().put(&doc).unwrap();
    let schema_tree = serialize_into(&doc, store.objects()).unwrap().into_raw();

    // `.schema` beside an `n` entry that is a blob, not the counter encoding.
    let garbage = gix::objs::Write::write(
        store.objects(),
        &gix::objs::Blob {
            data: b"not a counter".to_vec(),
        },
    )
    .unwrap();
    let bound = gix::objs::Write::write(
        store.objects(),
        &gix::objs::Tree {
            entries: vec![
                TreeEntry {
                    mode: gix::objs::tree::EntryKind::Tree.into(),
                    filename: ".schema".into(),
                    oid: schema_tree,
                },
                TreeEntry {
                    mode: gix::objs::tree::EntryKind::Blob.into(),
                    filename: "n".into(),
                    oid: garbage,
                },
            ],
        },
    )
    .unwrap();
    let name = entity("broken");
    let tip = commit_ref(
        &store,
        "refs/store/counter/broken",
        "undecodable value\n",
        bound,
        None,
    );

    let err = kind.delete_name(&name).unwrap_err();
    assert!(
        matches!(err, Error::NotDeletable { commit, .. } if commit == tip),
        "an undecodable bound tip must refuse deletion, got {err:?}"
    );
    assert_eq!(
        store.refs().read(&kind.reference(&name)).unwrap(),
        Some(tip)
    );
}

/// A malformed envelope — here a `.schema` entry naming an object the database
/// lacks — is not a bound document either, so `delete_name` refuses instead of
/// clearing the ref.
#[test]
fn delete_name_refuses_a_malformed_envelope() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    let doc = schema_of::<Counter>().unwrap();
    kind.schema().put(&doc).unwrap();
    let value_tree = store
        .encode_value(
            &value!({ "n": 1 }),
            SchemaTree::from(serialize_into(&doc, store.objects()).unwrap().into_raw()),
        )
        .unwrap();
    let absent = ObjectId::from_hex(b"0123456789012345678901234567890123456789").unwrap();
    let malformed = gix::objs::Write::write(
        store.objects(),
        &gix::objs::Tree {
            entries: vec![
                TreeEntry {
                    mode: gix::objs::tree::EntryKind::Tree.into(),
                    filename: ".schema".into(),
                    oid: absent,
                },
                TreeEntry {
                    mode: gix::objs::tree::EntryKind::Tree.into(),
                    filename: "n".into(),
                    oid: value_tree.object_id(),
                },
            ],
        },
    )
    .unwrap();
    let name = entity("malformed");
    let tip = commit_ref(
        &store,
        "refs/store/counter/malformed",
        "malformed envelope\n",
        malformed,
        None,
    );

    let err = kind.delete_name(&name).unwrap_err();
    assert!(
        matches!(err, Error::NotDeletable { commit, .. } if commit == tip),
        "a malformed envelope must refuse deletion, got {err:?}"
    );
    assert_eq!(
        store.refs().read(&kind.reference(&name)).unwrap(),
        Some(tip)
    );
}

/// A legacy tip stays retired the one way that needs no entity id: `remove`
/// prunes the ref outright, writing no tombstone. The name then reads `Absent`,
/// not `Deleted`, because a hard-deleted name is indistinguishable from one
/// that never existed — the documented cost of retiring a pre-envelope tip.
#[test]
fn a_legacy_tip_is_retired_by_removing_the_ref() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    let doc = schema_of::<Counter>().unwrap();
    kind.schema().put(&doc).unwrap();
    let value_tree = facet_git_tree::serialize_value_with_schema(
        &value!({ "n": 1 }),
        &doc.checked().expect("well-formed schema"),
        store.objects(),
    )
    .expect("write value tree")
    .into_raw();
    let name = entity("legacy/removed");
    commit_ref(
        &store,
        "refs/store/counter/legacy/removed",
        "pre-binding value\n",
        value_tree,
        None,
    );

    assert!(kind.remove(&name).unwrap());
    assert!(matches!(
        kind.read(name.clone()).unwrap(),
        EntityState::Absent
    ));
    assert!(matches!(
        kind.delete_name(&name).unwrap(),
        DeleteResult::Absent
    ));
    assert!(!kind.remove(&name).unwrap());
}

/// A user schema that happens to be structurally identical to the compiled-in
/// [`git_store::Tombstone`] schema must not make ordinary documents read as
/// deleted: a live value of a user type named `Tombstone` stays a live value.
#[test]
fn a_user_type_named_like_the_tombstone_is_not_a_deletion() {
    let store = store();
    let kind = store.kind::<user_tombstone::Tombstone>(seg("counter"));
    kind.publish().unwrap();

    let doc = user_tombstone::Tombstone {
        state: user_tombstone::TombstoneState::Deleted,
        kind: "counter".to_owned(),
        entity: "0123456789012345678901234567890123456789".to_owned(),
    };
    let id = kind.put_entity(&doc).unwrap();

    match kind.read(id).unwrap() {
        EntityState::Present(entry) => assert_eq!(entry.value, doc),
        other => panic!("live user document was read as {other:?}"),
    }
    assert_eq!(kind.list().unwrap(), vec![entity_id_name(id)]);
    assert_eq!(kind.entries().unwrap().len(), 1);
}

#[test]
fn republishing_over_a_tombstone_restores_the_name_and_materialized_views() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    let name = entity("legacy/counter");
    let value = value!({ "n": 1 });
    let id = kind.put_entity_with_alias(&name, &value).unwrap();

    let tombstone_commit = match kind.delete_name(&name).unwrap() {
        DeleteResult::Deleted(entry) => entry.commit,
        other => panic!("expected a new tombstone, got {other:?}"),
    };
    assert!(kind.list().unwrap().is_empty());
    assert!(kind.entries().unwrap().is_empty());

    assert_eq!(kind.put_entity_with_alias(&name, &value).unwrap(), id);
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
    assert!(matches!(kind.read(id).unwrap(), EntityState::Absent));
    assert!(matches!(
        kind.delete_entity(id).unwrap(),
        DeleteResult::Absent
    ));
}

/// A hard-deleted name is gone, not corrupt: the ref is absent, so every
/// address reads `Absent` and neither listing path errors. The lenient
/// classification that keeps tombstones off [`Kind::list`] must never turn a
/// legitimate absence into a failure.
#[test]
fn hard_deleted_name_stays_absent_across_listing_and_read() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    let name = entity("gone");
    kind.put_entity_with_alias(&name, &value!({ "n": 1 }))
        .unwrap();
    assert!(kind.remove(&name).unwrap());

    assert!(matches!(
        kind.read(name.clone()).unwrap(),
        EntityState::Absent
    ));
    assert_eq!(kind.list().unwrap(), Vec::<RefPath>::new());
    assert!(kind.entries().unwrap().is_empty());
    assert!(kind.list_entries().unwrap().is_empty());
}

/// A ref whose tip is not a decodable bound document — here a commit id that
/// is not in the object database — is corruption, not a live entity. `read`
/// surfaces it; the listing must agree rather than silently presenting the
/// name as a live entry with no value.
#[test]
fn listing_surfaces_a_ref_pointing_at_a_missing_object() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    let name = entity("orphan");
    kind.put_entity_with_alias(&name, &value!({ "n": 1 }))
        .unwrap();

    let reference = kind.reference(&name);
    let current = store.refs().read(&reference).unwrap().unwrap();
    let missing = ObjectId::from_hex(b"deadbeefdeadbeefdeadbeefdeadbeefdeadbeef").unwrap();
    store
        .refs()
        .apply(RefEdit::Update {
            name: reference,
            expected: current,
            new: missing,
        })
        .unwrap();

    assert!(
        kind.read(name.clone()).is_err(),
        "read should reject a ref pointing at a missing object"
    );
    assert!(
        kind.list().is_err(),
        "list hid a ref pointing at a missing object: {:?}",
        kind.list()
    );
}

/// A genuine tombstone published under a *different* kind is a foreign ref;
/// `read` rejects it as a kind mismatch, so the listing must not report the
/// name as live.
#[test]
fn listing_surfaces_a_ref_whose_tip_is_a_foreign_kind_tombstone() {
    let store = store();
    let counter = store.dynamic(seg("counter"));
    counter
        .schema()
        .put(&schema_of::<Counter>().unwrap())
        .unwrap();
    let name = entity("borrowed");
    counter
        .put_entity_with_alias(&name, &value!({ "n": 1 }))
        .unwrap();

    let gadget = store.dynamic(seg("gadget"));
    gadget
        .schema()
        .put(&schema_of::<Counter>().unwrap())
        .unwrap();
    let gid = gadget.put_entity(&value!({ "n": 2 })).unwrap();
    let foreign = match gadget.delete_entity(gid).unwrap() {
        DeleteResult::Deleted(entry) => entry.commit,
        other => panic!("expected a new tombstone, got {other:?}"),
    };

    let reference = counter.reference(&name);
    let current = store.refs().read(&reference).unwrap().unwrap();
    store
        .refs()
        .apply(RefEdit::Update {
            name: reference,
            expected: current,
            new: foreign,
        })
        .unwrap();

    assert!(
        matches!(counter.read(name.clone()), Err(Error::KindMismatch { .. })),
        "read should reject a foreign-kind tombstone"
    );
    assert!(
        counter.list().is_err(),
        "list hid a foreign-kind tombstone: {:?}",
        counter.list()
    );
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

    assert_eq!(
        recipe.read(entity("carbonara")).unwrap().value(),
        Some(carbonara)
    );
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
        store
            .dynamic(seg("recipe"))
            .read(entity("a"))
            .unwrap()
            .value(),
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
            .read(entity("b"))
            .unwrap()
            .value(),
        Some(carbonara.clone())
    );

    // The same content, written independently through both encodings with no
    // shared parent, lands on the same commit.
    let typed_commit = store
        .kind::<Recipe>(seg("recipe"))
        .write(&carbonara)
        .by_commit()
        .unwrap();
    let dynamic_commit = store
        .dynamic(seg("recipe"))
        .write(&carbonara_value)
        .by_commit()
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

    let GitObject::Commit(commit_obj) = store.objects().get(&typed(commit)).unwrap().unwrap()
    else {
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

    let values = [
        value!({ "n": 0 }),
        value!({ "n": 1 }),
        value!({ "n": 2 }),
        value!({ "n": 3 }),
    ];
    for (index, trailer) in ["Schema:", "Schema-Version:", "Schema-commit:", "Ents-Ref:"]
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

/// [`git_store::EntityState::present`]: the returned [`Entry`] names the exact
/// commit the value came from — the same
/// one [`git_store::Kind::history`] reports as the tip — and carries that
/// commit's summary, both for a default message and for one set through
/// [`git_store::kind::Put::message`].
#[test]
fn a_read_entry_carries_the_commit_and_message_it_was_written_with() {
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
        .read(entity("c"))
        .unwrap()
        .present()
        .expect("entity was written");
    assert_eq!(value, value!({ "n": 2 }));
    assert_eq!(commit, second);
    assert_eq!(message, "bump the counter");
    assert_eq!(
        store.dynamic(seg("counter")).history(&entity("c")).unwrap()[0],
        commit,
        "the read entry's commit must be the same tip history() reports"
    );

    // The earlier commit reads back with its own default-summary message,
    // distinct from the tip's.
    let earlier = store
        .dynamic(seg("counter"))
        .read(first)
        .unwrap()
        .present()
        .expect("the commit holds a value");
    assert_eq!(earlier.value, value!({ "n": 1 }));
    assert_eq!(earlier.commit, first);
    assert_eq!(earlier.message, "store counter/c");
}

/// Identity is derived from the complete bound document, so the same content
/// published under two names has one id and two independent refs.
#[test]
fn content_identity_is_shared_across_names() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    let value = value!({ "n": 1 });

    let first = kind
        .write(&value)
        .message("first metadata")
        .entity_with_alias(&entity("first"))
        .unwrap();
    let second = kind
        .write(&value)
        .message("different metadata")
        .entity_with_alias(&entity("second"))
        .unwrap();

    assert_eq!(first, second, "the bound document is the identity input");
    assert_eq!(
        kind.list().unwrap(),
        vec![entity("first"), entity("second")]
    );
    assert_eq!(
        kind.read(entity("first")).unwrap().value(),
        Some(value.clone())
    );
    assert_eq!(kind.read(entity("second")).unwrap().value(), Some(value));
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
        "the complete bound document includes the schema"
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
    let first_commit = kind
        .read(&name)
        .map(|state| state.present())
        .unwrap()
        .unwrap()
        .commit;
    kind.write(&value).message("two").at(&name).unwrap();
    let second_commit = kind
        .read(&name)
        .map(|state| state.present())
        .unwrap()
        .unwrap()
        .commit;

    // New metadata over unchanged content does not manufacture a commit.
    assert_eq!(first_commit, second_commit);
    assert_eq!(kind.history(&name).unwrap(), vec![first_commit]);
}
#[test]
fn by_commit_write_of_identical_content_twice_is_idempotent() {
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
        .by_commit()
        .unwrap();
    let second = store
        .dynamic(seg("counter"))
        .write(&value)
        .by_commit()
        .unwrap();
    assert_eq!(first, second);
}

/// The compatibility `entity_name` helper still resolves the old commit-named
/// alias, while the canonical ref is based on the complete bound
/// document tree rather than the publication commit.
#[test]
fn by_commit_names_an_entity_by_its_whole_commit_id() {
    let store = store();
    store
        .dynamic(seg("counter"))
        .schema()
        .put(&schema_of::<Counter>().unwrap())
        .unwrap();

    let commit = store
        .dynamic(seg("counter"))
        .write(&value!({ "n": 1 }))
        .by_commit()
        .unwrap();

    let name = entity_name(commit);
    assert_eq!(name.to_string(), commit.to_string());
    assert_eq!(store.dynamic(seg("counter")).list().unwrap(), vec![name]);
}

/// [`git_store::kind::Put::by_commit_under`] retains the old grouped
/// commit-named alias, so compatibility reads and `entity_name_under` continue
/// to work while the canonical identity remains content-derived.
#[test]
fn by_commit_under_names_an_entity_by_group_and_whole_commit_id() {
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
        .by_commit_under(&group)
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
    let original = counter
        .read(id)
        .map(|state| state.present())
        .unwrap()
        .unwrap()
        .commit;

    let gix::objs::Object::Commit(original_object) =
        store.objects().get(&typed(original)).unwrap().unwrap()
    else {
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
    assert!(matches!(counter.read(id).unwrap(), EntityState::Deleted(_)));
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
        counter.read(commit).unwrap().value().unwrap(),
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
            #[expect(dead_code, reason = "carried for Debug, never matched on")] git_store::Error,
        ),
    }

    impl From<git_store::Error> for Refusal {
        fn from(err: git_store::Error) -> Self {
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
        counter.read(entity("missing")).unwrap().value(),
        None,
        "a refused rebuild writes no ref"
    );
}

/// A write that lands between the read feeding `rebuild` and the read arming
/// the compare-and-swap must not be clobbered: `rebuild` must see — and land
/// over — the very commit the CAS expects, never one read before a concurrent
/// write landed.
#[test]
fn try_update_does_not_clobber_a_write_that_lands_during_rebuild() {
    let store = store();
    let counter = store.kind::<Counter>(seg("counter"));
    counter.publish().unwrap();

    let original = counter.put(&entity("c"), &Counter { n: 1 }).unwrap();

    // A legitimately schema-bound competing commit, parented on the same tip,
    // built without ever touching "c"'s ref directly.
    let scratch = entity("scratch");
    store
        .refs()
        .apply(RefEdit::Create {
            name: counter.reference(&scratch),
            new: original,
        })
        .unwrap();
    let winner = counter.write(&Counter { n: 50 }).at(&scratch).unwrap();

    // Land the concurrent write exactly between the read that produced
    // `current` (n=1) and the read that arms the compare-and-swap — the only
    // window in which `rebuild` sees state the CAS does not.
    let injected = std::cell::Cell::new(false);
    let commit = counter
        .try_update::<Error>(&entity("c"), |current| {
            if !injected.replace(true) {
                store
                    .refs()
                    .apply(RefEdit::Update {
                        name: counter.reference(&entity("c")),
                        expected: original,
                        new: winner,
                    })
                    .unwrap();
            }
            let n = current.map_or(0, |entry| entry.value.n);
            Ok((format!("bump to {}", n + 1), Counter { n: n + 1 }))
        })
        .unwrap();

    assert_eq!(
        counter.read(commit).unwrap().value().unwrap(),
        Counter { n: 51 },
        "rebuilt off the concurrent winner's n=50, not the stale n=1 read before it landed"
    );
    assert_eq!(
        counter.history(&entity("c")).unwrap(),
        vec![commit, winner, original]
    );
}

/// A [`Put::by_commit`] whose first `Create` loses to spurious backend
/// contention — nothing actually landed — retries the very same create
/// rather than reporting a collision: the name is still free, so there is
/// nothing to have collided with.
#[test]
fn by_commit_retries_to_success_on_pure_contention() {
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
        .by_commit()
        .unwrap();
    assert_eq!(
        store
            .dynamic(seg("counter"))
            .read(commit)
            .unwrap()
            .value()
            .unwrap(),
        value!({ "n": 1 })
    );
}

/// Two generations of one type. Derivation pairs definitions by name, so the
/// type keeps its name across the edge exactly as a real evolution would.
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
    assert_eq!(
        a.read(publications[0].entity_id()).unwrap().value(),
        Some(value!({ "n": 1 }))
    );
    assert_eq!(
        b.read(publications[1].entity_id()).unwrap().value(),
        Some(value!({ "n": 2 }))
    );
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

#[test]
fn transaction_alias_without_publish_is_a_typed_error() {
    let store = store();
    // No publish precedes the alias, so there is nothing to redirect. This
    // used to abort the caller's process; it is now a recoverable error.
    let err = store
        .transaction("alias")
        .alias(entity("name"))
        .err()
        .expect("alias without a preceding publish must fail");
    assert!(matches!(err, Error::AliasWithoutPublish), "{err:?}");
}

#[test]
fn transaction_alias_redirects_the_preceding_publish() {
    let store = store();
    let a = store.dynamic(seg("a"));
    a.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    let doc = DocumentTree::from(a.compile(&value!({ "n": 7 })).unwrap());

    let publications = store
        .transaction("aliased publish")
        .publish(&seg("a"), doc, Expectation::Absent)
        .alias(entity("custom-name"))
        .unwrap()
        .commit()
        .unwrap();

    assert_eq!(publications.len(), 1);
    // The document landed under the alias, not its content-derived name.
    let entries = a.list_entries().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].0, entity("custom-name"));
    assert_eq!(
        a.read(entity("custom-name")).unwrap().value(),
        Some(value!({ "n": 7 }))
    );
}

// --- fault-injecting RefStore: the no-op publication's compare-and-swap ---

/// A competing write to land the moment a watched ref is read, plus the
/// pre-race value to hand back so the caller resolves against state the ref
/// no longer holds.
struct ReadInjection {
    target: RefName,
    report: ObjectId,
    competing: RefEdit,
}

/// Wraps a [`MemoryRefStore`] to reproduce the window between the read that
/// resolves a publication and the caller being told it succeeded: the read of
/// one watched ref lands a competing write first, then reports the value from
/// before that write.
struct InjectOnReadRefStore {
    inner: MemoryRefStore,
    injection: RefCell<Option<ReadInjection>>,
}

impl InjectOnReadRefStore {
    fn new() -> Self {
        Self {
            inner: MemoryRefStore::new(),
            injection: RefCell::new(None),
        }
    }

    fn arm(&self, injection: ReadInjection) {
        *self.injection.borrow_mut() = Some(injection);
    }
}

impl RefStore for InjectOnReadRefStore {
    type Error = Infallible;

    fn read(&self, name: &RefName) -> Result<Option<ObjectId>, Self::Error> {
        let fired = {
            let mut slot = self.injection.borrow_mut();
            match slot.as_ref() {
                Some(injection) if injection.target == *name => slot.take(),
                _ => None,
            }
        };
        if let Some(injection) = fired {
            self.inner
                .apply(injection.competing)
                .expect("injected competing write applies");
            return Ok(Some(injection.report));
        }
        self.inner.read(name)
    }

    fn prefixed(&self, prefix: &RefPrefix) -> Result<Vec<(RefName, ObjectId)>, Self::Error> {
        self.inner.prefixed(prefix)
    }

    fn apply_batch(&self, edits: Vec<RefEdit>) -> Result<(), ApplyError<Self::Error>> {
        self.inner.apply_batch(edits)
    }
}

impl Committer for InjectOnReadRefStore {
    type Error = Infallible;

    fn signature(&self) -> Result<gix::actor::Signature, Self::Error> {
        self.inner.signature()
    }
}

/// Write a commit object directly into `store`'s object database, without
/// touching any ref — a fixture for a competing writer's commit.
fn manual_commit<R, O>(
    store: &Store<R, O>,
    tree: ObjectId,
    parent: Option<ObjectId>,
    message: &str,
) -> ObjectId
where
    R: RefStore + Committer,
    O: gix::prelude::Find + gix::objs::Write,
{
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
    gix::objs::Write::write(store.objects(), &commit).expect("write commit")
}

/// Republishing identical content is a no-op — but when the caller supplies an
/// explicit [`Expectation::Exactly`], that expectation is a one-shot
/// compare-and-swap and must still hold at the moment the call reports success.
/// A competing write landing between the read that resolves the publication and
/// the return must not be reported as a successful publication of the content
/// the caller asked for.
#[test]
fn no_op_republish_verifies_the_expectation_atomically() {
    let store = Store::new(InjectOnReadRefStore::new(), ObjectStore::default());
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();

    let alias = entity("c");
    let published = kind.write(&value!({ "n": 1 })).at(&alias).unwrap();

    // A legitimate competing commit for different content, parented on the
    // published one, built without touching `alias` or the kind index.
    let competing_tree = kind.compile(&value!({ "n": 2 })).unwrap();
    let competing = manual_commit(&store, competing_tree, Some(published), "competing\n");

    // Republish the identical content under `Exactly(published)`. Arm the
    // store so the read feeding `resolve_publish` lands the competing commit
    // first and reports the pre-race value, so the identity shortcut is taken
    // over a ref that no longer holds `published`.
    let tree = kind.compile(&value!({ "n": 1 })).unwrap();
    assert_eq!(
        match store.objects().get(&typed(published)).unwrap().unwrap() {
            GitObject::Commit(commit) => commit.tree,
            other => panic!("expected a commit, got {other:?}"),
        },
        tree,
        "the recompiled tree must equal the published commit's tree"
    );
    let prepared = match store.inspect_document(DocumentTree::from(tree)).unwrap() {
        DocumentInspection::Bound(document) => document,
        other => panic!("expected a bound document, got {other:?}"),
    };
    let reference = kind.reference(&alias);
    store.refs().arm(ReadInjection {
        target: reference.clone(),
        report: published,
        competing: RefEdit::Update {
            name: reference.clone(),
            expected: published,
            new: competing,
        },
    });

    let result = kind.publish_prepared(
        &prepared,
        PublishOptions::new("prepared publication")
            .with_alias(alias.clone())
            .with_expectation(Expectation::Exactly(published)),
    );

    // The documented promise: either the call fails because the expectation
    // went stale under it, or it succeeds and the ref really points at the
    // commit it reports.
    if let Ok(publication) = result {
        assert_eq!(
            store.refs().read(&reference).unwrap(),
            Some(publication.commit),
            "a successful publication must leave the ref at the commit it reports"
        );
    }
}

// --- ref-listing cost of maintaining the per-kind index ---

/// Wraps a [`MemoryRefStore`], counting the ref-listing work a run performs so
/// a test can assert that publishing does not re-list the whole kind.
struct CountingRefs {
    inner: MemoryRefStore,
    listed: Cell<usize>,
    scanned: Cell<usize>,
    /// Every prefix handed to [`RefStore::prefixed`], in order — a subtree
    /// read that scans the whole kind is visible here as the kind's own
    /// namespace rather than the requested subtree's.
    prefixes: RefCell<Vec<String>>,
}

impl CountingRefs {
    fn new() -> Self {
        Self {
            inner: MemoryRefStore::new(),
            listed: Cell::new(0),
            scanned: Cell::new(0),
            prefixes: RefCell::new(Vec::new()),
        }
    }
}

impl RefStore for CountingRefs {
    type Error = Infallible;

    fn read(&self, name: &RefName) -> Result<Option<ObjectId>, Self::Error> {
        self.inner.read(name)
    }

    fn prefixed(&self, prefix: &RefPrefix) -> Result<Vec<(RefName, ObjectId)>, Self::Error> {
        self.prefixes.borrow_mut().push(prefix.as_str().to_owned());
        let refs = self.inner.prefixed(prefix)?;
        self.listed.set(self.listed.get() + 1);
        self.scanned.set(self.scanned.get() + refs.len());
        Ok(refs)
    }

    fn apply_batch(&self, edits: Vec<RefEdit>) -> Result<(), ApplyError<Self::Error>> {
        self.inner.apply_batch(edits)
    }
}

impl Committer for CountingRefs {
    type Error = Infallible;

    fn signature(&self) -> Result<gix::actor::Signature, Self::Error> {
        self.inner.signature()
    }
}

/// Publishing N entities to one kind must not re-list the whole kind per
/// publish. The per-kind index is a materialized cache, but maintaining it
/// must not scan every existing entity ref: the ref-listing work across N
/// publishes must not grow with N.
#[test]
fn publishing_many_entities_does_not_relist_the_kind() {
    fn work_after(n: usize) -> (usize, usize) {
        let store = Store::new(CountingRefs::new(), ObjectStore::default());
        let counter = store.dynamic(seg("counter"));
        counter
            .schema()
            .put(&schema_of::<Counter>().unwrap())
            .unwrap();
        for i in 0..n {
            counter
                .put(&entity(&format!("e{i}")), &value!({ "n": i }))
                .unwrap();
        }
        (store.refs().listed.get(), store.refs().scanned.get())
    }

    let (small_listed, small_scanned) = work_after(8);
    let (large_listed, large_scanned) = work_after(16);
    assert_eq!(
        (small_listed, small_scanned),
        (large_listed, large_scanned),
        "doubling the number of publishes grew the ref-listing work \
         (listed {small_listed}->{large_listed}, scanned {small_scanned}->{large_scanned}): \
         the index is re-listed on every publish"
    );
}

/// A subtree listing must scan only that subtree's refs, not every entity of
/// the kind: `list_under` is documented as scanning the given subtree, so the
/// ref store must be asked for the subtree prefix and nothing broader.
#[test]
fn list_under_scans_only_the_requested_subtree() {
    let store = Store::new(CountingRefs::new(), ObjectStore::default());
    let note = store.dynamic(seg("note"));
    note.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    // Insert out of order to prove the result is ordered, and include a name
    // that shares a text prefix across a segment boundary (`ab` vs `a`) plus a
    // sibling subtree (`b`) that must not appear.
    for name in ["a/y", "b/z", "ab/q", "a/x"] {
        note.put(&entity(name), &value!({ "n": 1 })).unwrap();
    }
    store.refs().prefixes.borrow_mut().clear();

    let found = note.list_under(&entity("a")).unwrap();
    assert_eq!(
        found,
        ["a/x", "a/y"].map(entity),
        "subtree listing returned the wrong set"
    );

    let prefixes = store.refs().prefixes.borrow().clone();
    assert_eq!(
        prefixes,
        vec!["refs/store/note/a".to_owned()],
        "list_under listed {prefixes:?}, not just the subtree `refs/store/note/a`"
    );
}

// --- a real second writer racing the index's compare-and-swap ---

/// A one-shot hook run at the start of an `apply_batch`, shared so arming it
/// through one clone of a [`RacingRefs`] fires it through another.
type FireOnce = Rc<RefCell<Option<Box<dyn FnOnce()>>>>;

/// One in-memory ref store shared by cooperating [`Store`]s, with an optional
/// one-shot hook that runs another writer's publish at the moment an
/// `apply_batch` begins — the window between a reader's index read and its
/// compare-and-swap. Lets two same-kind publishes race deterministically on
/// one thread.
#[derive(Clone)]
struct RacingRefs {
    inner: Rc<MemoryRefStore>,
    fire: FireOnce,
}

impl RacingRefs {
    fn new(inner: Rc<MemoryRefStore>) -> Self {
        Self {
            inner,
            fire: Rc::new(RefCell::new(None)),
        }
    }

    fn arm(&self, fire: impl FnOnce() + 'static) {
        *self.fire.borrow_mut() = Some(Box::new(fire));
    }
}

impl RefStore for RacingRefs {
    type Error = Infallible;

    fn read(&self, name: &RefName) -> Result<Option<ObjectId>, Self::Error> {
        self.inner.read(name)
    }

    fn prefixed(&self, prefix: &RefPrefix) -> Result<Vec<(RefName, ObjectId)>, Self::Error> {
        self.inner.prefixed(prefix)
    }

    fn apply_batch(&self, edits: Vec<RefEdit>) -> Result<(), ApplyError<Self::Error>> {
        if let Some(fire) = self.fire.borrow_mut().take() {
            fire();
        }
        self.inner.apply_batch(edits)
    }
}

impl Committer for RacingRefs {
    type Error = Infallible;

    fn signature(&self) -> Result<gix::actor::Signature, Self::Error> {
        self.inner.signature()
    }
}

/// Two writers publishing different names to the same kind: the second lands
/// in the window between the first writer's index read and its index
/// compare-and-swap, so the first loses that compare-and-swap and retries.
/// The retry must re-derive the index from the winner's, or the winner's entry
/// would be dropped.
#[test]
fn a_concurrent_writers_entry_is_not_lost_from_the_kind_index() {
    let refs = Rc::new(MemoryRefStore::new());
    let objects = Rc::new(ObjectStore::default());
    let racing = RacingRefs::new(Rc::clone(&refs));
    let store = Store::new(racing.clone(), Rc::clone(&objects));
    let counter = store.dynamic(seg("counter"));
    counter
        .schema()
        .put(&schema_of::<Counter>().unwrap())
        .unwrap();
    counter.put(&entity("a"), &value!({ "n": 1 })).unwrap();

    // Writer B publishes "b" to the same kind — shared refs and objects, its
    // own `Store` — exactly when A first tries to apply.
    let (refs_b, objects_b) = (Rc::clone(&refs), Rc::clone(&objects));
    racing.arm(move || {
        let writer = Store::new(RacingRefs::new(refs_b), objects_b);
        writer
            .dynamic(seg("counter"))
            .put(&entity("b"), &value!({ "n": 2 }))
            .unwrap();
    });

    counter.put(&entity("c"), &value!({ "n": 3 })).unwrap();

    let names: Vec<RefPath> = counter
        .list_entries()
        .unwrap()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        names,
        vec![entity("a"), entity("b"), entity("c")],
        "A's retry dropped a concurrent writer's index entry"
    );

    // Inspect the maintained index directly: `list_entries` validates against
    // the entity refs and would silently fall back, masking a dropped entry.
    let cache = RefPrefix::new("refs/cache").unwrap();
    let (_, index_tree) = store
        .refs()
        .prefixed(&cache)
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let entries = store
        .objects()
        .get_tree(&typed(index_tree))
        .unwrap()
        .unwrap();
    assert_eq!(
        entries.len(),
        3,
        "the maintained index dropped a concurrent writer's entry"
    );
}

// --- fault-injecting RefStore: a publish that never wins its race ---

/// The number of `apply_batch` calls the wrapper below tolerates before it
/// declares the caller's loop unbounded. Comfortably above any sane retry
/// cap, so a bounded loop stops well short while an unbounded one fails the
/// test here instead of hanging the suite.
const ALWAYS_LOSES_LIMIT: usize = 64;

/// A [`RefStore`] that reports a lost race for a *watched* ref on every
/// `apply_batch`, counting those attempts. Every other ref (the index, the
/// schema) delegates to a [`MemoryRefStore`], so a kind can be set up
/// normally and only the watched publish is made to lose forever.
struct AlwaysLosesRefStore {
    inner: MemoryRefStore,
    watched: RefCell<Option<RefName>>,
    attempts: Cell<usize>,
}

impl AlwaysLosesRefStore {
    fn new() -> Self {
        Self {
            inner: MemoryRefStore::new(),
            watched: RefCell::new(None),
            attempts: Cell::new(0),
        }
    }

    fn watch(&self, name: RefName) {
        *self.watched.borrow_mut() = Some(name);
    }

    fn attempts(&self) -> usize {
        self.attempts.get()
    }
}

impl RefStore for AlwaysLosesRefStore {
    type Error = Infallible;

    fn read(&self, name: &RefName) -> Result<Option<ObjectId>, Self::Error> {
        self.inner.read(name)
    }

    fn prefixed(&self, prefix: &RefPrefix) -> Result<Vec<(RefName, ObjectId)>, Self::Error> {
        self.inner.prefixed(prefix)
    }

    fn apply_batch(&self, edits: Vec<RefEdit>) -> Result<(), ApplyError<Self::Error>> {
        let watched = self.watched.borrow();
        let Some(name) = watched.as_ref() else {
            return self.inner.apply_batch(edits);
        };
        let Some(edit) = edits.iter().find(|edit| edit.name() == name) else {
            return self.inner.apply_batch(edits);
        };
        let count = self.attempts.get() + 1;
        self.attempts.set(count);
        assert!(
            count <= ALWAYS_LOSES_LIMIT,
            "unbounded retry: >{ALWAYS_LOSES_LIMIT} attempts without giving up"
        );
        Err(ApplyError::LostRace {
            name: edit.name().clone(),
            expected: edit.expectation(),
        })
    }
}

impl Committer for AlwaysLosesRefStore {
    type Error = Infallible;

    fn signature(&self) -> Result<gix::actor::Signature, Self::Error> {
        self.inner.signature()
    }
}

/// A publish whose compare-and-swap loses forever must give up with a typed
/// contention error after a bounded number of attempts, never loop
/// unboundedly. Pre-fix the loop spins past [`ALWAYS_LOSES_LIMIT`] and the
/// wrapper's assert fires.
#[test]
fn always_losing_publish_is_bounded_and_reports_contention() {
    let store = Store::new(AlwaysLosesRefStore::new(), ObjectStore::default());
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    store.refs().watch(kind.reference(&entity("c")));

    let err = kind
        .put(&entity("c"), &value!({ "n": 1 }))
        .expect_err("a permanently lost race must not succeed");
    assert!(matches!(err, Error::Contention { .. }), "{err:?}");
    assert!(
        store.refs().attempts() <= 8,
        "gave up only after {} attempts, past the retry cap",
        store.refs().attempts()
    );
}

/// The uncontended path must not pay retry latency: a single publish against a
/// healthy store completes without the retry helper ever sleeping.
#[test]
fn uncontended_publish_does_not_sleep() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();

    let start = std::time::Instant::now();
    kind.put(&entity("c"), &value!({ "n": 1 })).unwrap();
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(20),
        "uncontended publish took {elapsed:?}; the retry path must not sleep on the happy path"
    );
}
