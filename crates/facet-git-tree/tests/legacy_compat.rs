//! Explicit compatibility coverage for objects written before the current
//! leaf framing and schema document shape.

use std::collections::BTreeMap;

use facet::Facet;
use facet_git_tree::{
    EntryKind, EntryMode, Node, ObjectId, ObjectStore, Schema, SchemaPinError, SchemaSchema,
    TreeEntry, deserialize, deserialize_legacy_leaves, deserialize_value_with_schema,
    deserialize_value_with_schema_legacy_leaves, schema_of, serialize_into,
};
use facet_value::{Value, value};
use gix_object::{Kind, Write};

#[derive(Debug, Facet)]
struct LegacyDocument {
    root: Node,
    defs: BTreeMap<String, Node>,
}

#[derive(Debug, Facet)]
struct LegacyValue {
    title: String,
    count: u32,
}

#[derive(Debug, Facet)]
struct LegacyIssue {
    title: String,
    closed_by: Option<String>,
}

fn tree(store: &ObjectStore, entries: Vec<(&str, EntryKind, ObjectId)>) -> ObjectId {
    let mut entries = entries
        .into_iter()
        .map(|(name, kind, oid)| TreeEntry {
            mode: EntryMode::from(kind),
            filename: name.into(),
            oid,
        })
        .collect::<Vec<_>>();
    entries.sort();
    store
        .write(&gix_object::Tree { entries })
        .expect("write tree")
}

/// Rewrite a current-format object graph with the historical no-newline blob
/// spelling. This stays in-memory and leaves the ordinary writer untouched.
fn remove_leaf_newlines(store: &ObjectStore, oid: ObjectId) -> ObjectId {
    match store.get(&oid).expect("fixture object") {
        facet_git_tree::GitObject::Blob(blob) => {
            let bytes = blob.data.strip_suffix(b"\n").unwrap_or(&blob.data);
            store
                .write_buf(Kind::Blob, bytes)
                .expect("write legacy blob")
        }
        facet_git_tree::GitObject::Tree(tree_object) => {
            let entries = tree_object
                .entries
                .into_iter()
                .map(|entry| {
                    let kind = entry.mode.kind();
                    (
                        String::from_utf8_lossy(&entry.filename).into_owned(),
                        kind,
                        entry.oid,
                    )
                })
                .map(|(name, kind, child)| (name, kind, remove_leaf_newlines(store, child)))
                .collect::<Vec<_>>();
            tree(
                store,
                entries
                    .iter()
                    .map(|(name, kind, child)| (name.as_str(), *kind, *child))
                    .collect(),
            )
        }
        other => panic!("unexpected fixture object: {other:?}"),
    }
}

fn legacy_schema(store: &ObjectStore) -> (ObjectId, Schema) {
    let current = schema_of::<LegacyValue>().expect("schema");
    let wire = LegacyDocument {
        root: current.root.clone(),
        defs: current.defs.clone(),
    };
    let wire_tree = serialize_into(&wire, store).expect("legacy schema wire tree");
    let legacy_tree = remove_leaf_newlines(store, wire_tree);
    let mut entries = store.get_tree(&legacy_tree).expect("legacy schema tree");
    entries.push(TreeEntry {
        mode: EntryMode::from(EntryKind::Tree),
        filename: "migration".into(),
        oid: tree(store, Vec::new()),
    });
    entries.sort();
    let legacy_tree = store
        .write(&gix_object::Tree { entries })
        .expect("legacy schema migration metadata");
    (legacy_tree, current)
}

fn pin_legacy_schema(store: &ObjectStore, tree: ObjectId) -> ObjectId {
    let mut entries = store.get_tree(&tree).expect("legacy schema tree");
    entries.push(TreeEntry {
        mode: EntryMode::from(EntryKind::Tree),
        filename: SchemaSchema::ENTRY.into(),
        oid: *SchemaSchema::LEGACY.tree(),
    });
    entries.sort();
    store
        .write(&gix_object::Tree { entries })
        .expect("legacy schema pin")
}

