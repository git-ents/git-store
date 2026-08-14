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

use std::collections::BTreeMap;

use gix_object::{Find, Write};

use crate::de::find_tree_entries;
use crate::error::{DeserializeError, SchemaPinError, SerializeError};
use crate::schema::{Node, Schema, codec, schema_of};
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

    /// The last schema-schema generation before [`Schema::kind`] existed.
    ///
    /// This generation is read-only compatibility support. Its documents have
    /// the historical `{root, defs}` shape and are decoded with
    /// [`LegacySchema`], then represented as [`Schema::LEGACY_KIND`]. It is
    /// deliberately not used by any writer.
    pub const LEGACY: SchemaSchema = SchemaSchema {
        tree: decode_oid(LEGACY_HEX),
        parent: None,
    };

    /// Generation zero: the canonical reification of `Schema`'s
    /// [`facet::Shape`], with its own `schema` entry pointing at Git's empty
    /// tree. That empty entry is the fixed-point base case: the meta-schema
    /// cannot point at itself before its object id exists.
    ///
    /// Changing this id is a semver-major break; see
    /// `genesis_constant_is_real` in `tests/schema_self_host.rs`, which pins
    /// it against the actual serialization and fixture materialization.
    pub const GENESIS: SchemaSchema = SchemaSchema {
        tree: decode_oid(GENESIS_HEX),
        parent: None,
    };

    /// The generation this build writes.
    pub const CURRENT: &'static SchemaSchema = &Self::GENESIS;

    /// Every generation this build speaks, oldest first. The legacy entry is
    /// retained solely so documents written before `Schema.kind` remain
    /// readable; new documents always pin [`Self::CURRENT`].
    pub const KNOWN: &'static [&'static SchemaSchema] = &[&Self::LEGACY, &Self::GENESIS];

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

    /// Check the canonical schema-schema fixed point in `store`.
    ///
    /// This performs one checked bootstrap sequence: construct the canonical
    /// meta-schema, add the empty-tree `schema` base entry, read it through the
    /// ordinary pinned-schema path, re-encode it, and compare the complete
    /// materialized tree with the compile-time digest. Keeping this check here
    /// makes bootstrap validation available to library users without coupling
    /// it to a CLI or a ref namespace.
    pub fn check_fixed_point<S: Write + Find + ?Sized>(store: &S) -> Result<(), SchemaPinError> {
        let schema_tree = canonical_tree(store)?;
        Schema::read_pin(&schema_tree, store)?;
        let decoded: Schema = crate::de::deserialize(&schema_tree, store).map_err(|source| {
            SchemaPinError::FixedPointDecode {
                expected: *Self::CURRENT.tree(),
                observed: schema_tree,
                source,
            }
        })?;
        decoded.validate()?;

        let reencoded = canonical_tree_from_doc(&decoded, store)?;
        if reencoded != schema_tree {
            return Err(SchemaPinError::FixedPoint {
                stage: "schema re-encode",
                expected: schema_tree,
                observed: reencoded,
            });
        }
        if reencoded != *Self::CURRENT.tree() {
            return Err(SchemaPinError::FixedPoint {
                stage: "compile-time digest validation",
                expected: *Self::CURRENT.tree(),
                observed: reencoded,
            });
        }
        Ok(())
    }
}

/// Git's well-known empty tree id. It is the schema identity of the
/// meta-schema itself, so the fixed-point construction has a finite base case.
///
/// This crate currently speaks SHA-1 Git objects only.
pub const EMPTY_TREE: ObjectId = decode_oid(EMPTY_TREE_HEX);
const EMPTY_TREE_HEX: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// The historical pre-`kind` schema-schema tree id.
const LEGACY_HEX: &str = "5896cb1c8ff662027c9a54232a5364a5072b60c1";

/// Hex text for [`SchemaSchema::GENESIS`]'s tree id, kept as a named constant
/// so it stays human-checkable against the golden-oid test.
const GENESIS_HEX: &str = "ea875f69726986da822cdb2670a089eddd09b6ce";

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

/// Construct the canonical meta-schema tree. Its `schema` entry is the
/// empty-tree identity; the codec fixture remains beside it so changes in
/// spelling are still covered by the generation digest.
fn canonical_tree<S: Write + Find + ?Sized>(store: &S) -> Result<ObjectId, SchemaPinError> {
    canonical_tree_from_doc(&schema_schema_doc(), store)
}

fn canonical_tree_from_doc<S: Write + Find + ?Sized>(
    doc: &Schema,
    store: &S,
) -> Result<ObjectId, SchemaPinError> {
    doc.validate()?;
    let tree = serialize_into(doc, store)?;
    let tree = splice_pin::<S, SchemaPinError>(tree, &EMPTY_TREE, store)?;
    let codec_tree = codec::codec_tree(store)?;
    splice_entry::<S, SchemaPinError>(tree, codec::ENTRY, &codec_tree, store)
}

