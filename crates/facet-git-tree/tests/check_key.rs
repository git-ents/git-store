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
