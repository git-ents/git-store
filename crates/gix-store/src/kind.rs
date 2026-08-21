//! One [`Kind`]: its schema ref and the entities beneath it.

use std::{marker::PhantomData, rc::Rc};

use facet::Facet;
use facet_git_tree::{
    Derivation, Hints, ObjectId, Schema, SchemaPinError, check_universe_at,
    deserialize_value_with_schema, identity_subtrees, migration::derive::derive, schema_of,
};
use facet_value::Value;
use gix::objs::{Find, Write};
use gix_refstore::{
    ApplyError, Committer, RefEdit, RefName, RefPath, RefPrefix, RefSegment, RefStore,
};

use crate::address::At;
use crate::document::PreparedDocument;
use crate::encoding::{Encoding, Typed};
use crate::error::Error;
use crate::identity::{EntityId, canonical_document_id};
use crate::index;
use crate::migrate::TargetSchema;
use crate::store::{Publication, PublishOptions, Store};
use crate::tombstone::{self, DeleteResult, EntityState, Tombstone, TombstoneEntry};

const KIND_FINGERPRINT_DOMAIN: &[u8] = b"gix-store\0kind-fingerprint\0v1\0";

/// One kind: its schema ref and the entities beneath it.
pub struct Kind<'s, E, R, O> {
    store: &'s Store<R, O>,
    name: RefSegment,
    schema_ref: RefName,
    entities: RefPrefix,
    encoding: PhantomData<fn() -> E>,
}

impl<'s, E, R, O> Kind<'s, E, R, O>
where
    R: RefStore,
    O: Find,
{
    pub(crate) fn new(store: &'s Store<R, O>, name: RefSegment) -> Self {
        let schema_ref = store.layout().schema.join(&name);
        let entities = store.layout().data.child(&name);
        Kind {
            store,
            name,
            schema_ref,
            entities,
            encoding: PhantomData,
        }
    }
}

