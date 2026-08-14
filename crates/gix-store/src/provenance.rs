//! Legacy commit provenance is intentionally not part of schema resolution.
//!
//! Older commits may contain `Schema:`, `Schema-Version:`, or `Ents-Ref:`
//! trailers. They are ignored: the embedded `schema/` tree is the sole source
//! of schema information for ordinary reads, and new commits do not write
//! provenance trailers.

/// A legacy schema label retained only for source compatibility.
///
/// New reads do not parse or expose commit trailers, and new commits never
/// write them. This type has no schema-resolution role.
#[deprecated(note = "legacy trailer metadata is ignored by gix-store")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchemaLabel;
