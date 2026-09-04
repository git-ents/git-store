//! Repository database state: the ref namespace, `HEAD`, and the staged and
//! working snapshots.

use git_prolly::DiffEntry;
use gix::ObjectId;
use gix::objs::Commit;
use gix_refstore::{ApplyError, Committer, GixRefStore, RefEdit, RefName, RefStore};

use crate::error::{CasConflict, ReadStateError, WriteStateError};
use crate::format::TableName;
use crate::snapshot::Snapshot;

/// The symbolic ref naming the current branch: `refs/db/HEAD`.
pub const HEAD_REF: &str = "refs/db/HEAD";
/// The prefix holding branch tips: `refs/db/heads`.
pub const HEADS_PREFIX: &str = "refs/db/heads";
/// The ref holding the staged snapshot: `refs/db/index`.
pub const INDEX_REF: &str = "refs/db/index";
/// The ref holding the working snapshot: `refs/db/working`.
pub const WORKING_REF: &str = "refs/db/working";

/// The resolved database `HEAD`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Head {
    /// `refs/db/HEAD` does not exist: `init` has not run here.
    Missing,
    /// `HEAD` names a branch that has no commits yet: the empty database.
    Unborn {
        /// The branch `HEAD` names.
        branch: RefName,
    },
    /// `HEAD` names a branch at a commit.
    Branch {
        /// The branch `HEAD` names.
        branch: RefName,
        /// The branch tip.
        commit: ObjectId,
    },
    /// `HEAD` points directly at a commit, with no branch behind it.
    Detached {
        /// The commit `HEAD` points at.
        commit: ObjectId,
    },
}

impl Head {
    /// The branch tip, when `HEAD` names a born branch.
    #[must_use]
    pub fn commit(&self) -> Option<ObjectId> {
        match self {
            Self::Branch { commit, .. } | Self::Detached { commit } => Some(*commit),
            Self::Unborn { .. } | Self::Missing => None,
        }
    }

    /// The branch `HEAD` names, when it names one.
    #[must_use]
    pub fn branch(&self) -> Option<&RefName> {
        match self {
            Self::Unborn { branch } | Self::Branch { branch, .. } => Some(branch),
            Self::Detached { .. } | Self::Missing => None,
        }
    }
}

/// A snapshot as anchored by one ref: the ref's commit and the snapshot its
/// tree holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceState {
    /// The commit the ref points at, when the ref exists.
    pub commit: Option<ObjectId>,
    /// The snapshot that commit's tree holds; empty when the ref is absent.
    pub snapshot: Snapshot,
}

/// One key-level row change within a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowChange {
    /// The changed key.
    pub key: Vec<u8>,
    /// What happened to the key.
    pub kind: ChangeKind,
    /// The value object before the change, for removals and modifications.
    pub old: Option<ObjectId>,
    /// The value object after the change, for insertions and modifications.
    pub new: Option<ObjectId>,
}

impl RowChange {
    /// Name the change the way status and diff output do.
    #[must_use]
    pub fn verb(&self) -> &'static str {
        match self.kind {
            ChangeKind::Added => "added",
            ChangeKind::Removed => "removed",
            ChangeKind::Modified => "modified",
        }
    }
}

/// How a table or row differs between two snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// Present only in the newer side.
    Added,
    /// Present only in the older side.
    Removed,
    /// Present on both sides with different content.
    Modified,
}

/// One table's difference between two snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableStatus {
    /// The table that differs.
    pub table: TableName,
    /// Whether the table itself was added, removed, or modified.
    pub kind: ChangeKind,
    /// The row-level changes, in key order. Empty when only the table's
    /// presence changed.
    pub rows: Vec<RowChange>,
}

/// The relationship between `HEAD`, the index, and the working snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Status {
    /// Changes between the branch tip and the index.
    pub staged: Vec<TableStatus>,
    /// Changes between the index and the working snapshot.
    pub unstaged: Vec<TableStatus>,
}

