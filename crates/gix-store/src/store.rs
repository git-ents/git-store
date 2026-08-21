//! [`Store`]: kinds, schemas, and entities as Git refs and commits.

use std::{cell::RefCell, collections::HashMap, rc::Rc};

use facet::Facet;
use facet_git_tree::ObjectId;
use facet_git_tree::{Schema, deserialize_value_with_schema_legacy_leaves};
use facet_value::Value;
use gix::objs::{Find, Write, WriteTo};
use gix_refstore::{
    ApplyError, Committer, ErasedSigner, Expectation, GixRefStore, RefEdit, RefName, RefPath,
    RefPrefix, RefSegment, RefStore, SignatureBytes, Signer,
};

use crate::document::{DocumentInspection, DocumentShapeError, PreparedDocument};
use crate::encoding::{Dynamic, Encoding, Typed};
use crate::error::{Error, Subtree};
use crate::identity::{DocumentTree, EntityId, SchemaTree, ValueTree};
use crate::kind::Kind;
use crate::transaction::Transaction;

/// Options controlling publication of a prepared document.
#[derive(Debug, Clone, Default)]
pub struct PublishOptions {
    /// An optional compatibility alias to maintain alongside the canonical ref.
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

    /// Maintain `alias` as a compatibility ref.
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

/// How strictly a [`Store`] decodes stored leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Compat {
    /// Reject pre-`kind` documents and pre-newline leaves.
    #[default]
    Strict,
    /// Accept legacy leaf framing: pre-`kind` schema documents and
    /// pre-newline leaf blobs.
    LegacyLeaves,
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
/// Stored values retain the schema version they were validated against. Reads
/// use that bound schema, and schema migrations transform values at read time
/// without rewriting existing Git objects.
pub struct Store<R, O> {
    refs: R,
    objects: O,
    layout: Layout,
    signer: Option<Box<dyn ErasedSigner>>,
    schemas: RefCell<HashMap<ObjectId, Rc<Schema>>>,
    compat: Compat,
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
const RESERVED_TRAILERS: [&str; 3] = ["Schema:", "Schema-Version:", "Ents-Ref:"];

/// Reject message lines that would recreate the legacy schema/provenance
/// trailers. They are reserved even though readers ignore them: accepting
/// them would let a caller make a newly written commit look like the old
/// metadata format.
fn validate_commit_message(message: &str) -> Result<(), Error> {
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
            schemas: RefCell::new(HashMap::new()),
            compat: Compat::default(),
        }
    }

    /// Configure a signer for commits written by this store.
    ///
    /// The signer output is stored as opaque bytes. A store without a signer
    /// writes unsigned commits.
    pub fn with_signer(mut self, signer: impl Signer + 'static) -> Self {
        self.signer = Some(Box::new(signer));
        self
    }

    /// Set how strictly this store's decode paths accept stored leaves.
    pub fn with_compat(mut self, compat: Compat) -> Self {
        self.compat = compat;
        self
    }

    /// This store's current leaf-decoding compatibility setting.
    pub(crate) fn compat(&self) -> Compat {
        self.compat
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

    pub(crate) fn schema(&self, tree: ObjectId) -> Result<Rc<Schema>, Error> {
        if let Some(schema) = self.schemas.borrow().get(&tree) {
            return Ok(Rc::clone(schema));
        }

        let schema = Rc::new(Schema::read_pinned(&tree, self.objects())?);
        self.schemas.borrow_mut().insert(tree, Rc::clone(&schema));
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

    /// Decode a bound `{value/, schema/}` tree using only its embedded schema.
    ///
    /// No kind name, schema ref, or schema history is consulted. This is the
    /// dynamic counterpart to [`Kind::decode`](crate::Kind::decode) for callers
    /// that have a document tree but no kind handle. Subject to
    /// [`with_compat`](Self::with_compat): [`Compat::Strict`] (the default)
    /// rejects pre-`kind` documents and pre-newline leaves,
    /// [`Compat::LegacyLeaves`] accepts them.
    pub fn decode(&self, tree: ObjectId) -> Result<Value, Error> {
        match self.compat {
            Compat::Strict => self.decode_with::<Dynamic>(tree),
            Compat::LegacyLeaves => {
                let (value_tree, schema_tree) = split_document(tree, tree, self.objects())?;
                self.decode_value_compat(value_tree, schema_tree)
            }
        }
    }

    /// Decode a value subtree using the explicitly supplied schema subtree.
    ///
    /// No kind name, schema ref, publication history, or trailer is consulted.
    /// The schema is read from `schema_tree` and the value is validated while it
    /// is decoded. Subject to [`with_compat`](Self::with_compat), as
    /// [`decode`](Self::decode) is.
    pub fn decode_value(
        &self,
        value_tree: ValueTree,
        schema_tree: SchemaTree,
    ) -> Result<Value, Error> {
        match self.compat {
            Compat::Strict => {
                let doc = self.schema(schema_tree.object_id())?;
                Dynamic::read(&value_tree.object_id(), &doc, self.objects())
            }
            Compat::LegacyLeaves => {
                self.decode_value_compat(value_tree.object_id(), schema_tree.object_id())
            }
        }
    }

    /// [`decode_value`](Self::decode_value) taking bare object ids.
    #[deprecated(
        since = "0.2.0",
        note = "use `Store::decode_value(ValueTree, SchemaTree)` instead"
    )]
    pub fn decode_value_untyped(
        &self,
        value_tree: ObjectId,
        schema_tree: ObjectId,
    ) -> Result<Value, Error> {
        self.decode_value(ValueTree::from(value_tree), SchemaTree::from(schema_tree))
    }

