use facet::Facet;
use facet_git_tree::{EntryKind, deserialize, serialize};
use proptest::prelude::*;
use rstest::rstest;

mod common;
use common::{find_entry, roundtrip};

#[derive(Debug, Facet, PartialEq, Clone)]
struct AllScalars {
    boolean: bool,
    character: char,
    text: String,
    i8_value: i8,
    i16_value: i16,
    i32_value: i32,
    i64_value: i64,
    i128_value: i128,
    isize_value: isize,
    u8_value: u8,
    u16_value: u16,
    u32_value: u32,
    u64_value: u64,
    u128_value: u128,
    usize_value: usize,
    f32_value: f32,
    f64_value: f64,
}

#[derive(Debug, Facet, PartialEq)]
struct WithBool {
    value: bool,
}

#[derive(Debug, Facet, PartialEq)]
struct WithChar {
    value: char,
}

#[derive(Debug, Facet, PartialEq)]
struct WithString {
    value: String,
}

#[derive(Debug, Facet, PartialEq)]
struct WithI8 {
    value: i8,
}

#[derive(Debug, Facet, PartialEq)]
struct WithI16 {
    value: i16,
}

#[derive(Debug, Facet, PartialEq)]
struct WithI32 {
    value: i32,
}

#[derive(Debug, Facet, PartialEq)]
struct WithI64 {
    value: i64,
}

#[derive(Debug, Facet, PartialEq)]
struct WithI128 {
    value: i128,
}

#[derive(Debug, Facet, PartialEq)]
struct WithIsize {
    value: isize,
}

#[derive(Debug, Facet, PartialEq)]
struct WithU8 {
    value: u8,
}

#[derive(Debug, Facet, PartialEq)]
struct WithU16 {
    value: u16,
}

#[derive(Debug, Facet, PartialEq)]
struct WithU32 {
    value: u32,
}

#[derive(Debug, Facet, PartialEq)]
struct WithU64 {
    value: u64,
}

#[derive(Debug, Facet, PartialEq)]
struct WithU128 {
    value: u128,
}

#[derive(Debug, Facet, PartialEq)]
struct WithUsize {
    value: usize,
}

#[derive(Debug, Facet, PartialEq)]
struct WithF32 {
    value: f32,
}

#[derive(Debug, Facet, PartialEq)]
struct WithF64 {
    value: f64,
}

#[test]
fn every_supported_scalar_roundtrips() {
    let value = AllScalars {
        boolean: true,
        character: 'Ω',
        text: "scalar".into(),
        i8_value: -8,
        i16_value: -16,
        i32_value: -32,
        i64_value: -64,
        i128_value: -128,
        isize_value: -1,
        u8_value: 8,
        u16_value: 16,
        u32_value: 32,
        u64_value: 64,
        u128_value: 128,
        usize_value: 1,
        f32_value: 3.25,
        f64_value: -6.5,
    };
    assert_eq!(roundtrip(value.clone()), value);
}

#[test]
fn scalar_leaves_have_one_trailing_newline() {
    let value = AllScalars {
        boolean: false,
        character: 'x',
        text: "text".into(),
        i8_value: -1,
        i16_value: -1,
        i32_value: -1,
        i64_value: -1,
        i128_value: -1,
        isize_value: -1,
        u8_value: 1,
        u16_value: 1,
        u32_value: 1,
        u64_value: 1,
        u128_value: 1,
        usize_value: 1,
        f32_value: 1.0,
        f64_value: 1.0,
    };
    let (root, store) = serialize(&value).expect("serialize");
    for entry in store.get_tree(&root).expect("root tree") {
        assert_eq!(entry.mode.kind(), EntryKind::Blob);
        let blob = store.get_blob(&entry.oid).expect("leaf blob");
        assert_eq!(blob.last(), Some(&b'\n'), "{}", entry.filename);
        assert_ne!(
            blob.get(blob.len().saturating_sub(2)),
            Some(&b'\n'),
            "{}",
            entry.filename
        );
    }
}

#[rstest]
#[case("")]
#[case("x")]
#[case("x\n")]
#[case("x\n\n")]
fn trailing_newlines_remain_lossless(#[case] text: &str) {
    let value = WithString { value: text.into() };
    let (root, store) = serialize(&value).expect("serialize");
    let entry = find_entry(&store, &root, "value");
    let blob = store.get_blob(&entry.oid).expect("string blob");
    assert_eq!(blob.last(), Some(&b'\n'));
    let back: WithString = deserialize(&root, &store).expect("deserialize");
    assert_eq!(back, value);
}

