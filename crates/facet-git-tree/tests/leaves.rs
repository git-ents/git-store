//! Integration tests for leaf byte encoding.
//!
//! Covers spec requirement:
//!   serialization.design.leaves.encoding
//!     — a leaf blob is the value's raw textual representation, with no
//!       delimiters or quoting, followed by exactly one trailing `\n`; a
//!       String is its UTF-8 bytes verbatim plus that one byte. The rule is
//!       "exactly one, always present" — not "at most one" — so the byte is
//!       appended even when the value's own text already ends in `\n`, and a
//!       leaf blob missing it on read is a malformed (foreign) object.

use facet::Facet;
use facet_git_tree::{
    DeserializeError, EntryKind, EntryMode, ObjectId, ObjectStore, TreeEntry, deserialize,
    serialize,
};
use gix_object::{Kind, Write};

mod common;
use common::find_entry;

// --- single-field scalar wrappers ---

#[derive(Debug, Facet)]
struct WithU32 {
    v: u32,
}

#[derive(Debug, Facet)]
struct WithI64 {
    v: i64,
}

#[derive(Debug, Facet)]
struct WithBool {
    v: bool,
}

#[derive(Debug, Facet)]
struct WithChar {
    v: char,
}

#[derive(Debug, Facet, PartialEq)]
struct WithString {
    v: String,
}

/// Serialize a single-field wrapper and return the raw bytes of its `v` leaf blob.
fn v_blob<T: for<'a> Facet<'a>>(value: &T) -> Vec<u8> {
    let (root_id, store) = serialize(value).expect("serialize should succeed");
    let entry = find_entry(&store, &root_id, "v");
    store
        .get_blob(&entry.oid)
        .expect("`v` must be a blob in store")
}

// --- integers ---

/// An unsigned integer encodes as its decimal text with no padding or sign,
/// plus the mandatory trailing newline.
#[test]

fn unsigned_integer_textual_form() {
    assert_eq!(v_blob(&WithU32 { v: 42 }), b"42\n");
}

/// A signed integer encodes as its decimal text including the minus sign,
/// plus the mandatory trailing newline.
#[test]

fn signed_integer_textual_form() {
    assert_eq!(v_blob(&WithI64 { v: -7 }), b"-7\n");
}

/// The extreme `i64::MIN` encodes exactly, with no overflow in formatting.
#[test]

fn i64_min_textual_form() {
    assert_eq!(v_blob(&WithI64 { v: i64::MIN }), b"-9223372036854775808\n");
}

// --- bool ---
//
// The spec says a scalar is stored as "the bytes of its textual form" but does
// not name bool's textual form. These tests pin it to Rust's `Display`
// (`true`/`false`) rather than `1`/`0`; change them if the intended form differs.

/// `true` encodes as the literal text `true`.
#[test]

fn bool_true_textual_form() {
    assert_eq!(v_blob(&WithBool { v: true }), b"true\n");
}

/// `false` encodes as the literal text `false`.
#[test]

fn bool_false_textual_form() {
    assert_eq!(v_blob(&WithBool { v: false }), b"false\n");
}

// --- char ---

/// An ASCII char encodes as its single byte.
#[test]

fn char_ascii_textual_form() {
    assert_eq!(v_blob(&WithChar { v: 'A' }), b"A\n");
}

/// A multi-byte char encodes as its UTF-8 bytes.
#[test]

fn char_multibyte_textual_form() {
    let mut expected = "é".as_bytes().to_vec();
    expected.push(b'\n');
    assert_eq!(v_blob(&WithChar { v: 'é' }), expected);
}

// --- string ---

/// A String is stored as its UTF-8 bytes verbatim, including non-ASCII.
#[test]

fn string_verbatim_utf8() {
    let mut expected = "héllo".as_bytes().to_vec();
    expected.push(b'\n');
    assert_eq!(
        v_blob(&WithString {
            v: "héllo".to_string()
        }),
        expected
    );
}

/// A String is not quoted or escaped, and an embedded newline is preserved as-is
/// — there is no delimiter, quoting, or escaping in a leaf blob, only the one
/// mandatory trailing newline appended after the value's own bytes.
#[test]

