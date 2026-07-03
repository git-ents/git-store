//! Integration tests for `#[facet(transparent)]` newtype encoding.
//!
//! Covers spec requirements:
//!   serialization.design.pointers — a transparent wrapper encodes as its inner value
//!   deserialization.roundtrip     — deserialize(serialize(x)) must equal x

use facet::Facet;
use facet_git_tree::serialize;

mod common;
use common::roundtrip;

#[derive(Debug, Clone, PartialEq, Facet)]
#[facet(transparent)]
struct Hex(String);

#[derive(Debug, PartialEq, Facet)]
struct WithHex {
    commit: Hex,
}

#[derive(Debug, PartialEq, Facet)]
struct WithString {
    commit: String,
}

/// A transparent newtype serializes exactly as its inner value would — no
/// extra tree layer for the wrapper.
#[test]
fn transparent_newtype_matches_inner_scalar() {
    let a = WithString {
        commit: "0123abcd".to_owned(),
    };
    let b = WithHex {
        commit: Hex("0123abcd".to_owned()),
    };
    let (oid_a, _) = serialize(&a).expect("serialize");
    let (oid_b, _) = serialize(&b).expect("serialize");
    assert_eq!(oid_a, oid_b);
}

/// A transparent newtype roundtrips, including as a bare root value.
#[test]
fn transparent_newtype_roundtrips() {
    assert_eq!(
        roundtrip(Hex("deadbeef".to_owned())),
        Hex("deadbeef".to_owned())
    );
    let with = WithHex {
        commit: Hex("cafef00d".to_owned()),
    };
    assert_eq!(
        roundtrip(with),
        WithHex {
            commit: Hex("cafef00d".to_owned())
        }
    );
}
