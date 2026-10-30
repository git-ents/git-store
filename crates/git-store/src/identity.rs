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

/// A written `value/` subtree: an entity's encoded content, distinct from
/// the [`SchemaTree`] it was validated against even though both are Git
/// tree ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ValueTree(ObjectId);

impl ValueTree {
    /// The underlying Git object id.
    pub const fn object_id(self) -> ObjectId {
        self.0
    }
}

impl fmt::Display for ValueTree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<ObjectId> for ValueTree {
    fn from(value: ObjectId) -> Self {
        Self(value)
    }
}

impl From<ValueTree> for ObjectId {
    fn from(value: ValueTree) -> Self {
        value.0
    }
}

/// A pinned schema subtree: the exact schema a value was, or will be,
/// validated against, distinct from the [`ValueTree`] it validates even
/// though both are Git tree ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SchemaTree(ObjectId);

impl SchemaTree {
    /// The underlying Git object id.
    pub const fn object_id(self) -> ObjectId {
        self.0
    }
}

impl fmt::Display for SchemaTree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<ObjectId> for SchemaTree {
    fn from(value: ObjectId) -> Self {
        Self(value)
    }
}

impl From<SchemaTree> for ObjectId {
    fn from(value: SchemaTree) -> Self {
        value.0
    }
}

/// A complete bound document tree: the root containing exactly `schema/`
/// and `value/`. Its object id is the entity's [`EntityId`]; convert with
/// `EntityId::from`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DocumentTree(ObjectId);

impl DocumentTree {
    /// The underlying Git object id.
    pub const fn object_id(self) -> ObjectId {
        self.0
    }
}

impl fmt::Display for DocumentTree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<ObjectId> for DocumentTree {
    fn from(value: ObjectId) -> Self {
        Self(value)
    }
}

impl From<DocumentTree> for ObjectId {
    fn from(value: DocumentTree) -> Self {
        value.0
    }
}

impl From<DocumentTree> for EntityId {
    /// The bound document-tree OID is the content-derived [`EntityId`]: this
    /// is the one documented derivation rule, encoded so it cannot drift
    /// from the type that carries it.
    fn from(tree: DocumentTree) -> Self {
        EntityId::from_document_tree(tree.0)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn oid() -> ObjectId {
        ObjectId::from_hex(b"0123456789012345678901234567890123456789").unwrap()
    }

    #[test]
    fn value_and_schema_trees_are_distinct_types_over_the_same_object_id() {
        let id = oid();
        let value = ValueTree::from(id);
        let schema = SchemaTree::from(id);
        assert_eq!(value.object_id(), id);
        assert_eq!(schema.object_id(), id);
        assert_eq!(ObjectId::from(value), id);
        assert_eq!(ObjectId::from(schema), id);
    }

    #[test]
    fn document_tree_converts_to_the_same_entity_id_as_from_document_tree() {
        let id = oid();
        let tree = DocumentTree::from(id);
        assert_eq!(EntityId::from(tree), EntityId::from_document_tree(id));
        assert_eq!(EntityId::from(tree).object_id(), id);
    }
}
