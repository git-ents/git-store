//! Integration tests for recursive types.
//!
//! Covers spec requirement:
//!   serialization.design.schemaless
//!     — no schema is stored, so a self-referential type cannot introduce a cycle
//!       into the encoding; the value itself is finite and cycle-free.

use facet::Facet;
use facet_git_tree::{SerializeError, serialize};

mod common;
use common::roundtrip;

/// A self-referential type: a node owns child nodes of the same type.
#[derive(Clone, Debug, Facet, PartialEq)]
struct TreeNode {
    value: i64,
    children: Vec<TreeNode>,
}

fn sample() -> TreeNode {
    TreeNode {
        value: 1,
        children: vec![
            TreeNode {
                value: 2,
                children: vec![TreeNode {
                    value: 4,
                    children: vec![],
                }],
            },
            TreeNode {
                value: 3,
                children: vec![],
            },
        ],
    }
}

/// A recursive type serializes without infinite recursion: only the finite value
/// is encoded, never the (self-referential) type definition.
#[test]
fn recursive_type_serializes() {
    let (root_id, store) = serialize(&sample()).expect("recursive type must serialize");
    assert!(
        store.get(&root_id).is_some(),
        "root id must resolve in the store"
    );
}

/// A recursive value roundtrips, preserving the whole tree of nodes.
#[test]
fn recursive_type_roundtrips() {
    assert_eq!(roundtrip(sample()), sample());
}

fn nested_tree(depth: usize) -> TreeNode {
    let mut node = TreeNode {
        value: depth as i64,
        children: vec![],
    };
    for value in (0..depth).rev() {
        node = TreeNode {
            value: value as i64,
            children: vec![node],
        };
    }
    node
}

#[test]
fn nested_collection_within_depth_limit_roundtrips() {
    let value = nested_tree(10);
    assert_eq!(roundtrip(value.clone()), value);
}

#[test]
fn nested_collection_beyond_depth_limit_is_rejected() {
    let err = serialize(&nested_tree(40)).expect_err("depth limit must reject deep values");
    assert!(matches!(err, SerializeError::MaxDepth(32)), "got {err:?}");
}
