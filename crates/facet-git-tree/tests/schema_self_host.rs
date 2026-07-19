//! Integration tests for schema self-hosting: `SchemaDoc` is an ordinary
//! `Facet` value stored through this crate's own encoding.
//!
//! Covers spec requirement:
//!   schema.representation
//!     — schemas are self-hosted with no special-cased representation, and
//!       their on-disk form is a public, semver-major contract (guarded here
//!       by a golden object id).

use facet_git_tree::{SchemaDoc, deserialize, schema_of, serialize};

mod common;
use common::{Event, Nested, Person, TreeNode};

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
#[test]
fn person_schema_golden_oid() -> anyhow::Result<()> {
    let doc = schema_of::<Person>()?;
    let (root, _store) = serialize(&doc)?;
    assert_eq!(root.to_string(), "ad6b1930fd55dfb72a18668fdf5b4135ad374d61");
    Ok(())
}