/// Write the current generation's own tree into `store`, so a pin to it
/// resolves from the store alone.
fn materialize<S: Write + Find + ?Sized>(store: &S) -> Result<ObjectId, SchemaPinError> {
    let tree = canonical_tree(store)?;
    if tree != *SchemaSchema::CURRENT.tree() {
        return Err(SchemaPinError::FixedPoint {
            stage: "generation materialization",
            expected: *SchemaSchema::CURRENT.tree(),
            observed: tree,
        });
    }
    Ok(tree)
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
        self.validate()?;
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
                if *pinned == EMPTY_TREE && *tree == *SchemaSchema::CURRENT.tree() {
                    return Ok(SchemaSchema::CURRENT);
                }
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
    ///
    /// Historical pre-`kind` documents are intentionally not accepted here;
    /// use [`read_pinned_legacy`](Self::read_pinned_legacy) to opt into that
    /// compatibility format.
    pub fn read_pinned<F: Find + ?Sized>(
        tree: &ObjectId,
        store: &F,
    ) -> Result<Schema, SchemaPinError> {
        Self::read_pin(tree, store)?;
        Ok(crate::de::deserialize(tree, store)?)
    }

    /// Read a historical pre-`kind` schema document and accept its legacy leaf
    /// blobs. Forge-era `migration` metadata is ignored after the top-level
    /// shape is validated. This is the explicit compatibility counterpart to
    /// [`read_pinned`](Self::read_pinned); ordinary schema reads remain strict.
    /// Current-generation schema documents are also decoded with the legacy
    /// leaf mode so this method can normalize old values paired with a current
    /// schema.
    pub fn read_pinned_legacy<F: Find + ?Sized>(
        tree: &ObjectId,
        store: &F,
    ) -> Result<Schema, SchemaPinError> {
        let legacy_shape = is_legacy_shape(tree, store)?;
        let generation = match Self::read_pin(tree, store) {
            Ok(generation) => Some(generation),
            Err(err @ SchemaPinError::Unpinned(unpinned)) => {
                if legacy_shape && unpinned == *tree {
                    None
                } else {
                    return Err(err);
                }
            }
            Err(err) => return Err(err),
        };

        let doc = if legacy_shape
            || generation.is_none()
            || generation.is_some_and(|g| g.tree() == SchemaSchema::LEGACY.tree())
        {
            decode_legacy(tree, store)?
        } else {
            crate::de::deserialize_legacy_leaves(tree, store)?
        };
        doc.validate()?;
        Ok(doc)
    }
}

/// Whether `tree` has the historical pre-`kind` schema-document shape.
///
/// Forge-era publications may carry a schema pin and migration metadata beside
/// the two schema fields. The compatibility decoder accepts only those known
/// entries and never treats an arbitrary unpinned tree as a schema.
fn is_legacy_shape<F: Find + ?Sized>(tree: &ObjectId, store: &F) -> Result<bool, SchemaPinError> {
    let names: Vec<String> = find_tree_entries(tree, store)?
        .into_iter()
        .map(|(name, _, _)| name)
        .collect();
    Ok(names.iter().any(|name| name == "root")
        && names.iter().any(|name| name == "defs")
        && names.iter().all(|name| {
            matches!(
                name.as_str(),
                "root" | "defs" | SchemaSchema::ENTRY | "migration"
            )
        }))
}

/// Decode the pre-`kind` schema representation after filtering its storage
/// metadata. Only `root` and `defs` are passed to the legacy leaf decoder;
/// the optional schema pin and migration metadata are not schema fields.
fn decode_legacy<F: Find + ?Sized>(tree: &ObjectId, store: &F) -> Result<Schema, SchemaPinError> {
    let mut root = None;
    let mut defs = None;
    for (name, oid, _) in find_tree_entries(tree, store)? {
        match name.as_str() {
            "root" => root = Some(oid),
            "defs" => defs = Some(oid),
            SchemaSchema::ENTRY | "migration" => {}
            _ => {
                return Err(SchemaPinError::LegacyFormat {
                    tree: *tree,
                    reason: "expected root and defs, with optional schema pin and migration",
                });
            }
        }
    }
    let root = root.ok_or(SchemaPinError::LegacyFormat {
        tree: *tree,
        reason: "expected root and defs, with optional schema pin and migration",
    })?;
    let defs = defs.ok_or(SchemaPinError::LegacyFormat {
        tree: *tree,
        reason: "expected root and defs, with optional schema pin and migration",
    })?;

    let root: Node = crate::de::deserialize_legacy_leaves(&root, store)?;
    let defs: BTreeMap<String, Node> = crate::de::deserialize_legacy_leaves(&defs, store)?;
    Ok(Schema {
        kind: Schema::LEGACY_KIND.to_owned(),
        root,
        defs,
    })
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
