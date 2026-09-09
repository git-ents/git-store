//! Persistent, content-addressed Prolly Trees whose physical representation is
//! ordinary Git objects.
//!
//! A Prolly node is a Git tree, and its Git [`ObjectId`] is its node identity.
//! A value is recursively represented as Git trees and scalar leaf blobs by
//! [`facet-git-tree`]. The database wire type tags null, booleans, each numeric
//! representation, strings, arrays, and objects, so dynamic values are
//! unambiguous without using JSON as storage. Git's existing object graph
//! provides reachability, deduplication, packing, fetching, and garbage
//! collection. There is no second object store and no second Merkle-node format.
//!
//! # Node representation
//!
//! A **leaf** is a Git tree with one entry per logical key:
//!
//! ```text
//! <mode> <hex-encoded-key> <value-object-id>
//! ```
//!
//! A value root is a tree for every container, with a narrow tagged Facet wire
//! type at the root and recursively at each child. `insert_value_object` is the
//! escape hatch for callers that already have a Git tree or blob; it never
//! re-encodes that object.
//!
//! An **internal node** is also a Git tree. Its first entry is a reserved
//! marker (name `!`, a blob whose content identifies the format and marks the
//! node internal), followed by one entry per child:
//!
//! ```text
//! 040000 <hex-encoded-separator-key> <child-tree-oid>
//! ```
//!
//! Chunk boundaries are derived from encoded keys and child separators, so
//! changing a row value does not move it between nodes. The separator is the
//! first logical key in that child's subtree, and the Git tree itself supplies ordering. Child edges are ordinary Git tree
//! entries, never bytes inside a blob, so Git reachability machinery sees the
//! whole Prolly structure.
//!
//! Because leaf keys are lowercase hexadecimal (see [`HexKeyCodec`]), bytewise
//! tree-entry ordering equals logical key ordering at both levels, and the
//! reserved marker name `!` (which sorts below every hexadecimal digit) can
//! never collide with a key.
//!
//! # Compatibility
//!
//! The on-disk node format is versioned independently from the database value
//! graph: the internal-node marker still carries [`FORMAT_VERSION`] 2 because
//! key-derived Prolly nodes did not change. Database snapshots use v3 to pin
//! the structural value codec. The [`ProllyConfig`] parameters are likewise
//! frozen per version.
//!
//! # Identity and deduplication
//!
//! ```text
//! same canonical node   → same Git tree   → same ObjectId
//! same value            → same object graph → same ObjectId
//! ```
//!
//! Nodes are immutable. A mutation produces a new root while leaving the old
//! tree intact, and unchanged subtrees keep their ObjectIds automatically.
//!
//! # Hash algorithm
//!
//! The repository's configured object hash is used throughout. Nothing in this
//! crate assumes SHA-1, ObjectId length, or a fixed hash size.
//!
//! # Example
//!
//! ```
//! use facet_value::Value;
//!
//! let dir = tempfile::TempDir::new()?;
//! let repo = gix::init(dir.path())?;
//! let store = git_prolly::ProllyStore::open(&repo);
//!
//! let empty = store.empty_root();
//! let root = store.insert(None, b"alice", &Value::from("first"))?;
//! let root = store.insert(Some(root), b"bob", &Value::from("second"))?;
//!
//! assert_eq!(
//!     store.get(root, b"alice")?,
//!     Some(Value::from("first")),
//! );
//! // Trees are immutable: both roots remain readable.
//! assert_eq!(store.get(empty, b"alice")?, None);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! [`facet-git-tree`]: https://docs.rs/facet-git-tree

#![forbid(unsafe_code)]

mod build;
mod chunk;
mod config;
mod diff;
mod error;
mod key;
mod lookup;
mod node;
mod store;
mod value;
mod verify;

pub use config::ProllyConfig;
pub use diff::DiffEntry;
pub use error::{ConfigError, Error, KeyError};
pub use key::{HexKeyCodec, KeyCodec, KeyCodecKind, MAX_ENCODED_KEY_LEN, MAX_KEY_LEN};
pub use lookup::ProllyIter;
pub use node::FORMAT_VERSION;
pub use store::ProllyStore;
pub use verify::VerifyError;

use gix::ObjectId;

/// A handle to a persisted Prolly tree.
///
/// The tree is identified entirely by its root [`ObjectId`] and the
/// [`ProllyConfig`] it was built with. Mutating a tree produces a new root;
/// this handle never changes and the referenced objects are never rewritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProllyTree {
    /// The root node's Git object id, or the Git empty tree for an empty map.
    pub root: ObjectId,
    /// The chunking and key-encoding configuration the tree was built with.
    pub config: ProllyConfig,
}

impl ProllyTree {
    /// Describe a tree by root object id and configuration.
    pub const fn new(root: ObjectId, config: ProllyConfig) -> Self {
        Self { root, config }
    }
}
