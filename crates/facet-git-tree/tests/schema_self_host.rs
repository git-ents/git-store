//! Integration tests for schema self-hosting: `Schema` is an ordinary
//! `Facet` value stored through this crate's own encoding.
//!
//! Covers spec requirements:
//!   schema.representation
//!     — schemas are self-hosted with no special-cased representation, and
//!       their on-disk form is a public, semver-major contract (guarded here
//!       by a golden object id).
//!   schema.representation.pin
//!     — every stored document pins the schema-schema tree it was written
//!       against as a `schema` entry spliced in at write time; an absent pin
//!       is legitimate only for a known root generation (the genesis rule);
//!       and the pin is read out of band, before any attempt to deserialize
//!       the rest of the document.

use std::collections::BTreeMap;

use facet::Facet;
use facet_git_tree::{
    DeserializeError, EMPTY_TREE, EntryKind, EntryMode, Node, ObjectId, ObjectStore, Schema,
    SchemaError, SchemaPinError, SchemaSchema, StructField, TreeEntry, deserialize, schema_of,
    serialize, serialize_into,
};
use gix_object::Write as _;

mod common;
use common::{Event, Nested, Person, TreeNode, find_entry, splice_codec};

fn splice_empty_schema(store: &ObjectStore, tree: &ObjectId) -> ObjectId {
    let mut entries = store.get_tree(tree).expect("tree present");
    entries.push(TreeEntry {
        mode: EntryMode::from(EntryKind::Tree),
        filename: "schema".into(),
        oid: EMPTY_TREE,
    });
    entries.sort();
    store
        .write(&gix_object::Tree { entries })
        .expect("tree write")
}

/// A schema document roundtrips through the crate's own serialize/deserialize.
#[test]
fn schema_doc_roundtrips() -> anyhow::Result<()> {
    let doc = schema_of::<Nested>()?;
    let (root, store) = serialize(&doc)?;
    let back: Schema = deserialize(&root, &store)?;
    assert_eq!(back, doc);
    Ok(())
}

/// An enum schema — exercising every `VariantKind` — roundtrips too.
#[test]
fn enum_schema_doc_roundtrips() -> anyhow::Result<()> {
    let doc = schema_of::<Event>()?;
    let (root, store) = serialize(&doc)?;
    let back: Schema = deserialize(&root, &store)?;
    assert_eq!(back, doc);
    Ok(())
}

/// The wire shape used before `Schema.kind` existed. Keeping this fixture in
/// the test makes the compatibility test independent of the current writer's
/// `Schema` shape while retaining the historical `Node` representation.
#[derive(Debug, Facet)]
struct LegacySchemaDocument {
    root: Node,
    defs: BTreeMap<String, Node>,
}

fn splice_legacy_pin(store: &ObjectStore, tree: &ObjectId) -> ObjectId {
    let mut entries = store.get_tree(tree).expect("tree present");
    entries.push(TreeEntry {
        mode: EntryMode::from(EntryKind::Tree),
        filename: SchemaSchema::ENTRY.into(),
        oid: *SchemaSchema::LEGACY.tree(),
    });
    entries.sort();
    store
        .write(&gix_object::Tree { entries })
        .expect("tree write")
}

fn splice_migration_metadata(store: &ObjectStore, tree: &ObjectId) -> ObjectId {
    let mut entries = store.get_tree(tree).expect("tree present");
    entries.push(TreeEntry {
        mode: EntryMode::from(EntryKind::Tree),
        filename: "migration".into(),
        oid: EMPTY_TREE,
    });
    entries.sort();
    store
        .write(&gix_object::Tree { entries })
        .expect("tree write")
}

fn splice_unexpected_metadata(store: &ObjectStore, tree: &ObjectId) -> ObjectId {
    let mut entries = store.get_tree(tree).expect("tree present");
    entries.push(TreeEntry {
        mode: EntryMode::from(EntryKind::Tree),
        filename: "unexpected".into(),
        oid: EMPTY_TREE,
    });
    entries.sort();
    store
        .write(&gix_object::Tree { entries })
        .expect("tree write")
}

