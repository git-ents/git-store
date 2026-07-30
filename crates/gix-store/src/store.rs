//! [`Store`]: kinds, schemas, and entities as Git refs and commits.

use facet::Facet;
use facet_git_tree::ObjectId;
use gix::objs::{Find, Write, WriteTo};
use gix_refstore::{
    ApplyError, Committer, ErasedSigner, GixRefStore, RefEdit, RefName, RefPrefix, RefSegment,
    RefStore, SignatureBytes, Signer,
};

use crate::encoding::{Dynamic, Encoding, Typed};
use crate::error::{Error, Subtree};
use crate::kind::Kind;
use crate::provenance::{self, SchemaLabel};

/// Where a store's refs live.
pub struct Layout {
    /// Entities: `<data>/<kind>/<name>`. Default `refs/store`.
    pub data: RefPrefix,
    /// Schemas: `<schema>/<kind>`. Default `refs/schema`.
    pub schema: RefPrefix,
}

impl Default for Layout {
    fn default() -> Self {
        // `RefPrefix` validates on construction and offers no infallible
        // constructor, so even a literal known good runs the fallible path.
        fn prefix(value: &'static str) -> RefPrefix {
            RefPrefix::new(value).expect("built-in ref prefix is valid")
        }
        Layout {
            data: prefix("refs/store"),
            schema: prefix("refs/schema"),
        }
    }
}

/// A content-addressed store layered over a [`RefStore`] and an object
/// database.
///
/// Every kind is a published [`Schema`](facet_git_tree::Schema); every
/// entity is a commit chain whose tree is `{value/, schema/}` — the encoded
/// entity, and the tree of the schema it was validated against. Binding the
/// schema by subtree rather than by parent or trailer keeps it reachable by
/// ordinary tree traversal, so a fetch of just the data ref brings the schema
/// along for free.
pub struct Store<R, O> {
    refs: R,
    objects: O,
    layout: Layout,
    signer: Option<Box<dyn ErasedSigner>>,
}

/// The extra commit header a signed write's bytes land in.
///
/// Git's own, so `git verify-commit` and `git log --show-signature` read a
/// store-written commit with no tooling of ours installed; the bytes go in
/// verbatim, and git's continuation-line folding of a multi-line value is
/// framing the object codec does on both sides, not interpretation — the store
/// reads back exactly the bytes the [`Signer`] produced and asks nothing else
/// of them.
const SIGNATURE_HEADER: &str = "gpgsig";

