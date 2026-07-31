//! One [`Kind`]: its schema ref and the entities beneath it.

use std::marker::PhantomData;

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

use crate::encoding::{Encoding, Typed};
use crate::error::Error;
use crate::store::Store;

/// One kind: its schema ref and the entities under it.
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

    /// The ref an entity of this kind lives at.
    pub fn reference(&self, name: &RefPath) -> RefName {
        self.entities.join_path(name)
    }

    /// This kind's schema.
    pub fn schema(&self) -> KindSchema<'s, R, O> {
        KindSchema {
            store: self.store,
            kind: self.name.clone(),
            reference: self.schema_ref.clone(),
        }
    }

    /// Store `value` at `name`, with a default commit summary.
    pub fn put(&self, name: &RefPath, value: &E::Value) -> Result<ObjectId, Error>
    where
        R: Committer,
        O: Write,
    {
        self.write(value).at(name)
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

    /// The current value at `name`, or `None` when absent.
    pub fn get(&self, name: &RefPath) -> Result<Option<E::Value>, Error> {
        Ok(self.get_entry(name)?.map(|entry| entry.value))
    }

    /// The value as of one data commit, read entirely out of that commit's
    /// own tree.
    pub fn get_at(&self, commit: ObjectId) -> Result<E::Value, Error> {
        Ok(self.get_entry_at(commit)?.value)
    }

    /// Decode a `{value/, schema/}` tree directly — the read-side mirror of
    /// [`compile`](Self::compile). Unlike [`get_at`](Self::get_at), `tree`
    /// need not be a commit's tree, and no ref is consulted: any tree of that
    /// shape decodes, however it was reached.
    pub fn decode(&self, tree: ObjectId) -> Result<E::Value, Error> {
        let (value_tree, schema_tree) = self.store.split(tree, tree)?;
        let doc = Schema::read_pinned(&schema_tree, self.store.objects())?;
        E::read(&value_tree, &doc, self.store.objects())
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
    /// `None` when absent.
    pub fn get_entry(&self, name: &RefPath) -> Result<Option<Entry<E::Value>>, Error> {
        match self
            .store
            .refs()
            .read(&self.reference(name))
            .map_err(Error::backend)?
        {
            Some(tip) => Ok(Some(self.get_entry_at(tip)?)),
            None => Ok(None),
        }
    }

    /// [`get_entry`](Self::get_entry) for one data commit directly, read
    /// entirely out of that commit's own tree.
    pub fn get_entry_at(&self, commit: ObjectId) -> Result<Entry<E::Value>, Error> {
        let (root, message) = self.store.commit_tree_and_summary(commit)?;
        let (value_tree, schema_tree) = self.store.split(root, commit)?;
        let doc = Schema::read_pinned(&schema_tree, self.store.objects())?;
        let value = E::read(&value_tree, &doc, self.store.objects())?;
        Ok(Entry {
            value,
            commit,
            message,
        })
    }

    /// The current value at `name`, upcast to this kind's current schema.
    ///
    /// A value written under an older schema is already readable through
    /// [`get`](Self::get) — its own commit binds the schema it conforms to —
    /// so this exists for the other half: reading it as the shape the kind
    /// has *since* evolved into. The result is a [`Value`] whatever the
    /// kind's encoding, because an upcast is defined over the schema, not
    /// over any Rust type that happens to match one generation of it.
    ///
    /// Nothing is written: the stored value keeps its tree hash, and with it
    /// every attestation made about it.
    pub fn get_migrated(&self, name: &RefPath) -> Result<Option<Value>, Error> {
        Ok(self.get_entry_migrated(name)?.map(|entry| entry.value))
    }

    /// [`get_migrated`](Self::get_migrated) for one data commit.
    pub fn get_at_migrated(&self, commit: ObjectId) -> Result<Value, Error> {
        Ok(self.get_entry_at_migrated(commit)?.value)
    }

    /// [`get_migrated`](Self::get_migrated), together with the commit the
    /// value was read from.
    pub fn get_entry_migrated(&self, name: &RefPath) -> Result<Option<Entry<Value>>, Error> {
        match self
            .store
            .refs()
            .read(&self.reference(name))
            .map_err(Error::backend)?
        {
            Some(tip) => Ok(Some(self.get_entry_at_migrated(tip)?)),
            None => Ok(None),
        }
    }

    /// [`get_entry_migrated`](Self::get_entry_migrated) for one data commit.
    pub fn get_entry_at_migrated(&self, commit: ObjectId) -> Result<Entry<Value>, Error> {
        let (root, message) = self.store.commit_tree_and_summary(commit)?;
        let (value_tree, schema_tree) = self.store.split(root, commit)?;
        let doc = Schema::read_pinned(&schema_tree, self.store.objects())?;
        let value = deserialize_value_with_schema(&value_tree, &doc, self.store.objects())?;
        let value = self.schema().upcast(&value, &schema_tree)?;
        Ok(Entry {
            value,
            commit,
            message,
        })
    }

    /// An entity's commits, tip-first along first parents; empty when absent.
    pub fn history(&self, name: &RefPath) -> Result<Vec<ObjectId>, Error> {
        self.store.ref_history(&self.reference(name))
    }

    /// Commit `rebuild`'s result forward at `name`, retried with a fresh
    /// [`get_entry`](Self::get_entry) whenever the compare-and-swap loses a
    /// race — so `rebuild` always sees the entry it actually commits over
    /// (`None` when `name` is unoccupied), never one read before a
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
    /// rather than [`Error`].
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
        let reference = self.reference(name);
        loop {
            let current = self.get_entry(name)?;
            let (summary, value) = rebuild(current.as_ref())?;
            let (message, tree) = commit_body(self, &value, summary)?;
            let parent = current.as_ref().map(|entry| entry.commit);
            let commit = self.store.write_commit(&message, tree, parent)?;
            let edit = match parent {
                Some(expected) => RefEdit::Update {
                    name: reference.clone(),
                    expected,
                    new: commit,
                },
                None => RefEdit::Create {
                    name: reference.clone(),
                    new: commit,
                },
            };
            match self.store.refs().apply(edit) {
                Ok(()) => return Ok(commit),
                Err(ApplyError::LostRace { .. }) => continue,
                Err(ApplyError::Backend(err)) => return Err(Error::backend(err).into()),
            }
        }
    }

    /// The entity names published under this kind, ascending. Nesting is
    /// preserved: an entity named `<a>/<b>` lists as that path, not as `<a>`.
    pub fn list(&self) -> Result<Vec<RefPath>, Error> {
        self.list_in(self.entities.clone())
    }

    /// [`list`](Self::list) narrowed to the entities nested under `group`,
    /// scanning only that subtree's refs instead of every entity of the kind.
    /// Names come back in full, `group` included.
    pub fn list_under(&self, group: &RefPath) -> Result<Vec<RefPath>, Error> {
        let prefix = group
            .segments()
            .iter()
            .fold(self.entities.clone(), |prefix, segment| {
                prefix.child(segment)
            });
        self.list_in(prefix)
    }

    fn list_in(&self, prefix: RefPrefix) -> Result<Vec<RefPath>, Error> {
        let mut names: Vec<RefPath> = self
            .store
            .refs()
            .prefixed(&prefix)
            .map_err(Error::backend)?
            .into_iter()
            .filter_map(|(name, _)| name.relative_to(&self.entities))
            .collect();
        // `prefixed` is ascending by *ref name*, which orders `a/b` against
        // `a-b` by the separator byte; `RefPath` orders segment by segment.
        // Sort so the result agrees with the type it is returned as.
        names.sort();
        Ok(names)
    }

    /// Delete an entity's ref. Returns whether it existed.
    pub fn remove(&self, name: &RefPath) -> Result<bool, Error>
    where
        R: Committer,
    {
        let reference = self.reference(name);
        loop {
            let Some(expected) = self.store.refs().read(&reference).map_err(Error::backend)? else {
                return Ok(false);
            };
            let edit = RefEdit::Delete {
                name: reference.clone(),
                expected,
            };
            match self.store.refs().apply(edit) {
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
        self.schema().put(&schema_of::<T>()?)
    }
}

/// A value read alongside the commit it came from, sparing the caller from
/// decoding that commit itself.
pub struct Entry<V> {
    /// The decoded value.
    pub value: V,
    /// The commit the value was read from.
    pub commit: ObjectId,
    /// That commit's summary — its message's first logical line, per
    /// [`gix::objs::CommitRef::message_summary`].
    pub message: String,
}

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
        self.check_identity_subtrees(doc)?;
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
                    && let Derivation::Complete(migration) = derive(&previous, doc, hints)
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

/// The name [`Put::anonymous`] stores a commit under: the commit's own id.
pub fn entity_name(commit: ObjectId) -> RefPath {
    commit_segment(commit).into()
}

/// The name [`Put::anonymous_under`] stores a commit under: `<group>/<commit-oid>`.
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
    /// Use `summary` as the commit summary instead of the default.
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
        kind.store
            .commit_forward(&kind.reference(name), &message, |_| Ok(tree))
    }

    fn build(self, default_summary: impl FnOnce() -> String) -> Result<(String, ObjectId), Error>
    where
        O: Write,
    {
        let summary = self.message.unwrap_or_else(default_summary);
        commit_body(self.kind, self.value, summary)
    }

    /// Commit the value at a fresh name: the commit's own id, in full.
    ///
    /// The returned id *is* the name, recoverable with [`entity_name`], so an
    /// anonymous entity collides only when two distinct commits share an
    /// object id.
    pub fn anonymous(self) -> Result<ObjectId, Error>
    where
        R: Committer,
        O: Write,
    {
        self.anonymous_at(entity_name)
    }

    /// Commit the value at a fresh name under `group`: `<group>/<commit-oid>`.
    ///
    /// Recoverable with [`entity_name_under`]. Grouping this way makes
    /// listing every anonymous entity in `group` a ref-prefix scan instead of
    /// a full-store scan.
    pub fn anonymous_under(self, group: &RefPath) -> Result<ObjectId, Error>
    where
        R: Committer,
        O: Write,
    {
        self.anonymous_at(|commit| entity_name_under(group, commit))
    }

    fn anonymous_at(self, name: impl FnOnce(ObjectId) -> RefPath) -> Result<ObjectId, Error>
    where
        R: Committer,
        O: Write,
    {
        let kind = self.kind;
        let default = || format!("store {}/<auto>", kind.name);
        let (message, tree) = self.build(default)?;
        let store = kind.store;

        // Write the commit before touching any ref, so its id — which
        // determines the entity's name — is known first.
        let commit = store.write_commit(&message, tree, None)?;
        let reference = kind.reference(&name(commit));

        loop {
            match store.refs().apply(RefEdit::Create {
                name: reference.clone(),
                new: commit,
            }) {
                Ok(()) => return Ok(commit),
                Err(ApplyError::LostRace { .. }) => {
                    match store.refs().read(&reference).map_err(Error::backend)? {
                        Some(existing) if existing == commit => return Ok(commit),
                        // The name is still unoccupied, so the loss was
                        // transient backend contention, not a genuine
                        // collision: retry the same create.
                        None => continue,
                        Some(_) => return Err(Error::NameTaken { name: reference }),
                    }
                }
                Err(ApplyError::Backend(err)) => return Err(Error::backend(err)),
            }
        }
    }
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
    let (schema_commit, tree) = kind.compile_with_schema(value)?;
    let message = format!("{summary}\n\nSchema: {schema_commit}\n");
    Ok((message, tree))
}
