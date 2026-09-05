//! Errors for the database layer, each distinguishing a failure a caller can
//! act on.

use git_prolly::ProllyConfig;
use gix::ObjectId;

use crate::format::{TableName, encode_config};

/// A snapshot tree is not a database snapshot this crate can read.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// The reserved metadata entry is missing, so the tree is not a snapshot.
    #[error("tree {tree} has no `{name}` metadata entry and is not a database snapshot")]
    MetadataMissing {
        /// The tree that was read.
        tree: ObjectId,
        /// The reserved metadata entry name.
        name: &'static str,
    },
    /// The metadata entry exists but is not a blob.
    #[error("snapshot metadata `{name}` in tree {tree} is a {kind}, not a blob")]
    MetadataNotBlob {
        /// The snapshot tree.
        tree: ObjectId,
        /// The reserved metadata entry name.
        name: &'static str,
        /// The kind actually found.
        kind: &'static str,
    },
    /// The metadata blob's format line names an unknown format or version.
    #[error("unknown snapshot format line {0:?}")]
    UnknownFormat(Vec<u8>),
    /// The metadata blob's Prolly configuration line is malformed.
    #[error("malformed snapshot metadata line: {0:?}")]
    MalformedMetadata(String),
    /// The metadata blob pins a configuration that cannot build a tree.
    #[error("snapshot pins an invalid Prolly configuration: {0}")]
    InvalidConfig(String),
    /// A snapshot's configuration differs from the one this database reads
    /// and writes with; its table roots may not be compatible.
    #[error(
        "snapshot configuration ({found}) differs from the database's ({expected}); refusing to mix configurations"
    )]
    ConfigMismatch {
        /// The configuration the snapshot pins, rendered.
        found: String,
        /// The configuration this database uses, rendered.
        expected: String,
    },
    /// A snapshot table entry is not a tree.
    #[error("snapshot table `{table}` in tree {tree} is a {kind}, not a tree")]
    TableNotTree {
        /// The snapshot tree.
        tree: ObjectId,
        /// The table name as found in the tree.
        table: String,
        /// The kind actually found.
        kind: &'static str,
    },
    /// A snapshot table entry's name is not a valid table name.
    #[error("snapshot tree {tree} holds an invalid table name: {source}")]
    TableName {
        /// The snapshot tree.
        tree: ObjectId,
        /// The validation failure.
        #[source]
        source: TableNameError,
    },
    /// A referenced object is absent from the repository.
    #[error("object {oid} not found in the repository")]
    ObjectNotFound {
        /// The absent object.
        oid: ObjectId,
    },
    /// An underlying Git object-database operation failed.
    #[error("git object operation failed: {0}")]
    Git(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
}

/// A table name is not representable in the snapshot format.
#[derive(Debug, thiserror::Error)]
pub enum TableNameError {
    /// Empty names are unrepresentable.
    #[error("table names may not be empty")]
    Empty,
    /// Git tree-entry names are bounded.
    #[error("table name is {0} bytes long, above the 255-byte Git tree-entry name limit")]
    TooLong(usize),
    /// `!` prefixes are reserved for snapshot metadata.
    #[error("table name {0:?} starts with the reserved `!` prefix")]
    Reserved(String),
    /// `/` would be read as a directory separator in the snapshot tree.
    #[error("table name {0:?} may not contain `/`")]
    Slash(String),
    /// Whitespace makes table names ambiguous in CLI output.
    #[error("table name {0:?} may not contain ASCII whitespace")]
    Whitespace(String),
    /// Tree-entry names must be valid UTF-8 to be table names.
    #[error("table names must be valid UTF-8")]
    NotUtf8,
}

impl TableNameError {
    /// The error for a tree-entry name that is not valid UTF-8.
    pub(crate) fn not_utf8() -> Self {
        Self::NotUtf8
    }
}

impl From<TableNameError> for SnapshotError {
    fn from(source: TableNameError) -> Self {
        Self::TableName {
            tree: ObjectId::empty_tree(gix::hash::Kind::Sha1),
            source,
        }
    }
}

/// Why a database ref write did not publish.
///
/// The classification is stable: scripts match on it instead of on text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CasConflict {
    /// `refs/db/working` moved under the caller: another process wrote to the
    /// working snapshot. Re-read state and retry.
    #[error("the working snapshot changed concurrently; nothing was written")]
    Working,
    /// `refs/db/index` moved under the caller.
    #[error("the index snapshot changed concurrently; nothing was written")]
    Index,
    /// The branch ref moved under the caller: another process committed.
    #[error("branch {0} moved concurrently; nothing was written")]
    Branch(String),
    /// A tag ref moved under the caller.
    #[error("tag {0} moved concurrently; nothing was written")]
    Tag(String),
}

