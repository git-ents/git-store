//! Integration tests for smart-pointer encoding.
//!
//! Covers spec requirements:
//!   serialization.design.pointers — a pointer encodes as its pointee
//!   serialization.design.bytes    — `Arc<[u8]>` is stored as a single blob
//!   deserialization.roundtrip     — deserialize(serialize(x)) must equal x

use std::rc::Rc;
use std::sync::Arc;

use facet::Facet;
use facet_git_tree::{EntryKind, serialize};

mod common;
use common::{Point, roundtrip};

#[derive(Debug, Facet, PartialEq)]
struct WithArcBytes {
    blob: Arc<[u8]>,
}

// `Arc::from(&[T])` requires `T: Clone`; the shared `Point` fixture is not Clone.
#[derive(Debug, Facet, PartialEq, Clone)]
struct Pt {
    x: i32,
    y: i32,
}

// --- sized pointees roundtrip ---

/// `Box<T>` roundtrips, reconstructing the box around the decoded pointee.
#[test]
fn box_roundtrip() {
    assert_eq!(
        roundtrip(Box::new(Point { x: 1.0, y: 2.0 })),
        Box::new(Point { x: 1.0, y: 2.0 })
    );
}

/// `Rc<T>` roundtrips.
#[test]
fn rc_roundtrip() {
    assert_eq!(roundtrip(Rc::new(7_i64)), Rc::new(7_i64));
}

/// `Arc<T>` roundtrips.
#[test]
fn arc_roundtrip() {
    assert_eq!(
        roundtrip(Arc::new("hello".to_string())),
        Arc::new("hello".to_string())
    );
}

// --- transparency: a pointer encodes as its pointee ---

/// `Box`, `Rc`, and `Arc` are transparent: each serializes to the exact same
/// root object as the bare pointee value.
#[test]
fn pointer_is_transparent() {
    let value = Point { x: 3.5, y: -1.0 };
    let (bare, _) = serialize(&value).expect("serialize");
    let (boxed, _) = serialize(&Box::new(Point { x: 3.5, y: -1.0 })).expect("serialize");
    let (rced, _) = serialize(&Rc::new(Point { x: 3.5, y: -1.0 })).expect("serialize");
    let (arced, _) = serialize(&Arc::new(Point { x: 3.5, y: -1.0 })).expect("serialize");
    assert_eq!(bare, boxed);
    assert_eq!(bare, rced);
    assert_eq!(bare, arced);
}

// --- unsized slice pointees ---

/// `Arc<[u8]>` roundtrips through the slice builder.
#[test]
fn arc_byte_slice_roundtrip() {
    let bytes: Arc<[u8]> = Arc::from(b"\x00\x01binary\xff".as_slice());
    assert_eq!(roundtrip(bytes.clone()), bytes);
}

/// `Rc<[u8]>` and `Box<[u8]>` roundtrip as well.
#[test]
fn rc_and_box_byte_slice_roundtrip() {
    let rc: Rc<[u8]> = Rc::from(b"abc".as_slice());
    assert_eq!(roundtrip(rc.clone()), rc);
    let boxed: Box<[u8]> = Box::from(b"xyz".as_slice());
    assert_eq!(roundtrip(boxed.clone()), boxed);
}

/// `Arc<[u8]>` lands as a single blob, like any other byte sequence.
#[test]
fn arc_byte_slice_is_a_single_blob() {
    let bytes: &[u8] = b"\x7fELF\x02";
    let (root, store) = serialize(&WithArcBytes {
        blob: Arc::from(bytes),
    })
    .expect("serialize");
    let (kind, oid) = common::get_tree_entry_mode(&store, &root, "blob");
    assert_eq!(kind, EntryKind::Blob);
    let mut expected = bytes.to_vec();
    expected.push(b'\n');
    assert_eq!(store.get_blob(&oid).expect("blob"), expected);
}

/// A non-`u8` slice pointee (`Arc<[u32]>`) is a tree, and roundtrips with order
/// preserved.
#[test]
fn arc_u32_slice_roundtrip() {
    let values: Arc<[u32]> = Arc::from([10_u32, 20, 30].as_slice());
    let (root, store) = serialize(&values).expect("serialize");
    assert!(
        store.get_tree(&root).is_some(),
        "a non-u8 slice must be a tree, not a blob"
    );
    assert_eq!(roundtrip(values.clone()), values);
}

/// A slice of composites (`Arc<[Pt]>`) roundtrips, exercising element-as-tree
/// nesting under a pointer.
#[test]
fn arc_struct_slice_roundtrip() {
    let pts: Arc<[Pt]> = Arc::from([Pt { x: 1, y: 2 }, Pt { x: 3, y: 4 }].as_slice());
    assert_eq!(roundtrip(pts.clone()), pts);
}
