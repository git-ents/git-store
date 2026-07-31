//! Integration tests for the dynamic value deserialization heuristic.
//!
//! Covers spec requirement:
//!   deserialization.dynamic.heuristic
//!     — with no type marker on disk, a dynamic read recovers shape from the
//!       object graph: a UTF-8 blob is a String, any other blob is Bytes, a
//!       non-empty tree with all-ordinal names is an Array, and any other tree
//!       (including the empty tree) is an Object.

use std::collections::HashMap;

use facet_git_tree::{
    DeserializeError, EntryKind, EntryMode, ObjectStore, TreeEntry, deserialize, serialize,
};
use facet_value::{Value, value};
use gix_object::{Kind, Write as _};

mod common;
use common::{Person, WithMap};

// --- blobs ---

/// A blob whose bytes are valid UTF-8 reads back as a String.
#[test]
fn utf8_blob_reads_as_string() -> anyhow::Result<()> {
    let (root, store) = serialize(&"hello".to_string())?;
    let v: Value = deserialize(&root, &store)?;
    assert_eq!(v, Value::from("hello"));
    Ok(())
}

/// A blob with invalid UTF-8 reads back as Bytes.
#[test]
fn non_utf8_blob_reads_as_bytes() -> anyhow::Result<()> {
    let data: Vec<u8> = vec![0xff, 0xfe, 0x00];
    let (root, store) = serialize(&data)?;
    let v: Value = deserialize(&root, &store)?;
    assert_eq!(v, Value::from(data));
    Ok(())
}

// --- trees ---

/// A non-empty tree whose entry names are all decimal ordinals reads back as
/// an Array, in ordinal order.
#[test]
fn ordinal_tree_reads_as_array() -> anyhow::Result<()> {
    let items = vec!["x".to_string(), "y".to_string()];
    let (root, store) = serialize(&items)?;
    let v: Value = deserialize(&root, &store)?;
    assert_eq!(v, value!(["x", "y"]));
    Ok(())
}

/// A tree with non-ordinal names reads back as an Object.
#[test]
fn named_tree_reads_as_object() -> anyhow::Result<()> {
    let (root, store) = serialize(&common::Config {
        name: "n".into(),
        value: 5,
    })?;
    let v: Value = deserialize(&root, &store)?;
    assert_eq!(v, value!({ "name": "n", "value": "5" }));
    Ok(())
}

/// The empty tree reads back as an empty Object — the designated reading for
/// the null/empty-Object collision.
#[test]
fn empty_tree_reads_as_empty_object() -> anyhow::Result<()> {
    let (root, store) = serialize(&Value::NULL)?;
    let v: Value = deserialize(&root, &store)?;
    assert_eq!(v, value!({}));
    Ok(())
}

/// A tree mixing an ordinal name with a non-ordinal one is not all-ordinal, so
/// it reads back as an Object keyed by the literal names.
#[test]
fn mixed_ordinal_and_named_tree_reads_as_object() -> anyhow::Result<()> {
    let mut table = HashMap::new();
    table.insert("0000".to_string(), "a".to_string());
    table.insert("name".to_string(), "b".to_string());
    let (root, store) = serialize(&WithMap { table })?;
    let v: Value = deserialize(&root, &store)?;
    assert_eq!(v, value!({ "table": { "0000": "a", "name": "b" } }));
    Ok(())
}

// --- typed values under the heuristic ---

/// A typed struct reads back as an Object of Strings: field names survive, but
/// scalar leaves (numbers, bools) have no marker and come back textual.
#[test]
fn typed_person_reads_as_object_of_strings() -> anyhow::Result<()> {
    let (root, store) = serialize(&Person {
        name: "Ada".into(),
        age: 36,
        active: true,
    })?;
    let v: Value = deserialize(&root, &store)?;
    assert_eq!(v, value!({ "name": "Ada", "age": "36", "active": "true" }));
    Ok(())
}

// --- recursion guard ---

/// Heuristic recursion is bounded by the same `MAX_DEPTH` guard as typed
/// recursion: a tree nested deeper fails rather than overflowing the stack.
#[test]
fn depth_beyond_max_is_rejected() -> anyhow::Result<()> {
    let store = ObjectStore::default();
    let mut root = store.write_buf(Kind::Blob, b"leaf\n").expect("write leaf");
    for _ in 0..40 {
        root = store
            .write(&gix_object::Tree {
                entries: vec![TreeEntry {
                    mode: EntryMode::from(EntryKind::Tree),
                    filename: "0000".into(),
                    oid: root,
                }],
            })
            .expect("write nesting tree");
    }
    let err = deserialize::<Value>(&root, &store).unwrap_err();
    assert!(matches!(err, DeserializeError::MaxDepth(_)), "got {err:?}");
    Ok(())
}
