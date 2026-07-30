//! Integration tests for the migration lens vocabulary.
//!
//! Covers:
//!   - `Migration`/`Op`/`Target`/`Change`/`Constant` are self-hosted: an
//!     ordinary `Facet` value describable by `schema_of` and roundtripping
//!     through this crate's own tree encoding, exactly like `Schema`.
//!   - `schema_and_hints_of` collects `#[facet(migrate::renamed_from = …)]`
//!     hints from named struct fields and struct enum variant fields, keyed
//!     by the right `Target`.
//!   - A type with no such attributes yields empty `Hints`, and the hint
//!     collection refactor leaves `schema_of`'s output unchanged.

use std::collections::BTreeMap;

use facet::Facet;
use facet_git_tree::{
    Change, Constant, Hints, Migration, Node, Op, Schema, StructField, Target, schema_and_hints_of,
    schema_of,
};

mod common;
use common::{Person, roundtrip};

// --- self-hosting ---

/// `Migration` is itself describable by a schema, exactly like `Schema`.
#[test]
fn migration_schema_of_succeeds() -> anyhow::Result<()> {
    schema_of::<Migration>()?;
    Ok(())
}

/// A `Migration` holding one of each `Change` variant, and a `Constant` of
/// every variant (including nested `List`/`Object`), roundtrips through this
/// crate's own encoding.
#[test]
fn migration_roundtrip() {
    let value = Migration {
        ops: vec![
            Op {
                at: Target::Def("Issue".into()),
                change: Change::Rename {
                    from: "old_id".into(),
                    to: "id".into(),
                },
            },
            Op {
                at: Target::Def("Issue".into()),
                change: Change::Add {
                    field: "meta".into(),
                    default: Constant::Object(BTreeMap::from([
                        ("null".to_string(), Constant::Null),
                        ("bool".to_string(), Constant::Bool(true)),
                        ("int".to_string(), Constant::Integer(-7)),
                        ("float".to_string(), Constant::Float(1.5)),
                        ("text".to_string(), Constant::Text("hi".into())),
                        (
                            "list".to_string(),
                            Constant::List(vec![
                                Constant::Integer(1),
                                Constant::Text("two".into()),
                                Constant::Bool(false),
                                Constant::Null,
                            ]),
                        ),
                    ])),
                },
            },
            Op {
                at: Target::Variant {
                    def: "Event".into(),
                    variant: "Login".into(),
                },
                change: Change::Remove {
                    field: "legacy_flag".into(),
                },
            },
            Op {
                at: Target::Variant {
                    def: "Event".into(),
                    variant: "Login".into(),
                },
                change: Change::Wrap {
                    field: "nickname".into(),
                },
            },
        ],
    };
    assert_eq!(roundtrip(value.clone()), value);
}

/// An empty `Migration` (no ops) roundtrips too.
#[test]
fn empty_migration_roundtrip() {
    assert_eq!(roundtrip(Migration::default()), Migration::default());
}

// --- hint collection: named struct fields ---

#[derive(Debug, Facet)]
#[allow(dead_code)]
struct Issue {
    #[facet(facet_git_tree::renamed_from = "old_id")]
    id: String,
    title: String,
}

/// `#[facet(migrate::renamed_from = …)]` on a named struct field is collected
/// against `Target::Def`, keyed by the field's own (target-side) name; a
/// field without the attribute contributes no hint.
#[test]
fn struct_field_rename_hint_is_collected() -> anyhow::Result<()> {
    let (_doc, hints) = schema_and_hints_of::<Issue>()?;
    let target = Target::Def("Issue".into());
    assert_eq!(hints.rename_of(&target, "id"), Some("old_id"));
    assert_eq!(hints.rename_of(&target, "title"), None);
    Ok(())
}

// --- hint collection: struct enum variant fields ---

#[derive(Debug, Facet)]
#[repr(u8)]
#[allow(dead_code)]
enum Event {
    Ping,
    Login {
        #[facet(facet_git_tree::renamed_from = "user")]
        username: String,
        ok: bool,
    },
}

/// The same attribute on a struct enum variant's field is collected against
/// `Target::Variant { def, variant }`.
#[test]
fn struct_variant_field_rename_hint_is_collected() -> anyhow::Result<()> {
    let (_doc, hints) = schema_and_hints_of::<Event>()?;
    let target = Target::Variant {
        def: "Event".into(),
        variant: "Login".into(),
    };
    assert_eq!(hints.rename_of(&target, "username"), Some("user"));
    assert_eq!(hints.rename_of(&target, "ok"), None);

    // The attribute-free unit variant `Ping` contributes nothing.
    let ping_target = Target::Variant {
        def: "Event".into(),
        variant: "Ping".into(),
    };
    assert_eq!(hints.rename_of(&ping_target, "anything"), None);
    Ok(())
}

// --- no attributes, no regression ---

/// A type with no `migrate` attributes yields empty `Hints`, and
/// `schema_and_hints_of`'s schema half matches plain `schema_of` exactly —
/// the hint-collection refactor of `Walker`/`define` changes nothing about
/// the generated `Schema`.
#[test]
fn no_attributes_yields_empty_hints_and_unchanged_schema() -> anyhow::Result<()> {
    let (doc, hints) = schema_and_hints_of::<Person>()?;
    assert_eq!(hints, Hints::default());
    assert_eq!(doc, schema_of::<Person>()?);

    let expected = Schema {
        root: Node::Ref("Person".into()),
        defs: BTreeMap::from([(
            "Person".into(),
            Node::Struct(BTreeMap::from([
                (
                    "name".into(),
                    StructField {
                        node: Node::String,
                        has_default: false,
                    },
                ),
                (
                    "age".into(),
                    StructField {
                        node: Node::U32,
                        has_default: false,
                    },
                ),
                (
                    "active".into(),
                    StructField {
                        node: Node::Bool,
                        has_default: false,
                    },
                ),
            ])),
        )]),
    };
    assert_eq!(doc, expected);
    Ok(())
}
