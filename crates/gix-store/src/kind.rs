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

    /// The compatibility ref for an entity alias.
    ///
    /// New code should prefer [`entity_reference`](Self::entity_reference).
    /// Existing named refs remain readable and are maintained as aliases by
    /// named writes.
    pub fn reference(&self, name: &RefPath) -> RefName {
        self.entities.join_path(name)
    }

    /// The canonical ref for a content-derived entity id.
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

    /// Store `value` with `name` as an optional compatibility alias.
    ///
    /// The returned commit id is retained for compatibility. The authoritative
    /// identity and canonical ref are derived from the complete bound document;
    /// use [`put_entity`](Self::put_entity) when the derived [`EntityId`] is
    /// what the caller needs.
    pub fn put(&self, name: &RefPath, value: &E::Value) -> Result<ObjectId, Error>
    where
        R: Committer,
        O: Write,
    {
        self.write(value).at(name)
    }

    /// Store `value` without a caller-selected name and return its derived id.
    pub fn put_entity(&self, value: &E::Value) -> Result<EntityId, Error>
    where
        R: Committer,
        O: Write,
    {
        self.write(value).canonical()
    }

    /// Store `value`, maintaining `alias` as a compatibility ref, and return
    /// the content-derived id.
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

    /// Read the current state at an alias or canonical ref path.
    ///
    /// Unlike [`get`](Self::get), this distinguishes an explicit tombstone
    /// from an absent or hard-deleted ref. The alias form is retained for
    /// compatibility with old named refs.
    pub fn read(&self, name: &RefPath) -> Result<EntityState<E::Value>, Error> {
        match self
            .store
            .refs()
            .read(&self.reference(name))
            .map_err(Error::backend)?
        {
            Some(commit) => self.read_at(commit),
            None => Ok(EntityState::Absent),
        }
    }

    /// Read the current state addressed by its content-derived id.
    pub fn read_entity(&self, id: EntityId) -> Result<EntityState<E::Value>, Error> {
        match self
            .store
            .refs()
            .read(&self.entity_reference(id))
            .map_err(Error::backend)?
        {
            Some(commit) => self.read_at(commit),
            None => Ok(EntityState::Absent),
        }
    }

    /// The current value at an alias or canonical ref path, or `None` when
    /// absent or explicitly deleted. This is the compatibility adapter for
    /// callers that cannot represent a deleted state.
    pub fn get(&self, name: &RefPath) -> Result<Option<E::Value>, Error> {
        Ok(match self.read(name)? {
            EntityState::Present(entry) => Some(entry.value),
            EntityState::Absent | EntityState::Deleted(_) => None,
        })
    }

    /// Read the current value addressed by its content-derived id, retaining
    /// the old `Option` behavior for compatibility.
    pub fn get_entity(&self, id: EntityId) -> Result<Option<E::Value>, Error> {
        Ok(match self.read_entity(id)? {
            EntityState::Present(entry) => Some(entry.value),
            EntityState::Absent | EntityState::Deleted(_) => None,
        })
    }

    /// The state as of one data commit, read entirely out of that commit's
    /// own tree.
    pub fn read_at(&self, commit: ObjectId) -> Result<EntityState<E::Value>, Error> {
        let (value_tree, _schema_tree, doc, message) = self.read_bound(commit)?;
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

    /// The value as of one data commit, preserving the old typed API.
    pub fn get_at(&self, commit: ObjectId) -> Result<E::Value, Error> {
        match self.read_at(commit)? {
            EntityState::Present(entry) => Ok(entry.value),
            EntityState::Deleted(_) => Err(Error::Deleted { commit }),
            EntityState::Absent => unreachable!("a commit is always present"),
        }
    }

    /// The current value at the canonical entity ref, together with its
    /// publication commit. Tombstones are represented as `None` for
    /// compatibility; use [`read_entity`](Self::read_entity) to observe them.
    pub fn get_entry_entity(&self, id: EntityId) -> Result<Option<Entry<E::Value>>, Error> {
        Ok(match self.read_entity(id)? {
            EntityState::Present(entry) => Some(entry),
            EntityState::Absent | EntityState::Deleted(_) => None,
        })
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
    pub fn get_entry(&self, name: &RefPath) -> Result<Option<Entry<E::Value>>, Error> {
        Ok(match self.read(name)? {
            EntityState::Present(entry) => Some(entry),
            EntityState::Absent | EntityState::Deleted(_) => None,
        })
    }

    /// [`get_entry`](Self::get_entry) for one data commit directly, read
    /// entirely out of that commit's own tree. A tombstone is reported as
    /// [`Error::Deleted`] for this compatibility API.
    pub fn get_entry_at(&self, commit: ObjectId) -> Result<Entry<E::Value>, Error> {
        match self.read_at(commit)? {
            EntityState::Present(entry) => Ok(entry),
            EntityState::Deleted(_) => Err(Error::Deleted { commit }),
            EntityState::Absent => unreachable!("a commit is always present"),
        }
    }

    /// The current value at `name`, upcast to this kind's current schema.
    ///
    /// This is a compatibility convenience: it resolves the current schema
    /// ref after reading a non-tombstone value. New code that already has a
    /// selected target should use [`get_migrated_to`](Self::get_migrated_to).
    pub fn get_migrated(&self, name: &RefPath) -> Result<Option<Value>, Error> {
        Ok(self.get_entry_migrated(name)?.map(|entry| entry.value))
    }

    /// Read `name` upcast to an explicitly selected target schema and history.
    ///
    /// The target is used only after the value's embedded schema has decoded,
    /// and tombstones are returned as [`EntityState::Deleted`] without
    /// validating or consulting the target history.
    pub fn read_migrated_to(
        &self,
        name: &RefPath,
        target: &TargetSchema,
    ) -> Result<EntityState<Value>, Error> {
        match self
            .store
            .refs()
            .read(&self.reference(name))
            .map_err(Error::backend)?
        {
            Some(commit) => self.read_at_migrated_to(commit, target),
            None => Ok(EntityState::Absent),
        }
    }

    /// The current value at `name`, upcast to an explicit target, or `None`
    /// when absent or deleted.
    pub fn get_migrated_to(
        &self,
        name: &RefPath,
        target: &TargetSchema,
    ) -> Result<Option<Value>, Error> {
        Ok(match self.read_migrated_to(name, target)? {
            EntityState::Present(entry) => Some(entry.value),
            EntityState::Absent | EntityState::Deleted(_) => None,
        })
    }

    /// [`get_migrated`](Self::get_migrated) for one data commit.
    pub fn get_at_migrated(&self, commit: ObjectId) -> Result<Value, Error> {
        Ok(self.get_entry_at_migrated(commit)?.value)
    }

    /// Read one data commit upcast to an explicitly selected target schema and
    /// history.
    pub fn read_at_migrated_to(
        &self,
        commit: ObjectId,
        target: &TargetSchema,
    ) -> Result<EntityState<Value>, Error> {
        self.read_at_migrated_with(commit, Some(target))
    }

    /// [`get_at_migrated`](Self::get_at_migrated) for an explicit target.
    pub fn get_at_migrated_to(
        &self,
        commit: ObjectId,
        target: &TargetSchema,
    ) -> Result<Value, Error> {
        match self.read_at_migrated_to(commit, target)? {
            EntityState::Present(entry) => Ok(entry.value),
            EntityState::Deleted(_) => Err(Error::Deleted { commit }),
            EntityState::Absent => unreachable!("a commit is always present"),
        }
    }

    /// Read the current state upcast to this kind's current schema.
    ///
    /// This compatibility convenience resolves the current schema only for a
    /// non-tombstone value. Tombstones remain readable without a schema ref.
    pub fn read_migrated(&self, name: &RefPath) -> Result<EntityState<Value>, Error> {
        match self
            .store
            .refs()
            .read(&self.reference(name))
            .map_err(Error::backend)?
        {
            Some(tip) => self.read_at_migrated(tip),
            None => Ok(EntityState::Absent),
        }
    }

    /// [`get_migrated`](Self::get_migrated), together with the commit the
    /// value was read from. Tombstones are represented as `None` for
    /// compatibility; use [`read_migrated`](Self::read_migrated) instead.
    pub fn get_entry_migrated(&self, name: &RefPath) -> Result<Option<Entry<Value>>, Error> {
        Ok(match self.read_migrated(name)? {
            EntityState::Present(entry) => Some(entry),
            EntityState::Absent | EntityState::Deleted(_) => None,
        })
    }

    /// [`get_entry_migrated`](Self::get_entry_migrated) for one data commit.
    pub fn get_entry_at_migrated(&self, commit: ObjectId) -> Result<Entry<Value>, Error> {
        match self.read_at_migrated(commit)? {
            EntityState::Present(entry) => Ok(entry),
            EntityState::Deleted(_) => Err(Error::Deleted { commit }),
            EntityState::Absent => unreachable!("a commit is always present"),
        }
    }

    /// [`read_migrated`](Self::read_migrated) for one data commit.
    pub fn read_at_migrated(&self, commit: ObjectId) -> Result<EntityState<Value>, Error> {
        self.read_at_migrated_with(commit, None)
    }

    fn read_at_migrated_with(
        &self,
        commit: ObjectId,
        target: Option<&TargetSchema>,
    ) -> Result<EntityState<Value>, Error> {
        let (value_tree, schema_tree, doc, message) = self.read_bound(commit)?;
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
            let state = self.read(name)?;
            let current = match state {
                EntityState::Present(entry) => Some(entry),
                EntityState::Absent | EntityState::Deleted(_) => None,
            };
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

    /// Canonical entity refs are the index source of truth. Named refs are
    /// deliberately excluded: they are aliases and may be added, removed, or
    /// retargeted without changing entity identity.
    fn canonical_entries(&self) -> Result<Vec<(RefPath, ObjectId)>, Error> {
        let mut entries = Vec::new();
        for (name, commit) in self.source_entries()? {
            let Some(last) = name.segments().last() else {
                continue;
            };
            if name.segments().len() != 1 {
                continue;
            }
            let Ok(id) = last.as_str().parse::<EntityId>() else {
                continue;
            };
            if self.canonical_target_matches(id, commit) {
                entries.push((name, commit));
            }
        }
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        Ok(entries)
    }

    fn is_canonical_ref(&self, name: &RefPath, commit: ObjectId) -> bool {
        if name.segments().len() != 1 {
            return false;
        }
        let Some(last) = name.segments().last() else {
            return false;
        };
        let Ok(id) = last.as_str().parse::<EntityId>() else {
            return false;
        };
        self.canonical_target_matches(id, commit)
    }

    fn canonical_target_matches(&self, id: EntityId, commit: ObjectId) -> bool {
        let Ok(root) = self.store.commit_tree(commit) else {
            return false;
        };
        let Ok((value_tree, _schema_tree, doc, _message)) = self.read_bound(commit) else {
            return false;
        };
        if doc.kind == tombstone::SCHEMA_KIND {
            return tombstone::read(self.store, &value_tree, &doc)
                .ok()
                .flatten()
                .is_some_and(|marker| {
                    marker.kind == self.name.as_str() && marker.entity_id() == Some(id)
                });
        }
        doc.kind == self.name.as_str()
            && tombstone::read(self.store, &value_tree, &doc)
                .ok()
                .flatten()
                .is_none()
            && id.object_id() == root
    }

    fn compatibility_identity(&self, commit: ObjectId) -> Option<EntityId> {
        let (value_tree, _schema_tree, doc, _message) = self.read_bound(commit).ok()?;
        if doc.kind != self.name.as_str()
            || tombstone::read(self.store, &value_tree, &doc)
                .ok()
                .flatten()
                .is_some()
        {
            return None;
        }
        self.store.commit_tree(commit).ok().map(EntityId::from)
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

    fn aliases_pointing_to(
        &self,
        commit: ObjectId,
        canonical_name: &RefPath,
    ) -> Result<Vec<RefPath>, Error> {
        Ok(self
            .source_entries()?
            .into_iter()
            .filter_map(|(name, target)| {
                (name != *canonical_name && target == commit).then_some(name)
            })
            .collect())
    }

    fn aliases_for_entity(
        &self,
        id: EntityId,
        canonical_name: &RefPath,
    ) -> Result<Vec<(RefPath, ObjectId)>, Error> {
        Ok(self
            .source_entries()?
            .into_iter()
            .filter(|(name, _)| name != canonical_name)
            .filter(|(_, commit)| {
                self.compatibility_identity(*commit) == Some(id)
                    || self.tombstone_for(*commit) == Some(id)
            })
            .collect())
    }

    /// Publish one complete document under its content-derived canonical ref,
    /// optionally maintaining a caller-selected compatibility alias.
    ///
    /// This is the compatibility implementation used by the legacy write
    /// builders; prepared callers use [`publish_prepared`](Self::publish_prepared).
    ///
    /// The document tree is validated before any ref is read or commit is
    /// written. Its identity is the object id of the complete `{schema/,
    /// value/}` tree, and the schema embedded in that tree must name this
    /// kind. Canonical ref, optional alias, and the materialized index advance
    /// in one ref-store batch.
    ///
    /// An explicit [`Expectation`](gix_refstore::Expectation) is applied to
    /// the canonical ref when no alias is supplied, or to the alias otherwise.
    /// It is a one-shot compare-and-swap: a stale expectation is returned as an
    /// error and is never retried. With no explicit expectation, this retains
    /// the compatibility writer's retry-on-lost-race behavior.
    pub fn publish_prepared(
        &self,
        prepared: &PreparedDocument,
        options: PublishOptions,
    ) -> Result<Publication, Error>
    where
        R: Committer,
        O: Write,
    {
        let tree = prepared.document_tree();
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
        // Canonical refs are direct children named by the document-tree id.
        // Every caller-selected path other than that direct ref is an alias,
        // including grouped compatibility paths.
        let canonical_name = RefPath::from(id.as_segment());
        let canonical_ref = self.reference(&canonical_name);
        let alias_ref = alias
            .filter(|name| **name != canonical_name)
            .map(|name| self.reference(name));
        if let Some(parent) = parent {
            // An explicit parent is a commit-level primitive, not an arbitrary
            // object edge. Validate it before attempting the ref-store CAS.
            self.store.commit_tree(parent)?;
        }

        loop {
            let canonical_current = self
                .store
                .refs()
                .read(&canonical_ref)
                .map_err(Error::backend)?;
            let alias_current = match &alias_ref {
                Some(reference) => self.store.refs().read(reference).map_err(Error::backend)?,
                None => None,
            };

            if let Some(expected) = expected_alias {
                let observed = if alias_ref.is_some() {
                    alias_current
                } else {
                    canonical_current
                };
                let matches = match expected {
                    gix_refstore::Expectation::Absent => observed.is_none(),
                    gix_refstore::Expectation::Exactly(old) => observed == Some(old),
                };
                if !matches {
                    if retry_on_race {
                        return Ok(None);
                    }
                    return Err(self.expectation_error(
                        alias_ref.as_ref().unwrap_or(&canonical_ref),
                        expected,
                    ));
                }
            }

            if let (Some(reference), Some(commit)) = (&alias_ref, alias_current)
                && self.is_canonical_ref(
                    &reference
                        .relative_to(&self.entities)
                        .unwrap_or_else(|| canonical_name.clone()),
                    commit,
                )
                && reference != &canonical_ref
            {
                return Err(Error::NameTaken {
                    name: reference.clone(),
                });
            }

            let restoring_from = match canonical_current {
                Some(commit)
                    if self.store.commit_tree(commit)? != tree
                        && self.tombstone_for(commit) == Some(id) =>
                {
                    Some(commit)
                }
                _ => None,
            };
            let aliases_to_restore = restoring_from
                .map(|commit| self.aliases_pointing_to(commit, &canonical_name))
                .transpose()?
                .unwrap_or_default();

            let (commit, canonical_edit) = match canonical_current {
                Some(commit) => {
                    let actual = self.store.commit_tree(commit)?;
                    if actual == tree {
                        (commit, None)
                    } else if restoring_from == Some(commit) {
                        // Recreating the exact content addressed by a
                        // tombstoned id is an explicit restore: retain the
                        // canonical ref and append a normal value commit.
                        let next = self.store.write_commit(message, tree, Some(commit))?;
                        (
                            next,
                            Some(RefEdit::Update {
                                name: canonical_ref.clone(),
                                expected: commit,
                                new: next,
                            }),
                        )
                    } else {
                        return Err(Error::EntityIdCollision {
                            id,
                            expected: tree,
                            found: actual,
                        });
                    }
                }
                None => {
                    // Reuse a legacy named commit whose complete document is
                    // already identical. This makes migration of old refs
                    // idempotent without manufacturing a metadata-dependent
                    // replacement commit.
                    let commit = alias_current
                        .filter(|commit| self.store.commit_tree(*commit).ok() == Some(tree))
                        .unwrap_or(self.store.write_commit(
                            message,
                            tree,
                            parent.or(alias_current),
                        )?);
                    (
                        commit,
                        Some(RefEdit::Create {
                            name: canonical_ref.clone(),
                            new: commit,
                        }),
                    )
                }
            };

            let mut edits = Vec::new();
            if let Some(edit) = canonical_edit {
                edits.push(edit);
            }
            for name in &aliases_to_restore {
                edits.push(RefEdit::Update {
                    name: self.reference(name),
                    expected: restoring_from.expect("aliases imply a restore"),
                    new: commit,
                });
            }
            let explicit_alias_will_restore = alias
                .is_some_and(|name| aliases_to_restore.iter().any(|restored| restored == name));
            if let Some(reference) = &alias_ref
                && !explicit_alias_will_restore
            {
                let current = alias_current;
                if current != Some(commit) {
                    edits.push(match current {
                        Some(expected) => RefEdit::Update {
                            name: reference.clone(),
                            expected,
                            new: commit,
                        },
                        None => RefEdit::Create {
                            name: reference.clone(),
                            new: commit,
                        },
                    });
                }
            }

            let current_index = self
                .store
                .refs()
                .read(&index::reference(&self.name))
                .map_err(Error::backend)?;
            let mut next = self.canonical_entries()?;
            if let Some((_, current)) = next.iter_mut().find(|(name, _)| name == &canonical_name) {
                *current = commit;
            } else {
                next.push((canonical_name.clone(), commit));
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

    /// The canonical entity ref paths and publication commit IDs, ascending.
    ///
    /// Caller-selected aliases are not included: they are compatibility refs,
    /// not members of the entity index.
    pub fn list_entries(&self) -> Result<Vec<(RefPath, ObjectId)>, Error> {
        let source = self.canonical_entries()?;
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
            let source = self.canonical_entries()?;
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
            .filter_map(|(name, commit)| match self.read_at(commit) {
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
            .compatibility_entries()?
            .into_iter()
            .filter(|(name, _)| self.entities.join_path(name).is_under(&prefix))
            .map(|(name, _)| name)
            .collect();
        entries.sort();
        Ok(entries)
    }

    fn compatibility_entries(&self) -> Result<Vec<(RefPath, ObjectId)>, Error> {
        let source = self.source_entries()?;
        // Prefer caller-selected names for the entity they name, but make that
        // preference per entity. A compatibility alias for one entity must not
        // hide unrelated canonical-only entities.
        let aliased_ids: Vec<_> = source
            .iter()
            .filter(|(name, commit)| !self.is_canonical_ref(name, *commit))
            .filter_map(|(_, commit)| self.compatibility_identity(*commit))
            .collect();
        let mut entries = Vec::new();
        for (name, commit) in &source {
            if self.tombstone_for(*commit).is_some() {
                continue;
            }
            let canonical = self.is_canonical_ref(name, *commit);
            let identity = self.compatibility_identity(*commit);
            if canonical && identity.is_some_and(|id| aliased_ids.contains(&id)) {
                continue;
            }
            entries.push((name.clone(), *commit));
        }
        Ok(entries)
    }

    fn list_entries_in(&self, prefix: RefPrefix) -> Result<Vec<(RefPath, ObjectId)>, Error> {
        if prefix == self.entities {
            return self.list_entries();
        }
        let mut entries: Vec<_> = self
            .canonical_entries()?
            .into_iter()
            .filter(|(name, _)| self.entities.join_path(name).is_under(&prefix))
            .collect();
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        Ok(entries)
    }

    /// Publish a typed tombstone at the canonical entity ref.
    ///
    /// The canonical ref, every compatibility alias currently pointing at the
    /// same publication, and the materialized index advance in one ref-store
    /// CAS batch. Repeating the operation is idempotent and returns
    /// [`DeleteResult::AlreadyDeleted`]. A missing ref is [`DeleteResult::Absent`].
    pub fn delete(&self, id: EntityId) -> Result<DeleteResult, Error>
    where
        R: Committer,
        O: Write,
    {
        self.delete_entity(id)
    }

    /// [`delete`](Self::delete) using the canonical entity id.
    pub fn delete_entity(&self, id: EntityId) -> Result<DeleteResult, Error>
    where
        R: Committer,
        O: Write,
    {
        let canonical_name = entity_id_name(id);
        let canonical_ref = self.reference(&canonical_name);
        loop {
            let canonical_current = self
                .store
                .refs()
                .read(&canonical_ref)
                .map_err(Error::backend)?;
            let aliases = self.aliases_for_entity(id, &canonical_name)?;

            if let Some(current) = canonical_current {
                if let EntityState::Deleted(existing) = self.read_at(current)?
                    && existing.tombstone.entity_id() == Some(id)
                {
                    // A historical alias can still point at an older
                    // publication of this same content. Repair all such names
                    // to the existing tombstone instead of claiming that the
                    // deletion is complete while a named read stays live.
                    let edits: Vec<_> = aliases
                        .iter()
                        .filter(|(_, target)| *target != current)
                        .map(|(name, target)| RefEdit::Update {
                            name: self.reference(name),
                            expected: *target,
                            new: current,
                        })
                        .collect();
                    if edits.is_empty() {
                        return Ok(DeleteResult::AlreadyDeleted(existing));
                    }
                    match self.store.refs().apply_batch(edits) {
                        Ok(()) => return Ok(DeleteResult::AlreadyDeleted(existing)),
                        Err(ApplyError::LostRace { .. }) => continue,
                        Err(ApplyError::Backend(err)) => return Err(Error::backend(err)),
                    }
                }

                let actual = self.store.commit_tree(current)?;
                if actual != id.object_id() {
                    return Err(Error::EntityIdCollision {
                        id,
                        expected: id.object_id(),
                        found: actual,
                    });
                }
            } else if aliases.is_empty() {
                // No canonical ref and no bound compatibility ref remains.
                return Ok(DeleteResult::Absent);
            }

            // If an alias-only entity was already tombstoned, promote that
            // tombstone to the canonical ref and repair every alias without
            // manufacturing a second deletion commit.
            if canonical_current.is_none()
                && let Some((_, existing_commit)) = aliases
                    .iter()
                    .find(|(_, commit)| self.tombstone_for(*commit) == Some(id))
            {
                let current_index = self
                    .store
                    .refs()
                    .read(&index::reference(&self.name))
                    .map_err(Error::backend)?;
                let mut edits = vec![RefEdit::Create {
                    name: canonical_ref.clone(),
                    new: *existing_commit,
                }];
                for (name, target) in &aliases {
                    if target != existing_commit {
                        edits.push(RefEdit::Update {
                            name: self.reference(name),
                            expected: *target,
                            new: *existing_commit,
                        });
                    }
                }
                let mut next = self.canonical_entries()?;
                next.push((canonical_name.clone(), *existing_commit));
                next.sort_by(|(a, _), (b, _)| a.cmp(b));
                if let Some(edit) = self.index_edit(current_index, &next)? {
                    edits.push(edit);
                }
                match self.store.refs().apply_batch(edits) {
                    Ok(()) => {
                        let EntityState::Deleted(existing) = self.read_at(*existing_commit)? else {
                            unreachable!("tombstone_for returned a tombstone commit")
                        };
                        return Ok(DeleteResult::AlreadyDeleted(existing));
                    }
                    Err(ApplyError::LostRace { .. }) => continue,
                    Err(ApplyError::Backend(err)) => return Err(Error::backend(err)),
                }
            }

            let parent = canonical_current.or_else(|| aliases.first().map(|(_, commit)| *commit));
            let tombstone = Tombstone::new(&self.name, id);
            let tree = tombstone::write(self.store, &tombstone)?;
            let message = format!("delete {}/{}\n", self.name, id);
            let commit = self.store.write_commit(&message, tree, parent)?;
            let mut edits = vec![match canonical_current {
                Some(expected) => RefEdit::Update {
                    name: canonical_ref.clone(),
                    expected,
                    new: commit,
                },
                None => RefEdit::Create {
                    name: canonical_ref.clone(),
                    new: commit,
                },
            }];

            // Match aliases by entity identity, not only by their current
            // publication commit, so historical alias targets are hidden too.
            for (name, expected) in aliases {
                edits.push(RefEdit::Update {
                    name: self.reference(&name),
                    expected,
                    new: commit,
                });
            }

            let current_index = self
                .store
                .refs()
                .read(&index::reference(&self.name))
                .map_err(Error::backend)?;
            let mut next = self.canonical_entries()?;
            if let Some((_, target)) = next.iter_mut().find(|(name, _)| name == &canonical_name) {
                *target = commit;
            } else {
                next.push((canonical_name.clone(), commit));
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

    /// Delete the entity currently addressed by a compatibility alias. The
    /// alias remains a compatibility ref and observes the tombstone, while
    /// canonical refs for other content-derived identities in the alias's
    /// history remain unchanged. Each distinct document tree is a distinct
    /// entity, so alias history is intentionally not traversed.
    pub fn delete_name(&self, name: &RefPath) -> Result<DeleteResult, Error>
    where
        R: Committer,
        O: Write,
    {
        let reference = self.reference(name);
        let Some(commit) = self.store.refs().read(&reference).map_err(Error::backend)? else {
            return Ok(DeleteResult::Absent);
        };

        let id = match self.read_at(commit)? {
            EntityState::Present(_) => canonical_document_id(self.store.commit_tree(commit)?),
            EntityState::Deleted(entry) => {
                entry.tombstone.entity_id().ok_or(Error::InvalidTombstone)?
            }
            EntityState::Absent => unreachable!("a ref read yielded a commit"),
        };
        self.delete_entity(id)
    }

    /// Delete a compatibility alias or canonical ref. Returns whether it
    /// existed. Removing an alias does not remove the canonical entity.
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
            let canonical = self.is_canonical_ref(name, expected);
            let mut edits = vec![RefEdit::Delete {
                name: reference.clone(),
                expected,
            }];
            if canonical {
                let current_index = self
                    .store
                    .refs()
                    .read(&index::reference(&self.name))
                    .map_err(Error::backend)?;
                let next: Vec<_> = self
                    .canonical_entries()?
                    .into_iter()
                    .filter(|(entry, _)| entry != name)
                    .collect();
                if let Some(edit) = self.index_edit(current_index, &next)? {
                    edits.push(edit);
                }
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

/// Return the direct canonical ref path for an [`EntityId`].
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

    /// Commit the value at the canonical content-derived ref and return its
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

    /// Commit the value at its canonical ref and maintain `alias` as an
    /// optional compatibility ref, returning the derived [`EntityId`].
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
        let (_, commit) = kind.publish_document(None, &message, tree)?;
        // Keep the old commit-named path readable for callers that use the
        // compatibility `entity_name*` helpers. It is an alias only; the
        // canonical ref was published from the document tree above.
        let alias = name(commit);
        let (_, commit) = kind.publish_document(Some(&alias), &message, tree)?;
        Ok(commit)
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
