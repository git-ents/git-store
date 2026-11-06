//! `DocumentBuilder`: the every-field-required refusal, the unknown-field
//! rejection, and the non-struct refusal are the parts most easily broken by
//! future edits.

use std::collections::BTreeMap;

use facet_git_tree::{Node, Schema};
use facet_value::{Value, value};
use git_store::{DocumentBuilder, DocumentError};

fn schema() -> Schema {
    Schema {
        kind: "person".to_owned(),
        root: Node::Struct(BTreeMap::from([
            ("name".to_owned(), Node::String),
            ("age".to_owned(), Node::U32),
        ])),
        defs: BTreeMap::new(),
    }
}

#[test]
fn refuses_until_every_field_is_set() {
    let doc = schema();
    let builder = DocumentBuilder::for_schema(&doc).unwrap();
    let err = builder.build().unwrap_err();
    assert!(matches!(
        err,
        DocumentError::MissingFields { names } if names == vec!["age".to_owned(), "name".to_owned()]
    ));

    let mut builder = DocumentBuilder::for_schema(&doc).unwrap();
    builder.set("name", "Alice").unwrap();
    let err = builder.build().unwrap_err();
    assert!(matches!(
        err,
        DocumentError::MissingFields { names } if names == vec!["age".to_owned()]
    ));
}

#[test]
fn emits_every_set_field() {
    let doc = schema();
    let mut builder = DocumentBuilder::for_schema(&doc).unwrap();
    builder.set("name", "Alice").unwrap();
    builder.set("age", 30u32).unwrap();
    let built = builder.build().unwrap();
    assert_eq!(built, value!({ "name": "Alice", "age": 30 }));
}

#[test]
fn refuses_unknown_field() {
    let doc = schema();
    let mut builder = DocumentBuilder::for_schema(&doc).unwrap();
    let err = builder.set("bogus", Value::NULL).unwrap_err();
    assert!(matches!(err, DocumentError::UnknownField { name } if name == "bogus"));
}

#[test]
fn refuses_a_node_that_is_not_a_struct() {
    let doc = schema();
    let err = match DocumentBuilder::for_node(&Node::String, &doc) {
        Ok(_) => panic!("a non-struct node must be refused"),
        Err(err) => err,
    };
    assert!(matches!(err, DocumentError::NotAStruct));
}
