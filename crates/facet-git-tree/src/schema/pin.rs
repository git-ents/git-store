//! The schema-schema pin: every stored [`Schema`] tree names, as a
//! [`SchemaSchema::ENTRY`] entry spliced onto its tree at write time, the
//! generation of the schema-schema it was written against.
//!
//! The pin is a storage-layer splice, not a Rust field — a `schema:` field on
//! `Schema` would make [`schema_of::<Schema>()`](crate::schema_of)
//! describe the pin and recurse forever. It works the same way `gix-store`'s
//! subtree schema binding splices `{value/, schema/}` onto a data commit's
//! tree: the pinned tree sits beside the document it governs, reachable by
//! ordinary tree walking, with nothing to deserialize to find it.
//!
//! Each generation's own tree also carries a [`codec`] entry: a fixture value
//! exercising every construct the codec can encode, so a change to how a
//! value is *spelled* — not just to `Schema`'s own shape — moves the
//! generation id too.

use gix_object::{Find, Write};

use crate::de::find_tree_entries;
use crate::error::{DeserializeError, SchemaPinError, SerializeError};
use crate::schema::{Schema, codec, schema_of};
use crate::ser::serialize_into;
use crate::{EntryKind, EntryMode, ObjectId, TreeEntry};

/// One generation of the schema-schema: the tree that a `Schema` written
/// by that generation pins, and the generation it pins in turn.
///
/// Generations chain by reachability — generation N's own tree carries a
/// [`ENTRY`](Self::ENTRY) entry naming N-1 — which is what restores the
/// *ordering* a version number gave and a bare object id does not.
#[derive(Debug)]
pub struct SchemaSchema {
    tree: ObjectId,
    parent: Option<&'static SchemaSchema>,
}

impl SchemaSchema {
    /// The tree entry name a document pins its schema-schema under.
    pub const ENTRY: &'static str = "schema";

    /// Generation zero: `serialize(&schema_of::<Schema>()?)` with no pin
    /// spliced in — the recursion bottoms out here.
    ///
    /// Changing this id is a semver-major break; see
    /// `genesis_constant_is_real` in `tests/schema_self_host.rs`, which pins
    /// it against the actual serialization.
    pub const GENESIS: SchemaSchema = SchemaSchema {
        tree: decode_oid(GENESIS_HEX),
        parent: None,
    };

    /// The generation this build writes.
    pub const CURRENT: &'static SchemaSchema = &Self::GENESIS;

    /// Every generation this build speaks, oldest first. The known-roots set
    /// later generations extend.
    pub const KNOWN: &'static [&'static SchemaSchema] = &[&Self::GENESIS];

    /// This generation's own schema-schema tree id.
    pub const fn tree(&self) -> &ObjectId {
        &self.tree
    }

    /// The generation this one pins, if any.
    pub const fn parent(&self) -> Option<&'static SchemaSchema> {
        self.parent
    }

    /// The known generation whose tree is `tree`, if any.
    pub fn recognize(tree: &ObjectId) -> Option<&'static SchemaSchema> {
        Self::KNOWN.iter().copied().find(|g| g.tree == *tree)
    }
}

/// Hex text for [`SchemaSchema::GENESIS`]'s tree id, kept as a named constant
/// so it stays human-checkable against the golden-oid test.
const GENESIS_HEX: &str = "b82f18bc0b9f8c5d389d0ca161480365d72b08d6";

/// Decode a 40-character lowercase-hex SHA-1 literal at compile time, so a
/// malformed constant is a compile error rather than a silent runtime bug.
///
/// `pub(crate)`: shared with `migration::pin`, which pins its own tower's
/// genesis the same way.
pub(crate) const fn decode_oid(hex: &str) -> ObjectId {
    let bytes = hex.as_bytes();
    assert!(
        bytes.len() == 40,
        "genesis id must be exactly 40 hex characters"
    );
    let mut out = [0u8; 20];
    let mut i = 0;
    while i < 20 {
        out[i] = (hex_nibble(bytes[2 * i]) << 4) | hex_nibble(bytes[2 * i + 1]);
        i += 1;
    }
    ObjectId::Sha1(out)
}

/// One hex digit's value, as a `const fn` (no `unsafe`, no dependency).
pub(crate) const fn hex_nibble(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        _ => panic!("invalid hex digit in genesis id"),
    }
}

/// The current generation's own schema-schema document —
/// `schema_of::<Schema>()`, unconditionally describable since it is this
/// crate's own fixed shape.
fn schema_schema_doc() -> Schema {
    schema_of::<Schema>().expect("Schema's own shape is always describable")
}

