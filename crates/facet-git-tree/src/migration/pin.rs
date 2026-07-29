//! The migration-schema pin: every stored [`Migration`] tree names, as a
//! [`MigrationSchema::ENTRY`] entry spliced onto its tree at write time, the
//! generation of `schema_of::<Migration>()` it was written against.
//!
//! A `Migration` is a stored, self-hosted document exactly as [`Schema`]
//! is, and its own shape will evolve — a future generation may add a
//! [`Change`](crate::migration::Change) variant. A reader that silently
//! ignored an operation it did not understand would produce a *wrong* value,
//! which is strictly worse than the schema case, so a stored migration
//! carries the same out-of-band pin: checked with one `ls-tree` lookup
//! *before* any deserialize.
//!
//! This tower is separate from the schema-schema tower
//! ([`crate::schema::pin`]): the two documents evolve independently, and
//! adding a `Change` variant must not invalidate every stored `Schema`.
//!
//! Each generation's own tree also carries a [`codec`] entry, exactly as the
//! schema-schema tower's does — the *same* fixture, spliced under the same
//! name, so the two towers share one content-addressed `codec` object.

use gix_object::{Find, Write};

use crate::ObjectId;
use crate::de::find_tree_entries;
use crate::error::MigrationPinError;
use crate::migration::Migration;
use crate::schema::pin::{decode_oid, splice_entry, splice_pin};
use crate::schema::{Schema, codec, schema_of};
use crate::ser::serialize_into;

/// One generation of the migration document's own schema: the tree that a
/// `Migration` written by that generation pins, and the generation it pins in
/// turn.
///
/// Generations chain by reachability exactly as [`SchemaSchema`]'s do — see
/// that type's documentation for why a bare object id is not enough on its
/// own.
///
/// [`SchemaSchema`]: crate::schema::pin::SchemaSchema
#[derive(Debug)]
pub struct MigrationSchema {
    tree: ObjectId,
    parent: Option<&'static MigrationSchema>,
}

impl MigrationSchema {
    /// The tree entry name a stored migration pins its generation under.
    pub const ENTRY: &'static str = "schema";

    /// Generation zero: `serialize(&schema_of::<Migration>()?)` with no pin
    /// spliced in — the recursion bottoms out here.
    ///
    /// Changing this id is a semver-major break; see
    /// `genesis_constant_is_real` in `tests/migration_pin.rs`, which pins it
    /// against the actual serialization.
    pub const GENESIS: MigrationSchema = MigrationSchema {
        tree: decode_oid(GENESIS_HEX),
        parent: None,
    };

    /// The generation this build writes.
    pub const CURRENT: &'static MigrationSchema = &Self::GENESIS;

    /// Every generation this build speaks, oldest first. The known-roots set
    /// later generations extend.
    pub const KNOWN: &'static [&'static MigrationSchema] = &[&Self::GENESIS];

    /// This generation's own migration-schema tree id.
    pub const fn tree(&self) -> &ObjectId {
        &self.tree
    }

    /// The generation this one pins, if any.
    pub const fn parent(&self) -> Option<&'static MigrationSchema> {
        self.parent
    }

    /// The known generation whose tree is `tree`, if any.
    pub fn recognize(tree: &ObjectId) -> Option<&'static MigrationSchema> {
        Self::KNOWN.iter().copied().find(|g| g.tree == *tree)
    }
}

/// Hex text for [`MigrationSchema::GENESIS`]'s tree id, kept as a named
/// constant so it stays human-checkable against the golden-oid test.
const GENESIS_HEX: &str = "0afeeeb9f8e78c485199757eea274bb1d0e8a8db";

/// The current generation's own migration-schema document —
/// `schema_of::<Migration>()`, unconditionally describable since it is this
/// crate's own fixed shape.
fn migration_schema_doc() -> Schema {
    schema_of::<Migration>().expect("Migration's own shape is always describable")
}

/// Write the current generation's own tree — including its [`codec`]
/// fixture — into `store`, so a pin to it resolves from the store alone.
fn materialize<S: Write + Find + ?Sized>(store: &S) -> Result<ObjectId, MigrationPinError> {
    let tree = serialize_into(&migration_schema_doc(), store)?;
    let codec_tree = codec::codec_tree(store)?;
    let tree = splice_entry::<S, MigrationPinError>(tree, codec::ENTRY, &codec_tree, store)?;
    match MigrationSchema::CURRENT.parent() {
        Some(parent) => splice_pin(tree, parent.tree(), store),
        None => Ok(tree),
    }
}

impl Migration {
    /// Write this document into `store` with the migration-schema pin
    /// spliced in, returning the stored tree's id.
    ///
    /// The pinned generation's own tree is written too, so the pin resolves
    /// from `store` alone.
    pub fn write_pinned<S: Write + Find + ?Sized>(
        &self,
        store: &S,
    ) -> Result<ObjectId, MigrationPinError> {
        materialize(store)?;
        let doc_tree = serialize_into(self, store)?;
        splice_pin(doc_tree, MigrationSchema::CURRENT.tree(), store)
    }

    /// Which migration-schema `tree` was written against, read out of band —
    /// one `ls-tree` entry lookup, no object read, no deserialize.
    pub fn read_pin<F: Find + ?Sized>(
        tree: &ObjectId,
        store: &F,
    ) -> Result<&'static MigrationSchema, MigrationPinError> {
        let entries = find_tree_entries(tree, store)?;
        match entries
            .iter()
            .find(|(name, _, _)| name == MigrationSchema::ENTRY)
        {
            Some((_, pinned, _)) => {
                MigrationSchema::recognize(pinned).ok_or(MigrationPinError::Unrecognized {
                    tree: *tree,
                    pinned: *pinned,
                })
            }
            // No pin entry: legitimate only when `tree` is itself a known
            // root generation — otherwise this is a truncated or
            // hand-written document, not the bottom of the tower.
            None => MigrationSchema::recognize(tree)
                .filter(|generation| generation.parent().is_none())
                .ok_or(MigrationPinError::Unpinned(*tree)),
        }
    }

    /// Read a stored migration, refusing one this build does not speak
    /// *before* deserializing it.
    ///
    /// A migration from a newer binary may contain a `Change` variant this
    /// build has never heard of, and a typed deserialize attempted first
    /// would fail with an opaque reflection error before the pin was ever
    /// checked — so [`read_pin`](Self::read_pin) runs first, unconditionally.
    pub fn read_pinned<F: Find + ?Sized>(
        tree: &ObjectId,
        store: &F,
    ) -> Result<Migration, MigrationPinError> {
        Self::read_pin(tree, store)?;
        Ok(crate::de::deserialize(tree, store)?)
    }
}

/// The known generations, rendered for [`MigrationPinError::Unrecognized`]'s
/// message. A private helper rather than a stored field: `KNOWN` is a
/// compile-time constant, not state the error needs to carry.
pub(crate) fn known_generations() -> String {
    MigrationSchema::KNOWN
        .iter()
        .map(|g| g.tree().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}
