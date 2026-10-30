//! Where to read one entity's document from.

use facet_git_tree::ObjectId;
use gix_refstore::RefPath;

use crate::identity::EntityId;

/// Where to read an entity from.
///
/// This is the address axis of a read, independent of the result shape
/// ([`EntityState`](crate::EntityState) and its projections). A single
/// [`Kind::read`](crate::Kind::read) method accepts every address variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum At {
    /// A caller-chosen alias under the data prefix.
    Name(RefPath),
    /// The content-derived identity: the `{schema/, value/}` tree id.
    Entity(EntityId),
    /// A specific publication commit.
    Commit(ObjectId),
    /// A bound document tree, addressed without any ref.
    Tree(ObjectId),
}

impl From<RefPath> for At {
    fn from(name: RefPath) -> Self {
        At::Name(name)
    }
}

impl From<EntityId> for At {
    fn from(id: EntityId) -> Self {
        At::Entity(id)
    }
}

impl From<ObjectId> for At {
    fn from(commit: ObjectId) -> Self {
        At::Commit(commit)
    }
}

impl From<&RefPath> for At {
    fn from(name: &RefPath) -> Self {
        At::Name(name.clone())
    }
}