fn string_with_special_chars_not_quoted_or_escaped() {
    assert_eq!(
        v_blob(&WithString {
            v: "a\"b\nc".to_string()
        }),
        b"a\"b\nc\n"
    );
}

// --- the mandatory trailing newline: "exactly one, always present" ---

/// A leaf blob carries exactly one trailing newline, appended after the
/// value's own textual form.
#[test]

fn leaf_has_exactly_one_trailing_newline() {
    let bytes = v_blob(&WithString { v: "x".to_string() });
    assert_eq!(bytes, b"x\n");
}

/// The trailing newline is unconditional, not "at most one": a String whose
/// own content already ends in `\n` still gets the byte appended, producing a
/// blob with *two* trailing newlines. This is what keeps the transform
/// exactly lossless — see `already_newline_terminated_string_roundtrips`.
#[test]

fn string_already_ending_in_newline_gets_a_second_one_appended() {
    assert_eq!(
        v_blob(&WithString {
            v: "x\n".to_string()
        }),
        b"x\n\n"
    );
}

/// A String ending in `\n` round-trips exactly: the mandatory trailing byte
/// is appended on write and stripped on read, recovering the original value
/// byte for byte rather than losing (or duplicating) its own newline.
#[test]

fn already_newline_terminated_string_roundtrips() {
    let value = WithString {
        v: "x\n".to_string(),
    };
    let (root_id, store) = serialize(&value).expect("serialize");
    let back: WithString = deserialize(&root_id, &store).expect("deserialize");
    assert_eq!(back, value);
}

/// An empty String serializes to the one-byte blob `\n` — not the empty
/// blob — because the mandatory trailing newline is appended unconditionally.
/// It still round-trips to `""`.
#[test]

fn empty_string_is_a_single_newline_byte() {
    let bytes = v_blob(&WithString { v: String::new() });
    assert_eq!(bytes, b"\n");

    let value = WithString { v: String::new() };
    let (root_id, store) = serialize(&value).expect("serialize");
    let back: WithString = deserialize(&root_id, &store).expect("deserialize");
    assert_eq!(back, value);
}

/// Before the mandatory-trailing-newline rule, an empty `String` and the
/// [presence marker](facet_git_tree) blob were both the literal empty blob
/// and therefore the *same* object. Now that every leaf blob (including an
/// empty String's) carries the trailing byte while the marker does not, the
/// two are distinguishable: the empty String's blob is a distinct object
/// from the well-known empty blob the marker uses.
#[test]

fn empty_string_blob_is_distinct_from_the_marker_blob() {
    let entry = v_blob_entry(&WithString { v: String::new() });
    // Git's well-known empty-blob object id (`git hash-object --stdin < /dev/null`).
    let empty_blob_oid =
        ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").expect("valid oid");
    assert_ne!(
        entry, empty_blob_oid,
        "an empty String's blob must no longer collide with the empty blob \
         the presence marker uses"
    );
}

fn v_blob_entry<T: for<'a> Facet<'a>>(value: &T) -> ObjectId {
    let (root_id, store) = serialize(value).expect("serialize should succeed");
    find_entry(&store, &root_id, "v").oid
}

/// A leaf blob whose final byte is not `\n` is malformed foreign input and is
/// rejected with a clean typed error (`MissingLeafNewline`), never silently
/// accepted as though the trailing byte were merely optional.
#[test]

fn leaf_blob_without_trailing_newline_is_rejected() {
    let store = ObjectStore::default();
    let blob_id = store.write_buf(Kind::Blob, b"42").expect("write blob");
    let tree_id = store
        .write(&gix_object::Tree {
            entries: vec![TreeEntry {
                mode: EntryMode::from(EntryKind::Blob),
                filename: "v".into(),
                oid: blob_id,
            }],
        })
        .expect("write tree");

    let result: Result<WithU32, _> = deserialize(&tree_id, &store);
    assert!(
        matches!(&result, Err(DeserializeError::MissingLeafNewline(oid)) if *oid == blob_id),
        "a leaf blob without its trailing newline must be MissingLeafNewline, got {result:?}"
    );
}