    /// Decode a historical unbound value tree to JSON-compatible [`Value`],
    /// unconditionally accepting legacy leaf framing regardless of this
    /// store's [`Compat`] setting.
    #[deprecated(
        since = "0.2.0",
        note = "use `Store::with_compat(Compat::LegacyLeaves)` then `Store::decode_value` instead"
    )]
    pub fn decode_value_legacy(
        &self,
        value_tree: ObjectId,
        schema_tree: ObjectId,
    ) -> Result<Value, Error> {
        self.decode_value_compat(value_tree, schema_tree)
    }

    /// Decode a historical bound document to JSON-compatible [`Value`],
    /// unconditionally accepting legacy leaf framing regardless of this
    /// store's [`Compat`] setting.
    ///
    /// This is the opt-in normalization path for old objects: the result is a
    /// value callers can serialize as JSON and then write through the current
    /// format. No old object is rewritten by this method.
    #[deprecated(
        since = "0.2.0",
        note = "use `Store::with_compat(Compat::LegacyLeaves)` then `Store::decode` instead"
    )]
    pub fn decode_legacy(&self, document_tree: ObjectId) -> Result<Value, Error> {
        let (value_tree, schema_tree) =
            split_document(document_tree, document_tree, self.objects())?;
        self.decode_value_compat(value_tree, schema_tree)
    }

    /// The legacy-leaves decode path shared by [`decode`](Self::decode) and
    /// [`decode_value`](Self::decode_value) under [`Compat::LegacyLeaves`],
    /// and by their deprecated `_legacy` counterparts unconditionally.
    fn decode_value_compat(&self, value_tree: ObjectId, schema_tree: ObjectId) -> Result<Value, Error> {
        let doc = Schema::read_pinned_legacy(&schema_tree, self.objects())?;
        Ok(deserialize_value_with_schema_legacy_leaves(
            &value_tree,
            &doc,
            self.objects(),
        )?)
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

    /// [`encode_value`](Self::encode_value) taking and returning bare object ids.
    #[deprecated(
        since = "0.2.0",
        note = "use `Store::encode_value(value, SchemaTree)` instead"
    )]
    pub fn encode_value_untyped(
        &self,
        value: &Value,
        schema_tree: ObjectId,
    ) -> Result<ObjectId, Error>
    where
        O: Write,
    {
        Ok(self
            .encode_value(value, SchemaTree::from(schema_tree))?
            .object_id())
    }

    /// Bind already-written value and schema subtrees into a prepared document.
    ///
    /// This writes only the root tree containing `schema/` and `value/`; it
    /// creates no commit and advances no ref. Call [`encode_value`](Self::encode_value)
    /// first when the value still needs schema-directed validation.
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
        let document_tree = self.bind_schema(value_oid, schema_oid)?;
        Ok(PreparedDocument {
            document_tree: DocumentTree::from(document_tree),
            value_tree,
            schema_tree,
        })
    }

    /// [`bind_document`](Self::bind_document) taking bare object ids.
    #[deprecated(
        since = "0.2.0",
        note = "use `Store::bind_document(ValueTree, SchemaTree)` instead"
    )]
    pub fn bind_document_untyped(
        &self,
        value_tree: ObjectId,
        schema_tree: ObjectId,
    ) -> Result<PreparedDocument, Error>
    where
        O: Write,
    {
        self.bind_document(ValueTree::from(value_tree), SchemaTree::from(schema_tree))
    }

    /// Inspect a document boundary without consulting a kind or schema ref.
    ///
    /// Exact `{schema/, value/}` roots are returned as [`DocumentInspection::Bound`].
    /// Trees without either envelope entry are reported as legacy unbound value
    /// roots; envelope-like but invalid trees are reported as structured
    /// [`DocumentInspection::Malformed`] metadata instead of being guessed.
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
        let value = entries
            .iter()
            .find(|entry| entry.0 == Subtree::Value.as_str());
        let schema = entries
            .iter()
            .find(|entry| entry.0 == Subtree::Schema.as_str());

        if entries.len() == 2
            && let (Some(value), Some(schema)) = (value, schema)
        {
            if !schema.2 {
                return Ok(DocumentInspection::Malformed {
                    document_tree,
                    found,
                    reason: DocumentShapeError::SchemaNotTree,
                });
            }
            if self.kind_of(value.1)?.is_none() {
                return Ok(DocumentInspection::Malformed {
                    document_tree,
                    found,
                    reason: DocumentShapeError::ValueMissing { oid: value.1 },
                });
            }
            match self.kind_of(schema.1)? {
                Some(gix::objs::Kind::Tree) => {
                    return Ok(DocumentInspection::Bound(PreparedDocument {
                        document_tree: DocumentTree::from(document_tree),
                        value_tree: ValueTree::from(value.1),
                        schema_tree: SchemaTree::from(schema.1),
                    }));
                }
                Some(_) => {
                    return Ok(DocumentInspection::Malformed {
                        document_tree,
                        found,
                        reason: DocumentShapeError::SchemaNotTree,
                    });
                }
                None => {
                    return Ok(DocumentInspection::Malformed {
                        document_tree,
                        found,
                        reason: DocumentShapeError::SchemaMissing { oid: schema.1 },
                    });
                }
            }
        }

        if value.is_none() && schema.is_none() {
            return Ok(DocumentInspection::LegacyValueRoot {
                value_tree: document_tree,
            });
        }

        Ok(DocumentInspection::Malformed {
            document_tree,
            found: found.clone(),
            reason: DocumentShapeError::UnexpectedEntries { found },
        })
    }

    /// [`inspect_document`](Self::inspect_document) taking a bare object id.
    #[deprecated(
        since = "0.2.0",
        note = "use `Store::inspect_document(DocumentTree)` instead"
    )]
    pub fn inspect_document_untyped(
        &self,
        document_tree: ObjectId,
    ) -> Result<DocumentInspection, Error> {
        self.inspect_document(DocumentTree::from(document_tree))
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
    pub(crate) fn bind_schema(&self, value: ObjectId, schema: ObjectId) -> Result<ObjectId, Error>
    where
        O: Write,
    {
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
        split_document(root, commit, self.objects())
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

/// Decode a bound `{value/, schema/}` tree using only the schema embedded in
/// that tree and the supplied object database.
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
    let (value_tree, schema_tree) = split_document(tree, tree, objects)?;
    let doc = Schema::read_pinned(&schema_tree, objects)?;
    E::read(&value_tree, &doc, objects)
}

fn split_document<S: Find + ?Sized>(
    root: ObjectId,
    commit: ObjectId,
    objects: &S,
) -> Result<(ObjectId, ObjectId), Error> {
    let mut buf = Vec::new();
    let data = objects
        .try_find(&root, &mut buf)
        .map_err(Error::backend)?
        .ok_or(Error::MissingObject { oid: root })?;
    if data.kind != gix::objs::Kind::Tree {
        return Err(Error::NotATree { oid: root });
    }
    let tree =
        gix::objs::TreeRef::from_bytes(data.data, data.object_hash).map_err(Error::backend)?;
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
    let value = value.oid.to_owned();
    let schema = schema.oid.to_owned();
    match object_kind(objects, value)? {
        Some(_) => {}
        None => {
            return Err(Error::SubtreeMissing {
                subtree: Subtree::Value,
                oid: value,
                commit,
            });
        }
    }
    match object_kind(objects, schema)? {
        Some(gix::objs::Kind::Tree) => {}
        Some(_) => return Err(Error::NotATree { oid: schema }),
        None => {
            return Err(Error::SubtreeMissing {
                subtree: Subtree::Schema,
                oid: schema,
                commit,
            });
        }
    }
    Ok((value, schema))
}

fn object_kind<S: Find + ?Sized>(
    objects: &S,
    oid: ObjectId,
) -> Result<Option<gix::objs::Kind>, Error> {
    let mut buf = Vec::new();
    Ok(objects
        .try_find(&oid, &mut buf)
        .map_err(Error::backend)?
        .map(|data| data.kind))
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
