//! Integration tests for schema-diff derivation (`migration::derive::derive`).
//!
//! Covers:
//!   - Each lens the vocabulary expresses (`Add` with an author default,
//!     `Add` with `Constant::Null` for an `Optional` field, `Remove`,
//!     `Rename`, `Wrap`, and a rename composed with a wrap) is classified
//!     into a `Complete` derivation with exactly the expected op.
//!   - What the vocabulary cannot express (an undefaulted addition, a
//!     retyped field, a removed enum variant, an unpaired definition, a
//!     root-schema change, a definition-kind mismatch) is classified into a
//!     `Partial` derivation carrying the right `Divergence`, never silently
//!     dropped or guessed at.
//!   - A diff alone cannot distinguish a rename from an unrelated
//!     {remove, add} pair — only a hint can.
//!   - Definitions are addressed by name (`Target::Def`/`Target::Variant`),
//!     not by root-relative path: a definition reached through both `Vec<T>`
//!     and `Option<T>`, and a recursive type's every self-occurrence, each
//!     produce exactly one op.
//!   - `derive_to` folds in a type's declared `#[facet(migrate::renamed_from
//!     = …)]` hints.
//!   - Determinism: the same inputs always yield an equal `Migration`, whose
//!     serialization always yields the same object id.

use std::collections::BTreeMap;

use facet::Facet;
use facet_git_tree::migration::derive::{derive, derive_to};
use facet_git_tree::{
    Change, Constant, Derivation, Divergence, Hints, Migration, Node, Op, Schema, Side,
    StructField, Target, VariantKind, schema_of, serialize,
};

// --- helpers (mirroring tests/schema_gen.rs) ---

/// [`Node::Struct`] fields, all non-defaulted.
fn fields(pairs: Vec<(&str, Node)>) -> BTreeMap<String, StructField> {
    pairs
        .into_iter()
        .map(|(k, v)| {
            (
                k.into(),
                StructField {
                    node: v,
                    has_default: false,
                },
            )
        })
        .collect()
}

/// A struct enum variant's fields, which carry no default marker.
fn variant_fields(pairs: Vec<(&str, Node)>) -> BTreeMap<String, Node> {
    pairs.into_iter().map(|(k, v)| (k.into(), v)).collect()
}

fn variants(pairs: Vec<(&str, VariantKind)>) -> BTreeMap<String, VariantKind> {
    pairs.into_iter().map(|(k, v)| (k.into(), v)).collect()
}

fn re(name: &str) -> Node {
    Node::Ref(name.into())
}

fn doc(root: Node, defs: Vec<(&str, Node)>) -> Schema {
    Schema {
        root,
        defs: defs.into_iter().map(|(k, v)| (k.into(), v)).collect(),
    }
}

fn def(name: &str, body: Node) -> Schema {
    doc(re(name), vec![(name, body)])
}

/// Unwrap a `Complete` derivation, panicking with the `Partial` report if it
/// wasn't.
fn complete(d: Derivation) -> Migration {
    match d {
        Derivation::Complete(m) => m,
        Derivation::Partial(i) => panic!("expected Complete, got Partial: {i:?}"),
    }
}

/// Unwrap a `Partial` derivation's unclassified divergences, panicking with
/// the migration if it wasn't.
fn partial(d: Derivation) -> Vec<Divergence> {
    match d {
        Derivation::Partial(i) => i.unclassified().to_vec(),
        Derivation::Complete(m) => panic!("expected Partial, got Complete: {m:?}"),
    }
}

fn op(at: Target, change: Change) -> Op {
    Op { at, change }
}

// --- Add ---

/// A field added with an author-supplied default classifies as one `Add`.
#[test]
fn field_added_with_default_is_complete() {
    let from = def("T", Node::Struct(fields(vec![])));
    let to = def("T", Node::Struct(fields(vec![("x", Node::I32)])));
    let hints = Hints::new().defaulted(Target::Def("T".into()), "x", Constant::Integer(7));

    let m = complete(derive(&from, &to, &hints));
    assert_eq!(
        m.ops,
        vec![op(
            Target::Def("T".into()),
            Change::Add {
                field: "x".into(),
                default: Constant::Integer(7),
            }
        )]
    );
}

