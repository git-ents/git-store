//! Migrations as they live in a repository: derived when a schema advances,
//! stored in the advancing schema commit's own tree, and applied at read time.
//!
//! Placing a migration in the schema commit rather than at a ref of its own is
//! what keeps it travelling: a value's commit binds its schema tree, the
//! schema commit carrying that tree also carries the migration off its
//! predecessor, so fetching one data ref brings the schema *and* the lineage
//! needed to upcast it. A separate migration ref would reintroduce exactly the
//! "data does not travel" failure the `{schema/, value/}` split exists to fix.

use facet_git_tree::{Edge, Migration, ObjectId, Schema, apply_chain};
use facet_value::Value;
use gix::objs::{Find, Write};
use gix_refstore::RefStore;

use crate::error::Error;
use crate::kind::KindSchema;
use crate::store::Store;

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

    /// Upcast `value` — read against the schema stored at `schema_tree` — to
    /// this kind's current schema.
    ///
    /// Walks the schema ref's history to locate `schema_tree`, then applies
    /// each recorded migration from there forward. Never writes an object:
    /// the stored value keeps its tree hash, and with it every attestation
    /// made about it.
    pub(crate) fn upcast(&self, value: &Value, schema_tree: &ObjectId) -> Result<Value, Error> {
        let steps = self.steps_from(schema_tree)?;
        let chain: Vec<Edge<'_>> = steps
            .iter()
            .map(|step| Edge {
                from: &step.from,
                migration: &step.migration,
            })
            .collect();
        Ok(apply_chain(value, &chain)?)
    }

    /// The migrations between the schema commit holding `schema_tree` and the
    /// current tip, oldest edge first.
    fn steps_from(&self, schema_tree: &ObjectId) -> Result<Vec<Step>, Error> {
        let history = self.history()?;
        // A value written against the tip needs no upcast at all.
        if let Some(tip) = history.first()
            && self.store.commit_tree(*tip)? == *schema_tree
        {
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
        Err(Error::SchemaNotInHistory {
            kind: self.kind.clone(),
            schema_tree: *schema_tree,
        })
    }
}
