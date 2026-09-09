use facet_value::{VArray, VObject, Value, value};
use git_prolly::ProllyStore;
use gix::objs::Write as _;
use gix::objs::tree::EntryKind;

mod common;
use common::{TestRepo, repo};

#[test]
fn dynamic_json_values_round_trip_with_tags() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let mut array = VArray::new();
    array.push(Value::NULL);
    array.push(false);
    array.push(42_i64);
    array.push("text");
    let mut object = VObject::new();
    object.insert("nested", Value::from(array));
    object.insert("empty_array", Value::from(VArray::new()));
    object.insert("empty_object", Value::from(VObject::new()));
    let expected = Value::from(object);
    let root = store.insert(None, b"row", &expected).expect("insert");
    assert_eq!(store.get(root, b"row").expect("get"), Some(expected));
}

#[test]
fn value_root_is_structural_tree() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let value = value!({ "unchanged": ["x", "y"], "changed": { "n": 1 } });
    let root = store.insert(None, b"row", &value).expect("insert");
    let value_oid = store.get_oid(root, b"row").expect("oid").expect("row");
    assert_eq!(
        repo.try_find_header(value_oid)
            .expect("header")
            .expect("object")
            .kind(),
        gix::objs::Kind::Tree
    );
    let value_tree = repo.find_tree(value_oid).expect("value tree");
    let tree = value_tree.decode().expect("decode");
    assert!(
        tree.entries
            .iter()
            .any(|entry| entry.filename == "Array" || entry.filename == "Object")
    );
}

#[test]
fn insert_value_object_keeps_existing_object() {
    let TestRepo { _dir, repo } = repo();
    let store = ProllyStore::open(&repo);
    let blob = repo
        .write_buf(gix::objs::Kind::Blob, b"external")
        .expect("blob");
    let root = store
        .insert_value_object(None, b"row", blob)
        .expect("insert");
    assert_eq!(store.get_oid(root, b"row").expect("oid"), Some(blob));
    let leaf = repo.find_tree(root).expect("leaf");
    let tree = leaf.decode().expect("decode");
    assert_eq!(tree.entries[0].mode.kind(), EntryKind::Blob);
}
