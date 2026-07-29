//! Integration tests for read-time migration application (`apply` /
//! `apply_chain`, `value` feature).
//!
//! Covers spec requirements:
//!   migration.no-rewrite
//!     — applying a migration touches no object store and writes no object;
//!       the source tree's id and the store's object population are
//!       unchanged.
//!   migration.addressing
//!     — a migration is applied by walking the value guided by its source
//!       schema document, applying each target's operations wherever the
//!       walk resolves that definition: at every occurrence of a type used
//!       twice, at every depth of a recursive type, and inside a struct enum
//!       variant via `Target::Variant`.
//!   migration.application
//!     — each operator's effect on a value, including the leniency `Rename`/
//!       `Remove` inherit from schema-driven deserialization for a missing
//!       field, and `Wrap`'s identity on the value.
//!   migration.composition
//!     — a chain of edges A→B→C applies each edge in series.

use std::collections::BTreeMap;

use facet::Facet;
use facet_git_tree::{
    Change, Constant, Edge, Migration, MigrationError, Node, Op, Schema, Target, apply,
    apply_chain, deserialize_value_with_schema, schema_of, serialize,
};
use facet_value::{Value, value};

/// `deserialize_value_with_schema(serialize(value))`, the schema-driven
/// analogue of `common::roundtrip` this suite's tests build their fixtures
/// from.
fn read<T>(value: &T) -> anyhow::Result<(Value, Schema)>
where
    T: for<'a> Facet<'a>,
{
    let (root, store) = serialize(value)?;
    let doc = schema_of::<T>()?;
    let v = deserialize_value_with_schema(&root, &doc, &store)?;
    Ok((v, doc))
}

// --- individual operators ---

#[derive(Facet)]
struct IssueOld {
    old_id: i64,
    title: String,
}

#[derive(Facet)]
struct IssueNew {
    id: i64,
    title: String,
}

/// `Rename` moves a member to its new name, and is a no-op when the source
/// value does not carry the field — the same leniency schema-driven
/// deserialization applies to a missing struct field.
#[test]
fn rename_moves_the_member_and_is_lenient_when_absent() -> anyhow::Result<()> {
    let (old_value, old_doc) = read(&IssueOld {
        old_id: 7,
        title: "bug".into(),
    })?;
    let migration = Migration {
        ops: vec![Op {
            at: Target::Def("IssueOld".into()),
            change: Change::Rename {
                from: "old_id".into(),
                to: "id".into(),
            },
        }],
    };
    let migrated = apply(&old_value, &old_doc, &migration)?;
    let (expected, _) = read(&IssueNew {
        id: 7,
        title: "bug".into(),
    })?;
    assert_eq!(migrated, expected);

    // Renaming a field the value does not carry is a no-op, not an error.
    let missing = Migration {
        ops: vec![Op {
            at: Target::Def("IssueOld".into()),
            change: Change::Rename {
                from: "nonexistent".into(),
                to: "also_nonexistent".into(),
            },
        }],
    };
    assert_eq!(apply(&old_value, &old_doc, &missing)?, old_value);
    Ok(())
}

#[derive(Facet)]
struct AddOld {
    name: String,
}

/// `Add` inserts a `Constant`-converted default, at every `Constant` kind
/// (including nested `List`/`Object`), overwriting any member already
/// present.
#[test]
fn add_inserts_every_constant_kind() -> anyhow::Result<()> {
    let (old_value, old_doc) = read(&AddOld { name: "x".into() })?;
    let migration = Migration {
        ops: vec![
            Op {
                at: Target::Def("AddOld".into()),
                change: Change::Add {
                    field: "flag".into(),
                    default: Constant::Bool(true),
                },
            },
            Op {
                at: Target::Def("AddOld".into()),
                change: Change::Add {
                    field: "count".into(),
                    default: Constant::Integer(-3),
                },
            },
            Op {
                at: Target::Def("AddOld".into()),
                change: Change::Add {
                    field: "ratio".into(),
                    default: Constant::Float(1.5),
                },
            },
            Op {
                at: Target::Def("AddOld".into()),
                change: Change::Add {
                    field: "label".into(),
                    default: Constant::Text("hi".into()),
                },
            },
            Op {
                at: Target::Def("AddOld".into()),
                change: Change::Add {
                    field: "empty".into(),
                    default: Constant::Null,
                },
            },
            Op {
                at: Target::Def("AddOld".into()),
                change: Change::Add {
                    field: "tags".into(),
                    default: Constant::List(vec![
                        Constant::Integer(1),
                        Constant::Text("two".into()),
                    ]),
                },
            },
            Op {
                at: Target::Def("AddOld".into()),
                change: Change::Add {
                    field: "meta".into(),
                    default: Constant::Object(BTreeMap::from([(
                        "k".to_string(),
                        Constant::Bool(false),
                    )])),
                },
            },
            // Add overwrites a member already present.
            Op {
                at: Target::Def("AddOld".into()),
                change: Change::Add {
                    field: "name".into(),
                    default: Constant::Text("overwritten".into()),
                },
            },
        ],
    };
    let migrated = apply(&old_value, &old_doc, &migration)?;
    assert_eq!(
        migrated,
        value!({
            "name": "overwritten",
            "flag": true,
            "count": (-3),
            "ratio": 1.5,
            "label": "hi",
            "empty": null,
            "tags": [1, "two"],
            "meta": { "k": false },
        })
    );
    Ok(())
}

