//! The database handle: every operation a caller performs over one
//! repository's refs, objects, and snapshots.

use facet_value::Value;
use git_prolly::{ProllyConfig, ProllyStore};
use gix::ObjectId;
use gix::bstr::ByteSlice;
use gix::objs::{CommitRef, Find as _, Kind};
use gix_refstore::{GixRefStore, RefEdit, RefName, RefStore};

use crate::error::{
    CasConflict, CheckoutError, CommitError, Error, MergeError, ReadStateError, WriteStateError,
};
use crate::format::{DEFAULT_BRANCH, TableName};
use crate::merge::{Merged, merge_snapshots};
use crate::snapshot::Snapshot;
use crate::workspace::HEAD_REF;
use crate::workspace::{
    self, ChangeKind, Head, Status, TableStatus, WorkspaceState, apply_cas, branch_ref, cas_edit,
    read_ref, row_change, write_commit,
};

/// A handle to one repository's database state.
///
/// The handle holds no caches: every operation re-reads the refs it depends
/// on, so two handles — or two processes — see the same state and are
/// serialized by the same compare-and-swap ref edits.
pub struct Database<'repo> {
    repo: &'repo gix::Repository,
    refs: GixRefStore<'repo>,
    store: ProllyStore<'repo>,
}

impl<'repo> Database<'repo> {
    /// Open a database over `repo` with the default [git_prolly::ProllyConfig].
    #[must_use]
    pub fn open(repo: &'repo gix::Repository) -> Self {
        Self::with_config(repo, ProllyConfig::default())
            .expect("default configuration is always valid")
    }

    /// Open a database over `repo` with an explicit configuration.
    ///
    /// # Errors
    ///
    /// Returns [`git_prolly::Error::Config`] when the configuration is
    /// invalid.
    pub fn with_config(
        repo: &'repo gix::Repository,
        config: ProllyConfig,
    ) -> Result<Self, git_prolly::Error> {
        Ok(Self {
            repo,
            refs: GixRefStore::new(repo),
            store: ProllyStore::with_config(repo, config)?,
        })
    }

