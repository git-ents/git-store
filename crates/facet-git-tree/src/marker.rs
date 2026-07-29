//! The presence marker: a single reserved-name blob entry written in place of
//! a literal empty tree for `None`, dynamic `Null`, and an empty collection
//! (`Vec`/array/`Map`, and dynamic `Array`/`Object`).
//!
//! Git's `ls-tree -r` and `diff` machinery is blob-oriented: `ls-tree -r`
//! never prints a line for a tree, only for the blobs reachable under it, and
//! `diff` compares blob content by path. A literal empty tree therefore
//! contributes nothing to either — the path it is written under simply does
//! not appear, so a field going empty (or always having been empty) looks
//! identical to the field never having existed at all. Writing one blob entry
//! named [`MARKER_KEY`] instead keeps the path visible: `git ls-tree -r`
//! shows `tags/_`, and `git diff` shows that entry appearing or disappearing
//! exactly as an ordinary element would.
//!
//! A single shared marker (one reserved name, one empty-content blob) covers
//! every one of these cases rather than a distinct marker per case, because
//! the marker's only job is presence-signaling for git tooling — every
//! consumer that decodes a possibly-marked tree already knows, from its
//! target `Facet` type or its [`crate::schema::Node`] node, exactly which
//! of `None`/`Null`/empty-`Vec`/empty-`Map`/… is expected at that path, so
//! the marker itself never needs to disambiguate one from another. The one
//! reader that cannot make that distinction — the schemaless
//! [`deserialization.dynamic.heuristic`](crate) read — already documents
//! `Null` and an empty collection as collapsing to the same empty `Object`
//! reading, from long before markers existed; a shared marker preserves that
//! exact (lossy, documented) behavior instead of introducing a new one.
//!
//! [`MARKER_KEY`] is reserved: [`crate::check_key`] rejects it for a dynamic
//! (map or dynamic-object) key for the same reason it rejects `/` — a real
//! entry named exactly [`MARKER_KEY`] would otherwise be indistinguishable,
//! on read, from the marker. Ordinal (sequence) names can never collide with
//! it, being always decimal digits. Field names of a `#[derive(Facet)]` type
//! cannot either, a bare `_` not being a valid Rust field identifier — but a
//! [`Schema`](crate::Schema) is *data*, and one authored by hand can
//! name a field anything at all, so the schema-directed writer checks field
//! names too rather than trusting the derive's guarantee.

use gix_object::{Kind, Write};

use crate::error::SerializeError;
use crate::{EntryKind, EntryMode, ObjectId, TreeEntry};

/// The reserved tree-entry name of the presence marker.
pub(crate) const MARKER_KEY: &str = "_";

/// Whether `entries` is exactly the marker entry written in place of a
/// literal empty tree.
///
/// Every read-side consumer of a possibly-marked tree (`Option`, `Vec`,
/// `Map`, dynamic `Null`/`Array`/`Object`, …) applies this before its own
/// arity or ordinal checks, so the marker never reaches those checks as if it
/// were real data.
pub(crate) fn is_marker(entries: &[(String, ObjectId, EntryKind)]) -> bool {
    matches!(entries, [(name, _, EntryKind::Blob)] if name == MARKER_KEY)
}

/// Write the marker tree: a single entry named [`MARKER_KEY`] pointing at the
/// empty blob.
///
/// The empty blob is content-addressed like any other object, so every
/// `None`, `Null`, and empty collection in a value resolves to the same one
/// marker blob (and, since the wrapping tree has exactly that one entry, the
/// same one marker tree) rather than allocating a fresh object each time.
pub(crate) fn write_marker_tree<W: Write + ?Sized>(store: &W) -> Result<ObjectId, SerializeError> {
    let marker_blob = store
        .write_buf(Kind::Blob, b"")
        .map_err(SerializeError::Backend)?;
    store
        .write(&gix_object::Tree {
            entries: vec![TreeEntry {
                mode: EntryMode::from(EntryKind::Blob),
                filename: MARKER_KEY.into(),
                oid: marker_blob,
            }],
        })
        .map_err(SerializeError::Backend)
}