/// Add an entry named `name` pointing at `target` to the already-written
/// tree `doc`.
///
/// Shared by both towers' pin splice ([`splice_pin`]) and their codec splice
/// (`materialize`, here and in `migration::pin`); generic over the error so
/// each tower keeps its own, rather than one collapsing into the other at a
/// call site that would have to name conditions this function cannot
/// produce.
pub(crate) fn splice_entry<S, E>(
    doc: ObjectId,
    name: &str,
    target: &ObjectId,
    store: &S,
) -> Result<ObjectId, E>
where
    S: Write + Find + ?Sized,
    E: From<SerializeError> + From<DeserializeError>,
{
    let mut entries: Vec<TreeEntry> = find_tree_entries(&doc, store)?
        .into_iter()
        .map(|(entry_name, oid, kind)| TreeEntry {
            mode: EntryMode::from(kind),
            filename: entry_name.into(),
            oid,
        })
        .collect();
    entries.push(TreeEntry {
        mode: EntryMode::from(EntryKind::Tree),
        filename: name.into(),
        oid: *target,
    });
    entries.sort();
    store
        .write(&gix_object::Tree { entries })
        .map_err(SerializeError::Backend)
        .map_err(E::from)
}

/// Add the [`SchemaSchema::ENTRY`] entry naming `pin` to the already-written
/// tree `doc`.
///
/// Shared with `migration::pin`, whose entry name (`MigrationSchema::ENTRY`)
/// is the same literal `"schema"`.
pub(crate) fn splice_pin<S, E>(doc: ObjectId, pin: &ObjectId, store: &S) -> Result<ObjectId, E>
where
    S: Write + Find + ?Sized,
    E: From<SerializeError> + From<DeserializeError>,
{
    splice_entry(doc, SchemaSchema::ENTRY, pin, store)
}

/// Write the current generation's own tree — including its [`codec`]
/// fixture — into `store`, so a pin to it resolves from the store alone.
fn materialize<S: Write + Find + ?Sized>(store: &S) -> Result<ObjectId, SchemaPinError> {
    let tree = serialize_into(&schema_schema_doc(), store)?;
    let codec_tree = codec::codec_tree(store)?;
    let tree = splice_entry::<S, SchemaPinError>(tree, codec::ENTRY, &codec_tree, store)?;
    match SchemaSchema::CURRENT.parent() {
        Some(parent) => splice_pin(tree, parent.tree(), store),
        None => Ok(tree),
    }
}

impl Schema {
    /// Write this document into `store` with the schema-schema pin spliced
    /// in, returning the stored tree's id.
    ///
    /// The pinned generation's own tree is written too, so the pin resolves
    /// from `store` alone.
    pub fn write_pinned<S: Write + Find + ?Sized>(
        &self,
        store: &S,
    ) -> Result<ObjectId, SchemaPinError> {
        materialize(store)?;
        let doc_tree = serialize_into(self, store)?;
        splice_pin(doc_tree, SchemaSchema::CURRENT.tree(), store)
    }

    /// Which schema-schema `tree` was written against, read out of band — one
    /// `ls-tree` entry lookup, no object read, no deserialize.
    pub fn read_pin<F: Find + ?Sized>(
        tree: &ObjectId,
        store: &F,
    ) -> Result<&'static SchemaSchema, SchemaPinError> {
        let entries = find_tree_entries(tree, store)?;
        match entries
            .iter()
            .find(|(name, _, _)| name == SchemaSchema::ENTRY)
        {
            Some((_, pinned, _)) => {
                SchemaSchema::recognize(pinned).ok_or(SchemaPinError::Unrecognized {
                    tree: *tree,
                    pinned: *pinned,
                })
            }
            // No pin entry: legitimate only when `tree` is itself a known
            // root generation — otherwise this is a truncated or
            // hand-written document, not the bottom of the tower.
            None => SchemaSchema::recognize(tree)
                .filter(|generation| generation.parent().is_none())
                .ok_or(SchemaPinError::Unpinned(*tree)),
        }
    }

    /// Read a stored schema document, refusing one this build does not speak
    /// *before* deserializing it.
    ///
    /// A document from a newer binary may contain a `Node` variant this
    /// build has never heard of, and a typed deserialize attempted first
    /// would fail with an opaque reflection error before the pin was ever
    /// checked — so [`read_pin`](Self::read_pin) runs first, unconditionally.
    pub fn read_pinned<F: Find + ?Sized>(
        tree: &ObjectId,
        store: &F,
    ) -> Result<Schema, SchemaPinError> {
        Self::read_pin(tree, store)?;
        Ok(crate::de::deserialize(tree, store)?)
    }
}

/// The known generations, rendered for [`SchemaPinError::Unrecognized`]'s
/// message. A private helper rather than a stored field: `KNOWN` is a
/// compile-time constant, not state the error needs to carry.
pub(crate) fn known_generations() -> String {
    SchemaSchema::KNOWN
        .iter()
        .map(|g| g.tree().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}
