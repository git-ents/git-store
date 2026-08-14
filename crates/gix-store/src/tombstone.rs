//! Typed deletion documents and stateful entity reads.
//!
//! Tombstones use the same two-subtree document frame as ordinary values. The
//! value subtree is a typed [`Tombstone`] rather than an empty tree, so a
//! fetched tombstone remains self-describing and cannot be confused with an
//! absent or pruned ref.

use std::str::FromStr;

use facet::Facet;
use facet_git_tree::{
    ObjectId, Schema, deserialize, schema_of, serialize_into, validate_with_schema,
};
use gix::objs::{Find, Write};
use gix_refstore::{RefSegment, RefStore};

use crate::{EntityId, Entry, Error, Store};

/// The embedded schema kind used by tombstone documents.
pub(crate) const SCHEMA_KIND: &str = "gix-store.tombstone.v1";

/// The explicit deletion state carried by a tombstone value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Facet)]
#[repr(u8)]
pub enum TombstoneState {
    /// The addressed entity has been deleted.
    Deleted,
}

/// A typed deletion marker stored in a document's `value/` subtree.
#[derive(Debug, Clone, PartialEq, Eq, Facet)]
pub struct Tombstone {
    /// The explicit state tag. This is intentionally not represented by an
    /// empty tree or a missing value.
    pub state: TombstoneState,
    /// The kind that owned the deleted entity.
    pub kind: String,
    /// The canonical [`EntityId`] encoded as text for the wire schema.
    pub entity: String,
}

impl Tombstone {
    /// Construct a tombstone for `kind` and its canonical entity id.
    pub fn new(kind: &RefSegment, entity: EntityId) -> Self {
        Self {
            state: TombstoneState::Deleted,
            kind: kind.as_str().to_owned(),
            entity: entity.to_string(),
        }
    }

    /// Decode the canonical entity id carried by this marker.
    pub fn entity_id(&self) -> Option<EntityId> {
        EntityId::from_str(&self.entity).ok()
    }
}

/// A tombstone read alongside the commit that published it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TombstoneEntry {
    /// The typed deletion marker.
    pub tombstone: Tombstone,
    /// The commit that published the marker.
    pub commit: ObjectId,
    /// That commit's summary.
    pub message: String,
}

/// The result of an idempotent delete operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteResult {
    /// A new tombstone was published.
    Deleted(TombstoneEntry),
    /// The entity already pointed at an equivalent tombstone.
    AlreadyDeleted(TombstoneEntry),
    /// No canonical ref exists, including an old hard-deleted ref.
    Absent,
}

/// The state of an entity ref.
///
/// `Absent` means there is no ref (including an old hard-deleted ref), while
/// `Deleted` means the ref exists and points at an explicit tombstone.
#[derive(Debug, PartialEq)]
pub enum EntityState<V> {
    /// No entity ref exists.
    Absent,
    /// A live value and its publication commit.
    Present(Entry<V>),
    /// An explicit typed deletion and its publication commit.
    Deleted(TombstoneEntry),
}

/// Compatibility spelling for callers that prefer a read-result name.
pub type ReadResult<V> = EntityState<V>;

/// Compatibility spelling for callers that prefer a state name.
pub type ReadState<V> = EntityState<V>;

impl<V> EntityState<V> {
    /// Whether this result is an explicit deletion.
    pub const fn is_deleted(&self) -> bool {
        matches!(self, Self::Deleted(_))
    }

    /// Whether this result is an ordinary present value.
    pub const fn is_present(&self) -> bool {
        matches!(self, Self::Present(_))
    }

    /// Consume the result, returning its tombstone when it is deleted.
    pub fn into_deleted(self) -> Option<TombstoneEntry> {
        match self {
            Self::Deleted(entry) => Some(entry),
            Self::Absent | Self::Present(_) => None,
        }
    }
}

pub(crate) fn schema() -> Result<Schema, Error> {
    Ok(schema_of::<Tombstone>()?.with_kind(SCHEMA_KIND)?)
}

pub(crate) fn write<R, O>(store: &Store<R, O>, tombstone: &Tombstone) -> Result<ObjectId, Error>
where
    R: RefStore,
    O: Find + Write,
{
    let doc = schema()?;
    let value = serialize_into(tombstone, store.objects())?;
    validate_with_schema(&value, &doc, store.objects())?;
    let schema = doc.write_pinned(store.objects())?;
    store.bind_schema(value, schema)
}

/// Return `None` for an ordinary document and decode a tombstone document
/// before any kind schema-history lookup can occur.
pub(crate) fn read<R, O>(
    store: &Store<R, O>,
    value: &ObjectId,
    doc: &Schema,
) -> Result<Option<Tombstone>, Error>
where
    R: RefStore,
    O: Find,
{
    if doc.kind != SCHEMA_KIND {
        return Ok(None);
    }
    let expected = schema()?;
    if *doc != expected {
        return Err(Error::TombstoneSchemaMismatch);
    }
    let tombstone: Tombstone = deserialize(value, store.objects())?;
    if tombstone.state != TombstoneState::Deleted
        || tombstone.entity_id().is_none()
        || tombstone.kind.is_empty()
    {
        return Err(Error::InvalidTombstone);
    }
    Ok(Some(tombstone))
}