/// Convert a Prolly diff entry into a row change.
#[must_use]
pub fn row_change(entry: &DiffEntry) -> RowChange {
    match entry {
        DiffEntry::Insert { key, new } => RowChange {
            key: key.clone(),
            kind: ChangeKind::Added,
            old: None,
            new: Some(*new),
        },
        DiffEntry::Delete { key, old } => RowChange {
            key: key.clone(),
            kind: ChangeKind::Removed,
            old: Some(*old),
            new: None,
        },
        DiffEntry::Modify { key, old, new } => RowChange {
            key: key.clone(),
            kind: ChangeKind::Modified,
            old: Some(*old),
            new: Some(*new),
        },
    }
}

/// Read `name` through the ref store.
pub(crate) fn read_ref(
    refs: &GixRefStore<'_>,
    name: &RefName,
) -> Result<Option<ObjectId>, ReadStateError> {
    refs.read(name).map_err(ReadStateError::ref_backend)
}

/// Build the CAS edit that moves `name` from `current` to `new`.
pub(crate) fn cas_edit(name: &RefName, current: Option<ObjectId>, new: ObjectId) -> RefEdit {
    match current {
        Some(expected) => RefEdit::Update {
            name: name.clone(),
            expected,
            new,
        },
        None => RefEdit::Create {
            name: name.clone(),
            new,
        },
    }
}

/// Apply one CAS edit, mapping a lost race to `conflict`.
pub(crate) fn apply_cas(
    refs: &GixRefStore<'_>,
    edit: RefEdit,
    conflict: CasConflict,
) -> Result<(), WriteStateError> {
    refs.apply(edit).map_err(|error| match error {
        ApplyError::LostRace { .. } => WriteStateError::Conflict(conflict),
        ApplyError::Backend(error) => WriteStateError::ref_backend(error),
    })
}

/// Write a commit object whose tree is `tree`, parented on `parents`.
pub(crate) fn write_commit(
    repo: &gix::Repository,
    refs: &GixRefStore<'_>,
    message: &str,
    tree: ObjectId,
    parents: &[ObjectId],
) -> Result<ObjectId, WriteStateError> {
    if message.trim().is_empty() {
        return Err(WriteStateError::git("commit message may not be empty"));
    }
    let commit = Commit {
        tree,
        parents: parents.to_vec().into(),
        author: refs.author().map_err(WriteStateError::ref_backend)?,
        committer: refs.signature().map_err(WriteStateError::ref_backend)?,
        encoding: None,
        message: message.to_owned().into(),
        extra_headers: Vec::new(),
    };
    repo.write_object(&commit)
        .map(|id| id.detach())
        .map_err(WriteStateError::git)
}

/// A branch's full ref name, validated.
///
/// # Errors
///
/// Returns the ref-store's name validation error for an unusable branch name.
pub fn branch_ref(branch: &str) -> Result<RefName, gix_refstore::InvalidRefName> {
    RefName::new(format!("{HEADS_PREFIX}/{branch}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::WriteStateError;
    use gix::objs::Write as _;

    #[test]
    fn a_stale_cas_is_classified_not_swallowed() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        test_support::init_repo(dir.path());
        let repo = gix::open(dir.path()).expect("open repo");
        let refs = GixRefStore::new(&repo);
        let name = RefName::new(WORKING_REF).expect("built-in ref name is valid");
        let oid = repo
            .write_buf(gix::objs::Kind::Blob, b"content")
            .expect("blob");

        // A fresh create applies; a second create races with the first.
        refs.apply(cas_edit(&name, None, oid))
            .expect("first create applies");
        match apply_cas(&refs, cas_edit(&name, None, oid), CasConflict::Working) {
            Err(WriteStateError::Conflict(CasConflict::Working)) => {}
            other => panic!("expected a classified working conflict, got {other:?}"),
        }
    }
}