impl<R, O> Store<R, O>
where
    R: RefStore + Committer,
    O: Find + Write,
{
    /// Open a store with the default `refs/store`/`refs/schema` [`Layout`].
    pub fn new(refs: R, objects: O) -> Self {
        Self::with_layout(refs, objects, Layout::default())
    }

    /// Open a store with a caller-supplied [`Layout`].
    pub fn with_layout(refs: R, objects: O, layout: Layout) -> Self {
        Store {
            refs,
            objects,
            layout,
            signer: None,
        }
    }

    /// Cover every commit this store writes with `signer`'s bytes.
    ///
    /// A store without one writes unsigned commits, which is the default: a
    /// signer is configured once here rather than passed at every write, so
    /// nothing on the write path changes shape when one is present.
    ///
    /// The bytes land in the standard `gpgsig` header, so a signer that emits
    /// the armored block its format calls for — what `ssh-keygen -Y sign`
    /// prints, under `gpg.format = ssh` — yields a commit stock `git
    /// verify-commit` accepts. The store neither requires nor checks that: it
    /// carries whatever bytes it is handed.
    ///
    /// ```no_run
    /// # use gix_refstore::{MemoryRefStore, SignatureBytes, Signer};
    /// # use gix_store::Store;
    /// struct Machine;
    ///
    /// impl Signer for Machine {
    ///     type Error = std::io::Error;
    ///
    ///     fn sign(&self, bytes: &[u8]) -> Result<SignatureBytes, Self::Error> {
    ///         Ok(SignatureBytes::from(bytes.to_vec()))
    ///     }
    /// }
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let objects = gix::open(".")?.objects.clone();
    /// let store = Store::new(MemoryRefStore::new(), objects).with_signer(Machine);
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_signer(mut self, signer: impl Signer + 'static) -> Self {
        self.signer = Some(Box::new(signer));
        self
    }

    /// The signature bytes `commit` carries, or `None` when it is unsigned.
    ///
    /// Verbatim: the store performs no verification, and never has an opinion
    /// on what the bytes mean — that is attest's business.
    ///
    /// The commit decoder un-indents git's continuation lines, so a multi-line
    /// armored block comes back exactly as the [`Signer`] produced it.
    pub fn signature(&self, commit: ObjectId) -> Result<Option<SignatureBytes>, Error> {
        self.with_commit(commit, |c| {
            Ok(c.extra_headers
                .iter()
                .find(|(name, _)| *name == SIGNATURE_HEADER)
                .map(|(_, value)| SignatureBytes::from(value.to_vec())))
        })
    }

    /// The ref store backing this store.
    pub fn refs(&self) -> &R {
        &self.refs
    }

    /// The object database backing this store.
    pub fn objects(&self) -> &O {
        &self.objects
    }

    /// This store's ref namespace.
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// A handle on the kind `name`, whose values are `T`.
    pub fn kind<T: for<'a> Facet<'a>>(&self, name: RefSegment) -> Kind<'_, Typed<T>, R, O> {
        self.kind_with(name)
    }

    /// A handle on the kind `name`, whose values are read and written as
    /// [`facet_value::Value`] under the kind's published schema.
    pub fn dynamic(&self, name: RefSegment) -> Kind<'_, Dynamic, R, O> {
        self.kind_with(name)
    }

    /// A handle on the kind `name` under a caller-supplied [`Encoding`].
    pub fn kind_with<E: Encoding>(&self, name: RefSegment) -> Kind<'_, E, R, O> {
        Kind::new(self, name)
    }

    /// Every kind that has a published schema, ascending.
    pub fn kinds(&self) -> Result<Vec<RefSegment>, Error> {
        list_segments(&self.refs, &self.layout.schema)
    }

    /// The schema commit a data commit records in its `Schema:` trailer.
    pub fn provenance(&self, commit: ObjectId) -> Result<SchemaLabel, Error> {
        self.with_commit(commit, |c| provenance::parse(commit, c.message))
    }

    /// The tree of the commit `id` points at.
    pub(crate) fn commit_tree(&self, id: ObjectId) -> Result<ObjectId, Error> {
        self.with_commit(id, |c| Ok(c.tree()))
    }

    /// A commit's tree and message summary, decoded from a single commit
    /// read.
    pub(crate) fn commit_tree_and_summary(
        &self,
        id: ObjectId,
    ) -> Result<(ObjectId, String), Error> {
        self.with_commit(id, |c| Ok((c.tree(), c.message_summary().to_string())))
    }

    /// A written tree's entries, as the mutable form a splice appends to.
    pub(crate) fn tree_entries(
        &self,
        tree: ObjectId,
    ) -> Result<Vec<gix::objs::tree::Entry>, Error> {
        self.with_tree(tree, |tree| {
            Ok(tree
                .entries
                .iter()
                .map(|entry| gix::objs::tree::Entry {
                    mode: entry.mode,
                    filename: entry.filename.into(),
                    oid: entry.oid.to_owned(),
                })
                .collect())
        })
    }

    /// One named entry of a tree, or `None` when the tree does not carry it.
    pub(crate) fn find_entry(&self, tree: ObjectId, name: &str) -> Result<Option<ObjectId>, Error> {
        self.with_tree(tree, |tree| {
            Ok(tree
                .entries
                .iter()
                .find(|entry| entry.filename == name)
                .map(|entry| entry.oid.to_owned()))
        })
    }

    /// Splice an already-written value tree and schema tree into the
    /// two-entry root a data commit's own tree becomes.
    pub(crate) fn bind_schema(&self, value: ObjectId, schema: ObjectId) -> Result<ObjectId, Error> {
        let mut entries = vec![
            gix::objs::tree::Entry {
                mode: self.entry_mode(value)?,
                filename: Subtree::Value.as_str().into(),
                oid: value,
            },
            gix::objs::tree::Entry {
                mode: self.entry_mode(schema)?,
                filename: Subtree::Schema.as_str().into(),
                oid: schema,
            },
        ];
        entries.sort();
        self.objects
            .write(&gix::objs::Tree { entries })
            .map_err(Error::backend)
    }

    /// Split a data commit's root tree into `(value, schema)`, the two-way
    /// split [`bind_schema`](Self::bind_schema) writes.
    ///
    /// The root must hold *exactly* `schema` and `value`, with `schema` a
    /// tree — requiring the whole shape, rather than looking up each name in
    /// isolation, is what makes a pre-binding commit (whose tree *was* the
    /// value) fail as one diagnosable [`Error::NotSubtreeBound`] rather than
    /// being half-matched and misreported.
    pub(crate) fn split(
        &self,
        root: ObjectId,
        commit: ObjectId,
    ) -> Result<(ObjectId, ObjectId), Error> {
        let (value, schema) = self.with_tree(root, |tree| {
            let not_bound = || Error::NotSubtreeBound {
                commit,
                found: tree
                    .entries
                    .iter()
                    .map(|e| String::from_utf8_lossy(e.filename).into_owned())
                    .collect::<Vec<_>>()
                    .join(", "),
            };
            let entry = |subtree: Subtree| {
                tree.entries
                    .iter()
                    .find(|e| e.filename == subtree.as_str())
                    .ok_or_else(not_bound)
            };
            if tree.entries.len() != 2 {
                return Err(not_bound());
            }
            let value = entry(Subtree::Value)?;
            let schema = entry(Subtree::Schema)?;
            if !schema.mode.is_tree() {
                return Err(not_bound());
            }
            Ok((value.oid.to_owned(), schema.oid.to_owned()))
        })?;
        self.require_present(value, Subtree::Value, commit)?;
        self.require_present(schema, Subtree::Schema, commit)?;
        Ok((value, schema))
    }

    /// Commit `tree` forward over the current tip of `name`, retrying on a
    /// lost compare-and-swap race.
    pub(crate) fn commit_forward(
        &self,
        name: &RefName,
        message: &str,
        tree: ObjectId,
    ) -> Result<ObjectId, Error> {
        loop {
            let parent = self.refs.read(name).map_err(Error::backend)?;
            let commit = self.write_commit(message, tree, parent)?;
            let edit = match parent {
                Some(expected) => RefEdit::Update {
                    name: name.clone(),
                    expected,
                    new: commit,
                },
                None => RefEdit::Create {
                    name: name.clone(),
                    new: commit,
                },
            };
            match self.refs.apply(edit) {
                Ok(()) => return Ok(commit),
                Err(ApplyError::LostRace { .. }) => continue,
                Err(ApplyError::Backend(err)) => return Err(Error::backend(err)),
            }
        }
    }

    /// Write a commit object, without touching any ref.
    pub(crate) fn write_commit(
        &self,
        message: &str,
        tree: ObjectId,
        parent: Option<ObjectId>,
    ) -> Result<ObjectId, Error> {
        let mut commit = gix::objs::Commit {
            tree,
            parents: parent.into_iter().collect(),
            author: self.refs.author().map_err(Error::backend)?,
            committer: self.refs.signature().map_err(Error::backend)?,
            encoding: None,
            message: message.into(),
            extra_headers: Vec::new(),
        };
        // The signature covers the commit as it stands without one, exactly as
        // git's own object signing does: the header cannot be inside the bytes
        // it attests to.
        if let Some(signer) = &self.signer {
            let mut bytes = Vec::new();
            commit.write_to(&mut bytes).map_err(Error::backend)?;
            let signature = signer.sign_erased(&bytes).map_err(Error::Signer)?;
            // The commit encoder folds a multi-line value onto git's
            // space-prefixed continuation lines itself, so the signer's bytes
            // are pushed as they came.
            commit
                .extra_headers
                .push((SIGNATURE_HEADER.into(), signature.as_bytes().into()));
        }
        self.objects.write(&commit).map_err(Error::backend)
    }

    /// First-parent walk of a ref's commits, tip-first; empty when absent.
    pub(crate) fn ref_history(&self, name: &RefName) -> Result<Vec<ObjectId>, Error> {
        let mut out = Vec::new();
        let mut cursor = self.refs.read(name).map_err(Error::backend)?;
        while let Some(id) = cursor {
            out.push(id);
            cursor = self.with_commit(id, |c| Ok(c.parents().next()))?;
        }
        Ok(out)
    }

    /// Read an object as a commit, failing with [`Error::MissingObject`] or
    /// [`Error::NotACommit`] rather than a bare decode error.
    fn with_commit<T>(
        &self,
        id: ObjectId,
        f: impl FnOnce(&gix::objs::CommitRef<'_>) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let mut buf = Vec::new();
        let data = self
            .objects
            .try_find(&id, &mut buf)
            .map_err(Error::backend)?
            .ok_or(Error::MissingObject { oid: id })?;
        if data.kind != gix::objs::Kind::Commit {
            return Err(Error::NotACommit { oid: id });
        }
        let commit = gix::objs::CommitRef::from_bytes(data.data, data.object_hash)
            .map_err(Error::backend)?;
        f(&commit)
    }

    /// Read an object as a tree, failing with [`Error::MissingObject`] rather
    /// than a bare decode error.
    fn with_tree<T>(
        &self,
        id: ObjectId,
        f: impl FnOnce(&gix::objs::TreeRef<'_>) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let mut buf = Vec::new();
        let data = self
            .objects
            .try_find(&id, &mut buf)
            .map_err(Error::backend)?
            .ok_or(Error::MissingObject { oid: id })?;
        let tree =
            gix::objs::TreeRef::from_bytes(data.data, data.object_hash).map_err(Error::backend)?;
        f(&tree)
    }

    fn kind_of(&self, oid: ObjectId) -> Result<Option<gix::objs::Kind>, Error> {
        let mut buf = Vec::new();
        Ok(self
            .objects
            .try_find(&oid, &mut buf)
            .map_err(Error::backend)?
            .map(|data| data.kind))
    }

    /// The tree-entry mode for an already-written object: `Tree` when it is
    /// one, `Blob` otherwise. A value encoding and a schema tree are the only
    /// things bound here, and neither is ever executable, a symlink, or a
    /// submodule.
    pub(crate) fn entry_mode(&self, oid: ObjectId) -> Result<gix::objs::tree::EntryMode, Error> {
        let kind = self.kind_of(oid)?.ok_or(Error::MissingObject { oid })?;
        Ok(gix::objs::tree::EntryMode::from(
            if kind == gix::objs::Kind::Tree {
                gix::objs::tree::EntryKind::Tree
            } else {
                gix::objs::tree::EntryKind::Blob
            },
        ))
    }

    /// Confirm a subtree object is actually present, so an incomplete
    /// transfer reports [`Error::SubtreeMissing`] naming the commit and which
    /// half is absent, instead of a bare object-not-found deeper in the read.
    fn require_present(
        &self,
        oid: ObjectId,
        subtree: Subtree,
        commit: ObjectId,
    ) -> Result<(), Error> {
        match self.kind_of(oid)? {
            Some(_) => Ok(()),
            None => Err(Error::SubtreeMissing {
                subtree,
                oid,
                commit,
            }),
        }
    }
}

/// Every ref name directly under `prefix` that is a single valid
/// [`RefSegment`], ascending. A nested ref under the namespace is skipped
/// rather than mis-reported.
pub(crate) fn list_segments<R: RefStore>(
    refs: &R,
    prefix: &RefPrefix,
) -> Result<Vec<RefSegment>, Error> {
    Ok(refs
        .prefixed(prefix)
        .map_err(Error::backend)?
        .into_iter()
        .filter_map(|(name, _)| Some(name.relative_to(prefix)?.as_segment()?.clone()))
        .collect())
}

/// A [`Store`] over a `gix` repository's own refs and object database.
pub type RepoStore<'r> = Store<GixRefStore<'r>, &'r gix::OdbHandle>;

impl<'r> RepoStore<'r> {
    /// Open a store over `repo`'s own refs and objects, with the default
    /// [`Layout`].
    pub fn open(repo: &'r gix::Repository) -> Self {
        Store::new(GixRefStore::new(repo), &repo.objects)
    }

    /// Open a store over `repo`'s own refs and objects, with a caller-supplied
    /// [`Layout`].
    pub fn open_with_layout(repo: &'r gix::Repository, layout: Layout) -> Self {
        Store::with_layout(GixRefStore::new(repo), &repo.objects, layout)
    }
}