#[test]
fn empty_byte_sequence_is_distinct_from_presence_marker() {
    #[derive(Debug, Facet, PartialEq)]
    struct WithBytes {
        value: Vec<u8>,
    }
    #[derive(Debug, Facet)]
    struct WithEmptyVec {
        value: Vec<u16>,
    }

    let (bytes_root, bytes_store) = serialize(&WithBytes { value: vec![] }).expect("serialize");
    let (marker_root, marker_store) =
        serialize(&WithEmptyVec { value: vec![] }).expect("serialize");
    let bytes_entry = find_entry(&bytes_store, &bytes_root, "value");
    let marker_entry = find_entry(&marker_store, &marker_root, "value");
    assert_eq!(bytes_entry.mode.kind(), EntryKind::Blob);
    assert_eq!(marker_entry.mode.kind(), EntryKind::Tree);
    assert_ne!(bytes_entry.oid, marker_entry.oid);
}

#[test]
fn equal_scalars_have_equal_root_ids() {
    let (first, _) = serialize(&WithI128 { value: -123456789 }).expect("serialize");
    let (second, _) = serialize(&WithI128 { value: -123456789 }).expect("serialize");
    assert_eq!(first, second);
}

#[test]
fn integer_boundaries_roundtrip() {
    assert_eq!(
        roundtrip(WithI8 { value: i8::MIN }),
        WithI8 { value: i8::MIN }
    );
    assert_eq!(
        roundtrip(WithI8 { value: i8::MAX }),
        WithI8 { value: i8::MAX }
    );
    assert_eq!(roundtrip(WithI8 { value: 0 }), WithI8 { value: 0 });
    assert_eq!(
        roundtrip(WithI16 { value: i16::MIN }),
        WithI16 { value: i16::MIN }
    );
    assert_eq!(
        roundtrip(WithI16 { value: i16::MAX }),
        WithI16 { value: i16::MAX }
    );
    assert_eq!(roundtrip(WithI16 { value: 0 }), WithI16 { value: 0 });
    assert_eq!(
        roundtrip(WithI32 { value: i32::MIN }),
        WithI32 { value: i32::MIN }
    );
    assert_eq!(
        roundtrip(WithI32 { value: i32::MAX }),
        WithI32 { value: i32::MAX }
    );
    assert_eq!(roundtrip(WithI32 { value: 0 }), WithI32 { value: 0 });
    assert_eq!(
        roundtrip(WithI64 { value: i64::MIN }),
        WithI64 { value: i64::MIN }
    );
    assert_eq!(
        roundtrip(WithI64 { value: i64::MAX }),
        WithI64 { value: i64::MAX }
    );
    assert_eq!(roundtrip(WithI64 { value: 0 }), WithI64 { value: 0 });
    assert_eq!(
        roundtrip(WithI128 { value: i128::MIN }),
        WithI128 { value: i128::MIN }
    );
    assert_eq!(
        roundtrip(WithI128 { value: i128::MAX }),
        WithI128 { value: i128::MAX }
    );
    assert_eq!(roundtrip(WithI128 { value: 0 }), WithI128 { value: 0 });
    assert_eq!(
        roundtrip(WithIsize { value: isize::MIN }),
        WithIsize { value: isize::MIN }
    );
    assert_eq!(
        roundtrip(WithIsize { value: isize::MAX }),
        WithIsize { value: isize::MAX }
    );
    assert_eq!(roundtrip(WithIsize { value: 0 }), WithIsize { value: 0 });
    assert_eq!(
        roundtrip(WithU8 { value: u8::MIN }),
        WithU8 { value: u8::MIN }
    );
    assert_eq!(
        roundtrip(WithU8 { value: u8::MAX }),
        WithU8 { value: u8::MAX }
    );
    assert_eq!(
        roundtrip(WithU16 { value: u16::MIN }),
        WithU16 { value: u16::MIN }
    );
    assert_eq!(
        roundtrip(WithU16 { value: u16::MAX }),
        WithU16 { value: u16::MAX }
    );
    assert_eq!(
        roundtrip(WithU32 { value: u32::MIN }),
        WithU32 { value: u32::MIN }
    );
    assert_eq!(
        roundtrip(WithU32 { value: u32::MAX }),
        WithU32 { value: u32::MAX }
    );
    assert_eq!(
        roundtrip(WithU64 { value: u64::MIN }),
        WithU64 { value: u64::MIN }
    );
    assert_eq!(
        roundtrip(WithU64 { value: u64::MAX }),
        WithU64 { value: u64::MAX }
    );
    assert_eq!(
        roundtrip(WithU128 { value: u128::MIN }),
        WithU128 { value: u128::MIN }
    );
    assert_eq!(
        roundtrip(WithU128 { value: u128::MAX }),
        WithU128 { value: u128::MAX }
    );
    assert_eq!(
        roundtrip(WithUsize { value: usize::MIN }),
        WithUsize { value: usize::MIN }
    );
    assert_eq!(
        roundtrip(WithUsize { value: usize::MAX }),
        WithUsize { value: usize::MAX }
    );
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, .. ProptestConfig::default() })]

    #[test]
    fn signed_integers_roundtrip(v in any::<i8>()) { prop_assert_eq!(roundtrip(WithI8 { value: v }), WithI8 { value: v }); }
    #[test]
    fn signed_wider_integers_roundtrip(v in any::<i16>()) { prop_assert_eq!(roundtrip(WithI16 { value: v }), WithI16 { value: v }); }
    #[test]
    fn signed_i32_roundtrip(v in any::<i32>()) { prop_assert_eq!(roundtrip(WithI32 { value: v }), WithI32 { value: v }); }
    #[test]
    fn signed_i64_roundtrip(v in any::<i64>()) { prop_assert_eq!(roundtrip(WithI64 { value: v }), WithI64 { value: v }); }
    #[test]
    fn signed_i128_roundtrip(v in any::<i128>()) { prop_assert_eq!(roundtrip(WithI128 { value: v }), WithI128 { value: v }); }
    #[test]
    fn signed_isize_roundtrip(v in any::<isize>()) { prop_assert_eq!(roundtrip(WithIsize { value: v }), WithIsize { value: v }); }
    #[test]
    fn unsigned_u8_roundtrip(v in any::<u8>()) { prop_assert_eq!(roundtrip(WithU8 { value: v }), WithU8 { value: v }); }
    #[test]
    fn unsigned_u16_roundtrip(v in any::<u16>()) { prop_assert_eq!(roundtrip(WithU16 { value: v }), WithU16 { value: v }); }
    #[test]
    fn unsigned_u32_roundtrip(v in any::<u32>()) { prop_assert_eq!(roundtrip(WithU32 { value: v }), WithU32 { value: v }); }
    #[test]
    fn unsigned_u64_roundtrip(v in any::<u64>()) { prop_assert_eq!(roundtrip(WithU64 { value: v }), WithU64 { value: v }); }
    #[test]
    fn unsigned_u128_roundtrip(v in any::<u128>()) { prop_assert_eq!(roundtrip(WithU128 { value: v }), WithU128 { value: v }); }
    #[test]
    fn unsigned_usize_roundtrip(v in any::<usize>()) { prop_assert_eq!(roundtrip(WithUsize { value: v }), WithUsize { value: v }); }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, .. ProptestConfig::default() })]

    #[test]
    fn finite_f32_roundtrips(v in any::<f32>().prop_filter("finite", |v| v.is_finite())) {
        prop_assert_eq!(roundtrip(WithF32 { value: v }), WithF32 { value: v });
    }
    #[test]
    fn finite_f64_roundtrips(v in any::<f64>().prop_filter("finite", |v| v.is_finite())) {
        prop_assert_eq!(roundtrip(WithF64 { value: v }), WithF64 { value: v });
    }
}

