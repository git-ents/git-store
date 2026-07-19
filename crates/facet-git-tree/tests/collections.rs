//! Integration tests for built-in collection type serialization.
//!
//! Covers spec requirement:
//!   serialization.design.trees.collections
//!     — Array, Vec, and Map are encoded as Git trees
//!     — no type marker is recorded; element and key types come from the Facet type
//!
//! Ordinal entry naming for sequence collections is covered in `ordinals.rs`.

use std::collections::HashMap;
use std::sync::Arc;

use facet::Facet;
use facet_git_tree::{EntryKind, deserialize, serialize};

mod common;
use common::{WithArray, WithMap, WithVec, get_tree_entry_mode, tree_entries};

#[derive(Facet)]
struct WithIntMap {
    table: HashMap<u32, String>,
}

#[derive(Facet, PartialEq, Debug)]
struct WithArcStrKeyMap {
    table: HashMap<Arc<str>, u32>,
}

#[derive(Facet, PartialEq, Eq, Hash, Debug, Clone)]
struct Coord {
    x: i32,
    y: i32,
}

#[derive(Facet, PartialEq, Debug)]
struct WithCompositeKeyMap {
    table: HashMap<Coord, String>,
}

// --- Vec ---

/// A Vec field is encoded as a tree (not a blob) holding only its elements.
#[test]
fn vec_field_is_tree() {
    let (root_id, store) = serialize(&WithVec {
        items: vec![1, 2, 3],
    })
    .expect("serialize should succeed");

    let (mode, items_id) = get_tree_entry_mode(&store, &root_id, "items");
    assert_eq!(mode, EntryKind::Tree, "Vec field must be a tree");
    assert_eq!(
        tree_entries(&store, &items_id).len(),
        3,
        "Vec with 3 elements should have 3 entries"
    );
}

/// An empty Vec serializes to an empty tree.
#[test]
fn empty_vec_is_empty_tree() {
    let (root_id, store) = serialize(&WithVec { items: vec![] }).expect("serialize should succeed");

    let (mode, items_id) = get_tree_entry_mode(&store, &root_id, "items");
    assert_eq!(mode, EntryKind::Tree, "empty Vec field must be a tree");
    assert!(
        tree_entries(&store, &items_id).is_empty(),
        "empty Vec must have no entries"
    );
}

// --- Array ---

/// A fixed-size array field is encoded as a tree holding only its elements.
#[test]
fn array_field_is_tree() {
    let (root_id, store) = serialize(&WithArray {
        values: [1, 2, 3, 4],
    })
    .expect("serialize should succeed");

    let (mode, arr_id) = get_tree_entry_mode(&store, &root_id, "values");
    assert_eq!(mode, EntryKind::Tree, "array field must be a tree");
    assert_eq!(
        tree_entries(&store, &arr_id).len(),
        4,
        "array of length 4 should have 4 entries"
    );
}

// --- Map ---

/// A HashMap field is encoded as a tree holding only its entries.
#[test]
fn map_field_is_tree() {
    let mut table = HashMap::new();
    table.insert("a".to_string(), "1".to_string());
    table.insert("b".to_string(), "2".to_string());

    let (root_id, store) = serialize(&WithMap { table }).expect("serialize should succeed");

    let (mode, map_id) = get_tree_entry_mode(&store, &root_id, "table");
    assert_eq!(mode, EntryKind::Tree, "Map field must be a tree");
    assert_eq!(
        tree_entries(&store, &map_id).len(),
        2,
        "map with 2 entries should have 2 entries"
    );
}

/// An empty map serializes to an empty tree.
#[test]
fn empty_map_is_empty_tree() {
    let (root_id, store) = serialize(&WithMap {
        table: HashMap::new(),
    })
    .expect("serialize should succeed");

    let (mode, map_id) = get_tree_entry_mode(&store, &root_id, "table");
    assert_eq!(mode, EntryKind::Tree, "empty Map field must be a tree");
    assert!(
        tree_entries(&store, &map_id).is_empty(),
        "empty Map must have no entries"
    );
}

/// A map entry is named by the textual form of its key and resolves to its value.
#[test]
fn map_entry_named_by_key() {
    let mut table = HashMap::new();
    table.insert("a".to_string(), "1".to_string());

    let (root_id, store) = serialize(&WithMap { table }).expect("serialize should succeed");

    let (_, map_id) = get_tree_entry_mode(&store, &root_id, "table");
    let (mode, value_id) = get_tree_entry_mode(&store, &map_id, "a");
    assert_eq!(mode, EntryKind::Blob, "map value must be a leaf blob");
    assert_eq!(
        store.get_blob(&value_id).expect("value blob in store"),
        b"1",
        "map entry named by key must resolve to the value"
    );
}

/// A map with scalar non-`String` keys is named by the textual form of each key.
///
/// The spec (serialization.design.trees.collections, item 2a) names a scalar-keyed
/// map entry by "the textual form of its key", which covers scalar keys such as
/// `u32` (key `42` → entry name `"42"`).
#[test]
fn map_with_int_keys_named_by_textual_key() {
    let mut table = HashMap::new();
    table.insert(42u32, "x".to_string());

    let (root_id, store) = serialize(&WithIntMap { table }).expect("serialize should succeed");

    let (_, map_id) = get_tree_entry_mode(&store, &root_id, "table");
    let (mode, value_id) = get_tree_entry_mode(&store, &map_id, "42");
    assert_eq!(mode, EntryKind::Blob, "map value must be a leaf blob");
    assert_eq!(
        store.get_blob(&value_id).expect("value blob in store"),
        b"x",
        "map entry named by the textual form of its key must resolve to the value"
    );
}

