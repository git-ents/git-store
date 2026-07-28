//! Store *anything* in Git as a real tree the stock plumbing can read.
//!
//! A kind is defined by publishing a [`SchemaDoc`] to `refs/schema/<kind>`;
//! entities of that kind live as commit chains at `refs/store/<kind>/<name>`.
//! Each data commit's tree is a two-way split, `{schema/, value/}`: `value/`
//! is the schema-directed encoding [`facet-git-tree`](facet_git_tree)
//! produces — not a blob of JSON, but one Git tree entry per field — and
//! `schema/` is the very tree `refs/schema/<kind>` points at, so the schema a
//! value was written against is reachable from the value's own commit and
//! travels with it through any fetch. Every write is a commit; history is the
//! audit trail. See the crate `README` for the `git ls-tree`/`git log` demo.
//!
//! [`Store`] is oid-in/oid-out over a `gix` repository; JSON belongs only at a
//! CLI boundary, never here.
#![forbid(unsafe_code)]

mod error;
mod refname;
mod store;

pub use error::Error;
pub use facet_git_tree::{ObjectId, SchemaDoc, schema_of};
pub use store::Store;
