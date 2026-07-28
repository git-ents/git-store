//! Integration tests for serialization structure.
//!
//! Covers spec requirements:
//!   serialization.mechanism — a serialize function exists and accepts Facet types
//!   serialization.design.leaves — scalar fields serialize to UTF-8 blobs
//!   serialization.design.trees.composites — composites are trees with no sentinel

use facet_git_tree::{EntryKind, serialize};

mod common;
use common::{Nested, Person, Point, find_entry, tree_entries};

// --- serialization.mechanism ---

/// The `serialize` function is callable with any Facet type and returns a root ID
/// plus an object store containing at least one object.
#[test]

fn serialize_returns_non_empty_store() {
    let (root_id, store) = serialize(&Point { x: 1.0, y: 2.0 }).expect("serialize should succeed");
    assert!(
        store.get(&root_id).is_some(),
        "root id must resolve in the store"
    );
}

// --- serialization.design.leaves ---

/// Scalar struct fields are stored as UTF-8 blobs, not sub-trees.
#[test]

fn scalar_fields_are_blobs() {
    let (root_id, store) = serialize(&Point { x: 1.0, y: 2.0 }).expect("serialize should succeed");

    let x_entry = find_entry(&store, &root_id, "x");
    assert_eq!(
        x_entry.mode.kind(),
        EntryKind::Blob,
        "field `x` must be a blob"
    );

    let y_entry = find_entry(&store, &root_id, "y");
    assert_eq!(
        y_entry.mode.kind(),
        EntryKind::Blob,
        "field `y` must be a blob"
    );
}

/// Blob content for numeric fields is valid UTF-8.
#[test]

fn blob_content_is_utf8() {
    let (root_id, store) = serialize(&Point { x: 2.5, y: -1.0 }).expect("serialize should succeed");

    for field in ["x", "y"] {
        let entry = find_entry(&store, &root_id, field);
        let bytes = store
            .get_blob(&entry.oid)
            .unwrap_or_else(|| panic!("blob missing for field {field}"));
        std::str::from_utf8(&bytes)
            .unwrap_or_else(|_| panic!("field {field} blob is not valid UTF-8"));
    }
}

/// Blob content for a string field is the UTF-8 encoding of the string value.
#[test]

fn string_field_blob_content() {
    let (root_id, store) = serialize(&Person {
        name: "Alice".to_string(),
        age: 30,
        active: true,
    })
    .expect("serialize should succeed");

    let entry = find_entry(&store, &root_id, "name");
    let bytes = store
        .get_blob(&entry.oid)
        .expect("name blob must be present");
    assert_eq!(
        bytes, b"Alice\n",
        "string field should encode as its UTF-8 bytes plus the mandatory trailing newline"
    );
}

// --- serialization.design.trees.composites ---

/// A composite is a plain tree of its fields, carrying no sentinel entry.
#[test]

fn struct_is_plain_tree_of_fields() {
    let (root_id, store) = serialize(&Point { x: 1.0, y: 2.0 }).expect("serialize should succeed");

    let names: Vec<String> = tree_entries(&store, &root_id)
        .iter()
        .map(|e| e.filename.to_string())
        .collect();
    assert_eq!(
        names,
        vec!["x".to_string(), "y".to_string()],
        "struct tree must hold only its fields, with no sentinel entry"
    );
}

/// A nested struct field is itself a plain tree; the inner structure is recovered
/// from the Facet type on read, not from any embedded schema.
#[test]

fn nested_struct_field_is_tree() {
    let (root_id, store) = serialize(&Nested {
        location: Point { x: 1.0, y: 2.0 },
        label: "origin".to_string(),
    })
    .expect("serialize should succeed");

    let location_entry = find_entry(&store, &root_id, "location");
    assert_eq!(
        location_entry.mode.kind(),
        EntryKind::Tree,
        "nested struct field must be encoded as a tree"
    );

    let inner: Vec<String> = tree_entries(&store, &location_entry.oid)
        .iter()
        .map(|e| e.filename.to_string())
        .collect();
    assert_eq!(
        inner,
        vec!["x".to_string(), "y".to_string()],
        "nested struct tree must hold only its fields"
    );
}
