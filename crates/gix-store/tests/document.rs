//! `DocumentBuilder`: the required-field refusal, default application, and
//! unknown-field rejection are the parts most easily broken by future edits.

use std::collections::BTreeMap;

use facet_git_tree::{Node, Schema, StructField};
use facet_value::{Value, value};
use gix_store::{DocumentBuilder, DocumentError};

fn schema() -> Schema {
    Schema {
        root: Node::Struct(BTreeMap::from([
            (
                "name".to_owned(),
                StructField {
                    node: Node::String,
                    has_default: false,
                },
            ),
            (
                "age".to_owned(),
                StructField {
                    node: Node::U32,
                    has_default: true,
                },
            ),
        ])),
        defs: BTreeMap::new(),
    }
}

#[test]
fn refuses_missing_required_field() {
    let doc = schema();
    let builder = DocumentBuilder::for_schema(&doc).unwrap();
    let err = builder.build().unwrap_err();
    assert!(matches!(
        err,
        DocumentError::MissingFields { names } if names == vec!["name".to_owned()]
    ));
}

#[test]
fn omits_defaulted_field_left_unset() {
    let doc = schema();
    let mut builder = DocumentBuilder::for_schema(&doc).unwrap();
    builder.set("name", "Alice").unwrap();
    let built = builder.build().unwrap();
    assert_eq!(built, value!({ "name": "Alice" }));
}

#[test]
fn refuses_unknown_field() {
    let doc = schema();
    let mut builder = DocumentBuilder::for_schema(&doc).unwrap();
    let err = builder.set("bogus", Value::NULL).unwrap_err();
    assert!(matches!(err, DocumentError::UnknownField { name } if name == "bogus"));
}
