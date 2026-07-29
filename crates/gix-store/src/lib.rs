//! Store *anything* in Git as a real tree the stock plumbing can read.
//!
//! A kind is a published [`SchemaDoc`]; its entities are commit chains whose
//! tree is a `{value/, schema/}` split, so the schema an entity was written
//! against travels with it through any fetch. [`Store`] is generic over a
//! [`RefStore`] and a `gix_object` `Find`/`Write` object database, with
//! [`RepoStore`] as the specialization over a real `gix::Repository`.
//! [`Store::kind`] hands out a [`Kind`] typed to a `Facet`-derived Rust type;
//! [`Store::dynamic`] hands out one that reads and writes
//! [`facet_value::Value`] under the kind's published schema instead.
#![forbid(unsafe_code)]

mod encoding;
mod error;
mod kind;
mod provenance;
mod store;

pub use encoding::{Dynamic, Encoding, Typed};
pub use error::{Error, Subtree};
pub use kind::{Kind, KindSchema, Put};
pub use provenance::SchemaLabel;
pub use store::{Layout, RepoStore, Store};

pub use facet_git_tree::{ObjectId, SchemaDoc, schema_of};
pub use gix_refstore::{
    ApplyError, Committer, Expectation, GixRefStore, InvalidRefName, MemoryRefStore, RefEdit,
    RefName, RefPrefix, RefSegment, RefStore,
};
