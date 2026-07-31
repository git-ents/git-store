//! Store structured values in Git with versioned schemas.
//!
//! Values remain readable against the schema bound to each stored version, and
//! schema migrations are applied when values are read without rewriting the
//! stored objects. [`Store`] works with configurable ref and object backends;
//! [`RepoStore`] provides the integration for a `gix::Repository`.
//!
//! [`tree`] documents the tree format used by this crate. [`Store::kind`]
//! provides typed access, while [`Store::dynamic`] provides access through
//! [`facet_value::Value`].
#![forbid(unsafe_code)]

mod document;
mod encoding;
mod error;
mod kind;
mod migrate;
mod provenance;
mod store;

pub use document::{DocumentBuilder, DocumentError};
pub use encoding::{Dynamic, Encoding, Typed};
pub use error::{Error, Subtree};
pub use kind::{Entry, Kind, KindSchema, Put, entity_name, entity_name_under};
pub use provenance::SchemaLabel;
pub use store::{Layout, RepoStore, Store};

pub use facet_git_tree::{ObjectId, Schema, schema_of};

/// The tree and schema format used by `gix-store`.
pub use facet_git_tree as tree;
pub use gix_refstore::{
    ApplyError, Committer, Expectation, GixRefStore, InvalidRefName, MemoryRefStore, RefEdit,
    RefName, RefPath, RefPrefix, RefSegment, RefStore, SignatureBytes, Signer,
};
