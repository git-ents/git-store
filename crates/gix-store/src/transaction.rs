//! A compare-and-swap batch spanning any number of entities and kinds.

use std::collections::BTreeMap;

use facet_git_tree::ObjectId;
use gix::objs::{Find, Write};
use gix_refstore::{ApplyError, Committer, Expectation, RefEdit, RefPath, RefSegment, RefStore};

use crate::error::Error;
use crate::identity::{DocumentTree, EntityId};
use crate::index;
use crate::kind::{PublishOutcome, entity_id_name};
use crate::store::{Publication, Store};
use crate::tombstone::{self, Tombstone};

/// A document staged for publication under [`Transaction::publish`].
struct StagedPublish {
    kind: RefSegment,
    document: DocumentTree,
    expect: Expectation,
    alias: Option<RefPath>,
}

/// A deletion staged under [`Transaction::delete`].
struct StagedDelete {
    kind: RefSegment,
    id: EntityId,
    expect: Expectation,
}

/// One operation staged into a [`Transaction`].
enum Staged {
    /// See [`StagedPublish`].
    Publish(StagedPublish),
    /// See [`StagedDelete`].
    Delete(StagedDelete),
}

/// A batch of entity publications and deletions, across any number of
/// kinds, staged to land as one all-or-nothing compare-and-swap.
///
/// [`RefStore::apply_batch`] checks every edit's [`Expectation`] before
/// publishing any of them, so a stale expectation on any staged operation
/// leaves every ref this transaction would have touched unchanged — the
/// atomicity [`Kind::publish_prepared`](crate::Kind::publish_prepared) gives
/// one entity, extended across entities and kinds. Build one with
/// [`Store::transaction`](crate::Store::transaction).
pub struct Transaction<'s, R, O> {
    store: &'s Store<R, O>,
    message: String,
    staged: Vec<Staged>,
}

impl<'s, R, O> Transaction<'s, R, O> {
    pub(crate) fn new(store: &'s Store<R, O>, message: String) -> Self {
        Self {
            store,
            message,
            staged: Vec::new(),
        }
    }

    /// Stage `document`'s publication under `kind`, subject to `expect` on
    /// its ref.
    ///
    /// The ref is the content-derived canonical entity ref unless the next
    /// call is [`alias`](Self::alias), which redirects this publication to a
    /// caller-chosen name instead. `document`'s embedded schema must name
    /// `kind`; a mismatch is reported when [`commit`](Self::commit) checks
    /// every staged operation.
    pub fn publish(
        mut self,
        kind: &RefSegment,
        document: DocumentTree,
        expect: Expectation,
    ) -> Self {
        self.staged.push(Staged::Publish(StagedPublish {
            kind: kind.clone(),
            document,
            expect,
            alias: None,
        }));
        self
    }

    /// Publish the document just staged by [`publish`](Self::publish) at
    /// `name` instead of its canonical entity ref.
    ///
    /// # Panics
    ///
    /// Panics if no [`publish`](Self::publish) call immediately precedes it.
    pub fn alias(mut self, name: RefPath) -> Self {
        match self.staged.last_mut() {
            Some(Staged::Publish(publish)) => publish.alias = Some(name),
            _ => panic!("Transaction::alias must immediately follow Transaction::publish"),
        }
        self
    }

    /// Stage a typed tombstone over `id`'s canonical ref under `kind`,
    /// subject to `expect` on that ref.
    pub fn delete(mut self, kind: &RefSegment, id: EntityId, expect: Expectation) -> Self {
        self.staged.push(Staged::Delete(StagedDelete {
            kind: kind.clone(),
            id,
            expect,
        }));
        self
    }
}

