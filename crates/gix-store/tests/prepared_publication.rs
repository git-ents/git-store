//! Focused coverage for prepared document publication.

use facet_git_tree::{GitObject, ObjectStore};
use facet_value::value;
use gix_store::{
    DocumentInspection, DocumentTree, Expectation, MemoryRefStore, RefPath, RefSegment, RefStore,
    Store, canonical_document_id,
};

fn seg(name: &str) -> RefSegment {
    RefSegment::new(name).unwrap()
}

fn path(name: &str) -> RefPath {
    RefPath::new(name).unwrap()
}

fn store() -> Store<MemoryRefStore, ObjectStore> {
    Store::new(MemoryRefStore::new(), ObjectStore::default())
}

fn prepared(
    store: &Store<MemoryRefStore, ObjectStore>,
    tree: gix_store::ObjectId,
) -> gix_store::PreparedDocument {
    match store.inspect_document(DocumentTree::from(tree)).unwrap() {
        DocumentInspection::Bound(document) => document,
        other => panic!("expected a bound document, got {other:?}"),
    }
}

fn options(alias: Option<RefPath>, expectation: Option<Expectation>) -> gix_store::PublishOptions {
    gix_store::PublishOptions {
        alias,
        message: "prepared publication".to_owned(),
        parent: None,
        expectation,
    }
}

#[test]
fn publishes_complete_document_at_the_requested_name() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema()
        .put(&facet_git_tree::schema_of::<Counter>().unwrap())
        .unwrap();
    let tree = kind.compile(&value!({ "n": 7 })).unwrap();
    let id = canonical_document_id(tree);
    let prepared = prepared(&store, tree);

    let publication = kind
        .publish_prepared(
            &prepared,
            options(Some(path("friendly")), Some(Expectation::Absent)),
        )
        .unwrap();
    assert_eq!(publication.id, id);

    let commit = store
        .refs()
        .read(&kind.reference(&path("friendly")))
        .unwrap()
        .unwrap();
    assert_eq!(publication.commit, commit);
    assert_eq!(
        kind.list_entries().unwrap(),
        vec![(path("friendly"), commit)]
    );
    assert_eq!(
        kind.get(&path("friendly")).unwrap(),
        Some(value!({ "n": 7 }))
    );
}

#[test]
fn explicit_parent_appends_above_legacy_tip_while_creating_absent_alias() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema()
        .put(&facet_git_tree::schema_of::<Counter>().unwrap())
        .unwrap();

    let legacy_commit = kind.put(&path("legacy"), &value!({ "n": 1 })).unwrap();
    let legacy_tree = match store.objects().get(&legacy_commit).unwrap() {
        GitObject::Commit(commit) => commit.tree,
        other => panic!("expected legacy commit, got {other:?}"),
    };
    let legacy_id = canonical_document_id(legacy_tree);
    kind.remove(&path("legacy")).unwrap();
    let canonical_name: RefPath = legacy_id.as_segment().into();
    kind.remove(&canonical_name).unwrap();

    let next_tree = kind.compile(&value!({ "n": 2 })).unwrap();
    let prepared = prepared(&store, next_tree);
    let publication = kind
        .publish_prepared(
            &prepared,
            gix_store::PublishOptions::new("normalize legacy")
                .with_alias(path("imported"))
                .with_parent(legacy_commit)
                .with_expectation(Expectation::Absent),
        )
        .unwrap();

    let commit = match store.objects().get(&publication.commit()).unwrap() {
        GitObject::Commit(commit) => commit,
        other => panic!("expected publication commit, got {other:?}"),
    };
    assert_eq!(commit.parents.as_ref(), &[legacy_commit]);
    assert_eq!(
        kind.get(&path("imported")).unwrap(),
        Some(value!({ "n": 2 }))
    );
}

#[test]
fn rejects_a_prepared_document_from_another_kind() {
    let store = store();
    let source = store.dynamic(seg("source"));
    source
        .schema()
        .put(&facet_git_tree::schema_of::<Counter>().unwrap())
        .unwrap();
    let tree = source.compile(&value!({ "n": 4 })).unwrap();
    let prepared = prepared(&store, tree);

    let target = store.dynamic(seg("target"));
    target
        .schema()
        .put(&facet_git_tree::schema_of::<Counter>().unwrap())
        .unwrap();
    assert!(
        target
            .publish_prepared(&prepared, options(None, Some(Expectation::Absent)))
            .is_err()
    );
    assert!(target.list_entries().unwrap().is_empty());
}

#[test]
fn stale_explicit_expectation_is_not_retried_or_published() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema()
        .put(&facet_git_tree::schema_of::<Counter>().unwrap())
        .unwrap();
    let alias = path("friendly");
    kind.put_with_alias(&alias, &value!({ "n": 1 })).unwrap();
    let old_commit = kind.get_entry(&alias).unwrap().unwrap().commit;
    kind.put_with_alias(&alias, &value!({ "n": 2 })).unwrap();
    let tree = kind.compile(&value!({ "n": 3 })).unwrap();
    let prepared = prepared(&store, tree);

    assert!(
        kind.publish_prepared(
            &prepared,
            options(Some(alias.clone()), Some(Expectation::Exactly(old_commit))),
        )
        .is_err()
    );
    assert_eq!(kind.get(&alias).unwrap(), Some(value!({ "n": 2 })));
    // The rejected write left no trace, and the name's history still reaches
    // the superseded publication.
    assert_eq!(kind.history(&alias).unwrap().len(), 2);
    assert_eq!(kind.get_at(old_commit).unwrap(), value!({ "n": 1 }));
}

#[test]
fn the_same_prepared_document_may_be_published_under_several_names() {
    let store = store();
    let kind = store.dynamic(seg("counter"));
    kind.schema()
        .put(&facet_git_tree::schema_of::<Counter>().unwrap())
        .unwrap();
    let tree = kind.compile(&value!({ "n": 9 })).unwrap();
    let id = canonical_document_id(tree);
    let prepared = prepared(&store, tree);

    kind.publish_prepared(
        &prepared,
        options(Some(path("first")), Some(Expectation::Absent)),
    )
    .unwrap();
    let second = kind
        .publish_prepared(
            &prepared,
            options(Some(path("second")), Some(Expectation::Absent)),
        )
        .unwrap();

    // Identity is derived from content, so both names address the same entity
    // while remaining independent refs.
    assert_eq!(second.id, id);
    for name in ["first", "second"] {
        assert_eq!(kind.get(&path(name)).unwrap(), Some(value!({ "n": 9 })));
    }
    assert_eq!(kind.list().unwrap(), vec![path("first"), path("second")]);
}

#[derive(facet::Facet)]
struct Counter {
    n: u32,
}
