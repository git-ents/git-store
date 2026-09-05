//! A versioned database experience on top of ordinary Git objects, refs, and
//! commits.
//!
//! The storage substrate is Git: a database **snapshot** is a canonical Git
//! tree naming each table's Prolly root, a table root is the root object id of
//! a [`git-prolly`] tree, and a database commit is an ordinary Git commit
//! whose tree is a snapshot. There is no second commit or ref implementation;
//! publication goes through compare-and-swap ref edits, so two writers never
//! silently overwrite one another.
//!
//! # Snapshot format
//!
//! A snapshot tree holds exactly one reserved metadata blob named
//! [`METADATA_NAME`] plus one tree entry per table:
//!
//! ```text
//! 100644 !database            format identity and Prolly configuration
//! 040000 <table-name>         <table's Prolly root tree>
//! ```
//!
//! The metadata blob's first line is the format identity (`git-store-database
//! v1`); the second pins the [git_prolly::ProllyConfig] the snapshot's table roots were
//! built with. Table names are validated UTF-8, contain no `/` or ASCII
//! whitespace, never start with `!` (the reserved prefix), and are never `.`
//! or `..`. An empty database is a snapshot with zero tables — a tree
//! containing only the metadata blob — so any snapshot is inspectable with
//! `git ls-tree`, and an unborn branch (no snapshot at all) is the empty
//! database.
//!
//! # Workspace model
//!
//! Repository state lives in refs under [`REF_PREFIX`]:
//!
//! ```text
//! refs/db/HEAD            symbolic ref naming the current branch
//! refs/db/heads/<branch>  branch tips; ordinary Git history
//! refs/db/index           the staged snapshot, as a commit
//! refs/db/working         the working snapshot, as a commit
//! ```
//!
//! `index` and `working` are repository-local, not branch-local; they are
//! commits parented on the current branch tip, which keeps every published
//! snapshot reachable for `git fsck` while it exists. Every write is a CAS on
//! the ref it advances, classified as a [`CasConflict`] on a lost race.
//!
//! # Example
//!
//! ```
//! use facet_value::Value;
//! use gix_database::Database;
//!
//! let dir = tempfile::TempDir::new()?;
//! let repo = gix::init(dir.path())?;
//! let db = Database::open(&repo);
//! db.init()?;
//!
//! db.create_table("users")?;
//! db.put("users", b"alice", &Value::from("42"))?;
//! db.stage("users")?;
//! let commit = db.commit("seed users")?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! [`git-prolly`]: ../git_prolly/index.html

#![forbid(unsafe_code)]

mod database;
mod error;
mod format;
mod merge;
mod snapshot;
mod workspace;

pub use database::{Database, LogEntry};
pub use error::{
    CasConflict, CheckoutError, CommitError, ConflictEntry, ConflictKind, Error, MergeError,
    ReadStateError, SnapshotError, TableNameError, WriteStateError,
};
pub use format::{DEFAULT_BRANCH, METADATA_NAME, REF_PREFIX, SNAPSHOT_FORMAT_LINE, TableName};
pub use merge::{Merged, merge_snapshots};
pub use snapshot::{Snapshot, metadata_bytes};
pub use workspace::{
    ChangeKind, HEAD_REF, HEADS_PREFIX, Head, INDEX_REF, RowChange, Status, TAGS_PREFIX,
    TableStatus, WORKING_REF, WorkspaceState,
};
