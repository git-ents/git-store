//! Prolly node primitives: leaf entries, internal markers, tree (de)composition.
//!
//! A Prolly node is a Git tree; this module holds the small vocabulary used to
//! compose and inspect such trees. It deliberately contains no tree walking —
//! building, lookup, diff, and verification each walk nodes in their own way.

use gix::ObjectId;
use gix::bstr::{BString, ByteSlice};
use gix::objs::tree::{Entry as TreeEntry, EntryKind, EntryMode};
use gix::objs::{Kind, Tree};

use crate::error::{Error, KeyError};
use crate::key::{HexKeyCodec, KeyCodec};

/// The tree-entry name reserved for the internal-node marker.
///
/// Hexadecimal user keys always sort above this name, so the marker can never
/// collide with a key and always sorts first in an internal node.
pub(crate) const INTERNAL_MARKER_NAME: &[u8] = b"!";

/// The marker blob's content: format identity and node role in one object.
///
/// The format version participates in canonical identity — a change to the
/// node format cannot make two incompatible representations appear equivalent.
pub(crate) const INTERNAL_MARKER_CONTENT: &[u8] = b"git-prolly:2:internal\n";

/// The current Prolly-on-Git format version.
pub const FORMAT_VERSION: u8 = 2;

/// One logical entry of a leaf: a raw key, the value object's id, and the
/// value object's kind, which is also the entry's Git mode.
///
/// The mode is carried alongside the id because it is fixed by the
/// content-addressed object and needed on every rebuild; carrying it avoids a
/// header read per entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    /// The raw (decoded) key.
    pub key: Vec<u8>,
    /// The facet-git-tree root object id backing the value.
    pub value_oid: ObjectId,
    /// The Git mode the leaf entry is written with.
    pub mode: EntryMode,
}

/// Whether a node is internal or a leaf, decided by the marker entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeKind {
    /// An internal node carrying the marker and child edges.
    Internal,
    /// A leaf carrying key/value entries and no marker.
    Leaf,
}

/// Classify a decoded node by its marker entry.
pub(crate) fn node_kind(entries: &[TreeEntry]) -> NodeKind {
    match entries.first() {
        Some(entry)
            if entry.filename.as_bytes() == INTERNAL_MARKER_NAME
                && entry.mode.kind() == EntryKind::Blob =>
        {
            NodeKind::Internal
        }
        _ => NodeKind::Leaf,
    }
}

/// Decode every non-marker entry name of a node into raw keys.
///
/// The marker (if any) is skipped. An undecodable name means the node was not
/// written by this format and is rejected rather than guessed at.
pub(crate) fn decode_entry_keys(
    entries: &[TreeEntry],
    codec: &HexKeyCodec,
) -> Result<Vec<(Vec<u8>, ObjectId, EntryKind)>, Error> {
    let mut decoded = Vec::with_capacity(entries.len());
    for entry in entries {
        if entry.filename.as_bytes() == INTERNAL_MARKER_NAME {
            continue;
        }
        let key = codec
            .decode(entry.filename.as_bytes())
            .map_err(|_invalid| Error::Key(KeyError::InvalidEncoding(entry.filename.to_vec())))?;
        decoded.push((key, entry.oid, entry.mode.kind()));
    }
    Ok(decoded)
}

/// A human-readable name for an object kind.
pub(crate) fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Tree => "tree",
        Kind::Blob => "blob",
        Kind::Commit => "commit",
        Kind::Tag => "tag",
    }
}

/// Encode one raw key as a tree-entry name.
pub(crate) fn entry_name(codec: &HexKeyCodec, key: &[u8]) -> Result<BString, Error> {
    codec.encode(key).map(BString::new).map_err(Error::Key)
}

/// Compose a leaf tree from ordered entries.
///
/// The caller supplies entries in key order; entries are re-sorted with Git's
/// own entry ordering before encoding so the emitted tree is always a valid
/// Git tree.
pub(crate) fn leaf_tree(codec: &HexKeyCodec, entries: &[Entry]) -> Result<Tree, Error> {
    let mut tree_entries: Vec<TreeEntry> = entries
        .iter()
        .map(|entry| {
            Ok::<TreeEntry, Error>(TreeEntry {
                mode: entry.mode,
                filename: entry_name(codec, &entry.key)?,
                oid: entry.value_oid,
            })
        })
        .collect::<Result<_, _>>()?;
    tree_entries.sort_unstable();
    Ok(Tree {
        entries: tree_entries,
    })
}

/// Compose an internal tree from ordered `(separator, child)` pairs.
///
/// The marker entry is prepended; the caller supplies children in key order.
pub(crate) fn internal_tree(
    codec: &HexKeyCodec,
    marker_oid: ObjectId,
    children: &[(Vec<u8>, ObjectId)],
) -> Result<Tree, Error> {
    let mut tree_entries: Vec<TreeEntry> = children
        .iter()
        .map(|(separator, child)| {
            Ok::<TreeEntry, Error>(TreeEntry {
                mode: EntryMode::from(EntryKind::Tree),
                filename: entry_name(codec, separator)?,
                oid: *child,
            })
        })
        .collect::<Result<_, _>>()?;
    tree_entries.push(TreeEntry {
        mode: EntryMode::from(EntryKind::Blob),
        filename: BString::new(INTERNAL_MARKER_NAME.to_vec()),
        oid: marker_oid,
    });
    tree_entries.sort_unstable();
    Ok(Tree {
        entries: tree_entries,
    })
}

/// The Git object id a tree would have, without writing it.
///
/// Used by verification, which must never mutate the repository.
pub(crate) fn hash_tree(tree: &Tree, hash_kind: gix::hash::Kind) -> Result<ObjectId, Error> {
    let mut encoded = Vec::new();
    use gix::objs::WriteTo;
    tree.write_to(&mut encoded)
        .map_err(|error| Error::Git(error.into()))?;
    gix::objs::compute_hash(hash_kind, Kind::Tree, &encoded).map_err(Error::git)
}
