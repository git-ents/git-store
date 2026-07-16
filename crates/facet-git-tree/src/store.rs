//! The bundled in-memory [`ObjectStore`] backend.

use std::io::Read;

use gix_hash::Kind as HashKind;
use gix_object::{Data, Find, Kind, ObjectRef, Write};

use crate::{GitObject, ObjectId, TreeEntry};

/// A content-addressed store of Git objects produced by
/// [`serialize`](crate::serialize).
///
/// This is a thin wrapper around [`gix_odb::memory::Proxy`], gitoxide's own
/// in-memory object database, so the `Find`/`Write` buffer handling lives in
/// `gix` rather than being reimplemented here. The accessors return owned values
/// because the proxy is `RefCell`-backed (required since [`Write::write_stream`]
/// takes `&self`).
///
/// This type is only a convenience default for callers that lack a backend of
/// their own. The actual contract is the generic `gix` [`Find`]/[`Write`] bounds
/// on [`serialize_into`](crate::serialize_into) and
/// [`deserialize`](crate::deserialize), which a real `gix` repository or
/// odb satisfies just as well. Like `gix`'s in-memory store it is `!Sync`;
/// cross-thread sharing is the job of the on-disk backends, not of this type.
pub struct ObjectStore(gix_odb::memory::Proxy<NoBackend>);

impl Default for ObjectStore {
    fn default() -> Self {
        Self(gix_odb::memory::Proxy::new(NoBackend, HashKind::Sha1))
    }
}

impl std::fmt::Debug for ObjectStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectStore")
            .field("objects", &self.0.num_objects_in_memory())
            .finish()
    }
}

impl ObjectStore {
    /// Decode and return the object stored under `id`, if present.
    pub fn get(&self, id: &ObjectId) -> Option<GitObject> {
        let mut buf = Vec::new();
        let data = self.0.try_find(id, &mut buf).ok().flatten()?;
        ObjectRef::from_bytes(data.data, data.kind, HashKind::Sha1)
            .ok()?
            .into_owned()
            .ok()
    }

    /// Return the entries of the tree stored under `id`, if it is a tree.
    pub fn get_tree(&self, id: &ObjectId) -> Option<Vec<TreeEntry>> {
        match self.get(id)? {
            GitObject::Tree(tree) => Some(tree.entries),
            _ => None,
        }
    }

    /// Return the raw bytes of the blob stored under `id`, if it is a blob.
    pub fn get_blob(&self, id: &ObjectId) -> Option<Vec<u8>> {
        match self.get(id)? {
            GitObject::Blob(blob) => Some(blob.data),
            _ => None,
        }
    }
}

impl Find for ObjectStore {
    fn try_find<'a>(
        &self,
        id: &gix_hash::oid,
        buffer: &'a mut Vec<u8>,
    ) -> Result<Option<Data<'a>>, gix_object::find::Error> {
        self.0.try_find(id, buffer)
    }
}

impl Write for ObjectStore {
    fn write_stream(
        &self,
        kind: Kind,
        size: u64,
        from: &mut dyn Read,
    ) -> Result<ObjectId, gix_object::write::Error> {
        self.0.write_stream(kind, size, from)
    }
}

/// Inert backing database for [`ObjectStore`]'s in-memory [`Proxy`].
///
/// [`gix_odb::memory::Proxy`] is generic over an inner object database it falls
/// back to, but [`ObjectStore`] keeps everything in the proxy's in-memory map, so
/// the inner is never read from or written to. gitoxide ships no type that is
/// both [`Find`] and [`Write`] while doing nothing, so this supplies one.
///
/// [`Proxy`]: gix_odb::memory::Proxy
#[derive(Debug, Default)]
struct NoBackend;

impl Find for NoBackend {
    fn try_find<'a>(
        &self,
        _id: &gix_hash::oid,
        _buffer: &'a mut Vec<u8>,
    ) -> Result<Option<Data<'a>>, gix_object::find::Error> {
        Ok(None)
    }
}

impl Write for NoBackend {
    fn write_stream(
        &self,
        _kind: Kind,
        _size: u64,
        _from: &mut dyn Read,
    ) -> Result<ObjectId, gix_object::write::Error> {
        // The enclosing `Proxy` always has its in-memory store enabled, so writes
        // are intercepted before reaching this inner database.
        Err("NoBackend: writes are handled by the in-memory proxy".into())
    }
}
