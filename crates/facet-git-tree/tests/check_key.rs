//! Table-driven tests for [`check_key`], the write-side tree-entry-name validator.
//!
//! Runnable today: `check_key` is independent of the not-yet-implemented
//! `serialize` that is expected to call it.

use std::collections::HashMap;

use facet_git_tree::{KeyError, SerializeError, check_key, serialize};
use rstest::rstest;

mod common;
use common::WithMap;

/// Names that are valid tree entry names and so are accepted. The encoding
/// reserves no names, so leading-dot keys are ordinary data.
#[rstest]
#[case("name")]
#[case("field0")]
#[case("0001")] // a zero-padded ordinal name is a perfectly ordinary key
#[case(".env")]
#[case(".schema")] // TODO remove: no longer reserved; the encoding stores no schema
#[case(".variant")] // TODO remove: no longer reserved; enums are externally tagged, no sentinel
#[case("")] // emptiness is not `check_key`'s concern
fn accepts_valid_keys(#[case] key: &str) {
    assert!(check_key(key).is_ok(), "{key:?} should be accepted");
}

/// Keys containing the path separator are rejected as [`KeyError`], which
/// carries the offending key.
#[rstest]
#[case("a/b")]
#[case("/")]
#[case("nested/key")]
#[case("trailing/")]
fn rejects_keys_with_slash(#[case] key: &str) {
    assert!(
        matches!(check_key(key), Err(KeyError { key: k }) if k == key),
        "{key:?} should be rejected as KeyError carrying the offending key"
    );
}

/// The reserved presence-marker name (`"_"`, written for an empty collection
/// or `None`/`Null`) is rejected for the same reason `/` is: a real dynamic
/// entry named exactly that would otherwise be indistinguishable, on read,
/// from the marker.
#[test]
fn rejects_the_reserved_marker_key() {
    assert!(
        matches!(check_key("_"), Err(KeyError { key }) if key == "_"),
        "\"_\" must be rejected as the reserved presence-marker key"
    );
}

// --- integration: serialize must apply `check_key` to dynamic (map) keys ---

/// `serialize` rejects a map key containing the path separator, surfacing it as
/// [`SerializeError::Key`] rather than emitting an invalid tree entry name.
#[test]
fn serialize_rejects_map_key_with_slash() {
    let mut table = HashMap::new();
    table.insert("a/b".to_string(), "v".to_string());
    assert!(
        matches!(
            serialize(&WithMap { table }),
            Err(SerializeError::Key(KeyError { key })) if key == "a/b"
        ),
        "a map key with '/' must be rejected by serialize"
    );
}

/// `serialize` also rejects a map key equal to the reserved marker name: were
/// it allowed, a map with the single entry `{"_": v}` would be
/// indistinguishable on read from an empty map.
#[test]
fn serialize_rejects_map_key_equal_to_marker() {
    let mut table = HashMap::new();
    table.insert("_".to_string(), "v".to_string());
    assert!(
        matches!(
            serialize(&WithMap { table }),
            Err(SerializeError::Key(KeyError { key })) if key == "_"
        ),
        "a map key equal to the reserved marker must be rejected by serialize"
    );
}