    /// The Prolly store all table roots are built and verified with.
    #[must_use]
    pub const fn store(&self) -> &ProllyStore<'repo> {
        &self.store
    }

    /// The repository backing this database.
    #[must_use]
    pub const fn repo(&self) -> &'repo gix::Repository {
        self.repo
    }

    /// Point `refs/db/HEAD` at an unborn `main` branch.
    ///
    /// Idempotent: an existing `HEAD` is left exactly as it is, so `init` on
    /// an initialized database is a no-op. An unborn branch is the empty
    /// database; no commit or snapshot is written until the first one is.
    ///
    /// # Errors
    ///
    /// Returns [`ReadStateError`] when the ref backend fails.
    pub fn init(&self) -> Result<(), ReadStateError> {
        let target =
            gix::refs::FullName::try_from(format!("{}/{DEFAULT_BRANCH}", workspace::HEADS_PREFIX))
                .map_err(|error| ReadStateError::git(error.to_string()))?;
        let edit = gix::refs::transaction::RefEdit {
            change: gix::refs::transaction::Change::Update {
                log: gix::refs::transaction::LogChange {
                    mode: gix::refs::transaction::RefLog::AndReference,
                    force_create_reflog: false,
                    message: "initialize database".into(),
                },
                expected: gix::refs::transaction::PreviousValue::Any,
                new: gix::refs::Target::Symbolic(target),
            },
            name: gix::refs::FullName::try_from(HEAD_REF)
                .map_err(|error| ReadStateError::git(error.to_string()))?,
            deref: false,
        };
        self.repo
            .edit_reference(edit)
            .map_err(ReadStateError::git)?;
        Ok(())
    }

    /// Resolve the database `HEAD`.
    ///
    /// # Errors
    ///
    /// Returns [`ReadStateError`] when a ref or object cannot be read.
    pub fn head(&self) -> Result<Head, ReadStateError> {
        let head = match self.repo.try_find_reference(HEAD_REF) {
            Ok(Some(head)) => head,
            Ok(None) => return Ok(Head::Missing),
            Err(error) => return Err(ReadStateError::git(error)),
        };
        match head.inner.target {
            gix::refs::Target::Symbolic(branch) => {
                let name = RefName::new(branch.as_bstr().to_str().map_err(|_| {
                    ReadStateError::git("refs/db/HEAD points at a non-UTF-8 ref name")
                })?)
                .map_err(ReadStateError::git)?;
                match read_ref(&self.refs, &name)? {
                    Some(commit) => Ok(Head::Branch {
                        branch: name,
                        commit,
                    }),
                    None => Ok(Head::Unborn { branch: name }),
                }
            }
            gix::refs::Target::Object(commit) => Ok(Head::Detached { commit }),
        }
    }

    /// Read the snapshot a commit's tree holds.
    ///
    /// # Errors
    ///
    /// Returns [`ReadStateError`] when the commit, tree, or snapshot cannot
    /// be read, or the snapshot pins a foreign configuration.
    pub fn snapshot_at(&self, commit: ObjectId) -> Result<Snapshot, ReadStateError> {
        let tree = self.commit_tree(commit)?;
        Snapshot::read(self.repo, tree, Some(*self.store.config())).map_err(ReadStateError::from)
    }

    /// The snapshot of the branch `HEAD` names; the empty snapshot when
    /// `HEAD` is unborn.
    ///
    /// # Errors
    ///
    /// Returns [`ReadStateError`] when `HEAD` is missing or unreadable.
    pub fn head_snapshot(&self) -> Result<Snapshot, ReadStateError> {
        match self.head()?.commit() {
            Some(commit) => self.snapshot_at(commit),
            None if self.head()?.branch().is_some() => Ok(Snapshot::empty(*self.store.config())),
            None => Err(ReadStateError::NoDatabase),
        }
    }

    /// The working snapshot: `refs/db/working` when it exists, otherwise the
    /// branch tip's snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`ReadStateError`] when a ref, object, or snapshot cannot be
    /// read.
    pub fn working_state(&self) -> Result<WorkspaceState, ReadStateError> {
        self.state_at(workspace::WORKING_REF)
    }

    /// The staged snapshot: `refs/db/index` when it exists, otherwise the
    /// branch tip's snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`ReadStateError`] when a ref, object, or snapshot cannot be
    /// read.
    pub fn index_state(&self) -> Result<WorkspaceState, ReadStateError> {
        self.state_at(workspace::INDEX_REF)
    }

    fn state_at(&self, name: &str) -> Result<WorkspaceState, ReadStateError> {
        let name = RefName::new(name).map_err(ReadStateError::git)?;
        match read_ref(&self.refs, &name)? {
            Some(commit) => Ok(WorkspaceState {
                commit: Some(commit),
                snapshot: self.snapshot_at(commit)?,
            }),
            None => Ok(WorkspaceState {
                commit: None,
                snapshot: self.head_snapshot()?,
            }),
        }
    }

    fn commit_tree(&self, commit: ObjectId) -> Result<ObjectId, ReadStateError> {
        let mut buf = Vec::new();
        let data = self
            .repo
            .try_find(&commit, &mut buf)
            .map_err(ReadStateError::git)?
            .ok_or(ReadStateError::ObjectNotFound { oid: commit })?;
        if data.kind != Kind::Commit {
            return Err(ReadStateError::NotACommit { oid: commit });
        }
        let decoded =
            CommitRef::from_bytes(data.data, data.object_hash).map_err(ReadStateError::git)?;
        Ok(decoded.tree())
    }

    /// A table's root in the working snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TableNotFound`] when the table does not exist, and
    /// read errors when state cannot be read.
    pub fn table_root(&self, table: &str) -> Result<ObjectId, Error> {
        let state = self.working_state()?;
        state.snapshot.table(table).ok_or_else(|| {
            Error::TableNotFound(TableName::new(table).expect("name from snapshot is valid"))
        })
    }

    /// Create a table in the working snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TableExists`] when the table is already present, and
    /// write errors when the working snapshot cannot be published.
    pub fn create_table(&self, table: &str) -> Result<ObjectId, Error> {
        let name = TableName::new(table)?;
        let mut snapshot = self.working_state()?.snapshot;
        if snapshot.table(name.as_str()).is_some() {
            return Err(Error::TableExists(name));
        }
        let root = self.store.empty_root();
        snapshot.set_table(name.clone(), root);
        self.write_working(snapshot)?;
        Ok(root)
    }

    /// Drop a table from the working snapshot, returning its former root.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TableNotFound`] when the table does not exist.
    pub fn drop_table(&self, table: &str) -> Result<ObjectId, Error> {
        let name = TableName::new(table)?;
        let mut snapshot = self.working_state()?.snapshot;
        let root = snapshot
            .remove_table(name.as_str())
            .ok_or_else(|| Error::TableNotFound(name))?;
        self.write_working(snapshot)?;
        Ok(root)
    }

    /// Set `key` to `value` in `table`, in the working snapshot.
    ///
    /// The value is serialized to its own object graph exactly as
    /// [`ProllyStore::insert`] does.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TableNotFound`] when the table does not exist, and
    /// write errors when the working snapshot cannot be published.
    pub fn put(&self, table: &str, key: &[u8], value: &Value) -> Result<ObjectId, Error> {
        let name = TableName::new(table)?;
        let mut snapshot = self.working_state()?.snapshot;
        let root = snapshot
            .table(name.as_str())
            .ok_or_else(|| Error::TableNotFound(name.clone()))?;
        let new_root = self.store.insert(Some(root), key, value)?;
        snapshot.set_table(name, new_root);
        self.write_working(snapshot)?;
        Ok(new_root)
    }

    /// Set `key` to an already-written value object, in the working snapshot.
    ///
    /// # Errors
    ///
    /// As [`Database::put`], plus object-kind errors for `value_oid`.
    pub fn put_value_object(
        &self,
        table: &str,
        key: &[u8],
        value_oid: ObjectId,
    ) -> Result<ObjectId, Error> {
        let name = TableName::new(table)?;
        let mut snapshot = self.working_state()?.snapshot;
        let root = snapshot
            .table(name.as_str())
            .ok_or_else(|| Error::TableNotFound(name.clone()))?;
        let new_root = self.store.insert_value_object(Some(root), key, value_oid)?;
        snapshot.set_table(name, new_root);
        self.write_working(snapshot)?;
        Ok(new_root)
    }

    /// Remove `key` from `table`, in the working snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TableNotFound`] or [`Error::KeyNotFound`].
    pub fn remove(&self, table: &str, key: &[u8]) -> Result<ObjectId, Error> {
        let name = TableName::new(table)?;
        let mut snapshot = self.working_state()?.snapshot;
        let root = snapshot
            .table(name.as_str())
            .ok_or_else(|| Error::TableNotFound(name.clone()))?;
        let new_root = self.store.remove(root, key)?;
        snapshot.set_table(name, new_root);
        self.write_working(snapshot)?;
        Ok(new_root)
    }

    /// Look up a key's value in `table`'s working root.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TableNotFound`] or a Prolly error.
    pub fn get(&self, table: &str, key: &[u8]) -> Result<Option<Value>, Error> {
        let root = self.table_root(table)?;
        Ok(self.store.get(root, key)?)
    }

    /// Iterate a table's `(key, value)` pairs in key order, from the working
    /// root.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TableNotFound`] or a Prolly error.
    pub fn scan(
        &self,
        table: &str,
    ) -> Result<impl Iterator<Item = Result<(Vec<u8>, Value), git_prolly::Error>>, Error> {
        let root = self.table_root(table)?;
        self.store.iter(root).map_err(Error::from)
    }

    /// Stage a table: copy its working root into the index snapshot.
    ///
    /// When the table exists in the working snapshot, its root is staged;
    /// when it exists only in the index, its removal is staged — the same
    /// "add stages deletions" rule `git add` follows. Refused when the table
    /// exists in neither snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TableNotFound`] when the table exists in neither the
    /// working nor the index snapshot, and write errors when the index
    /// cannot be published.
    pub fn stage(&self, table: &str) -> Result<ObjectId, Error> {
        let name = TableName::new(table)?;
        let working = self.working_state()?;
        let mut snapshot = self.index_state()?.snapshot;
        match working.snapshot.table(name.as_str()) {
            Some(root) => {
                snapshot.set_table(name, root);
                self.write_index(snapshot)?;
                Ok(root)
            }
            None => {
                let root = snapshot
                    .remove_table(name.as_str())
                    .ok_or_else(|| Error::TableNotFound(name.clone()))?;
                self.write_index(snapshot)?;
                Ok(root)
            }
        }
    }

    /// Restore a table's working root from the staged (index) snapshot —
    /// [`Database::stage`]'s mirror, discarding unstaged changes to one
    /// table. A table added after staging is removed from the working
    /// snapshot again.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TableNotFound`] when the table exists in neither the
    /// index nor the working snapshot, and write errors when the working
    /// snapshot cannot be published.
    pub fn restore_table(&self, table: &str) -> Result<ObjectId, Error> {
        let name = TableName::new(table)?;
        let index = self.index_state()?;
        let mut snapshot = self.working_state()?.snapshot;
        let root = match index.snapshot.table(name.as_str()) {
            Some(root) => {
                snapshot.set_table(name.clone(), root);
                root
            }
            None => snapshot
                .remove_table(name.as_str())
                .ok_or_else(|| Error::TableNotFound(name.clone()))?,
        };
        self.write_working(snapshot)?;
        Ok(root)
    }

    /// Unstage a table: restore its index root from the branch tip —
    /// [`Database::stage`]'s inverse. A table absent from the tip is removed
    /// from the index again.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TableNotFound`] when the table exists in neither the
    /// branch tip nor the index, and write errors when the index cannot be
    /// published.
    pub fn unstage(&self, table: &str) -> Result<ObjectId, Error> {
        let name = TableName::new(table)?;
        let tip = self.head_snapshot()?;
        let mut snapshot = self.index_state()?.snapshot;
        let root = match tip.table(name.as_str()) {
            Some(root) => {
                snapshot.set_table(name.clone(), root);
                root
            }
            None => snapshot
                .remove_table(name.as_str())
                .ok_or_else(|| Error::TableNotFound(name.clone()))?,
        };
        self.write_index(snapshot)?;
        Ok(root)
    }

    /// Discard everything: move the working and index snapshots back to the
    /// branch tip in one compare-and-swap batch.
    ///
    /// # Errors
    ///
    /// Returns [`Error::State::NoDatabase`] when there is no branch tip, and
    /// write errors on a lost race.
    pub fn reset_hard(&self) -> Result<ObjectId, Error> {
        let commit = self
            .head()?
            .commit()
            .ok_or(Error::State(ReadStateError::NoDatabase))?;
        let working = RefName::new(workspace::WORKING_REF).expect("built-in ref name is valid");
        let index = RefName::new(workspace::INDEX_REF).expect("built-in ref name is valid");
        let edits = vec![
            cas_edit(&working, read_ref(&self.refs, &working)?, commit),
            cas_edit(&index, read_ref(&self.refs, &index)?, commit),
        ];
        self.refs.apply_batch(edits).map_err(|error| match error {
            gix_refstore::ApplyError::LostRace { name, .. } => {
                Error::Write(WriteStateError::Conflict(classify(&name)))
            }
            gix_refstore::ApplyError::Backend(error) => {
                Error::Write(WriteStateError::ref_backend(error))
            }
        })?;
        Ok(commit)
    }

    /// Rename a table in the working snapshot, keeping its rows.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TableNotFound`] when `from` does not exist and
    /// [`Error::TableExists`] when `to` does.
    pub fn move_table(&self, from: &str, to: &str) -> Result<ObjectId, Error> {
        let from_name = TableName::new(from)?;
        let to_name = TableName::new(to)?;
        let mut snapshot = self.working_state()?.snapshot;
        let root = snapshot
            .remove_table(from_name.as_str())
            .ok_or_else(|| Error::TableNotFound(from_name.clone()))?;
        if snapshot.table(to_name.as_str()).is_some() {
            return Err(Error::TableExists(to_name.clone()));
        }
        snapshot.set_table(to_name, root);
        self.write_working(snapshot)?;
        Ok(root)
    }

    /// Compare two snapshots table by table, row by row.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] when a snapshot or node cannot be read.
    pub fn diff_snapshots(
        &self,
        older: &Snapshot,
        newer: &Snapshot,
    ) -> Result<Vec<TableStatus>, Error> {
        let mut statuses = Vec::new();
        let empty = self.store.empty_root();
        let names: std::collections::BTreeSet<&str> = older
            .tables()
            .keys()
            .chain(newer.tables().keys())
            .map(TableName::as_str)
            .collect();
        for name in names {
            let old = older.table(name);
            let new = newer.table(name);
            if old == new {
                continue;
            }
            let (kind, rows) = match (old, new) {
                (Some(old), Some(new)) => (ChangeKind::Modified, self.store.diff(old, new)?),
                (None, Some(new)) => (ChangeKind::Added, self.store.diff(empty, new)?),
                (Some(old), None) => (ChangeKind::Removed, self.store.diff(old, empty)?),
                (None, None) => unreachable!("equal roots were skipped"),
            };
            statuses.push(TableStatus {
                table: TableName::new(name).expect("names from snapshots are valid"),
                kind,
                rows: rows.iter().map(row_change).collect(),
            });
        }
        Ok(statuses)
    }

    /// The staged relationship: branch tip versus index.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] when state cannot be read.
    pub fn status(&self) -> Result<Status, Error> {
        let head = self.head_snapshot()?;
        let index = self.index_state()?.snapshot;
        let working = self.working_state()?.snapshot;
        Ok(Status {
            staged: self.diff_snapshots(&head, &index)?,
            unstaged: self.diff_snapshots(&index, &working)?,
        })
    }

    /// Row-level diff between the branch tip and the index.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] when state cannot be read.
    pub fn diff_staged(&self) -> Result<Vec<TableStatus>, Error> {
        self.diff_snapshots(&self.head_snapshot()?, &self.index_state()?.snapshot)
    }

    /// Row-level diff between the index and the working snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] when state cannot be read.
    pub fn diff_working(&self) -> Result<Vec<TableStatus>, Error> {
        self.diff_snapshots(
            &self.index_state()?.snapshot,
            &self.working_state()?.snapshot,
        )
    }

    /// Commit the index snapshot onto the current branch.
    ///
    /// The branch, working, and index refs move as one compare-and-swap
    /// batch: a concurrent commit anywhere is detected, and nothing is
    /// partially published. On success the working and index snapshots reset
    /// to the new tip.
    ///
    /// # Errors
    ///
    /// Returns [`CommitError`] for a detached `HEAD` or a lost race; the
    /// caller re-reads state and retries.
    pub fn commit(&self, message: &str) -> Result<ObjectId, CommitError> {
        let head = self.head().map_err(|error| match error {
            ReadStateError::NoDatabase => CommitError::Detached,
            other => CommitError::Write(WriteStateError::ref_backend(other.to_string())),
        })?;
        let Some(branch) = head.branch().cloned() else {
            return Err(CommitError::Detached);
        };
        let index = self
            .index_state()
            .map_err(|error| CommitError::Write(WriteStateError::ref_backend(error.to_string())))?;
        let tree = index.snapshot.write(self.repo).map_err(CommitError::from)?;
        let parents = head.commit().into_iter().collect::<Vec<_>>();
        let commit = write_commit(self.repo, &self.refs, message, tree, &parents)
            .map_err(CommitError::from)?;
        let working = RefName::new(workspace::WORKING_REF).expect("built-in ref name is valid");
        let index_name = RefName::new(workspace::INDEX_REF).expect("built-in ref name is valid");
        let working_commit = self
            .working_state()
            .map_err(|error| CommitError::Write(WriteStateError::ref_backend(error.to_string())))?
            .commit;
        let edits = vec![
            cas_edit(&branch, head.commit(), commit),
            cas_edit(&working, working_commit, commit),
            cas_edit(&index_name, index.commit, commit),
        ];
        self.refs.apply_batch(edits).map_err(|error| match error {
            gix_refstore::ApplyError::LostRace { name, .. } => {
                CommitError::Write(WriteStateError::Conflict(classify(&name)))
            }
            gix_refstore::ApplyError::Backend(error) => {
                CommitError::Write(WriteStateError::ref_backend(error))
            }
        })?;
        Ok(commit)
    }

    /// Walk the current branch's commits, tip first.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] when a commit cannot be read.
    pub fn log(&self) -> Result<Vec<LogEntry>, Error> {
        let head = self.head()?;
        let Some(mut cursor) = head.commit() else {
            return Ok(Vec::new());
        };
        let mut entries = Vec::new();
        loop {
            let mut buf = Vec::new();
            let data = self
                .repo
                .try_find(&cursor, &mut buf)
                .map_err(Error::state_from_git)?
                .ok_or_else(|| Error::state_from_git(format!("object {cursor} not found")))?;
            let commit = CommitRef::from_bytes(data.data, data.object_hash)
                .map_err(Error::state_from_git)?;
            entries.push(LogEntry {
                commit: cursor,
                tree: commit.tree(),
                message: commit.message().summary().to_string(),
                parents: commit.parents().collect(),
            });
            match commit.parents().next() {
                Some(parent) => cursor = parent,
                None => break,
            }
        }
        Ok(entries)
    }

    /// Create a branch at the current tip.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BranchExists`] when the branch is taken, and write
    /// errors on a lost race or a ref-backend failure.
    pub fn create_branch(&self, branch: &str) -> Result<ObjectId, Error> {
        let name = branch_ref(branch)?;
        if read_ref(&self.refs, &name)?.is_some() {
            return Err(Error::BranchExists(branch.to_owned()));
        }
        let commit = self
            .head()?
            .commit()
            .ok_or_else(|| Error::BranchNotFound("no commit to branch from".to_owned()))?;
        apply_cas(
            &self.refs,
            cas_edit(&name, None, commit),
            CasConflict::Branch(branch.to_owned()),
        )?;
        Ok(commit)
    }

    /// Delete a branch that is not the current one, returning its tip.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BranchNotFound`] when the branch does not exist and
    /// [`Error::CurrentBranch`] when it is the branch `HEAD` names.
    pub fn delete_branch(&self, branch: &str) -> Result<ObjectId, Error> {
        let name = branch_ref(branch)?;
        let tip =
            read_ref(&self.refs, &name)?.ok_or_else(|| Error::BranchNotFound(branch.to_owned()))?;
        let current = self
            .head()?
            .branch()
            .is_some_and(|head| head.as_str() == name.as_str());
        if current {
            return Err(Error::CurrentBranch(branch.to_owned()));
        }
        self.apply_delete(&name, tip, CasConflict::Branch(branch.to_owned()))?;
        Ok(tip)
    }

    /// Rename a branch, keeping its tip, and retarget `HEAD` when it named
    /// the old branch.
    ///
    /// The new ref is created first and the old one deleted last, so an
    /// interrupted rename leaves an extra branch — never a missing one.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BranchNotFound`] when `from` does not exist and
    /// [`Error::BranchExists`] when `to` does.
    pub fn rename_branch(&self, from: &str, to: &str) -> Result<ObjectId, Error> {
        let from_ref = branch_ref(from)?;
        let to_ref = branch_ref(to)?;
        let tip = read_ref(&self.refs, &from_ref)?
            .ok_or_else(|| Error::BranchNotFound(from.to_owned()))?;
        if read_ref(&self.refs, &to_ref)?.is_some() {
            return Err(Error::BranchExists(to.to_owned()));
        }
        apply_cas(
            &self.refs,
            cas_edit(&to_ref, None, tip),
            CasConflict::Branch(to.to_owned()),
        )?;
        if self
            .head()?
            .branch()
            .is_some_and(|head| head.as_str() == from_ref.as_str())
        {
            self.set_head_symbolic(&to_ref)?;
        }
        self.apply_delete(&from_ref, tip, CasConflict::Branch(from.to_owned()))?;
        Ok(tip)
    }

    /// List the branch tips under `refs/db/heads`, sorted by branch name.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] when the ref store cannot be listed.
    pub fn list_branches(&self) -> Result<Vec<(String, ObjectId)>, Error> {
        self.list_under(crate::workspace::HEADS_PREFIX)
    }

    /// Create a lightweight tag at `commit`, or at the current tip when
    /// `commit` is `None`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TagExists`] when the tag is taken, and read errors
    /// when there is no commit to tag.
    pub fn create_tag(&self, tag: &str, commit: Option<ObjectId>) -> Result<ObjectId, Error> {
        let name = crate::workspace::tag_ref(tag)?;
        if read_ref(&self.refs, &name)?.is_some() {
            return Err(Error::TagExists(tag.to_owned()));
        }
        let commit = match commit {
            Some(commit) => commit,
            None => self
                .head()?
                .commit()
                .ok_or(Error::State(ReadStateError::NoDatabase))?,
        };
        apply_cas(
            &self.refs,
            cas_edit(&name, None, commit),
            CasConflict::Tag(tag.to_owned()),
        )?;
        Ok(commit)
    }

    /// Delete a lightweight tag, returning the commit it held.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TagNotFound`] when the tag does not exist.
    pub fn delete_tag(&self, tag: &str) -> Result<ObjectId, Error> {
        let name = crate::workspace::tag_ref(tag)?;
        let commit =
            read_ref(&self.refs, &name)?.ok_or_else(|| Error::TagNotFound(tag.to_owned()))?;
        self.apply_delete(&name, commit, CasConflict::Tag(tag.to_owned()))?;
        Ok(commit)
    }

    /// List the tags under `refs/db/tags`, sorted by tag name.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] when the ref store cannot be listed.
    pub fn list_tags(&self) -> Result<Vec<(String, ObjectId)>, Error> {
        self.list_under(crate::workspace::TAGS_PREFIX)
    }

    /// Delete a ref that currently holds `expected`, mapping a lost race to
    /// `conflict`.
    fn apply_delete(
        &self,
        name: &RefName,
        expected: ObjectId,
        conflict: CasConflict,
    ) -> Result<(), Error> {
        self.refs
            .apply(RefEdit::Delete {
                name: name.clone(),
                expected,
            })
            .map_err(|error| match error {
                gix_refstore::ApplyError::LostRace { .. } => {
                    Error::Write(WriteStateError::Conflict(conflict))
                }
                gix_refstore::ApplyError::Backend(error) => {
                    Error::Write(WriteStateError::ref_backend(error))
                }
            })
    }

    /// List every ref under a built-in prefix as `(name, oid)`, sorted by
    /// name.
    fn list_under(&self, prefix: &str) -> Result<Vec<(String, ObjectId)>, Error> {
        let prefix =
            gix_refstore::RefPrefix::try_from(prefix).expect("built-in ref prefixes are valid");
        let mut out: Vec<(String, ObjectId)> = self
            .refs
            .prefixed(&prefix)
            .map_err(|error| Error::state_from_git(error.to_string()))?
            .into_iter()
            .filter_map(|(name, oid)| {
                name.relative_to(&prefix)
                    .map(|path| (path.to_string(), oid))
            })
            .collect();
        out.sort();
        Ok(out)
    }

    /// Switch the database to `branch`.
    ///
    /// Refused when the working snapshot differs from the current branch tip
    /// unless `force`, and when the branch does not exist. On success `HEAD`
    /// names the branch and the working and index snapshots reset to its tip.
    ///
    /// # Errors
    ///
    /// Returns [`CheckoutError`] for a dirty working snapshot or an unknown
    /// branch, and write errors on a lost race.
    pub fn checkout(&self, branch: &str, force: bool) -> Result<ObjectId, Error> {
        let head = self.head()?;
        if head.branch().is_none() {
            return Err(Error::Checkout(CheckoutError::Detached));
        }
        let target = branch_ref(branch)?;
        let Some(commit) = read_ref(&self.refs, &target)? else {
            return Err(Error::Checkout(CheckoutError::UnknownBranch(
                branch.to_owned(),
            )));
        };
        if !force {
            let tip = match head.commit() {
                Some(tip) => self.snapshot_at(tip)?,
                None => Snapshot::empty(*self.store.config()),
            };
            let working = self.working_state()?.snapshot;
            let working_tree = working.write(self.repo).map_err(Error::from)?;
            let tip_tree = tip.write(self.repo).map_err(Error::from)?;
            if working_tree != tip_tree {
                return Err(Error::Checkout(CheckoutError::Dirty {
                    branch: branch.to_owned(),
                }));
            }
        }
        let working = RefName::new(workspace::WORKING_REF).expect("built-in ref name is valid");
        let index = RefName::new(workspace::INDEX_REF).expect("built-in ref name is valid");
        let edits = vec![
            cas_edit(&working, read_ref(&self.refs, &working)?, commit),
            cas_edit(&index, read_ref(&self.refs, &index)?, commit),
        ];
        self.refs.apply_batch(edits).map_err(|error| match error {
            gix_refstore::ApplyError::LostRace { name, .. } => {
                Error::Write(WriteStateError::Conflict(classify(&name)))
            }
            gix_refstore::ApplyError::Backend(error) => {
                Error::Write(WriteStateError::ref_backend(error))
            }
        })?;
        self.set_head_symbolic(&target)?;
        Ok(commit)
    }

    /// Merge `branch` into the current branch.
    ///
    /// Finds the merge base, merges row by row, and either publishes an
    /// ordinary two-parent commit or refuses with explicit conflicts. A
    /// refused merge writes nothing.
    ///
    /// # Errors
    ///
    /// Returns [`MergeError::Conflicts`] with the conflicting rows, or other
    /// errors for missing history or a lost race.
    pub fn merge(&self, branch: &str) -> Result<ObjectId, MergeError> {
        let head = self.head()?;
        let ours = head
            .commit()
            .ok_or(MergeError::State(ReadStateError::NoDatabase))?;
        let target = branch_ref(branch)
            .map_err(|error| MergeError::State(ReadStateError::git(error.to_string())))?;
        let theirs =
            read_ref(&self.refs, &target)?.ok_or(MergeError::BranchNotFound(branch.to_owned()))?;
        let base = self
            .repo
            .merge_base(ours, theirs)
            .map_err(|_| MergeError::NoMergeBase { ours, theirs })?;
        let base_snapshot = self.snapshot_at(base.detach()).map_err(MergeError::State)?;
        let ours_snapshot = self.snapshot_at(ours).map_err(MergeError::State)?;
        let theirs_snapshot = self.snapshot_at(theirs).map_err(MergeError::State)?;
        let Merged {
            snapshot,
            conflicts,
        } = merge_snapshots(
            &self.store,
            &base_snapshot,
            &ours_snapshot,
            &theirs_snapshot,
        )?;
        if !conflicts.is_empty() {
            return Err(MergeError::Conflicts { conflicts });
        }
        let tree = snapshot
            .write(self.repo)
            .map_err(|error| MergeError::Write(error.into()))?;
        let commit = write_commit(
            self.repo,
            &self.refs,
            &format!("merge branch `{branch}`"),
            tree,
            &[ours, theirs],
        )
        .map_err(MergeError::Write)?;
        let branch_name = head
            .branch()
            .cloned()
            .ok_or(MergeError::State(ReadStateError::NoDatabase))?;
        let working = RefName::new(workspace::WORKING_REF).expect("built-in ref name is valid");
        let index = RefName::new(workspace::INDEX_REF).expect("built-in ref name is valid");
        let edits = vec![
            cas_edit(&branch_name, Some(ours), commit),
            cas_edit(&working, read_ref(&self.refs, &working)?, commit),
            cas_edit(&index, read_ref(&self.refs, &index)?, commit),
        ];
        self.refs.apply_batch(edits).map_err(|error| match error {
            gix_refstore::ApplyError::LostRace { name, .. } => {
                MergeError::Write(WriteStateError::Conflict(classify(&name)))
            }
            gix_refstore::ApplyError::Backend(error) => {
                MergeError::Write(WriteStateError::ref_backend(error))
            }
        })?;
        Ok(commit)
    }

    fn write_working(&self, snapshot: Snapshot) -> Result<ObjectId, WriteStateError> {
        self.write_state(workspace::WORKING_REF, snapshot, CasConflict::Working)
    }

    fn write_index(&self, snapshot: Snapshot) -> Result<ObjectId, WriteStateError> {
        self.write_state(workspace::INDEX_REF, snapshot, CasConflict::Index)
    }

    fn write_state(
        &self,
        name: &str,
        snapshot: Snapshot,
        conflict: CasConflict,
    ) -> Result<ObjectId, WriteStateError> {
        let tree = snapshot.write(self.repo)?;
        let parent = self
            .head()
            .map_err(|error| WriteStateError::ref_backend(error.to_string()))?
            .commit();
        let commit = write_commit(
            self.repo,
            &self.refs,
            "database state",
            tree,
            &parent.into_iter().collect::<Vec<_>>(),
        )?;
        let name = RefName::new(name).map_err(|error| WriteStateError::git(error.to_string()))?;
        let current = read_ref(&self.refs, &name)?;
        apply_cas(&self.refs, cas_edit(&name, current, commit), conflict)?;
        Ok(commit)
    }

    fn set_head_symbolic(&self, target: &RefName) -> Result<(), Error> {
        let edit = gix::refs::transaction::RefEdit {
            change: gix::refs::transaction::Change::Update {
                log: gix::refs::transaction::LogChange {
                    mode: gix::refs::transaction::RefLog::AndReference,
                    force_create_reflog: false,
                    message: format!("checkout {}", target.as_str()).into(),
                },
                expected: gix::refs::transaction::PreviousValue::Any,
                new: gix::refs::Target::Symbolic(
                    gix::refs::FullName::try_from(target.as_str().to_string())
                        .expect("a valid RefName is a valid FullName"),
                ),
            },
            name: gix::refs::FullName::try_from(HEAD_REF).expect("built-in ref name is valid"),
            deref: false,
        };
        self.repo
            .edit_reference(edit)
            .map_err(Error::state_from_git)?;
        Ok(())
    }
}

/// Which CAS conflict a ref name belongs to.
fn classify(name: &RefName) -> CasConflict {
    match name.as_str().as_bytes() {
        b"refs/db/working" => CasConflict::Working,
        b"refs/db/index" => CasConflict::Index,
        other => CasConflict::Branch(
            String::from_utf8_lossy(other.strip_prefix(b"refs/db/heads/").unwrap_or(other))
                .into_owned(),
        ),
    }
}

/// One commit of the database's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// The commit object.
    pub commit: ObjectId,
    /// The snapshot tree the commit holds.
    pub tree: ObjectId,
    /// The commit message, verbatim.
    pub message: String,
    /// The parents, first parent first.
    pub parents: Vec<ObjectId>,
}

impl Status {
    /// Whether nothing is staged and the working snapshot matches the index.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.staged.is_empty() && self.unstaged.is_empty()
    }
}
