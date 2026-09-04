//! The repository-backed store and its Git object operations.
//!
//! [`ProllyStore`] is the crate's entry point. All Git object reads and writes
//! are funneled through the private [`ObjectStore`] trait, so the Prolly layers
//! above never touch `gix` directly; the trait is about Git objects (trees,
//! blobs, kinds, hashes), not about Prolly nodes. It is implemented for
//! [`gix::Repository`], which is the only backend; a bespoke database
//! abstraction would not simplify testing, since real repositories are cheap
//! to create in tests.

use gix::ObjectId;
use gix::objs::tree::{Entry as TreeEntry, EntryKind, EntryMode};
use gix::objs::{Kind, Tree, Write as _};

use crate::config::ProllyConfig;
use crate::error::Error;
use crate::node::{self, kind_name};

/// Git object operations the Prolly layers need, and nothing more.
pub(crate) trait ObjectStore {
    /// The repository's configured object hash kind.
    fn hash_kind(&self) -> gix::hash::Kind;
    /// The kind of an object, or `None` when it is absent.
    fn try_object_kind(&self, oid: ObjectId) -> Result<Option<Kind>, Error>;
    /// Decode a tree's entries, failing when absent or not a tree.
    fn read_tree(&self, oid: ObjectId) -> Result<Vec<TreeEntry>, Error>;
    /// Read a blob's contents, failing when absent or not a blob.
    fn read_blob(&self, oid: ObjectId) -> Result<Vec<u8>, Error>;
    /// Write a tree (a no-op when it already exists) and return its id.
    fn write_tree(&self, tree: &Tree) -> Result<ObjectId, Error>;
}

impl ObjectStore for gix::Repository {
    fn hash_kind(&self) -> gix::hash::Kind {
        self.object_hash()
    }

    fn try_object_kind(&self, oid: ObjectId) -> Result<Option<Kind>, Error> {
        Ok(self
            .try_find_header(oid)
            .map_err(Error::git)?
            .map(|header| header.kind()))
    }

    fn read_tree(&self, oid: ObjectId) -> Result<Vec<TreeEntry>, Error> {
        if self.try_object_kind(oid)?.is_none() {
            return Err(Error::ObjectNotFound { oid });
        }
        let tree = self.find_tree(oid).map_err(Error::git)?;
        let decoded = tree.decode().map_err(Error::git)?;
        Ok(decoded
            .entries
            .iter()
            .map(|entry| TreeEntry {
                mode: entry.mode,
                filename: entry.filename.into(),
                oid: entry.oid.to_owned(),
            })
            .collect())
    }

    fn read_blob(&self, oid: ObjectId) -> Result<Vec<u8>, Error> {
        if self.try_object_kind(oid)?.is_none() {
            return Err(Error::ObjectNotFound { oid });
        }
        let blob = self.find_blob(oid).map_err(Error::git)?;
        Ok(blob.data.to_vec())
    }

    fn write_tree(&self, tree: &Tree) -> Result<ObjectId, Error> {
        self.write_object(tree)
            .map(|id| id.detach())
            .map_err(Error::git)
    }
}

/// A repository-backed store of content-addressed Prolly trees.
///
/// The store holds no mutable state: every operation takes the tree's root
/// [`ObjectId`] and, for mutations, returns the new root. Old roots stay
/// readable forever — a tree is immutable and identified entirely by its root.
#[derive(Debug, Clone, Copy)]
pub struct ProllyStore<'repo> {
    repo: &'repo gix::Repository,
    config: ProllyConfig,
}

impl<'repo> ProllyStore<'repo> {
    /// Open a store over `repo` with the default [`ProllyConfig`].
    pub fn open(repo: &'repo gix::Repository) -> Self {
        Self::with_config(repo, ProllyConfig::default())
            .expect("default configuration is always valid")
    }

    /// Open a store over `repo` with an explicit configuration.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] when the configuration is invalid.
    pub fn with_config(repo: &'repo gix::Repository, config: ProllyConfig) -> Result<Self, Error> {
        config.validate()?;
        Ok(Self { repo, config })
    }

    /// The configuration this store builds and verifies with.
    #[must_use]
    pub const fn config(&self) -> &ProllyConfig {
        &self.config
    }

    /// The repository backing this store.
    #[must_use]
    pub const fn repo(&self) -> &'repo gix::Repository {
        self.repo
    }

    /// The canonical root of an empty map: Git's empty tree object.
    ///
    /// Prefer [`ProllyStore::insert`] with a `None` root; this id is exposed so
    /// callers can represent the empty map anywhere an ObjectId is expected.
    #[must_use]
    pub fn empty_root(&self) -> ObjectId {
        ObjectId::empty_tree(self.repo.hash_kind())
    }

    /// Whether `root` is the canonical empty-map root.
    #[must_use]
    pub fn is_empty_root(&self, root: ObjectId) -> bool {
        root == self.empty_root()
    }

    /// Read the entries of a tree, failing when absent or not a tree.
    pub(crate) fn read_tree(&self, oid: ObjectId) -> Result<Vec<TreeEntry>, Error> {
        self.repo.read_tree(oid)
    }

    /// Write a tree (a no-op when it already exists) and return its id.
    pub(crate) fn write_tree(&self, tree: &Tree) -> Result<ObjectId, Error> {
        self.repo.write_tree(tree)
    }

    /// The kind of an object, mapped to a leaf-entry mode.
    pub(crate) fn entry_mode_of(&self, oid: ObjectId) -> Result<EntryMode, Error> {
        match self.repo.try_object_kind(oid)? {
            Some(Kind::Tree) => Ok(EntryMode::from(EntryKind::Tree)),
            Some(Kind::Blob) => Ok(EntryMode::from(EntryKind::Blob)),
            Some(other) => Err(Error::UnexpectedObjectKind {
                oid,
                kind: kind_name(other),
            }),
            None => Err(Error::ObjectNotFound { oid }),
        }
    }

    /// The repository's object hash kind.
    pub(crate) fn hash_kind(&self) -> gix::hash::Kind {
        self.repo.object_hash()
    }

    /// Whether an object exists in the repository.
    pub(crate) fn object_exists(&self, oid: ObjectId) -> Result<bool, Error> {
        self.repo.try_object_kind(oid).map(|kind| kind.is_some())
    }

    /// The id of the reserved internal-marker blob, writing it if needed.
    pub(crate) fn internal_marker_oid(&self) -> Result<ObjectId, Error> {
        self.repo
            .write_buf(Kind::Blob, node::INTERNAL_MARKER_CONTENT)
            .map_err(Error::git)
    }
}

/// Convert a decoded entry kind to the leaf-entry mode carrying it.
pub(crate) fn mode_of_kind(kind: EntryKind) -> EntryMode {
    match kind {
        EntryKind::Tree => EntryMode::from(EntryKind::Tree),
        EntryKind::Blob | EntryKind::BlobExecutable => EntryMode::from(EntryKind::Blob),
        EntryKind::Commit => EntryMode::from(EntryKind::Commit),
        EntryKind::Link => EntryMode::from(EntryKind::Link),
    }
}
