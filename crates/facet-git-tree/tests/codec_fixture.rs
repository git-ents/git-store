//! The codec fixture ([`facet_git_tree::schema::codec`]): coverage and
//! sharing checks for the value that binds a pin tower's generation id to the
//! value codec, not just to `Schema`'s/`Migration`'s own shape.

use std::collections::HashSet;

use facet_git_tree::schema::codec::{Fixture, fixture};
use facet_git_tree::{
    EntryKind, Migration, MigrationSchema, Node, ObjectStore, Schema, SchemaSchema, VariantKind,
    schema_of,
};

mod common;
use common::find_entry;

/// The variant names `T`'s own schema reflects, by walking `schema_of::<T>()`
/// to the `Node::Enum` definition of `T` itself — `T`'s full variant set,
/// regardless of which variant any particular value picks.
fn reflected_variants<T: for<'a> facet::Facet<'a>>() -> HashSet<String> {
    let doc = schema_of::<T>().expect("T's shape is describable");
    let Node::Ref(name) = &doc.root else {
        panic!("expected a named type at the root, found {:?}", doc.root)
    };
    let Some(Node::Enum(variants)) = doc.defs.get(name) else {
        panic!("expected {name:?} to be an Enum definition")
    };
    variants.keys().cloned().collect()
}

/// Collect the [`Node`] and [`VariantKind`] variant names actually used
/// anywhere in `schema`, recursing through every field, element, and
/// definition.
///
/// The match arms are exhaustive (no wildcard), so adding a `Node` or
/// `VariantKind` variant fails this file to *compile* until it is handled
/// here — the strongest form of "you forgot this" available.
fn used_variants(schema: &Schema) -> (HashSet<String>, HashSet<String>) {
    let mut nodes = HashSet::new();
    let mut kinds = HashSet::new();
    walk_node(&schema.root, &mut nodes, &mut kinds);
    for def in schema.defs.values() {
        walk_node(def, &mut nodes, &mut kinds);
    }
    (nodes, kinds)
}

fn walk_node(node: &Node, nodes: &mut HashSet<String>, kinds: &mut HashSet<String>) {
    let name = match node {
        Node::Unit => "Unit",
        Node::Bool => "Bool",
        Node::Char => "Char",
        Node::String => "String",
        Node::I8 => "I8",
        Node::I16 => "I16",
        Node::I32 => "I32",
        Node::I64 => "I64",
        Node::I128 => "I128",
        Node::ISize => "ISize",
        Node::U8 => "U8",
        Node::U16 => "U16",
        Node::U32 => "U32",
        Node::U64 => "U64",
        Node::U128 => "U128",
        Node::USize => "USize",
        Node::F32 => "F32",
        Node::F64 => "F64",
        Node::Bytes => "Bytes",
        Node::Struct(fields) => {
            for field in fields.values() {
                walk_node(field, nodes, kinds);
            }
            "Struct"
        }
        Node::Tuple(items) => {
            for item in items {
                walk_node(item, nodes, kinds);
            }
            "Tuple"
        }
        Node::List(elem) => {
            walk_node(elem, nodes, kinds);
            "List"
        }
        Node::Array { elem, .. } => {
            walk_node(elem, nodes, kinds);
            "Array"
        }
        Node::Map { key, value } => {
            walk_node(key, nodes, kinds);
            walk_node(value, nodes, kinds);
            "Map"
        }
        Node::Optional(inner) => {
            walk_node(inner, nodes, kinds);
            "Optional"
        }
        Node::Enum(variants) => {
            for kind in variants.values() {
                walk_variant_kind(kind, nodes, kinds);
            }
            "Enum"
        }
        Node::RawTree => "RawTree",
        Node::Dynamic => "Dynamic",
        Node::Ref(_) => "Ref",
    };
    nodes.insert(name.to_string());
}

fn walk_variant_kind(kind: &VariantKind, nodes: &mut HashSet<String>, kinds: &mut HashSet<String>) {
    let name = match kind {
        VariantKind::Unit => "Unit",
        VariantKind::Newtype(inner) => {
            walk_node(inner, nodes, kinds);
            "Newtype"
        }
        VariantKind::Tuple(items) => {
            for item in items {
                walk_node(item, nodes, kinds);
            }
            "Tuple"
        }
        VariantKind::Struct(fields) => {
            for field in fields.values() {
                walk_node(field, nodes, kinds);
            }
            "Struct"
        }
    };
    kinds.insert(name.to_string());
}

/// [`Fixture`]'s schema must reach every [`Node`] and [`VariantKind`] variant
/// this crate defines, reflected — not hand-listed — from `Node`'s and
/// `VariantKind`'s own schemas.
///
/// No constructs are exempted: every current variant is expressible in a
/// `Facet` value, so this fixture covers all of them.
#[test]
fn fixture_covers_every_construct() {
    let node_reflected = reflected_variants::<Node>();
    let kind_reflected = reflected_variants::<VariantKind>();

    let fixture_schema = schema_of::<Fixture>().expect("Fixture's shape is describable");
    let (node_used, kind_used) = used_variants(&fixture_schema);

    let missing_nodes: Vec<_> = node_reflected.difference(&node_used).collect();
    assert!(
        missing_nodes.is_empty(),
        "Fixture does not cover these Node variants: {missing_nodes:?}"
    );

    let missing_kinds: Vec<_> = kind_reflected.difference(&kind_used).collect();
    assert!(
        missing_kinds.is_empty(),
        "Fixture does not cover these VariantKind variants: {missing_kinds:?}"
    );
}

/// [`fixture`] is a fixed, deterministic value: two calls produce equal
/// values (and so, through the codec, equal object ids).
#[test]
fn fixture_value_is_deterministic() {
    assert_eq!(fixture(), fixture());
}

/// A materialized generation tree — for both towers — really contains
/// `codec/schema` and `codec/value`, and both towers splice the identical
/// `codec` tree object id.
#[test]
fn both_towers_splice_the_identical_codec_tree() -> anyhow::Result<()> {
    let store = ObjectStore::default();

    let schema_root = schema_of::<Schema>()?.write_pinned(&store)?;
    let schema_generation = find_entry(&store, &schema_root, SchemaSchema::ENTRY).oid;
    let schema_codec = find_entry(&store, &schema_generation, "codec");
    assert_eq!(schema_codec.mode.kind(), EntryKind::Tree);

    let migration_root = Migration::default().write_pinned(&store)?;
    let migration_generation = find_entry(&store, &migration_root, MigrationSchema::ENTRY).oid;
    let migration_codec = find_entry(&store, &migration_generation, "codec");
    assert_eq!(migration_codec.mode.kind(), EntryKind::Tree);

    assert_eq!(
        schema_codec.oid, migration_codec.oid,
        "both towers must splice the exact same codec tree object"
    );

    let schema_entry = find_entry(&store, &schema_codec.oid, "schema");
    assert_eq!(schema_entry.mode.kind(), EntryKind::Tree);
    let value_entry = find_entry(&store, &schema_codec.oid, "value");
    assert_eq!(value_entry.mode.kind(), EntryKind::Tree);

    Ok(())
}
