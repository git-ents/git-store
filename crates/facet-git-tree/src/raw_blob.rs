//! [`RawBlob`], the raw-passthrough blob field.

use facet::Facet;

use crate::ObjectId;

/// A Git blob already written into the backing store, embedded by object id
/// rather than walked field-by-field.
///
/// `serialize_node` and `deser_into` special-case this type ahead of the
/// generic scalar branch: a `RawBlob` field passes its wrapped object id
/// straight through as a blob entry (no recursion, no write), and reading one
/// back captures the child entry's object id without decoding its contents.
/// The referenced object is verified to be a blob during deserialization.
///
/// The wrapped blob must already exist in the store the caller serializes into;
/// `RawBlob` carries no content of its own to write. Sha-1 only, like the rest
/// of this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Facet)]
pub struct RawBlob {
    // Opaque to the normal field-by-field encoding: `serialize_node`/
    // `deser_into` intercept `RawBlob` by shape identity before this field is
    // ever visited.
    hash: [u8; 20],
}

impl RawBlob {
    /// Wrap a blob's object id for embedding as a passthrough field.
    pub fn new(oid: ObjectId) -> Self {
        let mut hash = [0u8; 20];
        hash.copy_from_slice(oid.as_slice());
        Self { hash }
    }

    /// The wrapped blob's object id.
    pub fn oid(&self) -> ObjectId {
        ObjectId::from_bytes_or_panic(&self.hash)
    }
}
