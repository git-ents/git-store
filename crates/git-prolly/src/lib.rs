//! Persistent, content-addressed Prolly Trees whose physical representation is
//! ordinary Git objects.
//!
//! A Prolly node is a Git tree, and its Git [`ObjectId`] is its node identity.
//! A value is recursively represented as Git trees and blobs by
//! [`facet-git-tree`], so Git's existing object graph provides reachability,
//! deduplication, packing, fetching, and garbage collection. There is no second
//! object store and no second Merkle-node format.
//!
//! # Node representation
//!
//! A **leaf** is a Git tree with one entry per logical key:
//!
//! ```text
//! <mode> <hex-encoded-key> <value-object-id>
//! ```
//!
//! The value object id is whatever root object [`facet-git-tree`] produced for
//! the value — usually a tree, but a blob for scalar values. The entry mode is
//! the kind of that object (`040000` for trees, `100644` for blobs), so every
//! emitted tree is valid for ordinary Git tooling including `git fsck`. Values
//! are opaque object ids at this layer; their internal object graph belongs to
//! `facet-git-tree`.
//!
//! An **internal node** is also a Git tree. Its first entry is a reserved
//! marker (name `!`, a blob whose content identifies the format and marks the
//! node internal), followed by one entry per child:
//!
//! ```text
//! 040000 <hex-encoded-separator-key> <child-tree-oid>
//! ```
//!
//! The separator is the first logical key in that child's subtree, and the
//! Git tree itself supplies ordering. Child edges are ordinary Git tree
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
//! The on-disk format is versioned: the internal-node marker carries the
//! format version, and the current version is [`FORMAT_VERSION`]. Within a
//! version, node representations are frozen — trees written by any release of
//! this crate remain readable by later releases. A format change requires a
//! new version and a reader that still accepts every earlier version. The
//! [`ProllyConfig`] parameters are likewise frozen per version: trees built
//! under different configurations are distinct, never silently comparable.
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
