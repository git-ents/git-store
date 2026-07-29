//! Serialize [`facet::Facet`] values to, and deserialize them from, Git trees.
//!
//! A value is encoded as a graph of Git objects — scalars and strings as blobs,
//! structs, enums, and collections as trees — addressed by SHA-1 exactly as Git
//! would compute them. The bundled [`ObjectStore`] is an in-memory backend, but
//! the entry points are generic over `gix`'s `Find` and `Write` traits, so a
//! real `gix` repository or object database works just as well.
//!
//! The normative encoding rules live in `docs/specification.adoc`.
#![forbid(unsafe_code)]

mod de;
mod error;
mod marker;
pub mod migration;
mod raw_tree;
pub mod schema;
mod ser;
mod store;

pub use gix_hash::ObjectId;
pub use gix_object::Object as GitObject;
pub use gix_object::tree::{Entry as TreeEntry, EntryKind, EntryMode};

pub use de::{check_key, deserialize, deserialize_into};
pub use error::{
    DeserializeError, KeyError, MigrationError, MigrationPinError, SchemaError, SchemaPinError,
    SchemaReadError, SchemaWriteError, SerializeError,
};
#[cfg(feature = "value")]
pub use migration::apply::{Edge, apply, apply_chain};
pub use migration::derive::{Derivation, Divergence, Incomplete, Side};
pub use migration::pin::MigrationSchema;
pub use migration::{Change, Constant, Hints, Migration, Op, Target};
pub use raw_tree::RawTree;
pub use schema::pin::SchemaSchema;
#[cfg(feature = "value")]
pub use schema::read::{deserialize_value_with_schema, validate_with_schema};
#[cfg(feature = "value")]
pub use schema::write::serialize_value_with_schema;
pub use schema::{Schema, SchemaDoc, VariantKind, schema_and_hints_of, schema_of};
pub use ser::{serialize, serialize_into, serialize_peek, serialize_peek_into};
pub use store::ObjectStore;
