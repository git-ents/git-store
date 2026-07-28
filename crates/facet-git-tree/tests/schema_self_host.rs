//! Integration tests for schema self-hosting: `SchemaDoc` is an ordinary
//! `Facet` value stored through this crate's own encoding.
//!
//! Covers spec requirements:
//!   schema.representation
//!     — schemas are self-hosted with no special-cased representation, and
//!       their on-disk form is a public, semver-major contract (guarded here
//!       by a golden object id).
//!   schema.representation.version
//!     — every stored document carries a `version` marker readable out of
//!       band, off a fixed top-level entry name, before any attempt to
//!       deserialize the rest of it.

use std::collections::BTreeMap;

use facet_git_tree::{
    DeserializeError, EntryKind, FieldSchema, Schema, SchemaDoc, SchemaVersionError, deserialize,
    schema_of, serialize,
};
use gix_object::Write as _;

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
///
/// Updated again for the leaf trailing-newline rule (issue 5b39f084): every
/// leaf blob, including a unit variant's name blob, now carries a mandatory
/// trailing `\n`, which changes every blob (and therefore every containing
/// tree) in the document.
///
/// Updated again for the `SchemaDoc::version` marker (issue d4f8aaaf): every
/// `SchemaDoc` — including the one `schema_of::<Person>()` returns — now
/// carries an extra top-level `version` entry, which changes the document's
/// root id even though `Person`'s own schema shape did not change.
#[test]
fn person_schema_golden_oid() -> anyhow::Result<()> {
    let doc = schema_of::<Person>()?;
    let (root, _store) = serialize(&doc)?;
    assert_eq!(root.to_string(), "ae6b94fb40c62a58635655e12473faf1a5e7dece");
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
            version: SchemaDoc::CURRENT_VERSION,
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
        b"U32\n"
    );
    assert_eq!(
        after_store.get_blob(&after_schema.oid).expect("blob"),
        b"String\n"
    );
    Ok(())
}

// --- version marker (issue d4f8aaaf) ---

/// The `version` marker reads back to [`SchemaDoc::CURRENT_VERSION`] for any
/// document this crate generates, via the out-of-band pre-read alone — no
/// full typed deserialize needed.
#[test]
fn read_stored_version_reads_the_current_version() -> anyhow::Result<()> {
    let doc = schema_of::<Nested>()?;
    let (root, store) = serialize(&doc)?;
    assert_eq!(
        SchemaDoc::read_stored_version(&root, &store)?,
        SchemaDoc::CURRENT_VERSION
    );
    Ok(())
}

/// The `version` entry is a leaf blob carrying the mandatory trailing
/// newline (issue 5b39f084) exactly like every other leaf, so it reads with
/// nothing more than `git cat-file blob <tree>:version`.
#[test]
fn version_blob_carries_its_trailing_newline() -> anyhow::Result<()> {
    let doc = schema_of::<Nested>()?;
    let (root, store) = serialize(&doc)?;
    let version_entry = find_entry(&store, &root, "version");
    assert_eq!(version_entry.mode.kind(), EntryKind::Blob);
    let raw = store
        .get_blob(&version_entry.oid)
        .expect("version entry must be a blob");
    assert_eq!(raw, b"1\n");
    Ok(())
}

/// A stored schema tree with no `version` entry at all — a document stored
/// before the field existed — is reported as [`SchemaVersionError::Missing`],
/// never silently treated as version 0 or 1.
#[test]
fn read_stored_version_reports_a_missing_entry() -> anyhow::Result<()> {
    let doc = schema_of::<Nested>()?;
    let (root, store) = serialize(&doc)?;
    let entries: Vec<_> = store
        .get_tree(&root)
        .expect("root is a tree")
        .into_iter()
        .filter(|e| e.filename != "version")
        .collect();
    let stripped = store.write(&gix_object::Tree { entries }).unwrap();

    let err = SchemaDoc::read_stored_version(&stripped, &store).unwrap_err();
    assert!(
        matches!(err, SchemaVersionError::Missing(oid) if oid == stripped),
        "{err:?}"
    );
    Ok(())
}

/// The whole point of reading `version` out of band: it succeeds even when
/// the rest of the document is not intelligible to this binary — simulated
/// here as a `root` tagged with a hypothetical `DateTime` variant `Schema`
/// does not define, the same repro the issue names. A full typed
/// deserialize of the same tree fails on that unknown variant with a
/// reflection error, which is exactly the failure `read_stored_version`
/// lets a caller check for *before* it happens.
#[test]
fn read_stored_version_ignores_an_undecodable_document() -> anyhow::Result<()> {
    let doc = schema_of::<Nested>()?;
    let (root, store) = serialize(&doc)?;

    let mut entries = store.get_tree(&root).expect("root is a tree");
    let bogus_root = store
        .write_buf(gix_object::Kind::Blob, b"DateTime\n")
        .unwrap();
    for entry in &mut entries {
        if entry.filename == "root" {
            entry.oid = bogus_root;
            entry.mode = gix_object::tree::EntryKind::Blob.into();
        }
    }
    let corrupt = store.write(&gix_object::Tree { entries }).unwrap();

    // The out-of-band read does not care: it inspects only `version`.
    assert_eq!(
        SchemaDoc::read_stored_version(&corrupt, &store)?,
        SchemaDoc::CURRENT_VERSION
    );

    // A full typed deserialize, in contrast, cannot get past the unknown
    // variant — a reflection error, not a version error.
    let err = deserialize::<SchemaDoc>(&corrupt, &store).unwrap_err();
    assert!(
        matches!(&err, DeserializeError::Reflect(msg) if msg.contains("DateTime")),
        "{err:?}"
    );
    Ok(())
}
