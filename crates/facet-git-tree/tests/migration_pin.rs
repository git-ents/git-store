//! Integration tests for the migration-schema pin: every stored [`Migration`]
//! tree names, out of band, the generation of `schema_of::<Migration>()` it
//! was written against — exactly as the schema-schema pin does for
//! [`Schema`] (see `tests/schema_self_host.rs`), but for a separate,
//! independently-evolving tower.

use facet_git_tree::{
    EntryKind, Migration, MigrationPinError, MigrationSchema, ObjectStore, SchemaSchema, schema_of,
    serialize, serialize_into,
};
use gix_object::Write as _;

mod common;
use common::find_entry;

fn sample_migration() -> Migration {
    use facet_git_tree::{Change, Op, Target};

    Migration {
        ops: vec![
            Op {
                at: Target::Def("Issue".into()),
                change: Change::Rename {
                    from: "old_id".into(),
                    to: "id".into(),
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
        ],
    }
}

/// Golden object id of [`MigrationSchema::GENESIS`]: `serialize(&schema_of::<
/// Migration>()?)`'s root, with no pin spliced in — the value the compiled-in
/// hex constant in `migration/pin.rs` must match.
///
/// Do not update the compiled-in constant without releasing accordingly: it
/// covers `Migration`'s own on-disk shape.
#[test]
fn genesis_constant_is_real() -> anyhow::Result<()> {
    let doc = schema_of::<Migration>()?;
    let (root, _store) = serialize(&doc)?;
    assert_eq!(&root, MigrationSchema::GENESIS.tree());
    Ok(())
}

/// A non-trivial `Migration` written by [`Migration::write_pinned`] carries a
/// resolvable pin and roundtrips back through [`Migration::read_pinned`].
#[test]
fn write_pinned_then_read_pinned_roundtrips() -> anyhow::Result<()> {
    let value = sample_migration();
    let store = ObjectStore::default();
    let root = value.write_pinned(&store)?;

    let pin_entry = find_entry(&store, &root, MigrationSchema::ENTRY);
    assert_eq!(pin_entry.mode.kind(), EntryKind::Tree);
    assert_eq!(&pin_entry.oid, MigrationSchema::CURRENT.tree());
    assert!(
        matches!(
            store.get(&pin_entry.oid),
            Some(facet_git_tree::GitObject::Tree(_))
        ),
        "the pinned migration-schema tree must actually be present in the store"
    );

    let back = Migration::read_pinned(&root, &store)?;
    assert_eq!(back, value);
    Ok(())
}

/// A migration tree written without the pin (plain [`serialize_into`], no
/// `schema` entry spliced in) is rejected by [`Migration::read_pin`] with
/// [`MigrationPinError::Unpinned`] — it is not itself a known root
/// generation, so it cannot be legitimately unpinned.
#[test]
fn unpinned_tree_is_rejected() -> anyhow::Result<()> {
    let value = sample_migration();
    let store = ObjectStore::default();
    let root = serialize_into(&value, &store)?;

    let err = Migration::read_pin(&root, &store).unwrap_err();
    assert!(
        matches!(err, MigrationPinError::Unpinned(oid) if oid == root),
        "{err:?}"
    );
    Ok(())
}

/// A tree carrying a pin entry that names an unrecognized migration-schema
/// tree is rejected with [`MigrationPinError::Unrecognized`] *before* a full
/// typed deserialize is attempted.
#[test]
fn unrecognized_pin_is_rejected_before_a_full_deserialize_is_attempted() -> anyhow::Result<()> {
    let value = sample_migration();
    let store = ObjectStore::default();
    let root = value.write_pinned(&store)?;

    // Repoint the pin at some other, non-migration-schema tree: the
    // migration document's own `ops` subtree.
    let bogus_pin = find_entry(&store, &root, "ops").oid;

    let mut entries = store.get_tree(&root).expect("root is a tree");
    for entry in &mut entries {
        if entry.filename == MigrationSchema::ENTRY {
            entry.oid = bogus_pin;
        }
    }
    let corrupt = store.write(&gix_object::Tree { entries }).unwrap();

    let err = Migration::read_pin(&corrupt, &store).unwrap_err();
    assert!(
        matches!(
            &err,
            MigrationPinError::Unrecognized { tree, pinned }
                if *tree == corrupt && *pinned == bogus_pin
        ),
        "{err:?}"
    );

    // read_pinned refuses the same way, without ever reaching deserialize.
    let err = Migration::read_pinned(&corrupt, &store).unwrap_err();
    assert!(
        matches!(err, MigrationPinError::Unrecognized { .. }),
        "{err:?}"
    );
    Ok(())
}

/// The migration tower and the schema-schema tower are independent: their
/// genesis object ids must differ, asserted rather than assumed.
#[test]
fn migration_genesis_differs_from_schema_genesis() {
    assert_ne!(
        MigrationSchema::GENESIS.tree(),
        SchemaSchema::GENESIS.tree(),
        "the migration and schema-schema towers must pin independently"
    );
}
