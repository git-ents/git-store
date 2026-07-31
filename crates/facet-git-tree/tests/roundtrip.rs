//! Integration tests for serialization/deserialization roundtrip.
//!
//! Covers spec requirement:
//!   deserialization.roundtrip — deserialize(serialize(x)) must equal x

use std::collections::HashMap;

use facet::Facet;
use facet_git_tree::{deserialize, serialize};
use proptest::prelude::*;

mod common;
use common::{Nested, Person, Point, WithArray, WithMap, WithOptional, WithVec, roundtrip};

// --- test types specific to nested-collection roundtrips ---

#[derive(Debug, Facet, PartialEq)]
struct VecOfPoints {
    pts: Vec<Point>,
}

#[derive(Debug, Facet, PartialEq)]
struct Matrix {
    rows: Vec<Vec<i64>>,
}

// --- tests ---

/// Primitive-field structs roundtrip exactly.
#[test]
fn point_roundtrip() {
    assert_eq!(
        roundtrip(Point { x: 1.5, y: -2.75 }),
        Point { x: 1.5, y: -2.75 }
    );
}

/// String and integer fields roundtrip exactly.
#[test]
fn person_roundtrip() {
    assert_eq!(
        roundtrip(Person {
            name: "Alice".to_string(),
            age: 42,
            active: true,
        }),
        Person {
            name: "Alice".to_string(),
            age: 42,
            active: true,
        }
    );
}

/// The empty string roundtrips without becoming None or any other value.
#[test]
fn empty_string_roundtrip() {
    assert_eq!(
        roundtrip(Person {
            name: String::new(),
            age: 0,
            active: false,
        }),
        Person {
            name: String::new(),
            age: 0,
            active: false,
        }
    );
}

/// Nested structs roundtrip without losing inner-field data.
#[test]
fn nested_struct_roundtrip() {
    assert_eq!(
        roundtrip(Nested {
            location: Point { x: 10.0, y: -5.0 },
            label: "home".to_string(),
        }),
        Nested {
            location: Point { x: 10.0, y: -5.0 },
            label: "home".to_string(),
        }
    );
}

/// Vec fields roundtrip with all elements in the same order.
#[test]
fn vec_roundtrip() {
    assert_eq!(
        roundtrip(WithVec {
            items: vec![10, 20, 30, 40],
        }),
        WithVec {
            items: vec![10, 20, 30, 40],
        }
    );
}

/// An empty Vec roundtrips as an empty Vec, not None.
#[test]
fn empty_vec_roundtrip() {
    assert_eq!(
        roundtrip(WithVec { items: vec![] }),
        WithVec { items: vec![] }
    );
}

/// Fixed-size array fields roundtrip with all elements preserved.
#[test]
fn array_roundtrip() {
    assert_eq!(
        roundtrip(WithArray {
            values: [7, 13, 42, 0]
        }),
        WithArray {
            values: [7, 13, 42, 0]
        }
    );
}

/// HashMap fields roundtrip with all key-value pairs preserved.
#[test]
fn map_roundtrip() {
    let mut table = HashMap::new();
    table.insert("alpha".to_string(), "one".to_string());
    table.insert("beta".to_string(), "two".to_string());

    let mut expected = HashMap::new();
    expected.insert("alpha".to_string(), "one".to_string());
    expected.insert("beta".to_string(), "two".to_string());

    assert_eq!(roundtrip(WithMap { table }), WithMap { table: expected });
}

/// A Vec of structs roundtrips, exercising element-as-tree nesting.
#[test]
fn vec_of_structs_roundtrip() {
    let value = VecOfPoints {
        pts: vec![Point { x: 1.0, y: 2.0 }, Point { x: -3.0, y: 4.5 }],
    };
    let expected = VecOfPoints {
        pts: vec![Point { x: 1.0, y: 2.0 }, Point { x: -3.0, y: 4.5 }],
    };
    assert_eq!(roundtrip(value), expected);
}

/// A Vec of Vecs roundtrips, exercising nested collection trees.
#[test]
fn nested_vec_roundtrip() {
    let value = Matrix {
        rows: vec![vec![1, 2, 3], vec![], vec![4, 5]],
    };
    let expected = Matrix {
        rows: vec![vec![1, 2, 3], vec![], vec![4, 5]],
    };
    assert_eq!(roundtrip(value), expected);
}

/// Option::Some roundtrips as Some.
#[test]
fn option_some_roundtrip() {
    assert_eq!(
        roundtrip(WithOptional { maybe: Some(99) }),
        WithOptional { maybe: Some(99) }
    );
}

/// Option::None roundtrips as None.
#[test]
fn option_none_roundtrip() {
    assert_eq!(
        roundtrip(WithOptional { maybe: None }),
        WithOptional { maybe: None }
    );
}

/// Positive infinity roundtrips as positive infinity.
#[test]
fn positive_infinity_roundtrip() {
    assert_eq!(
        roundtrip(Point {
            x: f64::INFINITY,
            y: 0.0
        }),
        Point {
            x: f64::INFINITY,
            y: 0.0
        }
    );
}

/// Negative infinity roundtrips as negative infinity.
#[test]
fn negative_infinity_roundtrip() {
    assert_eq!(
        roundtrip(Point {
            x: f64::NEG_INFINITY,
            y: 0.0
        }),
        Point {
            x: f64::NEG_INFINITY,
            y: 0.0
        }
    );
}

/// NaN deserializes back to NaN (roundtrip via the "nan" string representation).
#[test]
fn nan_roundtrip() {
    let (root_id, store) = serialize(&Point {
        x: f64::NAN,
        y: 0.0,
    })
    .expect("serialize ok");
    let recovered: Point = deserialize(&root_id, &store).expect("deserialize ok");
    assert!(
        recovered.x.is_nan(),
        "NaN must deserialize back to NaN, got {:?}",
        recovered.x
    );
    assert_eq!(recovered.y, 0.0);
}

/// Negative zero deserializes to a zero value (either sign — normalized on write).
#[test]
fn negative_zero_roundtrip() {
    let (root_id, store) = serialize(&Point {
        x: -0.0_f64,
        y: 0.0,
    })
    .expect("serialize ok");
    let recovered: Point = deserialize(&root_id, &store).expect("deserialize ok");
    // The spec mandates normalization of -0.0 to +0.0 on write.
    // On read, the value must be zero (the sign is unspecified but normalized form is +0.0).
    assert_eq!(recovered.x, 0.0_f64);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, .. ProptestConfig::default() })]

    #[test]
    fn bounded_nested_sequences_preserve_empty_and_non_empty_structure(
        rows in proptest::collection::vec(
            proptest::collection::vec(-20i64..20, 0..5),
            0..5,
        ),
    ) {
        let value = Matrix { rows: rows.clone() };
        let (root, store) = serialize(&value).expect("serialize");
        let recovered: Matrix = deserialize(&root, &store).expect("deserialize");
        prop_assert_eq!(recovered, value);
    }
}