/// A field added whose target schema is `Optional(_)` needs no author
/// default: its canonical inhabitant is absence, so the op takes
/// `Constant::Null`.
#[test]
fn field_added_optional_needs_no_default() {
    let from = def("T", Node::Struct(fields(vec![])));
    let to = def(
        "T",
        Node::Struct(fields(vec![("x", Node::Optional(Box::new(Node::I32)))])),
    );

    let m = complete(derive(&from, &to, &Hints::new()));
    assert_eq!(
        m.ops,
        vec![op(
            Target::Def("T".into()),
            Change::Add {
                field: "x".into(),
                default: Constant::Null,
            }
        )]
    );
}

/// A field added with a non-optional schema and no hint has no image under
/// the edge — `Undefaulted`, not a guess.
#[test]
fn field_added_non_optional_no_hint_is_undefaulted() {
    let from = def("T", Node::Struct(fields(vec![])));
    let to = def("T", Node::Struct(fields(vec![("x", Node::I32)])));

    let divergences = partial(derive(&from, &to, &Hints::new()));
    assert_eq!(
        divergences,
        vec![Divergence::Undefaulted {
            at: Target::Def("T".into()),
            field: "x".into(),
        }]
    );
}

// --- Remove ---

/// A field present only in the source classifies as one `Remove`.
#[test]
fn field_removed_is_complete() {
    let from = def("T", Node::Struct(fields(vec![("x", Node::I32)])));
    let to = def("T", Node::Struct(fields(vec![])));

    let m = complete(derive(&from, &to, &Hints::new()));
    assert_eq!(
        m.ops,
        vec![op(
            Target::Def("T".into()),
            Change::Remove { field: "x".into() }
        )]
    );
}

// --- Rename ---

/// A hinted rename classifies as one `Rename`; the consumed source field
/// does not also surface as a `Remove`.
#[test]
fn field_renamed_with_hint_is_complete_and_not_also_removed() {
    let from = def("T", Node::Struct(fields(vec![("old", Node::String)])));
    let to = def("T", Node::Struct(fields(vec![("new", Node::String)])));
    let hints = Hints::new().renamed(Target::Def("T".into()), "old", "new");

    let m = complete(derive(&from, &to, &hints));
    assert_eq!(
        m.ops,
        vec![op(
            Target::Def("T".into()),
            Change::Rename {
                from: "old".into(),
                to: "new".into(),
            }
        )]
    );
}

/// The same {remove, add} pair with no hint cannot be told apart from an
/// unrelated field swap: the removal is classified, but the addition is
/// `Undefaulted` rather than guessed at as a rename.
#[test]
fn same_remove_add_pair_without_hint_is_not_a_rename() {
    let from = def("T", Node::Struct(fields(vec![("old", Node::String)])));
    let to = def("T", Node::Struct(fields(vec![("new", Node::String)])));

    let divergences = partial(derive(&from, &to, &Hints::new()));
    assert_eq!(
        divergences,
        vec![Divergence::Undefaulted {
            at: Target::Def("T".into()),
            field: "new".into(),
        }]
    );
}

// --- Wrap ---

/// A field wrapped in `Optional` classifies as one `Wrap`.
#[test]
fn field_wrapped_is_complete() {
    let from = def("T", Node::Struct(fields(vec![("x", Node::I32)])));
    let to = def(
        "T",
        Node::Struct(fields(vec![("x", Node::Optional(Box::new(Node::I32)))])),
    );

    let m = complete(derive(&from, &to, &Hints::new()));
    assert_eq!(
        m.ops,
        vec![op(
            Target::Def("T".into()),
            Change::Wrap { field: "x".into() }
        )]
    );
}

/// A rename and a wrap on the same field both classify: `Rename` precedes
/// `Wrap`, and the `Wrap` names the *target* field.
#[test]
fn rename_and_wrap_on_same_field() {
    let from = def("T", Node::Struct(fields(vec![("old", Node::I32)])));
    let to = def(
        "T",
        Node::Struct(fields(vec![("new", Node::Optional(Box::new(Node::I32)))])),
    );
    let hints = Hints::new().renamed(Target::Def("T".into()), "old", "new");

    let m = complete(derive(&from, &to, &hints));
    assert_eq!(
        m.ops,
        vec![
            op(
                Target::Def("T".into()),
                Change::Rename {
                    from: "old".into(),
                    to: "new".into(),
                }
            ),
            op(
                Target::Def("T".into()),
                Change::Wrap {
                    field: "new".into()
                }
            ),
        ]
    );
}