/// A schema written against the generation immediately before `kind` is
/// decoded through the historical `{root, defs}` shape and gets an explicit
/// sentinel rather than failing as a missing-field reflection error.
#[test]
fn pre_kind_schema_reads_with_explicit_legacy_kind() -> anyhow::Result<()> {
    let current = schema_of::<Nested>()?;
    let legacy = LegacySchemaDocument {
        root: current.root.clone(),
        defs: current.defs.clone(),
    };
    let store = ObjectStore::default();
    let bare = serialize_into(&legacy, &store)?;
    let pinned = splice_legacy_pin(&store, &bare);
    let pinned = splice_migration_metadata(&store, &pinned);

    let generation = Schema::read_pin(&pinned, &store)?;
    assert_eq!(generation.tree(), SchemaSchema::LEGACY.tree());

    let decoded = Schema::read_pinned_legacy(&pinned, &store)?;
    assert_eq!(decoded.kind, Schema::LEGACY_KIND);
    assert_eq!(decoded.root, current.root);
    assert_eq!(decoded.defs, current.defs);
    Ok(())
}

/// A historical bare `{root, defs}` object is accepted by the compatibility
/// reader, while the lower-level pin check remains strict for unpinned trees.
#[test]
fn unpinned_pre_kind_schema_is_read_without_relaxing_read_pin() -> anyhow::Result<()> {
    let current = schema_of::<Nested>()?;
    let legacy = LegacySchemaDocument {
        root: current.root.clone(),
        defs: current.defs.clone(),
    };
    let store = ObjectStore::default();
    let bare = serialize_into(&legacy, &store)?;
    let bare = splice_migration_metadata(&store, &bare);

    assert!(matches!(
        Schema::read_pin(&bare, &store),
        Err(SchemaPinError::Unpinned(tree)) if tree == bare
    ));
    let decoded = Schema::read_pinned_legacy(&bare, &store)?;
    assert_eq!(decoded.kind, Schema::LEGACY_KIND);
    assert_eq!(decoded.root, current.root);
    assert_eq!(decoded.defs, current.defs);
    Ok(())
}

#[test]
fn legacy_schema_rejects_unknown_top_level_entries() -> anyhow::Result<()> {
    let current = schema_of::<Nested>()?;
    let legacy = LegacySchemaDocument {
        root: current.root,
        defs: current.defs,
    };
    let store = ObjectStore::default();
    let bare = serialize_into(&legacy, &store)?;
    let pinned = splice_legacy_pin(&store, &bare);
    let invalid = splice_unexpected_metadata(&store, &pinned);

    assert!(matches!(
        Schema::read_pinned_legacy(&invalid, &store),
        Err(SchemaPinError::LegacyFormat { tree, .. }) if tree == invalid
    ));
    Ok(())
}

/// The kind name is part of the content-addressed schema, and the decoder
/// preserves it from the tree rather than obtaining it from a ref namespace.
#[test]
fn embedded_kind_name_changes_content_and_roundtrips() -> anyhow::Result<()> {
    let base = schema_of::<Person>()?;
    let left = base.clone().with_kind("recipe")?;
    let right = base.with_kind("issue")?;
    let (left_root, left_store) = serialize(&left)?;
    let (right_root, _right_store) = serialize(&right)?;
    assert_ne!(left_root, right_root);

    let decoded: Schema = deserialize(&left_root, &left_store)?;
    assert_eq!(decoded.kind, "recipe");
    assert_eq!(decoded, left);
    Ok(())
}

/// Embedded names use the same single-segment Git ref rules as the higher
/// storage layer, with the first violated rule reported to the caller.
#[test]
fn embedded_kind_name_validation_is_actionable() {
    let mut doc = schema_of::<Person>().expect("Person schema");
    let err = doc.set_kind("bad name").unwrap_err();
    assert!(
        matches!(&err, SchemaError::InvalidKindName { name, reason } if name == "bad name" && reason.contains("spaces")),
        "expected actionable kind-name error, got {err:?}"
    );
}