impl<'s, E: Encoding, R, O> Kind<'s, E, R, O>
where
    R: RefStore,
    O: Find,
{
    /// This kind's name.
    pub fn name(&self) -> &RefSegment {
        &self.name
    }

    /// The ref backing an entity name.
    pub fn reference(&self, name: &RefPath) -> RefName {
        self.entities.join_path(name)
    }

    /// The ref backing an entity published under its content-derived id.
    pub fn entity_reference(&self, id: EntityId) -> RefName {
        self.reference(&entity_id_name(id))
    }

    /// This kind's schema.
    pub fn schema(&self) -> KindSchema<'s, R, O> {
        KindSchema {
            store: self.store,
            kind: self.name.clone(),
            reference: self.schema_ref.clone(),
        }
    }

    /// Store `value` under `name`, advancing that ref like a branch.
    ///
    /// The name is whatever the caller chooses; the store attaches no meaning
    /// to it. Republishing identical content is a no-op, and every earlier
    /// publication stays reachable from the name. Use
    /// [`compile_entity`](Self::compile_entity) for the content-derived
    /// [`EntityId`] of the same document.
    pub fn put(&self, name: &RefPath, value: &E::Value) -> Result<ObjectId, Error>
    where
        R: Committer,
        O: Write,
    {
        self.write(value).at(name)
    }

    /// Store `value` under its content-derived name and return that id.
    ///
    /// This is the naming policy to use when an entity has no meaningful name
    /// yet, or when identical content must land at one ref regardless of who
    /// publishes it.
    pub fn put_entity(&self, value: &E::Value) -> Result<EntityId, Error>
    where
        R: Committer,
        O: Write,
    {
        self.write(value).canonical()
    }

    /// Store `value` under `name` and return its content-derived id rather
    /// than the publication commit.
    pub fn put_with_alias(&self, alias: &RefPath, value: &E::Value) -> Result<EntityId, Error>
    where
        R: Committer,
        O: Write,
    {
        self.write(value).with_alias(alias)
    }

    /// The general form of [`put`](Self::put): set a message, then choose a
    /// name.
    pub fn write<'k>(&'k self, value: &'k E::Value) -> Put<'k, E, R, O>
    where
        R: Committer,
        O: Write,
    {
        Put {
            kind: self,
            value,
            message: None,
        }
    }

    /// Read the current state at `at` — a name, entity id, publication
    /// commit, or bare document tree — using this kind's native encoding.
    ///
    /// This covers every address this crate supports; [`EntityState`]'s
    /// projection methods ([`present`](EntityState::present),
    /// [`value`](EntityState::value), [`deleted`](EntityState::deleted))
    /// narrow the result at the call site instead of naming a differently
    /// shaped read method. `at` is [`At::Tree`] for an unpublished bound
    /// document: [`Entry::commit`] then holds the tree id itself (there is
    /// no publication commit) and [`Entry::message`] is empty.
    pub fn read(&self, at: impl Into<At>) -> Result<EntityState<E::Value>, Error> {
        match at.into() {
            At::Name(name) => match self.resolve_named(&name)? {
                Some(commit) => self.read_commit(commit),
                None => Ok(EntityState::Absent),
            },
            At::Entity(id) => match self.resolve_entity(id)? {
                Some(commit) => self.read_commit(commit),
                None => Ok(EntityState::Absent),
            },
            At::Commit(commit) => self.read_commit(commit),
            At::Tree(tree) => self.read_tree(tree),
        }
    }

    /// [`read`](Self::read), upcast to an explicitly selected `target`
    /// schema and history.
    ///
    /// The migration axis is available for every address [`read`](Self::read)
    /// accepts, including [`At::Entity`] — closing the gap where entity-id
    /// addressing had no migration support. The target is used only after
    /// the value's embedded schema has decoded, and a tombstone is returned
    /// as [`EntityState::Deleted`] without consulting `target` at all.
    pub fn read_as(
        &self,
        at: impl Into<At>,
        target: &TargetSchema,
    ) -> Result<EntityState<Value>, Error> {
        self.read_dynamic(at, Some(target))
    }

    fn resolve_named(&self, name: &RefPath) -> Result<Option<ObjectId>, Error> {
        self.store
            .refs()
            .read(&self.reference(name))
            .map_err(Error::backend)
    }

    fn resolve_entity(&self, id: EntityId) -> Result<Option<ObjectId>, Error> {
        self.store
            .refs()
            .read(&self.entity_reference(id))
            .map_err(Error::backend)
    }

    fn read_commit(&self, commit: ObjectId) -> Result<EntityState<E::Value>, Error> {
        let (value_tree, _schema_tree, doc, message) = self.read_bound(commit)?;
        self.bound_state(value_tree, doc, commit, message)
    }

    fn read_tree(&self, tree: ObjectId) -> Result<EntityState<E::Value>, Error> {
        let (value_tree, schema_tree) = self.store.split(tree, tree)?;
        let doc = self.store.schema(schema_tree)?;
        self.bound_state(value_tree, doc, tree, String::new())
    }

    fn bound_state(
        &self,
        value_tree: ObjectId,
        doc: Rc<Schema>,
        commit: ObjectId,
        message: String,
    ) -> Result<EntityState<E::Value>, Error> {
        if let Some(tombstone) = tombstone::read(self.store, &value_tree, &doc)? {
            self.ensure_kind(&tombstone.kind)?;
            return Ok(EntityState::Deleted(TombstoneEntry {
                tombstone,
                commit,
                message,
            }));
        }
        self.ensure_document_kind(&doc)?;
        let value = E::read(&value_tree, &doc, self.store.objects())?;
        Ok(EntityState::Present(Entry {
            value,
            commit,
            message,
        }))
    }

    fn read_commit_migrated(
        &self,
        commit: ObjectId,
        target: Option<&TargetSchema>,
    ) -> Result<EntityState<Value>, Error> {
        let (value_tree, schema_tree, doc, message) = self.read_bound(commit)?;
        self.migrated_state(value_tree, schema_tree, doc, commit, message, target)
    }

    fn read_tree_migrated(
        &self,
        tree: ObjectId,
        target: Option<&TargetSchema>,
    ) -> Result<EntityState<Value>, Error> {
        let (value_tree, schema_tree) = self.store.split(tree, tree)?;
        let doc = self.store.schema(schema_tree)?;
        self.migrated_state(value_tree, schema_tree, doc, tree, String::new(), target)
    }

    fn migrated_state(
        &self,
        value_tree: ObjectId,
        schema_tree: ObjectId,
        doc: Rc<Schema>,
        commit: ObjectId,
        message: String,
        target: Option<&TargetSchema>,
    ) -> Result<EntityState<Value>, Error> {
        if let Some(tombstone) = tombstone::read(self.store, &value_tree, &doc)? {
            self.ensure_kind(&tombstone.kind)?;
            return Ok(EntityState::Deleted(TombstoneEntry {
                tombstone,
                commit,
                message,
            }));
        }
        self.ensure_document_kind(&doc)?;
        let value = deserialize_value_with_schema(&value_tree, &doc, self.store.objects())?;
        let schema = self.schema();
        let value = match target {
            Some(target) => schema.upcast_to(&value, &schema_tree, target)?,
            None => schema.upcast_current(&value, &schema_tree)?,
        };
        Ok(EntityState::Present(Entry {
            value,
            commit,
            message,
        }))
    }

    /// The dynamic read shared by [`read_as`](Self::read_as) and the
    /// deprecated current-schema shims: `target: None` resolves this kind's
    /// current published schema lazily, only once a present (non-tombstone)
    /// value is actually reached, matching those shims' historical laziness.
    fn read_dynamic(
        &self,
        at: impl Into<At>,
        target: Option<&TargetSchema>,
    ) -> Result<EntityState<Value>, Error> {
        match at.into() {
            At::Name(name) => match self.resolve_named(&name)? {
                Some(commit) => self.read_commit_migrated(commit, target),
                None => Ok(EntityState::Absent),
            },
            At::Entity(id) => match self.resolve_entity(id)? {
                Some(commit) => self.read_commit_migrated(commit, target),
                None => Ok(EntityState::Absent),
            },
            At::Commit(commit) => self.read_commit_migrated(commit, target),
            At::Tree(tree) => self.read_tree_migrated(tree, target),
        }
    }

    /// Read the current state addressed by its content-derived id.
    #[deprecated(since = "0.2.0", note = "use `Kind::read(id)` instead")]
    pub fn read_entity(&self, id: EntityId) -> Result<EntityState<E::Value>, Error> {
        self.read(id)
    }

    /// The current value at an alias or canonical ref path, or `None` when
    /// absent or explicitly deleted. This is the compatibility adapter for
    /// callers that cannot represent a deleted state.
    #[deprecated(since = "0.2.0", note = "use `Kind::read(name).value()` instead")]
    pub fn get(&self, name: &RefPath) -> Result<Option<E::Value>, Error> {
        Ok(self.read(name.clone())?.value())
    }

    /// Read the current value addressed by its content-derived id, retaining
    /// the old `Option` behavior for compatibility.
    #[deprecated(since = "0.2.0", note = "use `Kind::read(id).value()` instead")]
    pub fn get_entity(&self, id: EntityId) -> Result<Option<E::Value>, Error> {
        Ok(self.read(id)?.value())
    }

    /// The state as of one data commit, read entirely out of that commit's
    /// own tree.
    #[deprecated(since = "0.2.0", note = "use `Kind::read(commit)` instead")]
    pub fn read_at(&self, commit: ObjectId) -> Result<EntityState<E::Value>, Error> {
        self.read(commit)
    }

    /// The value as of one data commit, preserving the old typed API.
    #[deprecated(since = "0.2.0", note = "use `Kind::read(commit)` instead")]
    pub fn get_at(&self, commit: ObjectId) -> Result<E::Value, Error> {
        match self.read(commit)? {
            EntityState::Present(entry) => Ok(entry.value),
            EntityState::Deleted(_) => Err(Error::Deleted { commit }),
            EntityState::Absent => unreachable!("a commit is always present"),
        }
    }

    /// The current value at the canonical entity ref, together with its
    /// publication commit. Tombstones are represented as `None` for
    /// compatibility; use [`read`](Self::read) to observe them.
    #[deprecated(since = "0.2.0", note = "use `Kind::read(id).present()` instead")]
    pub fn get_entry_entity(&self, id: EntityId) -> Result<Option<Entry<E::Value>>, Error> {
        Ok(self.read(id)?.present())
    }

    /// Decode a `{value/, schema/}` tree directly — the read-side mirror of
    /// [`compile`](Self::compile). Unlike [`get_at`](Self::get_at), `tree`
    /// need not be a commit's tree, and no ref is consulted: any tree of that
    /// shape decodes, however it was reached.
    pub fn decode(&self, tree: ObjectId) -> Result<E::Value, Error> {
        self.store.decode_with::<E>(tree)
    }

    /// Compile `value` under this kind's current schema into the same
    /// `{value/, schema/}` tree a write commits — but writes no commit and
    /// advances no ref. The returned tree hash is the document's identity: a
    /// pure function of the value and the schema it is checked against.
    pub fn compile(&self, value: &E::Value) -> Result<ObjectId, Error>
    where
        O: Write,
    {
        let (_, tree) = self.compile_with_schema(value)?;
        Ok(tree)
    }

    /// Compile `value` and return the content-derived identity of its complete
    /// bound document tree without publishing a commit or ref.
    pub fn compile_entity(&self, value: &E::Value) -> Result<EntityId, Error>
    where
        O: Write,
    {
        Ok(canonical_document_id(self.compile(value)?))
    }

    /// [`compile`](Self::compile), plus the schema commit it was compiled
    /// against — shared with the committing write path so the two agree by
    /// construction.
    fn compile_with_schema(&self, value: &E::Value) -> Result<(ObjectId, ObjectId), Error>
    where
        O: Write,
    {
        let (schema_commit, doc) = self.current_schema()?;
        let value_tree = E::write(value, &doc, self.store.objects())?;
        let schema_tree = self.store.commit_tree(schema_commit)?;
        Ok((
            schema_commit,
            self.store.bind_schema(value_tree, schema_tree)?,
        ))
    }

    /// The current value at `name` together with the commit it came from, or
    /// `None` when absent or deleted. Use [`read`](Self::read) for the
    /// distinct state result.
    #[deprecated(since = "0.2.0", note = "use `Kind::read(name).present()` instead")]
    pub fn get_entry(&self, name: &RefPath) -> Result<Option<Entry<E::Value>>, Error> {
        Ok(self.read(name.clone())?.present())
    }

    /// [`get_entry`](Self::get_entry) for one data commit directly, read
    /// entirely out of that commit's own tree. A tombstone is reported as
    /// [`Error::Deleted`] for this compatibility API.
    #[deprecated(since = "0.2.0", note = "use `Kind::read(commit)` instead")]
    pub fn get_entry_at(&self, commit: ObjectId) -> Result<Entry<E::Value>, Error> {
        match self.read(commit)? {
            EntityState::Present(entry) => Ok(entry),
            EntityState::Deleted(_) => Err(Error::Deleted { commit }),
            EntityState::Absent => unreachable!("a commit is always present"),
        }
    }

    /// The current value at `name`, upcast to this kind's current schema.
    ///
    /// This is a compatibility convenience: it resolves the current schema
    /// ref after reading a non-tombstone value. New code that already has a
    /// selected target should use
    /// [`read_as`](Self::read_as).
    #[deprecated(
        since = "0.2.0",
        note = "use `Kind::read_as(name, target).value()` with an explicit `TargetSchema` instead"
    )]
    pub fn get_migrated(&self, name: &RefPath) -> Result<Option<Value>, Error> {
        Ok(self.read_dynamic(name.clone(), None)?.value())
    }

    /// Read `name` upcast to an explicitly selected target schema and history.
    ///
    /// The target is used only after the value's embedded schema has decoded,
    /// and tombstones are returned as [`EntityState::Deleted`] without
    /// validating or consulting the target history.
    #[deprecated(since = "0.2.0", note = "use `Kind::read_as(name, target)` instead")]
    pub fn read_migrated_to(
        &self,
        name: &RefPath,
        target: &TargetSchema,
    ) -> Result<EntityState<Value>, Error> {
        self.read_as(name.clone(), target)
    }

    /// The current value at `name`, upcast to an explicit target, or `None`
    /// when absent or deleted.
    #[deprecated(
        since = "0.2.0",
        note = "use `Kind::read_as(name, target).value()` instead"
    )]
    pub fn get_migrated_to(
        &self,
        name: &RefPath,
        target: &TargetSchema,
    ) -> Result<Option<Value>, Error> {
        Ok(self.read_as(name.clone(), target)?.value())
    }

    /// [`get_migrated`](Self::get_migrated) for one data commit.
    #[deprecated(
        since = "0.2.0",
        note = "use `Kind::read_as(commit, target)` with an explicit `TargetSchema` instead"
    )]
    pub fn get_at_migrated(&self, commit: ObjectId) -> Result<Value, Error> {
        match self.read_dynamic(commit, None)? {
            EntityState::Present(entry) => Ok(entry.value),
            EntityState::Deleted(_) => Err(Error::Deleted { commit }),
            EntityState::Absent => unreachable!("a commit is always present"),
        }
    }

    /// Read one data commit upcast to an explicitly selected target schema and
    /// history.
    #[deprecated(since = "0.2.0", note = "use `Kind::read_as(commit, target)` instead")]
    pub fn read_at_migrated_to(
        &self,
        commit: ObjectId,
        target: &TargetSchema,
    ) -> Result<EntityState<Value>, Error> {
        self.read_as(commit, target)
    }

    /// [`get_at_migrated`](Self::get_at_migrated) for an explicit target.
    #[deprecated(since = "0.2.0", note = "use `Kind::read_as(commit, target)` instead")]
    pub fn get_at_migrated_to(
        &self,
        commit: ObjectId,
        target: &TargetSchema,
    ) -> Result<Value, Error> {
        match self.read_as(commit, target)? {
            EntityState::Present(entry) => Ok(entry.value),
            EntityState::Deleted(_) => Err(Error::Deleted { commit }),
            EntityState::Absent => unreachable!("a commit is always present"),
        }
    }

    /// Read the current state upcast to this kind's current schema.
    ///
    /// This compatibility convenience resolves the current schema only for a
    /// non-tombstone value. Tombstones remain readable without a schema ref.
    #[deprecated(
        since = "0.2.0",
        note = "use `Kind::read_as(name, target)` with an explicit `TargetSchema` instead"
    )]
    pub fn read_migrated(&self, name: &RefPath) -> Result<EntityState<Value>, Error> {
        self.read_dynamic(name.clone(), None)
    }

    /// [`get_migrated`](Self::get_migrated), together with the commit the
    /// value was read from. Tombstones are represented as `None` for
    /// compatibility; use [`read_migrated`](Self::read_migrated) instead.
    #[deprecated(
        since = "0.2.0",
        note = "use `Kind::read_as(name, target).present()` with an explicit `TargetSchema` instead"
    )]
    pub fn get_entry_migrated(&self, name: &RefPath) -> Result<Option<Entry<Value>>, Error> {
        Ok(self.read_dynamic(name.clone(), None)?.present())
    }

    /// [`get_entry_migrated`](Self::get_entry_migrated) for one data commit.
    #[deprecated(
        since = "0.2.0",
        note = "use `Kind::read_as(commit, target)` with an explicit `TargetSchema` instead"
    )]
    pub fn get_entry_at_migrated(&self, commit: ObjectId) -> Result<Entry<Value>, Error> {
        match self.read_dynamic(commit, None)? {
            EntityState::Present(entry) => Ok(entry),
            EntityState::Deleted(_) => Err(Error::Deleted { commit }),
            EntityState::Absent => unreachable!("a commit is always present"),
        }
    }

    /// [`read_migrated`](Self::read_migrated) for one data commit.
    #[deprecated(
        since = "0.2.0",
        note = "use `Kind::read_as(commit, target)` with an explicit `TargetSchema` instead"
    )]
    pub fn read_at_migrated(&self, commit: ObjectId) -> Result<EntityState<Value>, Error> {
        self.read_dynamic(commit, None)
    }

    fn read_bound(
        &self,
        commit: ObjectId,
    ) -> Result<(ObjectId, ObjectId, Rc<Schema>, String), Error> {
        let (root, message) = self.store.commit_tree_and_summary(commit)?;
        let (value_tree, schema_tree) = self.store.split(root, commit)?;
        let doc = self.store.schema(schema_tree)?;
        Ok((value_tree, schema_tree, doc, message))
    }

    fn ensure_kind(&self, found: &str) -> Result<(), Error> {
        if found == self.name.as_str() {
            Ok(())
        } else {
            Err(Error::KindMismatch {
                expected: self.name.clone(),
                found: found.to_owned(),
            })
        }
    }

    fn ensure_document_kind(&self, doc: &Schema) -> Result<(), Error> {
        self.ensure_kind(&doc.kind)
    }

    /// An entity's commits, tip-first along first parents; empty when absent.
    pub fn history(&self, name: &RefPath) -> Result<Vec<ObjectId>, Error> {
        self.store.ref_history(&self.reference(name))
    }

    /// Commit `rebuild`'s result forward at `name`, retried with a fresh
    /// [`get_entry`](Self::get_entry) whenever the compare-and-swap loses a
    /// race — so `rebuild` always sees the entry it actually commits over
    /// (`None` when `name` is absent or deleted), never one read before a
    /// concurrent write landed.
    pub fn update(
        &self,
        name: &RefPath,
        rebuild: impl Fn(Option<&Entry<E::Value>>) -> (String, E::Value),
    ) -> Result<ObjectId, Error>
    where
        R: Committer,
        O: Write,
    {
        self.try_update(name, |current| Ok(rebuild(current)))
    }

    /// [`update`](Self::update) where `rebuild` may fail — for a caller whose
    /// forwarding is conditional on what it commits over, such as one that
    /// refuses to recreate an entity a concurrent writer just deleted.
    ///
    /// `rebuild`'s error type carries through, so it may be the caller's own
    /// rather than [`Error`]. A deleted ref is passed as `None`, just like an
    /// absent ref; use [`read`](Self::read) when that distinction matters.
    pub fn try_update<Er>(
        &self,
        name: &RefPath,
        rebuild: impl Fn(Option<&Entry<E::Value>>) -> Result<(String, E::Value), Er>,
    ) -> Result<ObjectId, Er>
    where
        R: Committer,
        O: Write,
        Er: From<Error>,
    {
        loop {
            let current = self.read(name.clone())?.present();
            let (summary, value) = rebuild(current.as_ref())?;
            let (message, tree) = commit_body(self, &value, summary)?;
            // Keep the tombstone's commit as the CAS expectation when the
            // caller elects to recreate through the old Option-based API.
            let expected = self
                .store
                .refs()
                .read(&self.reference(name))
                .map_err(Error::backend)?;
            let expectation = match expected {
                Some(old) => gix_refstore::Expectation::Exactly(old),
                None => gix_refstore::Expectation::Absent,
            };
            match self.publish_document_checked(
                Some(name),
                &message,
                tree,
                None,
                Some(expectation),
                true,
            )? {
                Some((_, commit)) => return Ok(commit),
                None => continue,
            }
        }
    }

    fn source_entries(&self) -> Result<Vec<(RefPath, ObjectId)>, Error> {
        let mut entries: Vec<_> = self
            .store
            .refs()
            .prefixed(&self.entities)
            .map_err(Error::backend)?
            .into_iter()
            .filter_map(|(name, commit)| {
                name.relative_to(&self.entities).map(|name| (name, commit))
            })
            .collect();
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        Ok(entries)
    }

    /// Decode only enough of a bound document to recognize a tombstone. This
    /// is deliberately independent of the current schema ref/history.
    fn tombstone_for(&self, commit: ObjectId) -> Option<EntityId> {
        let (value, _schema, doc, _message) = self.read_bound(commit).ok()?;
        tombstone::read(self.store, &value, &doc)
            .ok()?
            .filter(|marker| marker.kind == self.name.as_str())
            .and_then(|marker| marker.entity_id())
    }

    /// Publish one complete document under a caller-selected name, defaulting
    /// to the content-derived name when none is given.
    ///
    /// The document tree is validated before any ref is read or commit is
    /// written. Its identity is the object id of the complete `{schema/,
    /// value/}` tree, and the schema embedded in that tree must name this
    /// kind. The name ref and the materialized index advance in one ref-store
    /// batch.
    ///
    /// An explicit [`Expectation`](gix_refstore::Expectation) is a one-shot
    /// compare-and-swap on that ref: a stale expectation is returned as an
    /// error and is never retried. With no explicit expectation, this retries
    /// on a lost race.
    pub fn publish_prepared(
        &self,
        prepared: &PreparedDocument,
        options: PublishOptions,
    ) -> Result<Publication, Error>
    where
        R: Committer,
        O: Write,
    {
        let tree = prepared.document_tree().object_id();
        let (id, commit) = self
            .publish_document_checked(
                options.alias.as_ref(),
                &options.message,
                tree,
                options.parent,
                options.expectation,
                options.expectation.is_none(),
            )?
            .ok_or_else(|| {
                Error::backend(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "prepared publication compare-and-swap did not apply",
                ))
            })?;
        Ok(Publication::new(id, commit))
    }

    fn publish_document(
        &self,
        alias: Option<&RefPath>,
        message: &str,
        tree: ObjectId,
    ) -> Result<(EntityId, ObjectId), Error>
    where
        R: Committer,
        O: Write,
    {
        self.publish_document_checked(alias, message, tree, None, None, true)
            .map(|result| result.expect("an unchecked publication always proceeds"))
    }

    fn publish_document_checked(
        &self,
        alias: Option<&RefPath>,
        message: &str,
        tree: ObjectId,
        parent: Option<ObjectId>,
        expected_alias: Option<gix_refstore::Expectation>,
        retry_on_race: bool,
    ) -> Result<Option<(EntityId, ObjectId)>, Error>
    where
        R: Committer,
        O: Write,
    {
        // Publication is only defined for a complete bound document, not for
        // an arbitrary tree that happens to have an object id. Reading the
        // embedded schema here also keeps compatibility writes subject to the
        // same kind check as prepared writes.
        let (_, schema_tree) = self.store.split(tree, tree)?;
        let document = self.store.schema(schema_tree)?;
        if document.kind != self.name.as_str() {
            return Err(Error::backend(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "prepared document belongs to kind {:?}, expected {:?}",
                    document.kind, self.name
                ),
            )));
        }
        let id = canonical_document_id(tree);
        // The caller-selected name is the ref. Callers that want a
        // content-derived name ask for one explicitly; the store does not
        // impose one.
        let name = alias
            .cloned()
            .unwrap_or_else(|| RefPath::from(id.as_segment()));
        let reference = self.reference(&name);
        if let Some(parent) = parent {
            // An explicit parent is a commit-level primitive, not an arbitrary
            // object edge. Validate it before attempting the ref-store CAS.
            self.store.commit_tree(parent)?;
        }

        loop {
            let current = self.store.refs().read(&reference).map_err(Error::backend)?;

            if let Some(expected) = expected_alias {
                let matches = match expected {
                    gix_refstore::Expectation::Absent => current.is_none(),
                    gix_refstore::Expectation::Exactly(old) => current == Some(old),
                };
                if !matches {
                    if retry_on_race {
                        return Ok(None);
                    }
                    return Err(self.expectation_error(&reference, expected));
                }
            }

            // Republishing identical content is a no-op, which keeps migration
            // and repeated writes idempotent. Otherwise the ref advances like a
            // branch, so every previous publication stays reachable.
            let (commit, edit) = match current {
                Some(commit) if self.store.commit_tree(commit)? == tree => (commit, None),
                Some(expected) => {
                    let next =
                        self.store
                            .write_commit(message, tree, Some(parent.unwrap_or(expected)))?;
                    (
                        next,
                        Some(RefEdit::Update {
                            name: reference.clone(),
                            expected,
                            new: next,
                        }),
                    )
                }
                None => {
                    let next = self.store.write_commit(message, tree, parent)?;
                    (
                        next,
                        Some(RefEdit::Create {
                            name: reference.clone(),
                            new: next,
                        }),
                    )
                }
            };

            let mut edits = Vec::new();
            if let Some(edit) = edit {
                edits.push(edit);
            }

            let current_index = self
                .store
                .refs()
                .read(&index::reference(&self.name))
                .map_err(Error::backend)?;
            let mut next = self.source_entries()?;
            if let Some((_, current)) = next.iter_mut().find(|(entry, _)| entry == &name) {
                *current = commit;
            } else {
                next.push((name.clone(), commit));
                next.sort_by(|(a, _), (b, _)| a.cmp(b));
            }
            if let Some(edit) = self.index_edit(current_index, &next)? {
                edits.push(edit);
            }

            if edits.is_empty() {
                return Ok(Some((id, commit)));
            }
            match self.store.refs().apply_batch(edits) {
                Ok(()) => return Ok(Some((id, commit))),
                Err(ApplyError::LostRace { .. }) if retry_on_race => continue,
                Err(lost @ ApplyError::LostRace { .. }) => {
                    return Err(Error::backend(lost));
                }
                Err(ApplyError::Backend(err)) => return Err(Error::backend(err)),
            }
        }
    }

    /// Publish a document under a name derived from its own publication
    /// commit. The commit must exist before its name is known, so this cannot
    /// go through the ordinary named path.
    fn publish_named_by_commit(
        &self,
        message: &str,
        tree: ObjectId,
        name: impl FnOnce(ObjectId) -> RefPath,
    ) -> Result<ObjectId, Error>
    where
        R: Committer,
        O: Write,
    {
        let (_, schema_tree) = self.store.split(tree, tree)?;
        let document = self.store.schema(schema_tree)?;
        if document.kind != self.name.as_str() {
            return Err(Error::backend(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "prepared document belongs to kind {:?}, expected {:?}",
                    document.kind, self.name
                ),
            )));
        }
        let commit = self.store.write_commit(message, tree, None)?;
        let name = name(commit);
        let reference = self.reference(&name);
        loop {
            if self.store.refs().read(&reference).map_err(Error::backend)? == Some(commit) {
                return Ok(commit);
            }
            let mut edits = vec![RefEdit::Create {
                name: reference.clone(),
                new: commit,
            }];
            let current_index = self
                .store
                .refs()
                .read(&index::reference(&self.name))
                .map_err(Error::backend)?;
            let mut next = self.source_entries()?;
            next.push((name.clone(), commit));
            next.sort_by(|(a, _), (b, _)| a.cmp(b));
            if let Some(edit) = self.index_edit(current_index, &next)? {
                edits.push(edit);
            }
            match self.store.refs().apply_batch(edits) {
                Ok(()) => return Ok(commit),
                Err(ApplyError::LostRace { .. }) => continue,
                Err(ApplyError::Backend(err)) => return Err(Error::backend(err)),
            }
        }
    }

    fn expectation_error(
        &self,
        reference: &RefName,
        expectation: gix_refstore::Expectation,
    ) -> Error
    where
        R: RefStore,
    {
        Error::backend(ApplyError::<R::Error>::LostRace {
            name: reference.clone(),
            expected: expectation,
        })
    }

    fn index_edit(
        &self,
        current: Option<ObjectId>,
        entries: &[(RefPath, ObjectId)],
    ) -> Result<Option<RefEdit>, Error>
    where
        O: Write,
    {
        if entries.is_empty() {
            return Ok(current.map(|expected| RefEdit::Delete {
                name: index::reference(&self.name),
                expected,
            }));
        }
        let tree = index::write(self.store, entries)?;
        Ok(Some(match current {
            Some(expected) if expected == tree => return Ok(None),
            Some(expected) => RefEdit::Update {
                name: index::reference(&self.name),
                expected,
                new: tree,
            },
            None => RefEdit::Create {
                name: index::reference(&self.name),
                new: tree,
            },
        }))
    }

    /// The entity names published under this kind, ascending. Nesting is
    /// preserved: an entity named `<a>/<b>` lists as that path, not as `<a>`.
    pub fn list(&self) -> Result<Vec<RefPath>, Error> {
        self.list_in(self.entities.clone())
    }

    /// Every entity name and publication commit, ascending, including names
    /// whose tip is a tombstone.
    pub fn list_entries(&self) -> Result<Vec<(RefPath, ObjectId)>, Error> {
        let source = self.source_entries()?;
        Ok(
            index::read_validated(self.store, &self.name, &self.entities, &source)?
                .unwrap_or(source),
        )
    }

    /// Rebuild this kind's materialized index from the entity refs.
    ///
    /// This is the explicit repair path for repositories created before the
    /// index existed or after an out-of-band ref write. It only publishes the
    /// cache ref; entity refs remain the source of truth.
    pub fn rebuild_index(&self) -> Result<(), Error>
    where
        O: Write,
    {
        let index_ref = index::reference(&self.name);
        loop {
            let source = self.source_entries()?;
            let current = self.store.refs().read(&index_ref).map_err(Error::backend)?;
            let edit = if source.is_empty() {
                current.map(|expected| RefEdit::Delete {
                    name: index_ref.clone(),
                    expected,
                })
            } else {
                let tree = index::write(self.store, &source)?;
                Some(match current {
                    Some(expected) => RefEdit::Update {
                        name: index_ref.clone(),
                        expected,
                        new: tree,
                    },
                    None => RefEdit::Create {
                        name: index_ref.clone(),
                        new: tree,
                    },
                })
            };
            let Some(edit) = edit else {
                return Ok(());
            };
            match self.store.refs().apply(edit) {
                Ok(()) => return Ok(()),
                Err(ApplyError::LostRace { .. }) => continue,
                Err(ApplyError::Backend(err)) => return Err(Error::backend(err)),
            }
        }
    }

    /// Decode every entity published under this kind, ascending by name.
    pub fn entries(&self) -> Result<NamedEntries<E::Value>, Error> {
        self.entries_from(self.list_entries()?)
    }

    /// [`entries`](Self::entries) narrowed to the entities nested under
    /// `group`, the bulk form of [`list_under`](Self::list_under).
    pub fn entries_under(&self, group: &RefPath) -> Result<NamedEntries<E::Value>, Error> {
        self.entries_from(self.list_entries_in(self.group_prefix(group))?)
    }

    fn entries_from(
        &self,
        entries: Vec<(RefPath, ObjectId)>,
    ) -> Result<NamedEntries<E::Value>, Error> {
        entries
            .into_iter()
            .filter_map(|(name, commit)| match self.read_commit(commit) {
                Ok(EntityState::Present(entry)) => Some(Ok((name, entry))),
                Ok(EntityState::Absent | EntityState::Deleted(_)) => None,
                Err(err) => Some(Err(err)),
            })
            .collect()
    }

    /// Return a digest of this kind's published entity refs without reading
    /// any Git objects.
    ///
    /// The digest changes when an entity name or commit ID changes. It is
    /// domain-separated and versioned, and includes this kind's name. The
    /// returned [`ObjectId`] is a digest for cache keys, not a Git object ID;
    /// schema refs are not included.
    pub fn fingerprint(&self) -> Result<ObjectId, Error> {
        let entries = self.list_entries()?;
        let mut hasher = gix::hash::hasher(gix::hash::Kind::Sha1);
        hasher.update(KIND_FINGERPRINT_DOMAIN);
        update_fingerprint_bytes(&mut hasher, self.name.as_str().as_bytes());
        hasher.update(&(entries.len() as u64).to_be_bytes());
        for (name, commit) in entries {
            hasher.update(&(name.segments().len() as u64).to_be_bytes());
            for segment in name.segments() {
                update_fingerprint_bytes(&mut hasher, segment.as_str().as_bytes());
            }
            hasher.update(commit.as_slice());
        }
        hasher.try_finalize().map_err(Error::Fingerprint)
    }

    /// [`list`](Self::list) narrowed to the entities nested under `group`,
    /// scanning only that subtree's refs instead of every entity of the kind.
    /// Names come back in full, `group` included.
    pub fn list_under(&self, group: &RefPath) -> Result<Vec<RefPath>, Error> {
        self.list_in(self.group_prefix(group))
    }

    /// The ref prefix the entities nested under `group` live beneath.
    fn group_prefix(&self, group: &RefPath) -> RefPrefix {
        group
            .segments()
            .iter()
            .fold(self.entities.clone(), |prefix, segment| {
                prefix.child(segment)
            })
    }

    fn list_in(&self, prefix: RefPrefix) -> Result<Vec<RefPath>, Error> {
        let mut entries: Vec<_> = self
            .live_entries()?
            .into_iter()
            .filter(|(name, _)| self.entities.join_path(name).is_under(&prefix))
            .map(|(name, _)| name)
            .collect();
        entries.sort();
        Ok(entries)
    }

    /// Every named ref under this kind whose tip is not a tombstone.
    fn live_entries(&self) -> Result<Vec<(RefPath, ObjectId)>, Error> {
        Ok(self
            .source_entries()?
            .into_iter()
            .filter(|(_, commit)| self.tombstone_for(*commit).is_none())
            .collect())
    }

    fn list_entries_in(&self, prefix: RefPrefix) -> Result<Vec<(RefPath, ObjectId)>, Error> {
        if prefix == self.entities {
            return self.list_entries();
        }
        let mut entries: Vec<_> = self
            .source_entries()?
            .into_iter()
            .filter(|(name, _)| self.entities.join_path(name).is_under(&prefix))
            .collect();
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        Ok(entries)
    }

    /// Publish a typed tombstone at the named entity ref.
    ///
    /// The ref and the materialized index advance in one ref-store CAS batch.
    /// Repeating the operation is idempotent and returns
    /// [`DeleteResult::AlreadyDeleted`]. A missing ref is [`DeleteResult::Absent`].
    pub fn delete(&self, name: &RefPath) -> Result<DeleteResult, Error>
    where
        R: Committer,
        O: Write,
    {
        self.delete_name(name)
    }

    /// [`delete`](Self::delete) for an entity published under a
    /// content-derived name.
    pub fn delete_entity(&self, id: EntityId) -> Result<DeleteResult, Error>
    where
        R: Committer,
        O: Write,
    {
        self.delete_name(&entity_id_name(id))
    }

    /// Publish a typed tombstone at `name`, retaining its history.
    pub fn delete_name(&self, name: &RefPath) -> Result<DeleteResult, Error>
    where
        R: Committer,
        O: Write,
    {
        let reference = self.reference(name);
        loop {
            let Some(current) = self.store.refs().read(&reference).map_err(Error::backend)? else {
                return Ok(DeleteResult::Absent);
            };
            if let EntityState::Deleted(existing) = self.read_commit(current)? {
                return Ok(DeleteResult::AlreadyDeleted(existing));
            }

            let id = canonical_document_id(self.store.commit_tree(current)?);
            let tombstone = Tombstone::new(&self.name, id);
            let tree = tombstone::write(self.store, &tombstone)?;
            let message = format!("delete {}/{}\n", self.name, id);
            let commit = self.store.write_commit(&message, tree, Some(current))?;
            let mut edits = vec![RefEdit::Update {
                name: reference.clone(),
                expected: current,
                new: commit,
            }];

            let current_index = self
                .store
                .refs()
                .read(&index::reference(&self.name))
                .map_err(Error::backend)?;
            let mut next = self.source_entries()?;
            if let Some((_, target)) = next.iter_mut().find(|(entry, _)| entry == name) {
                *target = commit;
            } else {
                next.push((name.clone(), commit));
                next.sort_by(|(a, _), (b, _)| a.cmp(b));
            }
            if let Some(edit) = self.index_edit(current_index, &next)? {
                edits.push(edit);
            }

            match self.store.refs().apply_batch(edits) {
                Ok(()) => {
                    return Ok(DeleteResult::Deleted(TombstoneEntry {
                        tombstone,
                        commit,
                        message: message.trim_end().to_owned(),
                    }));
                }
                Err(ApplyError::LostRace { .. }) => continue,
                Err(ApplyError::Backend(err)) => return Err(Error::backend(err)),
            }
        }
    }

    /// Prune a name and its history outright. Returns whether it existed.
    ///
    /// This is the non-typed counterpart to [`delete`](Self::delete): no
    /// tombstone is published, so a later reader cannot distinguish the name
    /// from one that never existed.
    pub fn remove(&self, name: &RefPath) -> Result<bool, Error>
    where
        R: Committer,
        O: Write,
    {
        let reference = self.reference(name);
        loop {
            let Some(expected) = self.store.refs().read(&reference).map_err(Error::backend)? else {
                return Ok(false);
            };
            let mut edits = vec![RefEdit::Delete {
                name: reference.clone(),
                expected,
            }];
            let current_index = self
                .store
                .refs()
                .read(&index::reference(&self.name))
                .map_err(Error::backend)?;
            let next: Vec<_> = self
                .source_entries()?
                .into_iter()
                .filter(|(entry, _)| entry != name)
                .collect();
            if let Some(edit) = self.index_edit(current_index, &next)? {
                edits.push(edit);
            }
            match self.store.refs().apply_batch(edits) {
                Ok(()) => return Ok(true),
                Err(ApplyError::LostRace { .. }) => continue,
                Err(ApplyError::Backend(err)) => return Err(Error::backend(err)),
            }
        }
    }

    /// The current schema tip, or [`Error::NoSchema`] when none is published.
    fn current_schema(&self) -> Result<(ObjectId, Schema), Error> {
        let tip = self
            .store
            .refs()
            .read(&self.schema_ref)
            .map_err(Error::backend)?
            .ok_or_else(|| Error::NoSchema {
                kind: self.name.clone(),
            })?;
        let tree = self.store.commit_tree(tip)?;
        let doc = Schema::read_pinned(&tree, self.store.objects())?;
        Ok((tip, doc))
    }
}