#[derive(Facet)]
struct RemoveOld {
    name: String,
    legacy: bool,
}

/// `Remove` drops the member, and is a no-op when the source value does not
/// carry the field.
#[test]
fn remove_drops_the_member_and_is_lenient_when_absent() -> anyhow::Result<()> {
    let (old_value, old_doc) = read(&RemoveOld {
        name: "x".into(),
        legacy: true,
    })?;
    let migration = Migration {
        ops: vec![
            Op {
                at: Target::Def("RemoveOld".into()),
                change: Change::Remove {
                    field: "legacy".into(),
                },
            },
            Op {
                at: Target::Def("RemoveOld".into()),
                change: Change::Remove {
                    field: "nonexistent".into(),
                },
            },
        ],
    };
    let migrated = apply(&old_value, &old_doc, &migration)?;
    assert_eq!(migrated, value!({ "name": "x" }));
    Ok(())
}

#[derive(Facet)]
struct WrapOld {
    count: i64,
}

#[derive(Facet)]
struct WrapNew {
    count: Option<i64>,
}

/// `Wrap` is the identity on the value: `Node::Optional` reads `Some(x)`
/// and `x` as the same `Value`, so the migrated value equals both the
/// unmigrated one and the far side's typed reading.
#[test]
fn wrap_is_the_identity_on_the_value() -> anyhow::Result<()> {
    let (old_value, old_doc) = read(&WrapOld { count: 5 })?;
    let migration = Migration {
        ops: vec![Op {
            at: Target::Def("WrapOld".into()),
            change: Change::Wrap {
                field: "count".into(),
            },
        }],
    };
    let migrated = apply(&old_value, &old_doc, &migration)?;
    assert_eq!(migrated, old_value);
    let (expected, _) = read(&WrapNew { count: Some(5) })?;
    assert_eq!(migrated, expected);
    Ok(())
}

// --- the no-rewrite guarantee ---

#[derive(Facet)]
struct RenameForNoRewrite {
    old_id: i64,
}

/// The point of the whole feature: `apply` never writes an object. The
/// source tree's id and the store's entire object population are exactly
/// what they were before `apply` ran, and re-reading the original tree
/// yields the original value byte-identically.
#[test]
fn apply_never_rewrites_the_object_store() -> anyhow::Result<()> {
    let (root, store) = serialize(&RenameForNoRewrite { old_id: 1 })?;
    let doc = schema_of::<RenameForNoRewrite>()?;
    let value = deserialize_value_with_schema(&root, &doc, &store)?;

    // `ObjectStore` exposes no public length accessor, so its `Debug` output
    // — which reports the in-memory object count — stands in as the
    // before/after snapshot.
    let store_snapshot_before = format!("{store:?}");

    let migration = Migration {
        ops: vec![Op {
            at: Target::Def("RenameForNoRewrite".into()),
            change: Change::Rename {
                from: "old_id".into(),
                to: "id".into(),
            },
        }],
    };
    let migrated = apply(&value, &doc, &migration)?;
    assert_eq!(migrated, value!({ "id": 1 }));

    let store_snapshot_after = format!("{store:?}");
    assert_eq!(
        store_snapshot_before, store_snapshot_after,
        "apply must not add or remove any object in the store"
    );

    let reread = deserialize_value_with_schema(&root, &doc, &store)?;
    assert_eq!(
        reread, value,
        "the tree at `root` must be byte-identical after apply"
    );
    Ok(())
}

// --- addressing: every occurrence, every depth, variant fields ---

#[derive(Facet)]
struct ItemOld {
    old_name: String,
}

#[derive(Facet)]
struct ContainerOld {
    items: Vec<ItemOld>,
    maybe: Option<ItemOld>,
}

/// A definition reached through `Vec<T>` and through `Option<T>` migrates at
/// every occurrence from a single `Target::Def` operation.
#[test]
fn one_operation_migrates_every_occurrence_through_vec_and_option() -> anyhow::Result<()> {
    let migration = Migration {
        ops: vec![Op {
            at: Target::Def("ItemOld".into()),
            change: Change::Rename {
                from: "old_name".into(),
                to: "name".into(),
            },
        }],
    };

    let (some_value, doc) = read(&ContainerOld {
        items: vec![
            ItemOld {
                old_name: "a".into(),
            },
            ItemOld {
                old_name: "b".into(),
            },
        ],
        maybe: Some(ItemOld {
            old_name: "c".into(),
        }),
    })?;
    let migrated = apply(&some_value, &doc, &migration)?;
    assert_eq!(
        migrated,
        value!({
            "items": [{ "name": "a" }, { "name": "b" }],
            "maybe": { "name": "c" },
        })
    );

    let (none_value, _) = read(&ContainerOld {
        items: vec![],
        maybe: None,
    })?;
    let migrated_none = apply(&none_value, &doc, &migration)?;
    assert_eq!(migrated_none, value!({ "items": [], "maybe": null }));
    Ok(())
}

