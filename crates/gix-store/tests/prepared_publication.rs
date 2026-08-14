//! Focused coverage for prepared document publication.

use facet_git_tree::{GitObject, ObjectStore};
use facet_value::value;
use gix_store::{
    DocumentInspection, Expectation, MemoryRefStore, RefPath, RefSegment, RefStore, Store,
    canonical_document_id,
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
    match store.inspect_document(tree).unwrap() {
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
fn publishes_complete_document_at_canonical_ref_and_alias() {
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

    let canonical = kind.entity_reference(id);
    let commit = store.refs().read(&canonical).unwrap().unwrap();
    assert_eq!(publication.commit, commit);
    assert_eq!(
        store
            .refs()
            .read(&kind.reference(&path("friendly")))
            .unwrap(),
        Some(commit)
    );
    assert_eq!(
        kind.list_entries().unwrap(),
        vec![(id.as_segment().into(), commit)]
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
    let first = kind.put_with_alias(&alias, &value!({ "n": 1 })).unwrap();
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
    assert_eq!(kind.get_entity(first).unwrap(), Some(value!({ "n": 1 })));
}

#[test]
fn same_prepared_content_deduplicates_while_aliases_are_updated_atomically() {
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
    let canonical_commit = store
        .refs()
        .read(&kind.entity_reference(id))
        .unwrap()
        .unwrap();
    kind.publish_prepared(
        &prepared,
        options(Some(path("second")), Some(Expectation::Absent)),
    )
    .unwrap();

    assert_eq!(
        store.refs().read(&kind.entity_reference(id)).unwrap(),
        Some(canonical_commit)
    );
    assert_eq!(
        store.refs().read(&kind.reference(&path("first"))).unwrap(),
        Some(canonical_commit)
    );
    assert_eq!(
        store.refs().read(&kind.reference(&path("second"))).unwrap(),
        Some(canonical_commit)
    );
    assert_eq!(
        kind.list_entries().unwrap(),
        vec![(id.as_segment().into(), canonical_commit)]
    );
}

#[derive(facet::Facet)]
struct Counter {
    n: u32,
}