impl<'s, R: RefStore + Committer, O: Find + Write> Transaction<'s, R, O> {
    /// Check every staged expectation, build one ref edit per touched entity
    /// plus one materialized-index update per touched kind, and apply them
    /// as a single [`RefStore::apply_batch`].
    ///
    /// Returns the publications for every staged [`publish`](Self::publish),
    /// in staging order. Nothing is written until every expectation has been
    /// checked: a mismatch aborts before the first object or ref write, and
    /// [`RefStore::apply_batch`] itself is one compare-and-swap, so a race
    /// lost after that point still leaves every touched ref unchanged.
    pub fn commit(self) -> Result<Vec<Publication>, Error> {
        let mut edits = Vec::new();
        let mut publications = Vec::with_capacity(self.staged.len());
        let mut index_deltas: BTreeMap<RefSegment, Vec<(RefPath, ObjectId)>> = BTreeMap::new();

        for staged in &self.staged {
            match staged {
                Staged::Publish(publish) => {
                    let (id, name, commit) = self.stage_publish(publish, &mut edits)?;
                    publications.push(Publication::new(id, commit));
                    index_deltas
                        .entry(publish.kind.clone())
                        .or_default()
                        .push((name, commit));
                }
                Staged::Delete(delete) => {
                    if let Some(commit) = self.stage_delete(delete, &mut edits)? {
                        index_deltas
                            .entry(delete.kind.clone())
                            .or_default()
                            .push((entity_id_name(delete.id), commit));
                    }
                }
            }
        }

        for (kind, deltas) in index_deltas {
            let entities = self.store.layout().data.child(&kind);
            let mut entries = index::source_entries(self.store, &entities)?;
            for (name, commit) in deltas {
                match entries.iter_mut().find(|(existing, _)| *existing == name) {
                    Some((_, current)) => *current = commit,
                    None => {
                        entries.push((name, commit));
                        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
                    }
                }
            }
            let current_index = self
                .store
                .refs()
                .read(&index::reference(&kind))
                .map_err(Error::backend)?;
            if let Some(edit) = index::edit(self.store, &kind, current_index, &entries)? {
                edits.push(edit);
            }
        }

        if !edits.is_empty() {
            self.store
                .refs()
                .apply_batch(edits)
                .map_err(Error::backend)?;
        }
        Ok(publications)
    }

    /// Validate and stage one publication's ref edit, without applying it,
    /// by resolving it through [`Kind::resolve_publish`](crate::kind::Kind::resolve_publish) —
    /// the same edit-building logic a lone [`Kind::publish_prepared`](crate::Kind::publish_prepared)
    /// uses, just applied later as part of this transaction's batch instead
    /// of on its own. Returns the entity id, the name it published under,
    /// and the commit its ref will point at once `edits` is applied.
    fn stage_publish(
        &self,
        publish: &StagedPublish,
        edits: &mut Vec<RefEdit>,
    ) -> Result<(EntityId, RefPath, ObjectId), Error> {
        let tree = publish.document.object_id();
        let kind = self.store.dynamic(publish.kind.clone());
        match kind.resolve_publish(
            publish.alias.as_ref(),
            &self.message,
            tree,
            None,
            Some(publish.expect),
        )? {
            PublishOutcome::Stale { reference } => {
                Err(kind.expectation_error(&reference, publish.expect))
            }
            PublishOutcome::Ready(resolved) => {
                if let Some(edit) = resolved.edit {
                    edits.push(edit);
                }
                Ok((resolved.id, resolved.name, resolved.commit))
            }
        }
    }

    /// Validate and stage one deletion's ref edit, without applying it.
    /// Returns `None` when the entity is already absent or already a
    /// tombstone, so this staged deletion contributes nothing.
    fn stage_delete(
        &self,
        delete: &StagedDelete,
        edits: &mut Vec<RefEdit>,
    ) -> Result<Option<ObjectId>, Error> {
        let name = entity_id_name(delete.id);
        let reference = self
            .store
            .layout()
            .data
            .child(&delete.kind)
            .join_path(&name);
        let current = self.store.refs().read(&reference).map_err(Error::backend)?;
        if !matches_expectation(delete.expect, current) {
            return Err(self.expectation_error(&reference, delete.expect));
        }
        let Some(current) = current else {
            return Ok(None);
        };

        let root = self.store.commit_tree(current)?;
        let (value_tree, schema_tree) = self.store.split(root, current)?;
        let doc = self.store.schema(schema_tree)?;
        if let Some(existing) = tombstone::read(self.store, &value_tree, &doc)? {
            if existing.kind != delete.kind.as_str() {
                return Err(Error::KindMismatch {
                    expected: delete.kind.clone(),
                    found: existing.kind,
                });
            }
            return Ok(None);
        }

        let marker = Tombstone::new(&delete.kind, delete.id);
        let tree = tombstone::write(self.store, &marker)?;
        let commit = self
            .store
            .write_commit(&self.message, tree, Some(current))?;
        edits.push(RefEdit::Update {
            name: reference,
            expected: current,
            new: commit,
        });
        Ok(Some(commit))
    }

    fn expectation_error(&self, reference: &gix_refstore::RefName, expected: Expectation) -> Error {
        Error::backend(ApplyError::<<R as RefStore>::Error>::LostRace {
            name: reference.clone(),
            expected,
        })
    }
}

fn matches_expectation(expect: Expectation, current: Option<ObjectId>) -> bool {
    match expect {
        Expectation::Absent => current.is_none(),
        Expectation::Exactly(old) => current == Some(old),
    }
}
