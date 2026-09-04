//! Structural diff between two tree roots.
//!
//! The walk compares ObjectIds and never deserializes a value: identical
//! ObjectIds mean identical subtrees and skip entire ranges, and changed keys
//! are reported as old/new value object ids for the caller to interpret. This
//! is a Git-native structural diff — `git diff-tree` is never invoked.

use gix::ObjectId;

use crate::chunk::MAX_LEVELS;
use crate::error::Error;
use crate::node::{self, Entry, NodeKind};
use crate::store::ProllyStore;

/// One key-level change between two tree roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffEntry {
    /// The key exists only in the newer tree.
    Insert {
        /// The inserted key.
        key: Vec<u8>,
        /// The value object backing the inserted value.
        new: ObjectId,
    },
    /// The key exists only in the older tree.
    Delete {
        /// The deleted key.
        key: Vec<u8>,
        /// The value object that backed the deleted value.
        old: ObjectId,
    },
    /// The key exists in both trees with different values.
    Modify {
        /// The changed key.
        key: Vec<u8>,
        /// The value object backing the old value.
        old: ObjectId,
        /// The value object backing the new value.
        new: ObjectId,
    },
}

impl ProllyStore<'_> {
    /// Diff two tree roots, returning key-level changes in key order.
    ///
    /// Subtrees with identical ObjectIds are skipped without being read. When
    /// both sides of a differing subtree are leaves, their entries are merged
    /// directly; otherwise both sides are expanded to entry lists (reading
    /// only node trees, never values) and merged, so results are exact even
    /// when chunk boundaries shifted between the two roots.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] when a node cannot be read.
    pub fn diff(&self, a: ObjectId, b: ObjectId) -> Result<Vec<DiffEntry>, Error> {
        let mut changes = Vec::new();
        if a != b {
            self.diff_node(a, b, &mut changes, 0)?;
        }
        Ok(changes)
    }

    fn diff_node(
        &self,
        a: ObjectId,
        b: ObjectId,
        out: &mut Vec<DiffEntry>,
        depth: usize,
    ) -> Result<(), Error> {
        if a == b {
            return Ok(());
        }
        if depth > MAX_LEVELS {
            return Err(Error::TooDeep {
                root: a,
                max: MAX_LEVELS,
            });
        }
        let entries_a = self.read_tree(a)?;
        let entries_b = self.read_tree(b)?;
        if node::node_kind(&entries_a) == NodeKind::Leaf
            && node::node_kind(&entries_b) == NodeKind::Leaf
        {
            let codec = self.config().key_codec.codec();
            let left = self.leaf_entries(&entries_a, &codec)?;
            let right = self.leaf_entries(&entries_b, &codec)?;
            merge_join(&left, &right, out);
        } else {
            // Structural mismatch or unaligned internal nodes: expand both
            // sides to their full entry lists. Identical subtrees were already
            // skipped above, so this still reads only differing ranges.
            let left = self.collect_entries(a)?;
            let right = self.collect_entries(b)?;
            merge_join(&left, &right, out);
        }
        Ok(())
    }

    /// Decode a leaf node's entries; internal nodes are rejected by the caller.
    fn leaf_entries(
        &self,
        entries: &[gix::objs::tree::Entry],
        codec: &crate::key::HexKeyCodec,
    ) -> Result<Vec<Entry>, Error> {
        node::decode_entry_keys(entries, codec)?
            .into_iter()
            .map(|(key, value_oid, kind)| {
                Ok(Entry {
                    key,
                    value_oid,
                    mode: crate::store::mode_of_kind(kind),
                })
            })
            .collect()
    }
}

/// Merge two sorted entry lists into key-level changes.
fn merge_join(left: &[Entry], right: &[Entry], out: &mut Vec<DiffEntry>) {
    let mut i = 0;
    let mut j = 0;
    while i < left.len() || j < right.len() {
        let ordering = match (left.get(i), right.get(j)) {
            (Some(a), Some(b)) => Some(a.key.cmp(&b.key)),
            (Some(_), None) => Some(std::cmp::Ordering::Less),
            (None, Some(_)) => Some(std::cmp::Ordering::Greater),
            (None, None) => None,
        };
        match ordering {
            None => break,
            Some(std::cmp::Ordering::Less) => {
                if let Some(entry) = left.get(i) {
                    out.push(DiffEntry::Delete {
                        key: entry.key.clone(),
                        old: entry.value_oid,
                    });
                }
                i += 1;
            }
            Some(std::cmp::Ordering::Greater) => {
                if let Some(entry) = right.get(j) {
                    out.push(DiffEntry::Insert {
                        key: entry.key.clone(),
                        new: entry.value_oid,
                    });
                }
                j += 1;
            }
            Some(std::cmp::Ordering::Equal) => {
                let a = left.get(i);
                let b = right.get(j);
                if let (Some(a), Some(b)) = (a, b)
                    && a.value_oid != b.value_oid
                {
                    out.push(DiffEntry::Modify {
                        key: a.key.clone(),
                        old: a.value_oid,
                        new: b.value_oid,
                    });
                }
                i += 1;
                j += 1;
            }
        }
    }
}