impl<'s, T: for<'a> Facet<'a>, R, O> Kind<'s, Typed<T>, R, O>
where
    R: RefStore,
    O: Find,
{
    /// Publish the schema derived from `T`.
    pub fn publish(&self) -> Result<ObjectId, Error>
    where
        R: Committer,
        O: Write,
    {
        let doc = schema_of::<T>()?.with_kind(self.name.as_str())?;
        self.schema().put(&doc)
    }
}

/// A value read alongside the commit it came from, sparing the caller from
/// decoding that commit itself.
#[derive(Debug, PartialEq)]
pub struct Entry<V> {
    /// The decoded value.
    pub value: V,
    /// The commit the value was read from.
    pub commit: ObjectId,
    /// That commit's summary — its message's first logical line, per
    /// [`gix::objs::CommitRef::message_summary`].
    pub message: String,
}

/// Named entries as the bulk readers return them: entity name paired with the
/// [`Entry`] read at that name, ascending by name.
pub type NamedEntries<V> = Vec<(RefPath, Entry<V>)>;

/// A kind's schema ref.
pub struct KindSchema<'s, R, O> {
    pub(crate) store: &'s Store<R, O>,
    pub(crate) kind: RefSegment,
    pub(crate) reference: RefName,
}

impl<'s, R, O> KindSchema<'s, R, O>
where
    R: RefStore,
    O: Find,
{
    /// The ref this schema is published at.
    pub fn reference(&self) -> &RefName {
        &self.reference
    }

    /// Publish (or evolve) the schema, committing it forward over the current
    /// tip.
    pub fn put(&self, doc: &Schema) -> Result<ObjectId, Error>
    where
        R: Committer,
        O: Write,
    {
        self.write(doc, &Hints::new())
    }

    /// [`put`](Self::put) with authoring hints, which are what let a
    /// remove-plus-add pair be recognised as the rename it actually is.
    pub fn write(&self, doc: &Schema, hints: &Hints) -> Result<ObjectId, Error>
    where
        R: Committer,
        O: Write,
    {
        // The ref segment is the caller-selected publication name. Do not
        // preserve a Rust Shape identifier (or a hand-authored JSON value)
        // in the embedded schema document.
        let doc = doc.clone().with_kind(self.kind.as_str())?;
        self.check_identity_subtrees(&doc)?;
        let message = format!("schema {}\n", self.kind);
        self.store
            .commit_forward(&self.reference, &message, |parent| {
                let previous = match parent {
                    Some(tip) => {
                        let tree = self.store.commit_tree(tip)?;
                        // A tip pinned to a schema-schema this binary does not
                        // recognize is refused: publishing over it would silently
                        // replace a document whose meaning was never established
                        // here. A tip that is unpinned or otherwise unreadable stays
                        // overwritable — republishing is the migration path those
                        // errors name.
                        if let Err(err @ SchemaPinError::Unrecognized { .. }) =
                            Schema::read_pin(&tree, self.store.objects())
                        {
                            return Err(err.into());
                        }
                        Schema::read_pinned(&tree, self.store.objects()).ok()
                    }
                    None => None,
                };
                let mut tree = doc.write_pinned(self.store.objects())?;
                // An edge this build can derive completely is recorded; a partial one
                // is not, and the gap surfaces at read time as MigrationMissing
                // naming the commit, rather than as a silently lossy upcast here.
                if let Some(previous) = previous
                    && let Derivation::Complete(migration) = derive(&previous, &doc, hints)
                {
                    let written = migration.write_pinned(self.store.objects())?;
                    tree = self.store.bind_migration(tree, written)?;
                }
                Ok(tree)
            })
    }

    /// Refuse a schema whose identity- or key-bearing subtree leaves the
    /// identity normal form's universe.
    ///
    /// A marked subtree (`#[facet(facet_git_tree::identity_key)]`, compiled
    /// into the document as a reserved definition) is what an anchor id or an
    /// action key is hashed from, so a subtree the frozen mapping cannot
    /// express is refused at registration — the one point where the whole
    /// schema is in hand and nothing has been published yet.
    fn check_identity_subtrees(&self, doc: &Schema) -> Result<(), Error> {
        for (subtree, node) in identity_subtrees(doc) {
            check_universe_at(node, &doc.defs, subtree).map_err(|source| {
                Error::IdentityUniverse {
                    kind: self.kind.clone(),
                    subtree: subtree.to_owned(),
                    source,
                }
            })?;
        }
        Ok(())
    }

    /// The current schema, or `None` when never published.
    pub fn get(&self) -> Result<Option<Schema>, Error> {
        match self
            .store
            .refs()
            .read(&self.reference)
            .map_err(Error::backend)?
        {
            Some(tip) => {
                let tree = self.store.commit_tree(tip)?;
                Ok(Some(Schema::read_pinned(&tree, self.store.objects())?))
            }
            None => Ok(None),
        }
    }

    /// The schema's evolution, tip-first.
    pub fn history(&self) -> Result<Vec<ObjectId>, Error> {
        self.store.ref_history(&self.reference)
    }
}