#[test]
fn legacy_schema_and_value_decode_only_through_explicit_mode() {
    let store = ObjectStore::default();
    let (schema_tree, current) = legacy_schema(&store);

    let title = store.write_buf(Kind::Blob, b"old issue").unwrap();
    let count = store.write_buf(Kind::Blob, b"7").unwrap();
    let value_tree = tree(
        &store,
        vec![
            ("title", EntryKind::Blob, title),
            ("count", EntryKind::Blob, count),
        ],
    );

    assert!(matches!(
        Schema::read_pinned(&schema_tree, &store),
        Err(SchemaPinError::Unpinned(tree)) if tree == schema_tree
    ));
    let decoded_schema = Schema::read_pinned_legacy(&schema_tree, &store).unwrap();
    assert_eq!(decoded_schema.kind, Schema::LEGACY_KIND);
    assert_eq!(decoded_schema.root, current.root);
    assert_eq!(decoded_schema.defs, current.defs);

    let pinned_tree = pin_legacy_schema(&store, schema_tree);
    let pinned_schema = Schema::read_pinned_legacy(&pinned_tree, &store).unwrap();
    assert_eq!(pinned_schema, decoded_schema);

    assert!(deserialize_value_with_schema(&value_tree, &decoded_schema, &store).is_err());
    let decoded =
        deserialize_value_with_schema_legacy_leaves(&value_tree, &decoded_schema, &store).unwrap();
    assert_eq!(decoded, value!({"title": "old issue", "count": 7}));
}

#[test]
fn legacy_issue_option_none_decodes_in_schema_and_dynamic_modes() {
    let store = ObjectStore::default();
    let current = schema_of::<LegacyIssue>().unwrap();
    let schema_tree = current.write_pinned(&store).unwrap();
    let schema = Schema::read_pinned_legacy(&schema_tree, &store).unwrap();
    let title = store.write_buf(Kind::Blob, b"old issue").unwrap();
    let closed_by = tree(&store, Vec::new());
    let value_tree = tree(
        &store,
        vec![
            ("title", EntryKind::Blob, title),
            ("closed_by", EntryKind::Tree, closed_by),
        ],
    );

    assert!(deserialize_value_with_schema(&value_tree, &schema, &store).is_err());
    let decoded =
        deserialize_value_with_schema_legacy_leaves(&value_tree, &schema, &store).unwrap();
    assert_eq!(decoded, value!({"title": "old issue", "closed_by": null}));

    let strict: Result<Value, _> = deserialize(&value_tree, &store);
    assert!(strict.is_err());
    let dynamic: Value = deserialize_legacy_leaves(&value_tree, &store).unwrap();
    assert_eq!(dynamic, value!({"title": "old issue", "closed_by": null}));
}

#[test]
fn legacy_mode_accepts_old_leaves_with_a_current_pinned_schema() {
    let store = ObjectStore::default();
    let current = schema_of::<LegacyValue>().unwrap();
    let schema_tree = current.write_pinned(&store).unwrap();
    let schema = Schema::read_pinned_legacy(&schema_tree, &store).unwrap();
    let title = store.write_buf(Kind::Blob, b"old issue").unwrap();
    let count = store.write_buf(Kind::Blob, b"7").unwrap();
    let value_tree = tree(
        &store,
        vec![
            ("title", EntryKind::Blob, title),
            ("count", EntryKind::Blob, count),
        ],
    );

    assert!(deserialize_value_with_schema(&value_tree, &schema, &store).is_err());
    assert_eq!(
        deserialize_value_with_schema_legacy_leaves(&value_tree, &schema, &store).unwrap(),
        value!({"title": "old issue", "count": 7})
    );
}

#[test]
fn legacy_leaf_mode_decodes_a_dynamic_value_without_relaxing_ordinary_decode() {
    let store = ObjectStore::default();
    let blob = store.write_buf(Kind::Blob, b"legacy text").unwrap();
    let strict: Result<facet_value::Value, _> = facet_git_tree::deserialize(&blob, &store);
    assert!(strict.is_err());
    let decoded: facet_value::Value =
        facet_git_tree::deserialize_legacy_leaves(&blob, &store).expect("legacy dynamic leaf");
    assert_eq!(decoded, value!("legacy text"));
}
