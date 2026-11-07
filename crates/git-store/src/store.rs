//! [`Store`]: kinds, schemas, and entities as Git refs and commits.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use facet::Facet;
use facet_git_tree::ObjectId;
use facet_git_tree::{Schema, deserialize, validate_with_schema};
use facet_value::Value;
use gix::objs::{Find, Write, WriteTo};
use gix_refstore::{
    ApplyError, Committer, ErasedSigner, Expectation, GixRefStore, RefEdit, RefName, RefPath,
    RefPrefix, RefSegment, RefStore, SignatureBytes, Signer,
};

use crate::document::{DocumentInspection, DocumentShapeError, PreparedDocument};
use crate::encoding::{Dynamic, Encoding, Typed};
use crate::envelope::{self, SCHEMA_ENTRY, ensure_struct_root};
use crate::error::Error;
use crate::identity::{DocumentTree, EntityId, SchemaTree, ValueTree};
use crate::kind::Kind;
use crate::retry::Budget;
use crate::transaction::Transaction;

/// Options controlling publication of a prepared document.
#[derive(Debug, Clone, Default)]
pub struct PublishOptions {
    /// The ref name to publish under, replacing the content-derived default.
    pub alias: Option<RefPath>,
    /// The publication commit message.
    pub message: String,
    /// An optional explicit parent for a newly written publication commit.
    ///
    /// This is useful when importing a document above a legacy tip while the
    /// destination alias is being created with [`Expectation::Absent`]. It is
    /// generic plumbing: the store does not interpret the parent's kind or
    /// provenance.
    pub parent: Option<ObjectId>,
    /// An optional one-shot compare-and-swap expectation for the alias or
    /// canonical ref selected by the publication.
    pub expectation: Option<Expectation>,
}

impl PublishOptions {
    /// Create options with `message` and no alias, parent, or expectation.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            ..Self::default()
        }
    }

    /// Publish under `alias` instead of the content-derived entity-id name.
    pub fn with_alias(mut self, alias: RefPath) -> Self {
        self.alias = Some(alias);
        self
    }

    /// Set an explicit parent for a newly written publication commit.
    pub fn with_parent(mut self, parent: ObjectId) -> Self {
        self.parent = Some(parent);
        self
    }

    /// Apply `expectation` as a one-shot compare-and-swap.
    pub fn with_expectation(mut self, expectation: Expectation) -> Self {
        self.expectation = Some(expectation);
        self
    }
}

/// The identities produced by publishing a prepared document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Publication {
    /// The content-derived entity identity, equal to the document tree id.
    pub id: EntityId,
    /// The publication commit containing the document tree.
    pub commit: ObjectId,
}

impl Publication {
    pub(crate) const fn new(id: EntityId, commit: ObjectId) -> Self {
        Self { id, commit }
    }

    /// The content-derived entity identity.
    pub const fn entity_id(self) -> EntityId {
        self.id
    }