/// Everything wrong a state read can be.
#[derive(Debug, thiserror::Error)]
pub enum ReadStateError {
    /// `refs/db/HEAD` does not exist: `init` has not run in this repository.
    #[error("no database here; run `init` first")]
    NoDatabase,
    /// The snapshot referenced by a database commit is unreadable.
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
    /// A referenced object is absent from the repository.
    #[error("object {oid} not found in the repository")]
    ObjectNotFound {
        /// The absent object.
        oid: ObjectId,
    },
    /// An object expected to be a commit is not one.
    #[error("object {oid} is not a commit")]
    NotACommit {
        /// The offending object.
        oid: ObjectId,
    },
    /// A referenced object is absent from the repository.
    #[error("git object operation failed: {0}")]
    Git(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
    /// The ref backend failed for a reason retrying will not fix.
    #[error("ref store failed: {0}")]
    Ref(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
}

/// A write to working, index, or branch state failed.
#[derive(Debug, thiserror::Error)]
pub enum WriteStateError {
    /// The write lost its compare-and-swap race; nothing was published.
    #[error(transparent)]
    Conflict(#[from] CasConflict),
    /// The snapshot produced by the write is unreadable.
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
    /// A referenced object is absent from the repository.
    #[error("object {oid} not found in the repository")]
    ObjectNotFound {
        /// The absent object.
        oid: ObjectId,
    },
    /// A commit write failed.
    #[error("git object operation failed: {0}")]
    Git(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
    /// The ref backend failed for a reason retrying will not fix.
    #[error("ref store failed: {0}")]
    Ref(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
}

/// A top-level database operation failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The requested table does not exist in the relevant snapshot.
    #[error("table `{0}` does not exist")]
    TableNotFound(TableName),
    /// A create targeted a table name already present.
    #[error("table `{0}` already exists")]
    TableExists(TableName),
    /// A removal targeted a key the table does not contain.
    #[error("key {0:?} not found in table")]
    KeyNotFound(Vec<u8>),
    /// Repository state is wrong for the requested operation.
    #[error(transparent)]
    State(#[from] ReadStateError),
    /// A state write failed.
    #[error(transparent)]
    Write(#[from] WriteStateError),
    /// A Prolly tree operation failed.
    #[error("prolly operation failed: {0}")]
    Prolly(#[from] git_prolly::Error),
    /// A snapshot was unreadable.
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
    /// A database commit was refused.
    #[error(transparent)]
    Commit(#[from] CommitError),
    /// A merge was requested and refused; nothing was written.
    #[error(transparent)]
    Merge(#[from] MergeError),
    /// A checkout was requested and refused; nothing was written.
    #[error(transparent)]
    Checkout(#[from] CheckoutError),
    /// A branch or tag name failed validation.
    #[error("invalid branch or tag name: {0}")]
    BranchName(#[from] gix_refstore::InvalidRefName),
    /// A table name is not representable in the snapshot format.
    #[error(transparent)]
    InvalidTable(#[from] TableNameError),
    /// The named branch does not exist.
    #[error("branch `{0}` does not exist")]
    BranchNotFound(String),
    /// A create targeted a branch name already present.
    #[error("branch `{0}` already exists")]
    BranchExists(String),
    /// A deletion targeted the branch that is currently checked out.
    #[error("cannot delete branch `{0}`: it is the current branch")]
    CurrentBranch(String),
    /// A deletion targeted a branch whose commits no other ref reaches.
    #[error("branch `{0}` has unmerged commits; delete with force to discard them")]
    BranchNotMerged(String),
    /// The database is initialized but holds no commits, so the operation
    /// has nothing to act on.
    #[error("the database has no commits yet")]
    EmptyDatabase,
    /// The named tag does not exist.
    #[error("tag `{0}` does not exist")]
    TagNotFound(String),
    /// A create targeted a tag name already present.
    #[error("tag `{0}` already exists")]
    TagExists(String),
}
/// A merge was refused because it cannot be completed without a decision.
#[derive(Debug, thiserror::Error)]
pub enum MergeError {
    /// The two commits have no common ancestor.
    #[error("no merge base between {ours} and {theirs}")]
    NoMergeBase {
        /// The current branch's commit.
        ours: ObjectId,
        /// The commit being merged in.
        theirs: ObjectId,
    },
    /// Same-key changes on both sides disagree; nothing was written.
    #[error("merge refused: {} conflicting row(s); nothing was written", conflicts.len())]
    Conflicts {
        /// Every conflicting table entry, in table then key order.
        conflicts: Vec<ConflictEntry>,
    },
    /// The branch named in the merge does not exist.
    #[error("branch `{0}` does not exist")]
    BranchNotFound(String),
    /// Reading a side's snapshot failed.
    #[error(transparent)]
    State(#[from] ReadStateError),
    /// Publishing the merged snapshot failed.
    #[error(transparent)]
    Write(#[from] WriteStateError),
    /// A Prolly tree operation failed.
    #[error("prolly operation failed: {0}")]
    Prolly(#[from] git_prolly::Error),
}

/// One conflicting row in a refused merge.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ConflictEntry {
    /// The table holding the conflict.
    pub table: TableName,
    /// The conflicting key.
    pub key: Vec<u8>,
    /// What the two sides did to the key.
    pub kind: ConflictKind,
}

impl ConflictEntry {
    /// The human-facing one-line rendering used in [`MergeError::Conflicts`].
    #[must_use]
    pub fn describe(&self) -> String {
        let detail = match &self.kind {
            ConflictKind::OursDeleted { theirs } => format!("ours deleted, theirs set {theirs}"),
            ConflictKind::TheirsDeleted { ours } => format!("theirs deleted, ours set {ours}"),
            ConflictKind::DifferentValues { ours, theirs } => {
                format!("ours set {ours}, theirs set {theirs}")
            }
        };
        format!(
            "  {} {} ({})",
            self.table,
            String::from_utf8_lossy(&self.key),
            detail
        )
    }
}

impl std::fmt::Display for ConflictEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.describe())
    }
}

/// What the two sides of a merge did to the same key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConflictKind {
    /// Our side deleted the key; their side set it to `theirs`.
    OursDeleted {
        /// Their side's value object.
        theirs: ObjectId,
    },
    /// Their side deleted the key; our side set it to `ours`.
    TheirsDeleted {
        /// Our side's value object.
        ours: ObjectId,
    },
    /// Both sides set the key, to different values.
    DifferentValues {
        /// Our side's value object.
        ours: ObjectId,
        /// Their side's value object.
        theirs: ObjectId,
    },
}

impl ConflictKind {
    /// Our side's value object, when we set one.
    #[must_use]
    pub const fn ours_oid(&self) -> Option<ObjectId> {
        match self {
            Self::OursDeleted { .. } => None,
            Self::TheirsDeleted { ours } | Self::DifferentValues { ours, .. } => Some(*ours),
        }
    }

    /// Their side's value object, when they set one.
    #[must_use]
    pub const fn theirs_oid(&self) -> Option<ObjectId> {
        match self {
            Self::TheirsDeleted { .. } => None,
            Self::OursDeleted { theirs } | Self::DifferentValues { theirs, .. } => Some(*theirs),
        }
    }
}

/// A checkout was refused.
#[derive(Debug, thiserror::Error)]
pub enum CheckoutError {
    /// The working snapshot differs from the current branch tip; checking
    /// out would discard it. Pass a force flag (or commit first) to proceed.
    #[error("working snapshot differs from `{branch}`'s tip; commit, reset, or force the checkout")]
    Dirty {
        /// The branch whose tip the working snapshot was compared against.
        branch: String,
    },
    /// The target branch ref does not exist.
    #[error("branch `{0}` does not exist")]
    UnknownBranch(String),
    /// `HEAD` points directly at a commit; a checkout needs a branch to
    /// leave.
    #[error("HEAD is detached; force or resolve the detached state first")]
    Detached,
}

/// A database commit was refused.
#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    /// `HEAD` is detached; commits need a branch to advance.
    #[error("HEAD is detached; a database commit needs a branch")]
    Detached,
    /// The index snapshot's configuration cannot be written.
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
    /// Publishing the commit failed.
    #[error(transparent)]
    Write(#[from] WriteStateError),
}

/// Render a [git_prolly::ProllyConfig] the way metadata and error messages do.
#[must_use]
pub fn config_summary(config: &ProllyConfig) -> String {
    encode_config(*config)
}

impl Error {
    pub(crate) fn state_from_git<E>(error: E) -> Self
    where
        E: Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    {
        Self::State(ReadStateError::git(error))
    }
}

impl From<ReadStateError> for WriteStateError {
    fn from(source: ReadStateError) -> Self {
        Self::ref_backend(source)
    }
}

impl SnapshotError {
    pub(crate) fn git<E>(error: E) -> Self
    where
        E: Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    {
        Self::Git(error.into())
    }
}

impl ReadStateError {
    pub(crate) fn git<E>(error: E) -> Self
    where
        E: Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    {
        Self::Git(error.into())
    }

    pub(crate) fn ref_backend<E>(error: E) -> Self
    where
        E: Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    {
        Self::Ref(error.into())
    }
}

impl WriteStateError {
    pub(crate) fn git<E>(error: E) -> Self
    where
        E: Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    {
        Self::Git(error.into())
    }

    pub(crate) fn ref_backend<E>(error: E) -> Self
    where
        E: Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    {
        Self::Ref(error.into())
    }
}
