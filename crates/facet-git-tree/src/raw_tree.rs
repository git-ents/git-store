//! [`RawTree`], the raw-passthrough tree field.

use facet::Facet;

use crate::ObjectId;

/// A Git tree already written into the backing store, embedded by object id
/// rather than walked field-by-field.
///
/// `serialize_node` and `deser_into` special-case this type ahead of the
/// generic struct branch: a `RawTree` field passes its wrapped object id
/// straight through as a tree entry (no recursion, no write), and reading one
/// back captures the child entry's object id without decoding its contents.
/// This lets a `Facet`-derived struct embed an arbitrarily-shaped subtree — a
/// directory with no fixed layout, such as an imported toolchain's `bin/` —
/// next to ordinarily-encoded fields.
///
/// The wrapped tree must already exist in the store the caller serializes
/// into; `RawTree` carries no content of its own to write. Sha-1 only, like
/// the rest of this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Facet)]
pub struct RawTree {
    // Opaque to the normal field-by-field encoding: `serialize_node`/
    // `deser_into` intercept `RawTree` by shape identity before this field is
    // ever visited.
    hash: [u8; 20],
}

impl RawTree {
    /// Wrap a tree's object id for embedding as a passthrough field.
    pub fn new(oid: ObjectId) -> Self {
        let mut hash = [0u8; 20];
        hash.copy_from_slice(oid.as_slice());
        Self { hash }
    }

    /// The wrapped tree's object id.
    pub fn oid(&self) -> ObjectId {
        ObjectId::from_bytes_or_panic(&self.hash)
    }
}