fn commit_segment(commit: ObjectId) -> RefSegment {
    RefSegment::new(commit.to_string()).expect("object id hex is a valid ref segment")
}

/// Compatibility helper for the old commit-named anonymous alias.
pub fn entity_name(commit: ObjectId) -> RefPath {
    commit_segment(commit).into()
}

/// Return the ref path naming an entity by its [`EntityId`].
pub fn entity_id_name(id: EntityId) -> RefPath {
    id.as_segment().into()
}

/// Compatibility helper for a canonical entity path under `group`.
pub fn entity_name_under(group: &RefPath, commit: ObjectId) -> RefPath {
    group.join(&commit_segment(commit))
}

/// A pending write of one value. Consumed by [`at`](Self::at) or
/// [`anonymous`](Self::anonymous).
pub struct Put<'k, E: Encoding, R, O> {
    kind: &'k Kind<'k, E, R, O>,
    value: &'k E::Value,
    message: Option<String>,
}

impl<'k, E: Encoding, R, O> Put<'k, E, R, O>
where
    R: RefStore,
    O: Find,
{
    /// Use `summary` as the commit message instead of the default.
    ///
    /// The final write rejects lines beginning with the reserved legacy
    /// trailers `Schema:`, `Schema-Version:`, or `Ents-Ref:`. This keeps new
    /// commits free of schema/provenance trailers without silently changing a
    /// caller's message; see [`Error::ReservedTrailer`].
    pub fn message(mut self, summary: impl Into<String>) -> Self {
        self.message = Some(summary.into());
        self
    }

    /// Commit the value forward at `name`.
    pub fn at(self, name: &RefPath) -> Result<ObjectId, Error>
    where
        R: Committer,
        O: Write,
    {
        let kind = self.kind;
        let default = || format!("store {}/{name}", kind.name);
        let (message, tree) = self.build(default)?;
        let (_, commit) = kind.publish_document(Some(name), &message, tree)?;
        Ok(commit)
    }

    /// Commit the value under its content-derived name and return its
    /// [`EntityId`].
    pub fn canonical(self) -> Result<EntityId, Error>
    where
        R: Committer,
        O: Write,
    {
        let kind = self.kind;
        let default = || format!("store {}/<auto>", kind.name);
        let (message, tree) = self.build(default)?;
        let (id, _) = kind.publish_document(None, &message, tree)?;
        Ok(id)
    }

    /// Commit the value under `alias`, returning the derived [`EntityId`]
    /// rather than the publication commit.
    pub fn with_alias(self, alias: &RefPath) -> Result<EntityId, Error>
    where
        R: Committer,
        O: Write,
    {
        let kind = self.kind;
        let default = || format!("store {}/{alias}", kind.name);
        let (message, tree) = self.build(default)?;
        let (id, _) = kind.publish_document(Some(alias), &message, tree)?;
        Ok(id)
    }

    fn build(self, default_summary: impl FnOnce() -> String) -> Result<(String, ObjectId), Error>
    where
        O: Write,
    {
        let summary = self.message.unwrap_or_else(default_summary);
        commit_body(self.kind, self.value, summary)
    }

    /// Compatibility adapter for [`canonical`](Self::canonical): publish at
    /// the content-derived ref and return the publication commit id.
    ///
    /// This compatibility method returns the publication commit. The canonical
    /// ref is derived from the complete bound document tree, not this commit.
    pub fn anonymous(self) -> Result<ObjectId, Error>
    where
        R: Committer,
        O: Write,
    {
        self.anonymous_at(entity_name)
    }

    /// Compatibility adapter for publishing at the content-derived ref under
    /// `group`; returns the publication commit id.
    pub fn anonymous_under(self, group: &RefPath) -> Result<ObjectId, Error>
    where
        R: Committer,
        O: Write,
    {
        self.anonymous_at(|commit| entity_name_under(group, commit))
    }

    /// Publish at the canonical content-derived ref and maintain a grouped
    /// compatibility alias under `group`, returning the [`EntityId`].
    pub fn canonical_under(self, group: &RefPath) -> Result<EntityId, Error>
    where
        R: Committer,
        O: Write,
    {
        let kind = self.kind;
        let default = || format!("store {}/<auto>", kind.name);
        let (message, tree) = self.build(default)?;
        let id = canonical_document_id(tree);
        let name = entity_name_under(group, id.object_id());
        let (id, _) = kind.publish_document(Some(&name), &message, tree)?;
        Ok(id)
    }

    fn anonymous_at(self, name: impl FnOnce(ObjectId) -> RefPath) -> Result<ObjectId, Error>
    where
        R: Committer,
        O: Write,
    {
        let kind = self.kind;
        let default = || format!("store {}/<auto>", kind.name);
        let (message, tree) = self.build(default)?;
        kind.publish_named_by_commit(&message, tree, name)
    }
}

fn update_fingerprint_bytes(hasher: &mut gix::hash::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

/// The tree and commit message a value builds down to, under `kind`'s
/// current schema — shared by `Put`'s builders and [`Kind::update`].
fn commit_body<E: Encoding, R, O>(
    kind: &Kind<'_, E, R, O>,
    value: &E::Value,
    summary: String,
) -> Result<(String, ObjectId), Error>
where
    R: RefStore,
    O: Find + Write,
{
    let (_, tree) = kind.compile_with_schema(value)?;
    let message = format!("{summary}\n");
    Ok((message, tree))
}
