//! Integration tests for schema self-hosting: `SchemaDoc` is an ordinary
//! `Facet` value stored through this crate's own encoding.
//!
//! Covers spec requirement:
//!   schema.representation
//!     — schemas are self-hosted with no special-cased representation, and
//!       their on-disk form is a public, semver-major contract (guarded here
//!       by a golden object id).

use std::collections::BTreeMap;

use facet_git_tree::{
    EntryKind, FieldSchema, Schema, SchemaDoc, deserialize, schema_of, serialize,
};

mod common;
use common::{Event, Nested, Person, TreeNode, find_entry};

/// A schema document roundtrips through the crate's own serialize/deserialize.
#[test]
fn schema_doc_roundtrips() -> anyhow::Result<()> {
    let doc = schema_of::<Nested>()?;
    let (root, store) = serialize(&doc)?;
    let back: SchemaDoc = deserialize(&root, &store)?;
    assert_eq!(back, doc);
    Ok(())
}

/// An enum schema — exercising every `VariantKind` — roundtrips too.
#[test]
fn enum_schema_doc_roundtrips() -> anyhow::Result<()> {
    let doc = schema_of::<Event>()?;
    let (root, store) = serialize(&doc)?;
    let back: SchemaDoc = deserialize(&root, &store)?;
    assert_eq!(back, doc);
    Ok(())
}

/// A recursive schema (with a `Ref` cycle) roundtrips.
#[test]
fn recursive_schema_doc_roundtrips() -> anyhow::Result<()> {
    let doc = schema_of::<TreeNode>()?;
    let (root, store) = serialize(&doc)?;
    let back: SchemaDoc = deserialize(&root, &store)?;
    assert_eq!(back, doc);
    Ok(())
}

/// Golden object id of `schema_of::<Person>()`'s serialized root.
///
/// The schema types' on-disk form is a public contract: if this id changes,
/// the schema encoding itself changed, which is a semver-MAJOR break — every
/// published schema in every downstream repository would stop resolving. Do
/// not update the literal without releasing accordingly.
///
/// Updated for the unit-enum-variant blob collapse (issue 8d109650): a
/// `Schema` unit variant such as `String`/`U32`/`Bool` now tags with a bare
/// blob holding its name instead of a tree wrapping an empty tree, so
/// `Person`'s schema — three unit-variant scalar fields — reproduces to a
/// different, but still deterministic, root id.
#[test]
fn person_schema_golden_oid() -> anyhow::Result<()> {
    let doc = schema_of::<Person>()?;
    let (root, _store) = serialize(&doc)?;
    assert_eq!(root.to_string(), "867c324f4eaa5f10a6ec272f6f2e95250933b21a");
    Ok(())
}

/// Regression for issue 8d109650's second repro: changing a schema field's
/// type (`U32` → `String`, mirroring `refs/schema/recipe~1` vs.
/// `refs/schema/recipe`) must change a *blob*, at a stable path, not just an
/// empty tree's entry name — otherwise `git diff` on the schema ref sees
/// nothing, exactly the bug this issue reports.
///
/// `Schema::U32` and `Schema::String` are themselves unit variants of the
/// `Schema` enum, so this exercises the same blob-collapse fix as the
/// `priority: Low → High` value-level repro, just one level up (in the
/// schema's own self-hosted encoding).
#[test]
fn schema_field_type_change_is_a_blob_level_diff() -> anyhow::Result<()> {
    fn recipe_doc(servings_schema: Schema) -> SchemaDoc {
        let mut defs = BTreeMap::new();
        defs.insert(
            "Recipe".to_string(),
            Schema::Struct(vec![FieldSchema {
                name: "servings".into(),
                schema: servings_schema,
            }]),
        );
        SchemaDoc {
            root: Schema::Ref("Recipe".into()),
            defs,
        }
    }

    let (before_root, before_store) = serialize(&recipe_doc(Schema::U32))?;
    let (after_root, after_store) = serialize(&recipe_doc(Schema::String))?;
    assert_ne!(
        before_root, after_root,
        "changing the field's schema must change the document's root id"
    );

    // Walk the same path in both trees: defs → Recipe → Struct → 0000 (the
    // sole field) → schema.
    let walk = |store: &facet_git_tree::ObjectStore, root: &facet_git_tree::ObjectId| {
        let defs = find_entry(store, root, "defs");
        let recipe = find_entry(store, &defs.oid, "Recipe");
        let struct_ = find_entry(store, &recipe.oid, "Struct");
        let field0 = find_entry(store, &struct_.oid, "0000");
        find_entry(store, &field0.oid, "schema")
    };
    let before_schema = walk(&before_store, &before_root);
    let after_schema = walk(&after_store, &after_root);

    assert_eq!(
        before_schema.mode.kind(),
        EntryKind::Blob,
        "a unit-variant Schema node's own tag must be a blob"
    );
    assert_eq!(after_schema.mode.kind(), EntryKind::Blob);
    assert_ne!(
        before_schema.oid, after_schema.oid,
        "the `schema` entry's oid must differ at this stable path"
    );
    assert_eq!(
        before_store.get_blob(&before_schema.oid).expect("blob"),
        b"U32"
    );
    assert_eq!(
        after_store.get_blob(&after_schema.oid).expect("blob"),
        b"String"
    );
    Ok(())
}
