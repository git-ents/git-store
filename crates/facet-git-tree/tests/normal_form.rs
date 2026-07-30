//! The identity normal form: the frozen mapping's bytes, the type-universe
//! check, and the `#[facet(facet_git_tree::identity_key)]` marker that says
//! which subtree the check applies to.

use std::collections::BTreeMap;

use facet::Facet;
use facet_git_tree::normal_form::{self, Key, NormalForm};
use facet_git_tree::{
    IDENTITY_DEF_PREFIX, Node, ObjectId, UniverseError, check_identity_subtrees, check_universe,
    identity_subtrees, schema_of, serialize,
};

/// An anchor identity: the three non-derivable coordinates, all in the
/// universe.
#[derive(Facet)]
#[facet(facet_git_tree::identity_key)]
struct Identity {
    genesis_rev: [u8; 20],
    path: String,
    span: (u64, u64),
}

#[derive(Facet)]
struct Binding {
    identity: Identity,
    hints: Hints,
}

#[derive(Facet)]
struct Hints {
    fingerprints: Vec<String>,
    descriptor: Option<String>,
}

/// A key subtree marked on the field rather than on the type, holding a value
/// the universe excludes.
#[derive(Facet)]
struct Action {
    #[facet(facet_git_tree::identity_key)]
    key: ActionKey,
}

#[derive(Facet)]
struct ActionKey {
    executor: String,
    params: Params,
}

#[derive(Facet)]
#[repr(u8)]
enum Params {
    None,
    Diff,
}

fn identity() -> NormalForm {
    NormalForm::Struct(BTreeMap::from([
        (
            "genesis_rev".to_owned(),
            NormalForm::Hash(
                ObjectId::from_hex(b"1111111111111111111111111111111111111111").unwrap(),
            ),
        ),
        (
            "path".to_owned(),
            NormalForm::Str("src/refdb.rs".to_owned()),
        ),
        (
            "span".to_owned(),
            NormalForm::List(vec![NormalForm::U64(4180), NormalForm::U64(4630)]),
        ),
    ]))
}

/// The frozen mapping is a change detector on purpose: this hash is a
/// published identity, so a mapping change must break loudly here rather than
/// silently re-home every anchor id in existence.
#[test]
fn identity_hash_is_frozen() {
    let (oid, _store) = normal_form::hash(&identity()).unwrap();
    assert_eq!(oid.to_string(), "8e50840508db65f7296d8cbedaee54a042efe948");
}

/// Likewise for one value of each scalar family, plus the empty composite.
#[test]
fn scalar_hashes_are_frozen() {
    let cases: [(NormalForm, &str); 8] = [
        (
            NormalForm::Bool(true),
            "6b2aaa7640726588bcd3d57e1de4b1315b7f315e",
        ),
        (
            NormalForm::U32(4630),
            "22866326a657064681eda6590703ab22e510f93f",
        ),
        (
            NormalForm::I64(-1),
            "8663f7de5dd7a79b04f7fcc4fea0046e2659a1a1",
        ),
        (
            NormalForm::F64(1.5),
            "a1c26dc1fcd2a8afd2f6e90e3edf3b85a16213cc",
        ),
        (
            NormalForm::Char('é'),
            "4b04fff51468d8ab5201ab02b725dc477bc7cb45",
        ),
        (
            NormalForm::Str("src/refdb.rs".to_owned()),
            "913276d4b9c715e40a79ed2244b28a26c4cb43e6",
        ),
        (
            NormalForm::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
            "7d174b13ea69e384bbc26af0ff1119aafc9abfb7",
        ),
        (
            NormalForm::List(Vec::new()),
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904",
        ),
    ];
    for (value, expected) in cases {
        let (oid, _store) = normal_form::hash(&value).unwrap();
        assert_eq!(oid.to_string(), expected, "for {value:?}");
    }
}

/// Leaf blobs carry the frozen bytes themselves, with no trailing newline —
/// the general codec's newline is a readability affordance of a format that is
/// still free to change.
#[test]
fn leaf_blobs_hold_exactly_the_frozen_bytes() {
    let (oid, store) = normal_form::hash(&NormalForm::U32(4630)).unwrap();
    assert_eq!(store.get_blob(&oid).unwrap(), 4630u32.to_be_bytes());

    let (oid, store) = normal_form::hash(&NormalForm::Bool(false)).unwrap();
    assert_eq!(store.get_blob(&oid).unwrap(), [0x00]);

    let (oid, store) = normal_form::hash(&NormalForm::Str("ab".to_owned())).unwrap();
    assert_eq!(store.get_blob(&oid).unwrap(), b"ab");
}

