//! Integration tests for ordinal entry naming.
//!
//! Covers spec requirement:
//!   serialization.design.trees.ordering
//!     — sequence elements (Array, Vec) are named by their zero-based index as
//!       zero-padded decimal, at least four digits wide (0000, 0001, …, 0010, …)
//!     — correctness MUST NOT depend on tree-entry ordering; indices are parsed
//!       numerically on read, and collections larger than 9999 remain correct.
//!
//! (Tuple-variant ordinal naming is covered in `variants.rs`.)

use facet_git_tree::{deserialize, serialize};
use proptest::prelude::*;

mod common;
use common::{WithArray, WithVec, get_tree_entry_mode, roundtrip, tree_entries};

/// Vec elements are named by their zero-padded, zero-based index.
#[test]
fn vec_elements_named_by_zero_padded_ordinal() {
    let (root_id, store) = serialize(&WithVec {
        items: vec![10, 20, 30],
    })
    .expect("serialize ok");

    let (_, items_id) = get_tree_entry_mode(&store, &root_id, "items");
    let names: Vec<String> = tree_entries(&store, &items_id)
        .iter()
        .map(|e| e.filename.to_string())
        .collect();

    assert!(
        names.contains(&"0000".to_string()),
        "missing 0000 in {names:?}"
    );
    assert!(
        names.contains(&"0001".to_string()),
        "missing 0001 in {names:?}"
    );
    assert!(
        names.contains(&"0002".to_string()),
        "missing 0002 in {names:?}"
    );
}

/// Array elements are named by their zero-padded, zero-based index.
#[test]
fn array_elements_named_by_zero_padded_ordinal() {
    let (root_id, store) = serialize(&WithArray {
        values: [10, 20, 30, 40],
    })
    .expect("serialize ok");

    let (_, arr_id) = get_tree_entry_mode(&store, &root_id, "values");
    let names: Vec<String> = tree_entries(&store, &arr_id)
        .iter()
        .map(|e| e.filename.to_string())
        .collect();
    for expected in ["0000", "0001", "0002", "0003"] {
        assert!(
            names.contains(&expected.to_string()),
            "missing {expected} in {names:?}"
        );
    }
}

/// Ordinal names are at least four digits wide and parse as their numeric index.
#[test]
fn ordinal_names_are_at_least_four_digits() {
    let (root_id, store) = serialize(&WithVec {
        items: vec![10, 20, 30],
    })
    .expect("serialize ok");

    let (_, items_id) = get_tree_entry_mode(&store, &root_id, "items");
    for entry in tree_entries(&store, &items_id) {
        let name = entry.filename.to_string();
        assert!(
            name.len() >= 4,
            "ordinal name {name:?} must be ≥4 digits wide"
        );
        name.parse::<usize>()
            .unwrap_or_else(|_| panic!("ordinal name {name:?} must parse numerically"));
    }
}

/// A collection larger than 9999 remains correct: index 10000 needs a five-digit
/// name (`10000`), which sorts *before* `9999` lexically, so a correct roundtrip
/// proves indices are parsed numerically rather than by tree-entry order.
#[test]
fn large_vec_roundtrips_with_wide_ordinals() {
    let items: Vec<i64> = (0..=10_000).map(|i| i * 2).collect();

    // The five-digit ordinal must appear for the 10000th element.
    let (root_id, store) = serialize(&WithVec {
        items: items.clone(),
    })
    .expect("serialize ok");
    let (_, items_id) = get_tree_entry_mode(&store, &root_id, "items");
    let names: Vec<String> = tree_entries(&store, &items_id)
        .iter()
        .map(|e| e.filename.to_string())
        .collect();
    assert!(
        names.contains(&"10000".to_string()),
        "missing five-digit ordinal 10000"
    );

    // …and the values come back in numeric index order despite lexical sorting.
    let recovered = roundtrip(WithVec {
        items: items.clone(),
    });
    assert_eq!(recovered, WithVec { items });
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, .. ProptestConfig::default() })]

    #[test]
    fn sequence_ordinals_are_unique_and_cover_all_elements(items in proptest::collection::vec(-100i64..100, 1..8)) {
        let (root, store) = serialize(&WithVec { items: items.clone() }).expect("serialize");
        let (_, items_id) = get_tree_entry_mode(&store, &root, "items");
        let mut ordinals: Vec<_> = tree_entries(&store, &items_id)
            .into_iter()
            .map(|entry| entry.filename.to_string().parse::<usize>().expect("ordinal"))
            .collect();
        ordinals.sort_unstable();
        prop_assert_eq!(ordinals, (0..items.len()).collect::<Vec<_>>());
    }

    #[test]
    fn sequence_order_survives_serialization(items in proptest::collection::vec(-100i64..100, 0..8)) {
        let (root, store) = serialize(&WithVec { items: items.clone() }).expect("serialize");
        let recovered: WithVec = deserialize(&root, &store).expect("deserialize");
        prop_assert_eq!(recovered, WithVec { items });
    }

    #[test]
    fn empty_sequences_use_the_marker_tree(_unit in Just(())) {
        let (root, store) = serialize(&WithVec { items: vec![] }).expect("serialize");
        let (_, items_id) = get_tree_entry_mode(&store, &root, "items");
        let entries = tree_entries(&store, &items_id);
        prop_assert_eq!(entries.len(), 1);
        prop_assert_eq!(entries[0].filename.to_string(), "_");
        prop_assert_eq!(entries[0].mode.kind(), facet_git_tree::EntryKind::Blob);
    }
}