/// Map insertion order does not affect the serialized tree: git sorts tree entries
/// by name, so two maps with the same pairs produce the same root object ID.
#[test]
fn map_insertion_order_is_irrelevant() {
    let mut a = HashMap::new();
    a.insert("alpha".to_string(), "1".to_string());
    a.insert("beta".to_string(), "2".to_string());
    a.insert("gamma".to_string(), "3".to_string());

    let mut b = HashMap::new();
    b.insert("gamma".to_string(), "3".to_string());
    b.insert("alpha".to_string(), "1".to_string());
    b.insert("beta".to_string(), "2".to_string());

    let (id_a, _) = serialize(&WithMap { table: a }).expect("serialize should succeed");
    let (id_b, _) = serialize(&WithMap { table: b }).expect("serialize should succeed");
    assert_eq!(
        id_a, id_b,
        "maps with identical pairs must serialize identically regardless of insertion order"
    );
}

/// A map keyed by a smart pointer to a scalar (`Arc<str>`) is name-keyed by
/// the textual form of the *collapsed* key shape (`str`'s own textual form),
/// not treated as composite merely because the key's own static shape is
/// `Def::Pointer`. This is the encoder-side half of the map-key transparency
/// collapse: `schema_of::<HashMap<Arc<str>, u32>>()` classifies the same key
/// scalar (covered in `schema_driven.rs`), and both sides must agree on what
/// actually got written.
#[test]
fn map_with_smart_pointer_scalar_keys_is_name_keyed() {
    let mut table: HashMap<Arc<str>, u32> = HashMap::new();
    table.insert(Arc::from("hello"), 5);

    let (root_id, store) = serialize(&WithArcStrKeyMap {
        table: table.clone(),
    })
    .expect("serialize should succeed");

    let (mode, map_id) = get_tree_entry_mode(&store, &root_id, "table");
    assert_eq!(mode, EntryKind::Tree, "Map field must be a tree");

    // A composite-keyed layout would name this entry "0000" and point at a
    // `{k, v}` pair sub-tree; a name-keyed layout points straight at the value.
    let (vmode, v_id) = get_tree_entry_mode(&store, &map_id, "hello");
    assert_eq!(
        vmode,
        EntryKind::Blob,
        "an Arc<str> key must be name-keyed by its collapsed scalar's textual \
         form, not wrapped in a {{k, v}} pair sub-tree"
    );
    assert_eq!(
        store.get_blob(&v_id).expect("value blob in store"),
        b"5",
        "name-keyed entry must resolve directly to the value"
    );

    let got: WithArcStrKeyMap = deserialize(&root_id, &store).expect("deserialize should succeed");
    assert_eq!(got.table, table, "Arc<str>-keyed map must round-trip");
}

// --- composite map keys ---

/// A map with composite (struct) keys records each pair as a `{ k, v }` sub-tree.
///
/// Per spec serialization.design.trees.collections item 2b, composite keys have no
/// faithful textual form, so the map's entries are ordinal-named and point at a
/// two-entry sub-tree carrying the independently-encoded key and value.
#[test]
fn map_with_composite_keys_uses_pair_subtrees() {
    let mut table = HashMap::new();
    table.insert(Coord { x: 1, y: 2 }, "a".to_string());

    let (root_id, store) = serialize(&WithCompositeKeyMap { table }).expect("serialize");

    let (mode, map_id) = get_tree_entry_mode(&store, &root_id, "table");
    assert_eq!(mode, EntryKind::Tree, "Map field must be a tree");

    let pairs = tree_entries(&store, &map_id);
    assert_eq!(pairs.len(), 1, "one pair entry expected");
    assert_eq!(pairs[0].filename, "0000", "pair entries are ordinal-named");

    let (kmode, k_id) = get_tree_entry_mode(&store, &pairs[0].oid, "k");
    let (vmode, v_id) = get_tree_entry_mode(&store, &pairs[0].oid, "v");
    assert_eq!(kmode, EntryKind::Tree, "struct key encodes to a sub-tree");
    assert_eq!(vmode, EntryKind::Blob, "string value is a leaf blob");
    assert_eq!(
        store.get_blob(&v_id).expect("value blob"),
        b"a",
        "value sub-entry resolves to the value"
    );
    let (_, x_id) = get_tree_entry_mode(&store, &k_id, "x");
    assert_eq!(
        store.get_blob(&x_id).expect("x field blob"),
        b"1",
        "key sub-tree carries the struct fields"
    );
}

/// A composite-keyed map round-trips, and insertion order does not affect identity.
#[test]
fn map_with_composite_keys_roundtrips_order_independently() {
    let mut a = HashMap::new();
    a.insert(Coord { x: 1, y: 2 }, "a".to_string());
    a.insert(Coord { x: 3, y: 4 }, "b".to_string());

    let mut b = HashMap::new();
    b.insert(Coord { x: 3, y: 4 }, "b".to_string());
    b.insert(Coord { x: 1, y: 2 }, "a".to_string());

    let (id_a, store) = serialize(&WithCompositeKeyMap { table: a.clone() }).expect("serialize");
    let (id_b, _) = serialize(&WithCompositeKeyMap { table: b }).expect("serialize");
    assert_eq!(
        id_a, id_b,
        "composite-keyed maps must be content-addressed independent of insertion order"
    );

    let got: WithCompositeKeyMap = deserialize(&id_a, &store).expect("deserialize");
    assert_eq!(got.table, a, "composite-keyed map must round-trip");
}