/// List entries are named by eight-digit ordinal; struct entries by field
/// name; map entries by the key's name form.
#[test]
fn composite_entry_names_are_frozen() {
    let (oid, store) = normal_form::hash(&NormalForm::List(vec![
        NormalForm::U8(0),
        NormalForm::U8(1),
    ]))
    .unwrap();
    let names: Vec<String> = store
        .get_tree(&oid)
        .unwrap()
        .iter()
        .map(|entry| entry.filename.to_string())
        .collect();
    assert_eq!(names, ["00000000", "00000001"]);

    let (oid, store) = normal_form::hash(&NormalForm::Map(BTreeMap::from([
        (Key::U16(7), NormalForm::U8(0)),
        (Key::U16(11), NormalForm::U8(1)),
    ])))
    .unwrap();
    let names: Vec<String> = store
        .get_tree(&oid)
        .unwrap()
        .iter()
        .map(|entry| entry.filename.to_string())
        .collect();
    assert_eq!(names, ["11", "7"]);
}

/// A key whose name form is not a usable path segment is refused, rather than
/// producing a tree git could not hold.
#[test]
fn unusable_map_key_names_are_refused() {
    for key in [Key::Str(String::new()), Key::Str("a/b".to_owned())] {
        let value = NormalForm::Map(BTreeMap::from([(key, NormalForm::U8(0))]));
        assert!(matches!(
            normal_form::hash(&value),
            Err(facet_git_tree::NormalFormError::InvalidKey { .. })
        ));
    }
}

#[test]
fn an_identity_shaped_schema_is_in_the_universe() {
    let doc = schema_of::<Identity>().unwrap();
    check_universe(&doc.root, &doc.defs).unwrap();
    check_identity_subtrees(&doc).unwrap();
}

/// The check names the field path and the node variant that left the universe.
#[test]
fn an_enum_leaves_the_universe() {
    let doc = schema_of::<Action>().unwrap();
    let err = check_identity_subtrees(&doc).unwrap_err();
    let UniverseError::Excluded { path, found } = err else {
        panic!("expected an exclusion, got {err}");
    };
    assert_eq!(found, "Enum");
    assert!(path.ends_with(".params"), "{path}");
}

/// `Option` is excluded: a coordinate that may be absent is a different
/// identity, not the same one with a hole.
#[test]
fn options_and_dynamic_nodes_are_excluded() {
    let doc = schema_of::<Hints>().unwrap();
    let excluded = |node: &Node| match check_universe(node, &doc.defs) {
        Err(UniverseError::Excluded { found, .. }) => found,
        other => panic!("expected an exclusion, got {other:?}"),
    };
    assert_eq!(excluded(&doc.root), "Optional");
    assert_eq!(excluded(&Node::Dynamic), "Dynamic");
    assert_eq!(excluded(&Node::RawTree), "RawTree");
    assert_eq!(excluded(&Node::Unit), "Unit");
    assert_eq!(excluded(&Node::USize), "USize");
    assert_eq!(
        excluded(&Node::Map {
            key: Box::new(Node::Unit),
            value: Box::new(Node::U8),
        }),
        "Unit"
    );
}

/// A reference with no definition is reported as such, not silently accepted.
#[test]
fn a_dangling_ref_is_reported() {
    let err = check_universe(&Node::Ref("Nope".to_owned()), &BTreeMap::new()).unwrap_err();
    assert!(matches!(
        err,
        UniverseError::UnknownRef { ref name, .. } if name == "Nope"
    ));
}

/// The marker is compiled into the schema document as a reserved definition,
/// so it survives into whatever registers the schema.
#[test]
fn the_marker_lands_in_the_schema_document() {
    let doc = schema_of::<Binding>().unwrap();
    let marked: Vec<&str> = identity_subtrees(&doc).map(|(name, _)| name).collect();
    assert_eq!(marked, [format!("{IDENTITY_DEF_PREFIX}0").as_str()]);

    let Node::Struct(fields) = &doc.defs["Binding"] else {
        panic!("Binding is a named-field struct");
    };
    assert_eq!(
        fields["identity"].node,
        Node::Ref(format!("{IDENTITY_DEF_PREFIX}0"))
    );
}

/// A reference adds no tree level, so marking a subtree cannot move a single
/// byte of the value encoding.
#[test]
fn marking_does_not_change_the_encoding() {
    #[derive(Facet)]
    struct Unmarked {
        span: (u64, u64),
    }

    #[derive(Facet)]
    struct Marked {
        #[facet(facet_git_tree::identity_key)]
        span: (u64, u64),
    }

    let (marked, _) = serialize(&Marked { span: (1, 2) }).unwrap();
    let (unmarked, _) = serialize(&Unmarked { span: (1, 2) }).unwrap();
    assert_eq!(marked, unmarked);
}