#[derive(Facet)]
struct NodeOld {
    old_label: String,
    children: Vec<NodeOld>,
}

/// A recursive type migrates at every depth from a single operation.
#[test]
fn one_operation_migrates_a_recursive_type_at_every_depth() -> anyhow::Result<()> {
    let tree = NodeOld {
        old_label: "root".into(),
        children: vec![NodeOld {
            old_label: "child".into(),
            children: vec![NodeOld {
                old_label: "grandchild".into(),
                children: vec![],
            }],
        }],
    };
    let (old_value, old_doc) = read(&tree)?;
    let migration = Migration {
        ops: vec![Op {
            at: Target::Def("NodeOld".into()),
            change: Change::Rename {
                from: "old_label".into(),
                to: "label".into(),
            },
        }],
    };
    let migrated = apply(&old_value, &old_doc, &migration)?;
    assert_eq!(
        migrated,
        value!({
            "label": "root",
            "children": [{
                "label": "child",
                "children": [{ "label": "grandchild", "children": [] }],
            }],
        })
    );
    Ok(())
}

#[derive(Facet)]
#[repr(u8)]
#[allow(dead_code)] // fields are read via `Facet` reflection, not directly
enum AccountOld {
    Guest,
    Login { user: String, active: bool },
}

/// A struct enum variant's field migrates via `Target::Variant`, and a
/// `Def`-scoped operation does not leak into a variant's fields.
#[test]
fn struct_variant_field_migrates_via_target_variant() -> anyhow::Result<()> {
    let (old_value, old_doc) = read(&AccountOld::Login {
        user: "bob".into(),
        active: true,
    })?;
    let migration = Migration {
        ops: vec![Op {
            at: Target::Variant {
                def: "AccountOld".into(),
                variant: "Login".into(),
            },
            change: Change::Rename {
                from: "active".into(),
                to: "ok".into(),
            },
        }],
    };
    let migrated = apply(&old_value, &old_doc, &migration)?;
    assert_eq!(migrated, value!({ "Login": { "user": "bob", "ok": true } }));

    let (guest_value, _) = read(&AccountOld::Guest)?;
    assert_eq!(apply(&guest_value, &old_doc, &migration)?, guest_value);
    Ok(())
}

// --- composition ---

#[derive(Facet)]
struct StageA {
    old: String,
}

#[derive(Facet)]
struct StageB {
    mid: String,
}

#[derive(Facet)]
struct StageC {
    mid: String,
    extra: i64,
}

/// A chain of edges A->B->C applies each edge in series, agreeing with
/// applying the two edges by hand and with the fully-upcast typed value.
#[test]
fn apply_chain_composes_edges_in_series() -> anyhow::Result<()> {
    let (value_a, doc_a) = read(&StageA { old: "x".into() })?;
    let doc_b = schema_of::<StageB>()?;
    let migration_ab = Migration {
        ops: vec![Op {
            at: Target::Def("StageA".into()),
            change: Change::Rename {
                from: "old".into(),
                to: "mid".into(),
            },
        }],
    };
    let migration_bc = Migration {
        ops: vec![Op {
            at: Target::Def("StageB".into()),
            change: Change::Add {
                field: "extra".into(),
                default: Constant::Integer(9),
            },
        }],
    };

    let chain = [
        Edge {
            from: &doc_a,
            migration: &migration_ab,
        },
        Edge {
            from: &doc_b,
            migration: &migration_bc,
        },
    ];
    let via_chain = apply_chain(&value_a, &chain)?;

    let step1 = apply(&value_a, &doc_a, &migration_ab)?;
    let via_manual = apply(&step1, &doc_b, &migration_bc)?;
    assert_eq!(via_chain, via_manual);

    let (expected, _) = read(&StageC {
        mid: "x".into(),
        extra: 9,
    })?;
    assert_eq!(via_chain, expected);

    // An empty chain is the identity.
    assert_eq!(apply_chain(&value_a, &[])?, value_a);
    Ok(())
}

// --- errors ---

/// `Node::Ref` naming no definition is `MigrationError::UnknownRef`.
#[test]
fn unknown_ref_is_reachable() {
    let value = value!(42);
    let doc = Schema {
        root: Node::Ref("nope".into()),
        defs: Default::default(),
    };
    let err = apply(&value, &doc, &Migration::default()).unwrap_err();
    assert!(
        matches!(&err, MigrationError::UnknownRef { name, .. } if name == "nope"),
        "expected UnknownRef, got {err:?}"
    );
}

/// A value whose runtime kind does not match its schema node is
/// `MigrationError::Mismatch`.
#[test]
fn mismatch_is_reachable() {
    let value = value!("not an object");
    let doc = Schema {
        root: Node::Struct(Default::default()),
        defs: Default::default(),
    };
    let err = apply(&value, &doc, &Migration::default()).unwrap_err();
    assert!(
        matches!(
            &err,
            MigrationError::Mismatch { expected, found, .. }
                if *expected == "object" && *found == "string"
        ),
        "expected Mismatch, got {err:?}"
    );
}
