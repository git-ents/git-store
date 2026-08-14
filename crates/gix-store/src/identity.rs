//! Content-derived identities for complete bound documents.

use std::{fmt, str::FromStr};

use facet::Facet;
use facet_git_tree::{ObjectId, SerializeError, serialize_into};
use gix::objs::Write;
use gix_refstore::RefSegment;

/// The stable identity of a stored entity.
///
/// An entity id is the Git object id of the complete bound document tree: the
/// root containing exactly the `schema` and `value` subtrees. It is therefore
/// independent of commit parents, messages, signatures, and timestamps, while
/// still changing when either the schema or value changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntityId(ObjectId);

impl EntityId {
    /// Derive an id from a complete bound document tree.
    pub fn from_document_tree(tree: ObjectId) -> Self {
        Self(tree)
    }

    /// The underlying Git object id.
    pub fn object_id(self) -> ObjectId {
        self.0
    }

    /// The canonical ref segment for this id.
    pub fn as_segment(self) -> RefSegment {
        RefSegment::new(self.0.to_string()).expect("object id hex is a valid ref segment")
    }
}

impl fmt::Display for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<ObjectId> for EntityId {
    fn from(value: ObjectId) -> Self {
        Self::from_document_tree(value)
    }
}

impl From<EntityId> for ObjectId {
    fn from(value: EntityId) -> Self {
        value.0
    }
}

impl FromStr for EntityId {
    type Err = <ObjectId as FromStr>::Err;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(value.parse()?))
    }
}

/// Derive the entity id of a complete bound document tree.
pub fn canonical_document_id(tree: ObjectId) -> EntityId {
    EntityId::from_document_tree(tree)
}

/// Derive an entity id from a typed value's canonical object encoding.
///
/// This is retained as a compatibility helper for callers that used the
/// original identity utility before store writes gained bound-document
/// identities. Store publication uses [`canonical_document_id`] instead, so
/// schema-sensitive ids must be obtained from [`crate::Kind::compile`].
/// Equal typed values still produce the same canonical object id.
pub fn canonical_object_id<T, W>(value: &T, objects: &W) -> Result<ObjectId, SerializeError>
where
    T: for<'a> Facet<'a>,
    W: Write + ?Sized,
{
    serialize_into(value, objects)
}