#[test]
fn float_special_values_use_canonical_semantics() {
    let (pos_zero, _) = serialize(&WithF64 { value: 0.0 }).expect("serialize");
    let (neg_zero, _) = serialize(&WithF64 { value: -0.0 }).expect("serialize");
    assert_eq!(pos_zero, neg_zero);

    for value in [f32::INFINITY, f32::NEG_INFINITY] {
        let back: WithF32 = roundtrip(WithF32 { value });
        assert_eq!(back.value, value);
    }
    for value in [f64::INFINITY, f64::NEG_INFINITY] {
        let back: WithF64 = roundtrip(WithF64 { value });
        assert_eq!(back.value, value);
    }

    let nan32: WithF32 = roundtrip(WithF32 { value: f32::NAN });
    let nan64: WithF64 = roundtrip(WithF64 { value: f64::NAN });
    assert!(nan32.value.is_nan());
    assert!(nan64.value.is_nan());
}

#[test]
fn vec_u8_is_one_leaf_blob() {
    #[derive(Debug, Facet)]
    struct WithBytes {
        value: Vec<u8>,
    }
    let bytes = vec![0, 1, 2, 255];
    let (root, store) = serialize(&WithBytes {
        value: bytes.clone(),
    })
    .expect("serialize");
    let entry = find_entry(&store, &root, "value");
    assert_eq!(entry.mode.kind(), EntryKind::Blob);
    let mut expected = bytes;
    expected.push(b'\n');
    assert_eq!(store.get_blob(&entry.oid).expect("blob"), expected);
}
