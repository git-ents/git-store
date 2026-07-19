//! Shared fixtures and helpers for the integration suites.
//!
//! Each `tests/*.rs` file is its own crate, so anything common is pulled in with
//! `mod common;`. `dead_code` is allowed because no single test binary uses every
//! item here.
#![allow(dead_code)]

use std::collections::HashMap;
use std::fmt::Debug;

use facet::Facet;
use facet_git_tree::{EntryKind, ObjectId, ObjectStore, TreeEntry, deserialize, serialize};

// --- shared fixtures ---

#[derive(Debug, Facet, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Facet, PartialEq)]
pub struct Person {
    pub name: String,
    pub age: u32,
    pub active: bool,
}

#[derive(Debug, Facet, PartialEq)]
pub struct Nested {
    pub location: Point,
    pub label: String,
}

#[derive(Debug, Facet, PartialEq)]
pub struct Config {
    pub name: String,
    pub value: i64,
}

#[derive(Debug, Facet, PartialEq)]
pub struct WithVec {
    pub items: Vec<i64>,
}

#[derive(Debug, Facet, PartialEq)]
pub struct WithArray {
    pub values: [i32; 4],
}

#[derive(Debug, Facet, PartialEq)]
pub struct WithMap {
    pub table: HashMap<String, String>,
}

#[derive(Debug, Facet, PartialEq)]
pub struct WithOptional {
    pub maybe: Option<i32>,
}

/// A typed struct carrying a dynamic [`facet_value::Value`] field, for the
/// dynamic-value suites.
#[derive(Debug, Facet, PartialEq)]
pub struct WithValue {
    pub meta: facet_value::Value,
}

/// A self-referential type: a node owns child nodes of the same type. Used by
/// the schema suites to exercise `Schema::Ref` recursion.
#[derive(Debug, Facet, PartialEq)]
pub struct TreeNode {
    pub value: i64,
    pub children: Vec<TreeNode>,
}

/// An enum exercising all four variant payload kinds: unit, newtype, tuple,
/// and struct.
#[derive(Debug, Facet, PartialEq)]
#[repr(u8)]
pub enum Event {
    Ping,
    Message(String),
    Move(i32, i32),
    Login { user: String, ok: bool },
}

// --- git ground truth ---

/// The git blob OID of `b"hello"`, i.e. `printf 'hello' | git hash-object --stdin`.
pub const HELLO_BLOB_OID: [u8; 20] = [
    0xb6, 0xfc, 0x4c, 0x62, 0x0b, 0x67, 0xd9, 0x5f, 0x95, 0x3a, 0x5c, 0x1c, 0x12, 0x30, 0xaa, 0xab,
    0x5d, 0xb5, 0xa1, 0xb0,
];

// --- tree accessors ---

/// Find a named entry in a tree, panicking with a helpful message if absent.
pub fn find_entry(store: &ObjectStore, tree_id: &ObjectId, name: &str) -> TreeEntry {
    let entries = store
        .get_tree(tree_id)
        .unwrap_or_else(|| panic!("expected tree at {tree_id:?}"));
    entries
        .into_iter()
        .find(|e| e.filename == name)
        .unwrap_or_else(|| panic!("no entry named {name:?} in tree"))
}

/// The kind and target OID of a named tree entry.
pub fn get_tree_entry_mode(
    store: &ObjectStore,
    tree_id: &ObjectId,
    name: &str,
) -> (EntryKind, ObjectId) {
    let entry = find_entry(store, tree_id, name);
    (entry.mode.kind(), entry.oid)
}

/// The entries of a tree. The encoding writes no sentinel entries, so every entry
/// is data — a struct field, a collection element, a map entry, or a variant tag.
pub fn tree_entries(store: &ObjectStore, tree_id: &ObjectId) -> Vec<TreeEntry> {
    store
        .get_tree(tree_id)
        .unwrap_or_else(|| panic!("expected tree at {tree_id:?}"))
}

// --- roundtrip ---

/// `deserialize(serialize(value))`, the canonical roundtrip used across suites.
pub fn roundtrip<T>(value: T) -> T
where
    T: for<'a> Facet<'a> + PartialEq + Debug,
{
    let (root_id, store) = serialize(&value).expect("serialize should succeed");
    deserialize(&root_id, &store).expect("deserialize should succeed")
}