// --- def-scoped addressing ---

/// A definition reached through both `Vec<T>` and `Option<T>` is addressed
/// once, by `Target::Def`, not once per root-relative occurrence.
#[test]
fn nested_def_reached_two_ways_produces_one_op() {
    let root_struct = |inner: &str| {
        Node::Struct(fields(vec![
            ("a", Node::List(Box::new(re(inner)))),
            ("b", Node::Optional(Box::new(re(inner)))),
        ]))
    };
    let from = doc(
        re("Root"),
        vec![
            ("Root", root_struct("Inner")),
            ("Inner", Node::Struct(fields(vec![("x", Node::I32)]))),
        ],
    );
    let to = doc(
        re("Root"),
        vec![
            ("Root", root_struct("Inner")),
            (
                "Inner",
                Node::Struct(fields(vec![("x", Node::I32), ("y", Node::String)])),
            ),
        ],
    );
    let hints = Hints::new().defaulted(Target::Def("Inner".into()), "y", Constant::Text("".into()));

    let m = complete(derive(&from, &to, &hints));
    assert_eq!(
        m.ops,
        vec![op(
            Target::Def("Inner".into()),
            Change::Add {
                field: "y".into(),
                default: Constant::Text("".into()),
            }
        )]
    );
}

/// A recursive type's field rename applies once, addressed at its own
/// definition, regardless of how many times it self-occurs.
#[test]
fn recursive_type_produces_one_op() -> anyhow::Result<()> {
    mod old {
        #[derive(facet::Facet)]
        pub struct Branch {
            pub children: Vec<Branch>,
            pub label: String,
        }
    }
    mod new {
        #[derive(facet::Facet)]
        pub struct Branch {
            pub children: Vec<Branch>,
            pub name: String,
        }
    }

    let from = schema_of::<old::Branch>()?;
    let to = schema_of::<new::Branch>()?;
    let hints = Hints::new().renamed(Target::Def("Branch".into()), "label", "name");

    let m = complete(derive(&from, &to, &hints));
    assert_eq!(
        m.ops,
        vec![op(
            Target::Def("Branch".into()),
            Change::Rename {
                from: "label".into(),
                to: "name".into(),
            }
        )]
    );
    Ok(())
}

// --- enums ---

/// A struct enum variant's field change is addressed by
/// `Target::Variant { def, variant }`.
#[test]
fn struct_variant_field_change_is_addressed_by_variant_target() {
    let from = def(
        "E",
        Node::Enum(variants(vec![(
            "V",
            VariantKind::Struct(variant_fields(vec![("x", Node::I32)])),
        )])),
    );
    let to = def(
        "E",
        Node::Enum(variants(vec![(
            "V",
            VariantKind::Struct(variant_fields(vec![("x", Node::I32), ("y", Node::String)])),
        )])),
    );
    let hints = Hints::new().defaulted(
        Target::Variant {
            def: "E".into(),
            variant: "V".into(),
        },
        "y",
        Constant::Text("".into()),
    );

    let m = complete(derive(&from, &to, &hints));
    assert_eq!(
        m.ops,
        vec![op(
            Target::Variant {
                def: "E".into(),
                variant: "V".into(),
            },
            Change::Add {
                field: "y".into(),
                default: Constant::Text("".into()),
            }
        )]
    );
}

/// An enum variant added in the target is a pure widening: no old value
/// holds it, so it needs no lens and yields no op.
#[test]
fn enum_variant_added_is_complete_with_no_ops() {
    let from = def("E", Node::Enum(variants(vec![("A", VariantKind::Unit)])));
    let to = def(
        "E",
        Node::Enum(variants(vec![
            ("A", VariantKind::Unit),
            ("B", VariantKind::Unit),
        ])),
    );

    let m = complete(derive(&from, &to, &Hints::new()));
    assert_eq!(m.ops, vec![]);
}

/// An enum variant removed from the target has no image under the edge:
/// `VariantRemoved`.
#[test]
fn enum_variant_removed_is_partial() {
    let from = def(
        "E",
        Node::Enum(variants(vec![
            ("A", VariantKind::Unit),
            ("B", VariantKind::Unit),
        ])),
    );
    let to = def("E", Node::Enum(variants(vec![("A", VariantKind::Unit)])));

    let divergences = partial(derive(&from, &to, &Hints::new()));
    assert_eq!(
        divergences,
        vec![Divergence::VariantRemoved {
            def: "E".into(),
            variant: "B".into(),
        }]
    );
}

