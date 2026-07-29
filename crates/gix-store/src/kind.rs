//! One [`Kind`]: its schema ref and the entities beneath it.

use std::marker::PhantomData;

use facet::Facet;
use facet_git_tree::{
    Derivation, Hints, ObjectId, Schema, SchemaPinError, deserialize_value_with_schema,
    migration::derive::derive, schema_of,
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
    R: RefStore + Committer,
    O: Find + Write,
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
    R: RefStore + Committer,
    O: Find + Write,
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
    pub fn put(&self, name: &RefPath, value: &E::Value) -> Result<ObjectId, Error> {
        self.write(value).at(name)
    }

    /// The general form of [`put`](Self::put): set a message, then choose a
    /// name.
    pub fn write<'k>(&'k self, value: &'k E::Value) -> Put<'k, E, R, O> {
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

    /// The entity names published under this kind, ascending. Nesting is
    /// preserved: an entity named `<a>/<b>` lists as that path, not as `<a>`.
    pub fn list(&self) -> Result<Vec<RefPath>, Error> {
        let mut names: Vec<RefPath> = self
            .store
            .refs()
            .prefixed(&self.entities)
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
    pub fn remove(&self, name: &RefPath) -> Result<bool, Error> {
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
    R: RefStore + Committer,
    O: Find + Write,
{
    /// Publish the schema derived from `T`.
    pub fn publish(&self) -> Result<ObjectId, Error> {
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
    R: RefStore + Committer,
    O: Find + Write,
{
    /// The ref this schema is published at.
    pub fn reference(&self) -> &RefName {
        &self.reference
    }

    /// Publish (or evolve) the schema, committing it forward over the current
    /// tip.
    pub fn put(&self, doc: &Schema) -> Result<ObjectId, Error> {
        self.write(doc, &Hints::new())
    }

    /// [`put`](Self::put) with authoring hints, which are what let a
    /// remove-plus-add pair be recognised as the rename it actually is.
    pub fn write(&self, doc: &Schema, hints: &Hints) -> Result<ObjectId, Error> {
        let previous = match self
            .store
            .refs()
            .read(&self.reference)
            .map_err(Error::backend)?
        {
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
        let message = format!("schema {}\n", self.kind);
        self.store.commit_forward(&self.reference, &message, tree)
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
    R: RefStore + Committer,
    O: Find + Write,
{
    /// Use `summary` as the commit summary instead of the default.
    pub fn message(mut self, summary: impl Into<String>) -> Self {
        self.message = Some(summary.into());
        self
    }

    /// Commit the value forward at `name`.
    pub fn at(self, name: &RefPath) -> Result<ObjectId, Error> {
        let kind = self.kind;
        let default = || format!("store {}/{name}", kind.name);
        let (message, tree) = self.build(default)?;
        kind.store
            .commit_forward(&kind.reference(name), &message, tree)
    }

    /// Commit the value at a fresh name: the commit's own id, in full.
    ///
    /// The returned id *is* the name, recoverable with [`entity_name`], so an
    /// anonymous entity collides only when two distinct commits share an
    /// object id.
    pub fn anonymous(self) -> Result<ObjectId, Error> {
        self.anonymous_at(entity_name)
    }

    /// Commit the value at a fresh name under `group`: `<group>/<commit-oid>`.
    ///
    /// Recoverable with [`entity_name_under`]. Grouping this way makes
    /// listing every anonymous entity in `group` a ref-prefix scan instead of
    /// a full-store scan.
    pub fn anonymous_under(self, group: &RefPath) -> Result<ObjectId, Error> {
        self.anonymous_at(|commit| entity_name_under(group, commit))
    }

    fn anonymous_at(self, name: impl FnOnce(ObjectId) -> RefPath) -> Result<ObjectId, Error> {
        let kind = self.kind;
        let default = || format!("store {}/<auto>", kind.name);
        let (message, tree) = self.build(default)?;
        let store = kind.store;

        // Write the commit before touching any ref, so its id — which
        // determines the entity's name — is known first.
        let commit = store.write_commit(&message, tree, None)?;
        let reference = kind.reference(&name(commit));

        match store.refs().apply(RefEdit::Create {
            name: reference.clone(),
            new: commit,
        }) {
            Ok(()) => Ok(commit),
            Err(ApplyError::LostRace { .. }) => {
                match store.refs().read(&reference).map_err(Error::backend)? {
                    Some(existing) if existing == commit => Ok(commit),
                    _ => Err(Error::NameTaken { name: reference }),
                }
            }
            Err(ApplyError::Backend(err)) => Err(Error::backend(err)),
        }
    }

    fn build(self, default_summary: impl FnOnce() -> String) -> Result<(String, ObjectId), Error> {
        let store = self.kind.store;
        let (schema_commit, doc) = self.kind.current_schema()?;
        let value_tree = E::write(self.value, &doc, store.objects())?;
        let schema_tree = store.commit_tree(schema_commit)?;
        let tree = store.bind_schema(value_tree, schema_tree)?;
        let summary = self.message.unwrap_or_else(default_summary);
        let message = format!("{summary}\n\nSchema: {schema_commit}\n");
        Ok((message, tree))
    }
}
