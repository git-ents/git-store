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
    /// The content-derived identity: the bound document tree id, the value's
    /// entries beside `.schema`.
    ///
    /// This resolves the ref [`entity_id_name`](crate::entity_id_name) derives
    /// from `id`, exactly like [`At::Name`] holding that path — it is not a
    /// promise that the ref holds the document `id` names. The store attaches
    /// no meaning to a name, so a canonical ref advances like any other and a
    /// caller-chosen name may coincide with an entity id; reading the id then
    /// returns whatever the ref currently points at, or [`Absent`] when no
    /// such ref exists.
    ///
    /// [`Absent`]: crate::EntityState::Absent
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