// --- Retyped ---

/// A field retyped other than by an `Optional` wrap has no lens: `Retyped`.
#[test]
fn field_retyped_is_partial() {
    let from = def("T", Node::Struct(fields(vec![("x", Node::String)])));
    let to = def("T", Node::Struct(fields(vec![("x", Node::U32)])));

    let divergences = partial(derive(&from, &to, &Hints::new()));
    assert_eq!(
        divergences,
        vec![Divergence::Retyped {
            at: Target::Def("T".into()),
            field: "x".into(),
            from: Node::String,
            to: Node::U32,
        }]
    );
}

// --- Root and Unpaired ---

/// A root-schema change (not merely a `Ref` name change reachable through
/// `defs`) has no lens: `Root`, plus the definition the new root no longer
/// reaches.
#[test]
fn root_change_is_partial() {
    let from = def("T", Node::Struct(fields(vec![])));
    let to = doc(Node::I32, vec![]);

    let divergences = partial(derive(&from, &to, &Hints::new()));
    assert_eq!(
        divergences,
        vec![
            Divergence::Root {
                from: re("T"),
                to: Node::I32,
            },
            Divergence::Unpaired {
                name: "T".into(),
                side: Side::From,
            },
        ]
    );
}

/// A definition reachable from one document only is `Unpaired`, tagged with
/// the side it's present on.
#[test]
fn unpaired_definition_is_partial() {
    let from = doc(Node::Unit, vec![("Orphan", Node::Struct(fields(vec![])))]);
    let to = doc(Node::Unit, vec![]);

    let divergences = partial(derive(&from, &to, &Hints::new()));
    assert_eq!(
        divergences,
        vec![Divergence::Unpaired {
            name: "Orphan".into(),
            side: Side::From,
        }]
    );

    let divergences = partial(derive(&to, &from, &Hints::new()));
    assert_eq!(
        divergences,
        vec![Divergence::Unpaired {
            name: "Orphan".into(),
            side: Side::To,
        }]
    );
}

// --- derive_to ---

/// `derive_to::<T>` folds in the rename hints `T`'s
/// `#[facet(migrate::renamed_from = …)]` attributes declare.
#[derive(Facet)]
#[allow(dead_code)]
struct RenamedTarget {
    #[facet(facet_git_tree::renamed_from = "old_id")]
    id: String,
}

#[test]
fn derive_to_picks_up_declared_rename_hints() -> anyhow::Result<()> {
    let from = def(
        "RenamedTarget",
        Node::Struct(fields(vec![("old_id", Node::String)])),
    );

    let m = complete(derive_to::<RenamedTarget>(&from, &Hints::new())?);
    assert_eq!(
        m.ops,
        vec![op(
            Target::Def("RenamedTarget".into()),
            Change::Rename {
                from: "old_id".into(),
                to: "id".into(),
            }
        )]
    );
    Ok(())
}

// --- determinism ---

/// Deriving the same edge twice yields equal `Migration`s, and serializing
/// the migration twice yields the same object id.
#[test]
fn derivation_is_deterministic() -> anyhow::Result<()> {
    let from = def(
        "T",
        Node::Struct(fields(vec![
            ("old", Node::I32),
            ("gone", Node::Bool),
            ("same", Node::String),
        ])),
    );
    let to = def(
        "T",
        Node::Struct(fields(vec![
            ("new", Node::Optional(Box::new(Node::I32))),
            ("same", Node::String),
            ("fresh", Node::I32),
        ])),
    );
    let hints = Hints::new()
        .renamed(Target::Def("T".into()), "old", "new")
        .defaulted(Target::Def("T".into()), "fresh", Constant::Integer(0));

    let d1 = derive(&from, &to, &hints);
    let d2 = derive(&from, &to, &hints);
    assert_eq!(d1, d2);

    let m = complete(d1);
    assert_eq!(m, complete(derive(&from, &to, &hints)));

    let (id1, _) = serialize(&m)?;
    let (id2, _) = serialize(&m)?;
    assert_eq!(id1, id2);
    Ok(())
}