/// A recursive schema (with a `Ref` cycle) roundtrips.
#[test]
fn recursive_schema_doc_roundtrips() -> anyhow::Result<()> {
    let doc = schema_of::<TreeNode>()?;
    let (root, store) = serialize(&doc)?;
    let back: Schema = deserialize(&root, &store)?;
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
/// `Node` unit variant such as `String`/`U32`/`Bool` now tags with a bare
/// blob holding its name instead of a tree wrapping an empty tree, so
/// `Person`'s schema — three unit-variant scalar fields — reproduces to a
/// different, but still deterministic, root id.
///
/// Updated again for the leaf trailing-newline rule (issue 5b39f084): every
/// leaf blob, including a unit variant's name blob, now carries a mandatory
/// trailing `\n`, which changes every blob (and therefore every containing
/// tree) in the document.
///
/// Updated again for name-keyed struct/enum schema nodes: `Node::Struct`
/// and `Node::Enum` are now `BTreeMap`s keyed by field/variant name rather
/// than ordinal-indexed lists of `FieldSchema`/`VariantSchema` pairs, so a
/// struct's fields serialize directly under their own names (`Struct/name`)
/// instead of behind an ordinal directory holding separate `name`/`schema`
/// entries (`Struct/0000/{name,schema}`). Declaration order is no longer
/// recorded — it was never load-bearing for the codec — so `Person`'s schema
/// reproduces to a different, but still deterministic, root id.
///
/// Updated again for the schema-schema pin: `Schema::version` is gone —
/// `schema_of::<Person>()` itself is unpinned, so this golden id covers the
/// bare document (`kind`/`root`/`defs`, no storage pin). The pin is a
/// storage-layer splice [`Schema::write_pinned`] adds on top, covered
/// separately by `genesis_constant_is_real`.
///
/// Updated again for the field-level default-presence marker:
/// `Node::Struct`'s field map now holds `StructField { node, has_default }`
/// instead of a bare `Node`, so each field's own entry is a small tree
/// (`node`, `has_default`) rather than the field's schema directly, moving
/// every struct field's encoding and therefore this root id.
#[test]
fn person_schema_golden_oid() -> anyhow::Result<()> {
    let doc = schema_of::<Person>()?;
    let (root, _store) = serialize(&doc)?;
    assert_eq!(root.to_string(), "e3d79a02fa322d49db71f976a15ec5fbe5ddf5cc");
    Ok(())
}

/// Regression for issue 8d109650's second repro: changing a schema field's
/// type (`U32` → `String`, mirroring `refs/schema/recipe~1` vs.
/// `refs/schema/recipe`) must change a *blob*, at a stable path, not just an
/// empty tree's entry name — otherwise `git diff` on the schema ref sees
/// nothing, exactly the bug this issue reports.
///
/// `Node::U32` and `Node::String` are themselves unit variants of the
/// `Node` enum, so this exercises the same blob-collapse fix as the
/// `priority: Low → High` value-level repro, just one level up (in the
/// schema's own self-hosted encoding).
#[test]
fn schema_field_type_change_is_a_blob_level_diff() -> anyhow::Result<()> {
    fn recipe_doc(servings_schema: Node) -> Schema {
        let mut defs = BTreeMap::new();
        defs.insert(
            "Recipe".to_string(),
            Node::Struct(BTreeMap::from([(
                "servings".to_string(),
                StructField {
                    node: servings_schema,
                    has_default: false,
                },
            )])),
        );
        Schema {
            kind: "Recipe".into(),
            root: Node::Ref("Recipe".into()),
            defs,
        }
    }

    let (before_root, before_store) = serialize(&recipe_doc(Node::U32))?;
    let (after_root, after_store) = serialize(&recipe_doc(Node::String))?;
    assert_ne!(
        before_root, after_root,
        "changing the field's schema must change the document's root id"
    );

    // Walk the same path in both trees: defs → Recipe → Struct → servings →
    // node — the field is name-keyed rather than living under an ordinal, and
    // `servings` is itself a `StructField` tree (`node`, `has_default`) since
    // the default-presence marker was added.
    let walk = |store: &facet_git_tree::ObjectStore, root: &facet_git_tree::ObjectId| {
        let defs = find_entry(store, root, "defs");
        let recipe = find_entry(store, &defs.oid, "Recipe");
        let struct_ = find_entry(store, &recipe.oid, "Struct");
        let servings = find_entry(store, &struct_.oid, "servings");
        find_entry(store, &servings.oid, "node")
    };
    let before_schema = walk(&before_store, &before_root);
    let after_schema = walk(&after_store, &after_root);

    assert_eq!(
        before_schema.mode.kind(),
        EntryKind::Blob,
        "a unit-variant Node node's own tag must be a blob"
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

// --- schema-schema pin ---

/// Golden object id of [`SchemaSchema::GENESIS`]: `serialize(&schema_of::<
/// Schema>()?)`'s root, with the shared [`codec`] fixture spliced under
/// `codec` and no pin spliced in — the value the compiled-in hex constant in
/// `pin.rs` must match.
///
/// This is the golden-oid guard for the whole format, one level above
/// [`person_schema_golden_oid`]: if this id changes, `Schema`'s own shape
/// changed, the encoding did, or the codec fixture did, and every schema tree
/// in every downstream repository pins a generation that no longer exists.
/// Do not update the compiled-in constant without releasing accordingly.
#[test]
fn genesis_constant_is_real() -> anyhow::Result<()> {
    let store = ObjectStore::default();
    let doc = schema_of::<Schema>()?;
    let root = serialize_into(&doc, &store)?;
    let root = splice_empty_schema(&store, &root);
    let root = splice_codec(&store, &root);

    assert_eq!(&root, SchemaSchema::GENESIS.tree());
    Ok(())
}

/// The library bootstrap check exercises the full canonical decode/re-encode
/// path and verifies the materialized generation digest.
#[test]
fn canonical_schema_fixed_point_passes() -> anyhow::Result<()> {
    let store = ObjectStore::default();
    SchemaSchema::check_fixed_point(&store)?;
    Ok(())
}

/// A document written by [`Schema::write_pinned`] carries a top-level
/// `schema` tree entry naming [`SchemaSchema::CURRENT`], that pinned object
/// is actually present in the store, and [`Schema::read_pin`] resolves it
/// back to [`SchemaSchema::CURRENT`].
#[test]
fn write_pinned_document_has_a_resolvable_pin() -> anyhow::Result<()> {
    let doc = schema_of::<Person>()?;
    let store = ObjectStore::default();
    let root = doc.write_pinned(&store)?;

    let pin_entry = find_entry(&store, &root, SchemaSchema::ENTRY);
    assert_eq!(pin_entry.mode.kind(), EntryKind::Tree);
    assert_eq!(&pin_entry.oid, SchemaSchema::CURRENT.tree());
    assert!(
        matches!(
            store.get(&pin_entry.oid),
            Some(facet_git_tree::GitObject::Tree(_))
        ),
        "the pinned schema-schema tree must actually be present in the store"
    );

    let recognized = Schema::read_pin(&root, &store)?;
    assert_eq!(recognized.tree(), SchemaSchema::CURRENT.tree());
    Ok(())
}

/// The canonical meta-schema carries an empty-tree `schema` entry. The
/// empty entry is the fixed-point base case, and [`Schema::read_pin`] resolves
/// it as genesis.
#[test]
fn empty_schema_pin_on_the_meta_schema_reads_as_genesis() -> anyhow::Result<()> {
    let store = ObjectStore::default();
    let doc = schema_of::<Schema>()?;
    let root = serialize_into(&doc, &store)?;
    let root = splice_empty_schema(&store, &root);
    let root = splice_codec(&store, &root);

    let recognized = Schema::read_pin(&root, &store)?;
    assert_eq!(recognized.tree(), SchemaSchema::GENESIS.tree());
    assert!(recognized.parent().is_none());
    Ok(())
}

/// No pin entry, and the tree's own id is *not* a known root generation: a
/// truncated or hand-written document, rejected as
/// [`SchemaPinError::Unpinned`] rather than read as though it were genesis.
#[test]
fn absent_pin_on_an_unknown_tree_is_rejected() -> anyhow::Result<()> {
    let doc = schema_of::<Person>()?;
    let store = ObjectStore::default();
    let root = doc.write_pinned(&store)?;

    let entries: Vec<_> = store
        .get_tree(&root)
        .expect("root is a tree")
        .into_iter()
        .filter(|e| e.filename != SchemaSchema::ENTRY)
        .collect();
    let stripped = store.write(&gix_object::Tree { entries }).unwrap();

    let err = Schema::read_pin(&stripped, &store).unwrap_err();
    assert!(
        matches!(err, SchemaPinError::Unpinned(oid) if oid == stripped),
        "{err:?}"
    );
    Ok(())
}

/// The whole point of reading the pin out of band: an unrecognized pin is
/// caught *before* a full typed deserialize is attempted, which is exactly
/// what lets it catch a document a full deserialize could not otherwise get
/// through. Simulated here exactly as the old version-marker tests did: the
/// pin is repointed at some other, non-schema-schema tree (the genesis
/// document's own `defs` subtree), and `root` is separately corrupted into a
/// bogus blob tagged with a hypothetical `DateTime` variant `Node` does not
/// define.
#[test]
fn unrecognized_pin_is_rejected_before_a_full_deserialize_is_attempted() -> anyhow::Result<()> {
    let doc = schema_of::<Nested>()?;
    let store = ObjectStore::default();
    let root = doc.write_pinned(&store)?;

    let genesis_doc = schema_of::<Schema>()?;
    let genesis_root = serialize_into(&genesis_doc, &store)?;
    let bogus_pin = find_entry(&store, &genesis_root, "defs").oid;

    let bogus_root_blob = store
        .write_buf(gix_object::Kind::Blob, b"DateTime\n")
        .unwrap();

    let mut entries = store.get_tree(&root).expect("root is a tree");
    for entry in &mut entries {
        if entry.filename == SchemaSchema::ENTRY {
            entry.oid = bogus_pin;
        } else if entry.filename == "root" {
            entry.oid = bogus_root_blob;
            entry.mode = gix_object::tree::EntryKind::Blob.into();
        }
    }
    let corrupt = store.write(&gix_object::Tree { entries }).unwrap();

    let err = Schema::read_pin(&corrupt, &store).unwrap_err();
    assert!(
        matches!(
            &err,
            SchemaPinError::Unrecognized { tree, pinned }
                if *tree == corrupt && *pinned == bogus_pin
        ),
        "{err:?}"
    );

    // A full typed deserialize, in contrast, cannot get past the unknown
    // variant — a reflection error, not a pin error — confirming the check
    // really did land before a deserialize that could not have completed.
    let err = deserialize::<Schema>(&corrupt, &store).unwrap_err();
    assert!(
        matches!(&err, DeserializeError::Reflect(msg) if msg.contains("DateTime")),
        "{err:?}"
    );
    Ok(())
}

/// `git ls-tree -r` shape of a small struct's `write_pinned` document: the
/// exact sorted set of blob leaves under `defs`/`root` reads like a type
/// declaration, plus a `schema` subtree at the top level (its own contents —
/// the genesis schema-schema document — are covered by the pin tests above,
/// not re-walked here).
#[test]
fn write_pinned_ls_tree_shape_matches_the_type_declaration() -> anyhow::Result<()> {
    let doc = schema_of::<Person>()?;
    let store = ObjectStore::default();
    let root = doc.write_pinned(&store)?;

    let mut leaves = Vec::new();
    for top in ["defs", "root"] {
        let entry = find_entry(&store, &root, top);
        walk_blobs(&store, &entry.oid, top, &mut leaves);
    }
    leaves.sort();

    assert_eq!(
        leaves,
        vec![
            (
                "defs/Person/Struct/active/has_default".to_owned(),
                b"false\n".to_vec()
            ),
            (
                "defs/Person/Struct/active/node".to_owned(),
                b"Bool\n".to_vec()
            ),
            (
                "defs/Person/Struct/age/has_default".to_owned(),
                b"false\n".to_vec()
            ),
            ("defs/Person/Struct/age/node".to_owned(), b"U32\n".to_vec()),
            (
                "defs/Person/Struct/name/has_default".to_owned(),
                b"false\n".to_vec()
            ),
            (
                "defs/Person/Struct/name/node".to_owned(),
                b"String\n".to_vec()
            ),
            ("root/Ref".to_owned(), b"Person\n".to_vec()),
        ]
    );

    let pin_entry = find_entry(&store, &root, SchemaSchema::ENTRY);
    assert_eq!(pin_entry.mode.kind(), EntryKind::Tree);
    assert_eq!(&pin_entry.oid, SchemaSchema::CURRENT.tree());
    Ok(())
}

/// Recursively collect every blob leaf under `root` as `(slash-joined path,
/// content)` pairs, `git ls-tree -r` style.
fn walk_blobs(
    store: &ObjectStore,
    root: &ObjectId,
    prefix: &str,
    out: &mut Vec<(String, Vec<u8>)>,
) {
    for entry in store.get_tree(root).expect("tree present") {
        let name = String::from_utf8_lossy(&entry.filename);
        let path = format!("{prefix}/{name}");
        match entry.mode.kind() {
            EntryKind::Blob => {
                let content = store.get_blob(&entry.oid).expect("blob present");
                out.push((path, content));
            }
            _ => walk_blobs(store, &entry.oid, &path, out),
        }
    }
}
