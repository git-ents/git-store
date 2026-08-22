//! Schema migrations are applied when values are read.
//!
//! Existing Git objects are preserved; newer schema versions are used to
//! transform older values in memory.

use facet_git_tree::{Edge, Migration, ObjectId, Schema, apply_chain};
use facet_value::Value;
use gix::objs::{Find, Write};
use gix_refstore::RefStore;

use crate::error::Error;
use crate::kind::KindSchema;
use crate::store::Store;

/// A schema selected as the destination for an explicit migrated read.
///
/// `history` is tip-first, as returned by [`KindSchema::history`]. Keeping the
/// schema document and its history together makes the migration target
/// independent of the store's live schema ref: callers may capture it before a
/// ref is deleted, or supply a history received through another channel.
#[derive(Debug, Clone, PartialEq)]
pub struct TargetSchema {
    schema: Schema,
    history: Vec<ObjectId>,
}

impl TargetSchema {
    /// Select `schema` and its tip-first schema-commit `history` as a
    /// migration target.
    ///
    /// The first commit should contain `schema`, and every following commit
    /// should be its first-parent predecessor. The target document is checked
    /// against the tip when a migrated value is read; an empty history is
    /// accepted here so callers can construct a target before choosing a
    /// history, and reads report it clearly.
    pub fn new(schema: Schema, history: Vec<ObjectId>) -> Self {
        Self { schema, history }
    }

    /// The selected destination schema document.
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// The selected schema history, tip-first.
    pub fn history(&self) -> &[ObjectId] {
        &self.history
    }
}

/// The tree entry a schema commit records its migration under.
pub(crate) const ENTRY: &str = "migration";

impl<R, O> Store<R, O>
where
    R: RefStore,
    O: Find,
{
    /// Add `migration` to the already-written schema tree `doc`.
    pub(crate) fn bind_migration(
        &self,
        doc: ObjectId,
        migration: ObjectId,
    ) -> Result<ObjectId, Error>
    where
        O: Write,
    {
        let mut entries = self.tree_entries(doc)?;
        entries.push(gix::objs::tree::Entry {
            mode: self.entry_mode(migration)?,
            filename: ENTRY.into(),
            oid: migration,
        });
        entries.sort();
        self.objects()
            .write(&gix::objs::Tree { entries })
            .map_err(Error::backend)
    }
}

/// One step of a chain: the document values on this side conform to, and the
/// migration off it.
struct Step {
    from: Schema,
    migration: Migration,
}

impl<R, O> KindSchema<'_, R, O>
where
    R: RefStore,
    O: Find,
{
    /// The migration a schema commit records off its predecessor, or `None`
    /// for a commit that established the kind.
    pub fn migration_at(&self, commit: ObjectId) -> Result<Option<Migration>, Error> {
        let tree = self.store.commit_tree(commit)?;
        match self.store.find_entry(tree, ENTRY)? {
            Some(oid) => Ok(Some(Migration::read_pinned(&oid, self.store.objects())?)),
            None => Ok(None),
        }
    }

    /// Build a target snapshot from this kind's current schema ref.
    ///
    /// This is the ordinary way to reach [`Kind::read_as`](crate::Kind::read_as)
    /// when the destination is whatever the kind currently publishes. Callers
    /// that must not consult the live ref construct [`TargetSchema`] directly.
    pub fn current_target(&self) -> Result<TargetSchema, Error> {
        let history = self.history()?;
        let tip = history.first().copied().ok_or_else(|| Error::NoSchema {
            kind: self.kind.clone(),
        })?;
        let tree = self.store.commit_tree(tip)?;
        let schema = Schema::read_pinned(&tree, self.store.objects())?;
        Ok(TargetSchema::new(schema, history))
    }

    /// Upcast `value` — read against the schema stored at `schema_tree` — to an
    /// explicitly selected target schema and history.
    ///
    /// No schema ref is consulted. Never writes an object: the stored value
    /// keeps its tree hash, and with it every attestation made about it.
    pub(crate) fn upcast_to(
        &self,
        value: &Value,
        schema_tree: &ObjectId,
        target: &TargetSchema,
    ) -> Result<Value, Error> {
        let steps = self.steps_from(schema_tree, target)?;
        let chain: Vec<Edge<'_>> = steps
            .iter()
            .map(|step| Edge {
                from: &step.from,
                migration: &step.migration,
            })
            .collect();
        Ok(apply_chain(value, &chain)?)
    }

    /// The migrations between the source schema tree and an explicit target,
    /// oldest edge first.
    fn steps_from(
        &self,
        schema_tree: &ObjectId,
        target: &TargetSchema,
    ) -> Result<Vec<Step>, Error> {
        let history = target.history();
        let tip = history
            .first()
            .copied()
            .ok_or_else(|| Error::TargetHistoryEmpty {
                kind: self.kind.clone(),
            })?;
        let target_tree = self.store.commit_tree(tip)?;
        let stored_target = self.store.schema(target_tree)?;
        if stored_target.as_ref() != target.schema() || target.schema().kind != self.kind.as_str() {
            return Err(Error::TargetSchemaMismatch {
                kind: self.kind.clone(),
                commit: tip,
                schema_tree: target_tree,
            });
        }

        // A value written against the selected tip needs no upcast at all.
        if target_tree == *schema_tree {
            return Ok(Vec::new());
        }
        // `history` is tip-first, so each window is (newer, older) and the
        // edges accumulate newest-first; they are reversed before returning.
        let mut steps = Vec::new();
        for pair in history.windows(2) {
            let (newer, older) = (pair[0], pair[1]);
            let older_tree = self.store.commit_tree(older)?;
            steps.push(Step {
                from: Schema::read_pinned(&older_tree, self.store.objects())?,
                migration: self.migration_at(newer)?.ok_or(Error::MigrationMissing {
                    kind: self.kind.clone(),
                    commit: newer,
                })?,
            });
            if older_tree == *schema_tree {
                steps.reverse();
                return Ok(steps);
            }
        }
        Err(Error::TargetSchemaNotInHistory {
            kind: self.kind.clone(),
            schema_tree: *schema_tree,
        })
    }
}