    /// The publication commit.
    pub const fn commit(self) -> ObjectId {
        self.commit
    }
}

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
/// Stored values retain the schema against which they were validated. Reads
/// use that bound schema without consulting publication history.
pub struct Store<R, O> {
    refs: R,
    objects: O,
    layout: Layout,
    signer: Option<Box<dyn ErasedSigner + Send + Sync>>,
    /// Decoded schemas, keyed by their content-addressed schema tree id.
    ///
    /// An `RwLock` rather than a `RefCell` lets a `Store` be shared across
    /// threads when its ref store and object store are themselves `Sync`.
    /// Concurrent misses may compute the same schema twice; the value is
    /// content-addressed, so the duplicate is harmless and needs no
    /// invalidation.
    schemas: RwLock<HashMap<ObjectId, Arc<Schema>>>,
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

/// Trailer names a caller's commit message may not begin a line with.
///
/// [`Schema-Version:`](crate) and [`Schema-commit:`](crate) are the advisory
/// schema trailers this crate's specification defines; `Schema:` and
/// `Ents-Ref:` are the legacy schema/provenance names it replaced. All four are
/// reserved even though readers ignore trailers entirely: accepting one would
/// let a caller make a newly written commit look like a schema binding that
/// the embedded `.schema` entry, not a trailer, actually carries.
const RESERVED_TRAILERS: [&str; 4] = ["Schema:", "Schema-Version:", "Schema-commit:", "Ents-Ref:"];

/// Reject message lines that would recreate a schema/provenance trailer.
pub(crate) fn validate_commit_message(message: &str) -> Result<(), Error> {
    for line in message.lines() {
        let line = line.trim_start();
        if let Some(&trailer) = RESERVED_TRAILERS
            .iter()
            .find(|trailer| line.starts_with(**trailer))
        {
            return Err(Error::ReservedTrailer { trailer });
        }
    }
    Ok(())
}

impl<R, O> Store<R, O>
where
    R: RefStore,
    O: Find,
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
            schemas: RwLock::new(HashMap::new()),
        }
    }

    /// Configure a signer for commits written by this store.
    ///
    /// The signer output is stored as opaque bytes. A store without a signer
    /// writes unsigned commits. The signer must be `Send + Sync` so a store
    /// whose backends are `Sync` can be shared across threads.
    pub fn with_signer(mut self, signer: impl Signer + Send + Sync + 'static) -> Self {
        self.signer = Some(Box::new(signer));
        self
    }

    /// Stage publications and deletions, across any number of kinds, to
    /// land as one all-or-nothing compare-and-swap batch. `message` is used
    /// as the commit message for every publication and tombstone the
    /// transaction writes.
    pub fn transaction(&self, message: impl Into<String>) -> Transaction<'_, R, O>
    where
        R: Committer,
        O: Write,
    {
        Transaction::new(self, message.into())
    }

    /// Return the opaque signature bytes carried by `commit`, or `None` when
    /// it is unsigned.
    ///
    /// The store does not verify or interpret the returned bytes.
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

    pub(crate) fn schema(&self, tree: ObjectId) -> Result<Arc<Schema>, Error> {
        {
            let cache = self
                .schemas
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(schema) = cache.get(&tree) {
                return Ok(Arc::clone(schema));
            }
        }

        // Decode outside the cache lock: a miss must not hold a read lock
        // while another thread computes the same schema, and a hit returns
        // the cached `Arc` without ever touching the object store.
        let schema = Arc::new(deserialize::<Schema>(&tree, self.objects())?);
        self.schemas
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(tree, Arc::clone(&schema));
        Ok(schema)
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

    /// Decode a bound document root using only its embedded `.schema` entry.
    ///
    /// No kind name, schema ref, or schema history is consulted. This is the
    /// dynamic counterpart to [`Kind::decode`](crate::Kind::decode) for callers
    /// that have a document tree but no kind handle.
    pub fn decode(&self, tree: ObjectId) -> Result<Value, Error> {
        self.decode_with::<Dynamic>(tree)
    }

    /// Decode a value subtree using the explicitly supplied schema subtree.
    ///
    /// No kind name, schema ref, publication history, or trailer is consulted.
    /// The schema is read from `schema_tree` and the value is validated while it
    /// is decoded.
    pub fn decode_value(
        &self,
        value_tree: ValueTree,
        schema_tree: SchemaTree,
    ) -> Result<Value, Error> {
        let doc = self.schema(schema_tree.object_id())?;
        Dynamic::read(&value_tree.object_id(), &doc, self.objects())
    }

    /// Encode a dynamic value under an explicitly supplied schema subtree.
    ///
    /// Encoding is validation: a value that does not conform to the schema is
    /// rejected before any document envelope or publication is written.
    pub fn encode_value(&self, value: &Value, schema_tree: SchemaTree) -> Result<ValueTree, Error>
    where
        O: Write,
    {
        let doc = self.schema(schema_tree.object_id())?;
        Ok(ValueTree::from(Dynamic::write(
            value,
            &doc,
            self.objects(),
        )?))
    }

    /// Bind an already-written value tree into a prepared document over an
    /// explicitly supplied schema tree.
    ///
    /// This writes only the document root — the value's own entries beside the
    /// reserved `.schema` entry — and creates no commit and advances no ref.
    /// The value is validated against the schema first, so a value that does
    /// not conform, or a schema whose resolved root is not a struct, is
    /// rejected before any tree is written.
    pub fn bind_document(
        &self,
        value_tree: ValueTree,
        schema_tree: SchemaTree,
    ) -> Result<PreparedDocument, Error>
    where
        O: Write,
    {
        let value_oid = value_tree.object_id();
        let schema_oid = schema_tree.object_id();
        match self.kind_of(value_oid)? {
            Some(_) => {}
            None => return Err(Error::MissingObject { oid: value_oid }),
        }
        match self.kind_of(schema_oid)? {
            Some(gix::objs::Kind::Tree) => {}
            Some(_) => return Err(Error::NotATree { oid: schema_oid }),
            None => return Err(Error::MissingObject { oid: schema_oid }),
        }
        let doc = self.schema(schema_oid)?;
        ensure_struct_root(&doc)?;
        validate_with_schema(&value_oid, &doc, self.objects())?;
        let document_tree = self.bind_schema(value_oid, schema_oid)?;
        Ok(PreparedDocument {
            document_tree: DocumentTree::from(document_tree),
            schema_tree,
        })
    }

    /// Classify a document boundary without consulting a kind or schema ref.
    ///
    /// This is a *classifier*, not a validating parser: it answers "does this
    /// tree carry the `.schema` envelope entry?" and never fails on shape.
    /// `envelope::split` — the read path — is the *validating parser*: it
    /// accepts only a well-formed envelope and rejects every shape below as
    /// [`Error::NotSubtreeBound`] or [`Error::SubtreeMissing`]. The two
    /// therefore answer "is this a document envelope?" identically — this
    /// method reports `Bound` exactly when `envelope::split` succeeds — while
    /// `decode` adds the schema deserialization and read on top.
    ///
    /// A root carrying a readable `.schema` entry beside at least one entry of
    /// the value's own is returned as [`DocumentInspection::Bound`]. A tree with
    /// no `.schema` entry is reported as [`DocumentInspection::LegacyValueRoot`]
    /// — the absence of an envelope, not a claim that the tree is a value.
    /// Envelope-like but invalid trees — a `.schema` that is not a tree or is
    /// missing, or one with no value entries beside it — are reported as
    /// structured [`DocumentInspection::Malformed`] metadata instead of being
    /// guessed. An empty root is malformed rather than a legacy value candidate:
    /// it is neither an envelope nor a bare value tree.
    pub fn inspect_document(
        &self,
        document_tree: DocumentTree,
    ) -> Result<DocumentInspection, Error> {
        let document_tree = document_tree.object_id();
        let mut entries = self.with_tree(document_tree, |tree| {
            Ok(tree
                .entries
                .iter()
                .map(|entry| {
                    (
                        String::from_utf8_lossy(entry.filename).into_owned(),
                        entry.oid.to_owned(),
                        entry.mode.is_tree(),
                    )
                })
                .collect::<Vec<_>>())
        })?;
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        let found: Vec<String> = entries.iter().map(|entry| entry.0.clone()).collect();
        let schema = entries.iter().find(|entry| entry.0 == SCHEMA_ENTRY);

        let Some(schema) = schema else {
            // No `.schema` entry: not an envelope. Every other shape here could
            // be a pre-binding bare value tree, which the classifier reports
            // without interpreting. An empty root cannot: it is neither an
            // envelope nor a bare value tree, so it is refused rather than
            // offered as a legacy value candidate.
            if entries.is_empty() {
                return Ok(DocumentInspection::Malformed {
                    document_tree,
                    found,
                    reason: DocumentShapeError::EmptyRoot,
                });
            }
            return Ok(DocumentInspection::LegacyValueRoot {
                value_tree: document_tree,
            });
        };
        if !schema.2 {
            return Ok(DocumentInspection::Malformed {
                document_tree,
                found,
                reason: DocumentShapeError::SchemaNotTree,
            });
        }
        let no_value_entries = entries.len() < 2;
        match self.kind_of(schema.1)? {
            Some(gix::objs::Kind::Tree) if no_value_entries => Ok(DocumentInspection::Malformed {
                document_tree,
                found,
                reason: DocumentShapeError::NoValueEntries,
            }),
            Some(gix::objs::Kind::Tree) => Ok(DocumentInspection::Bound(PreparedDocument {
                document_tree: DocumentTree::from(document_tree),
                schema_tree: SchemaTree::from(schema.1),
            })),
            Some(_) => Ok(DocumentInspection::Malformed {
                document_tree,
                found,
                reason: DocumentShapeError::SchemaNotTree,
            }),
            None => Ok(DocumentInspection::Malformed {
                document_tree,
                found,
                reason: DocumentShapeError::SchemaMissing { oid: schema.1 },
            }),
        }
    }

    pub(crate) fn decode_with<E: Encoding>(&self, tree: ObjectId) -> Result<E::Value, Error> {
        decode_with::<E, _>(tree, self.objects())
    }

    /// A handle on the kind `name` under a caller-supplied tree [`Encoding`].
    pub fn kind_with<E: Encoding>(&self, name: RefSegment) -> Kind<'_, E, R, O> {
        Kind::new(self, name)
    }

    /// Every kind that has a published schema, ascending.
    pub fn kinds(&self) -> Result<Vec<RefSegment>, Error> {
        list_segments(&self.refs, &self.layout.schema)
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

    /// Splice an already-written value tree into the document root its own
    /// tree becomes: the value's entries beside the reserved `.schema` entry.
    pub(crate) fn bind_schema(&self, value: ObjectId, schema: ObjectId) -> Result<ObjectId, Error>
    where
        O: Write,
    {
        let mut entries = self.tree_entries(value)?;
        entries.push(gix::objs::tree::Entry {
            mode: self.entry_mode(schema)?,
            filename: SCHEMA_ENTRY.into(),
            oid: schema,
        });
        entries.sort();
        self.objects
            .write(&gix::objs::Tree { entries })
            .map_err(Error::backend)
    }

    /// Split a document root into the root itself and its embedded schema.
    ///
    /// The root must carry a `.schema` entry holding a tree, and at least one
    /// entry of the value's own. Requiring the whole shape, rather than looking
    /// up `.schema` in isolation, is what makes a bare value tree — or a
    /// pre-binding commit, whose tree *was* the value — fail as one diagnosable
    /// [`Error::NotSubtreeBound`] rather than being half-matched and
    /// misreported.
    ///
    /// The first element is the *document* root, not a bare value tree: reading
    /// its value goes through [`UnboundValue`](crate::envelope::UnboundValue).
    pub(crate) fn split(
        &self,
        root: ObjectId,
        commit: ObjectId,
    ) -> Result<(ObjectId, ObjectId), Error> {
        envelope::split(root, commit, self.objects())
    }

    /// Build and commit a tree forward over the current tip of `name`,
    /// retrying on a lost compare-and-swap race.
    pub(crate) fn commit_forward(
        &self,
        name: &RefName,
        message: &str,
        mut build_tree: impl FnMut(Option<ObjectId>) -> Result<ObjectId, Error>,
    ) -> Result<ObjectId, Error>
    where
        R: Committer,
        O: Write,
    {
        let mut budget = Budget::new();
        loop {
            let parent = self.refs.read(name).map_err(Error::backend)?;
            let tree = build_tree(parent)?;
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
                Err(ApplyError::LostRace { .. }) if budget.retry() => continue,
                Err(ApplyError::LostRace { .. }) => return Err(budget.exhausted()),
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
    ) -> Result<ObjectId, Error>
    where
        R: Committer,
        O: Write,
    {
        validate_commit_message(message)?;
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
        if data.kind != gix::objs::Kind::Tree {
            return Err(Error::NotATree { oid: id });
        }
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
}

/// Decode a bound document root using only the schema embedded as its
/// `.schema` entry and the supplied object database.
///
/// The object database is the only source consulted. In particular, this
/// function does not need a [`Store`], a kind name, a schema ref, or schema
/// history, which makes it suitable for decoding a document reached by any
/// content-addressed path.
pub fn decode<S: Find + ?Sized>(tree: ObjectId, objects: &S) -> Result<Value, Error> {
    decode_with::<Dynamic, _>(tree, objects)
}

fn decode_with<E: Encoding, S: Find + ?Sized>(
    tree: ObjectId,
    objects: &S,
) -> Result<E::Value, Error> {
    let (root, schema_tree) = envelope::split(tree, tree, objects)?;
    let doc = deserialize::<Schema>(&schema_tree, objects)?;
    envelope::read_value::<E, _>(root, &doc, objects)
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
