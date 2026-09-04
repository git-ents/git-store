//! Verification: Prolly-tree invariants on top of Git object validity.
//!
//! `git fsck` establishes that an object is a valid Git object graph; it says
//! nothing about whether that graph obeys Prolly invariants.
//! [`ProllyStore::verify`] checks both layers separately:
//!
//! * per-node structure: node kind by marker, entry modes against object
//!   kinds, key decodability and ordering, separator correctness, and the
//!   existence of every referenced object;
//! * canonicality: chunk boundaries recomputed from the entry fingerprints
//!   and compared against the actual node grouping at every level, ending in
//!   an expected root that must equal the actual root.
//!
//! A tree that passes verification is exactly the tree this crate's builder
//! would produce for its contained entries and configuration.
//!
//! Verification never writes to the repository: expected node ids are computed
//! by hashing, not by writing.

use gix::ObjectId;
use gix::bstr::ByteSlice;
use gix::objs::Kind;
use gix::objs::tree::{Entry as TreeEntry, EntryKind, EntryMode};

use crate::chunk::{self, MAX_LEVELS};
use crate::error::{Error, KeyError};
use crate::key::{HexKeyCodec, KeyCodec};
use crate::node::{self, Entry, INTERNAL_MARKER_CONTENT, INTERNAL_MARKER_NAME, NodeKind};
use crate::store::{ObjectStore, ProllyStore};

/// A violated Prolly-tree invariant, or a backend failure encountered while
/// checking one.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    /// A Git object operation failed while walking the tree.
    #[error(transparent)]
    Backend(#[from] Error),
    /// A configuration is not usable.
    #[error(transparent)]
    Config(#[from] crate::error::ConfigError),
    /// The tree descends through more levels than the format allows.
    #[error("tree rooted at {root} exceeds the maximum of {max} levels")]
    TooDeep {
        /// The root whose structure was rejected.
        root: ObjectId,
        /// The format's level bound.
        max: usize,
    },
    /// Leaf and internal nodes share a level, so the tree is not balanced.
    #[error("node {node} at level {level} mixes leaf and internal siblings")]
    Unbalanced {
        /// The offending node.
        node: ObjectId,
        /// The level (the root is level 0 of the collection) holding both kinds.
        level: usize,
    },
    /// An internal node lacks its leading marker entry.
    #[error("internal node {node} is missing its {name:?} marker entry")]
    MissingMarker {
        /// The node missing the marker.
        node: ObjectId,
        /// The reserved marker name.
        name: Vec<u8>,
    },
    /// An internal node's marker blob does not carry the format's content.
    #[error("internal node {node} has invalid marker content")]
    BadMarker {
        /// The node whose marker is wrong.
        node: ObjectId,
    },
    /// An entry name is not a valid encoding for the configured codec.
    #[error("node {node}: entry name {name:?} is not a valid key encoding")]
    BadKey {
        /// The node holding the entry.
        node: ObjectId,
        /// The undecodable entry name.
        name: Vec<u8>,
        /// Why the name was rejected.
        source: KeyError,
    },
    /// Two entries in a node are out of order.
    #[error("node {node}: keys {first:?} and {second:?} are out of order")]
    Unordered {
        /// The node holding the entries.
        node: ObjectId,
        /// The first key.
        first: Vec<u8>,
        /// The second key.
        second: Vec<u8>,
    },
    /// An entry's mode does not match the kind of the object it references.
    #[error("node {node}: entry {name:?} has mode {found}, expected {expected}")]
    WrongMode {
        /// The node holding the entry.
        node: ObjectId,
        /// The entry name.
        name: Vec<u8>,
        /// The mode the referenced object kind requires.
        expected: &'static str,
        /// The mode the entry actually carries.
        found: &'static str,
    },
    /// A child referenced by an internal node is absent from the repository.
    #[error("node {node} references missing child {child}")]
    ChildMissing {
        /// The referencing node.
        node: ObjectId,
        /// The missing child.
        child: ObjectId,
    },
    /// A leaf value object is absent from the repository.
    #[error("leaf {node} references missing value object {value}")]
    ValueMissing {
        /// The referencing leaf.
        node: ObjectId,
        /// The missing value object.
        value: ObjectId,
    },
    /// An internal entry's separator is not the first key of its child's subtree.
    #[error(
        "node {node}: separator {found:?} for child {child} is not the child's first key {expected:?}"
    )]
    BadSeparator {
        /// The node holding the entry.
        node: ObjectId,
        /// The child whose first key was compared.
        child: ObjectId,
        /// The child subtree's actual first key.
        expected: Vec<u8>,
        /// The separator the entry carries.
        found: Vec<u8>,
    },
    /// A node is not the tree this crate's builder would produce.
    #[error("node {node} is not canonical: expected node {expected} for these entries")]
    NonCanonical {
        /// The actual node.
        node: ObjectId,
        /// The node the builder would have produced.
        expected: ObjectId,
    },
}

/// One collected node of the level structure, before bottom-up checking.
#[derive(Debug)]
struct CollectedNode {
    oid: ObjectId,
    /// Leaf nodes: decoded entries. Internal nodes: empty.
    entries: Vec<Entry>,
    /// Internal nodes: `(separator, child_oid)` pairs in entry order.
    children: Vec<(Vec<u8>, ObjectId)>,
    /// Internal nodes: this node's children's positions within the next-lower
    /// level's node list.
    child_slots: Vec<usize>,
    /// The first key in this node's subtree, filled bottom-up.
    first_key: Vec<u8>,
}

impl ProllyStore<'_> {
    /// Verify that `root` is a canonical tree under this store's
    /// [`ProllyConfig`](crate::ProllyConfig).
    ///
    /// The empty-map root always verifies.
    ///
    /// # Errors
    ///
    /// Returns the first violated [`VerifyError`].
    pub fn verify(&self, root: ObjectId) -> Result<(), VerifyError> {
        self.config().validate()?;
        if self.is_empty_root(root) {
            return Ok(());
        }
        let levels = self.collect_levels(root)?;
        self.verify_levels(root, levels)
    }

    /// Collect the level structure top-down, checking per-node structure that
    /// does not depend on the rest of the tree.
    fn collect_levels(&self, root: ObjectId) -> Result<Vec<Vec<CollectedNode>>, VerifyError> {
        let codec = self.config().key_codec.codec();
        let mut levels: Vec<Vec<CollectedNode>> = Vec::new();
        let mut current = vec![root];
        let mut level_number = 0;
        loop {
            let mut nodes: Vec<CollectedNode> = Vec::with_capacity(current.len());
            let mut next: Vec<ObjectId> = Vec::new();
            let mut saw_internal = false;
            let mut saw_leaf = false;
            for oid in current {
                let entries = self.read_tree(oid)?;
                match node::node_kind(&entries) {
                    NodeKind::Internal => {
                        saw_internal = true;
                        self.check_marker(oid, &entries)?;
                        let children = self.decode_children(oid, &entries, &codec)?;
                        let child_slots: Vec<usize> =
                            (next.len()..next.len() + children.len()).collect();
                        for (_, child) in &children {
                            next.push(*child);
                        }
                        nodes.push(CollectedNode {
                            oid,
                            entries: Vec::new(),
                            children,
                            child_slots,
                            first_key: Vec::new(),
                        });
                    }
                    NodeKind::Leaf => {
                        saw_leaf = true;
                        let entries = self.decode_leaf(oid, &entries, &codec)?;
                        let first_key = entries
                            .first()
                            .map(|entry| entry.key.clone())
                            .unwrap_or_default();
                        nodes.push(CollectedNode {
                            oid,
                            entries,
                            children: Vec::new(),
                            child_slots: Vec::new(),
                            first_key,
                        });
                    }
                }
            }
            if saw_internal && saw_leaf {
                let mixed = nodes
                    .iter()
                    .find(|collected| !collected.children.is_empty())
                    .map(|collected| collected.oid)
                    .unwrap_or(root);
                return Err(VerifyError::Unbalanced {
                    node: mixed,
                    level: level_number,
                });
            }
            levels.push(nodes);
            if !saw_internal {
                // Collected top-down; verification consumes bottom-up.
                levels.reverse();
                return Ok(levels);
            }
            level_number += 1;
            if level_number > MAX_LEVELS {
                return Err(VerifyError::TooDeep {
                    root,
                    max: MAX_LEVELS,
                });
            }
            current = next;
        }
    }

    /// Check an internal node's marker entry name, mode, and blob content.
    fn check_marker(&self, oid: ObjectId, entries: &[TreeEntry]) -> Result<(), VerifyError> {
        let missing = || VerifyError::MissingMarker {
            node: oid,
            name: INTERNAL_MARKER_NAME.to_vec(),
        };
        let marker = entries.first().ok_or_else(missing)?;
        if marker.filename.as_bytes() != INTERNAL_MARKER_NAME {
            return Err(missing());
        }
        if marker.mode.kind() != EntryKind::Blob {
            return Err(VerifyError::BadMarker { node: oid });
        }
        let contents = self.repo().read_blob(marker.oid)?;
        if contents != INTERNAL_MARKER_CONTENT {
            return Err(VerifyError::BadMarker { node: oid });
        }
        Ok(())
    }

    /// Decode an internal node's `(separator, child)` pairs, checking child modes.
    fn decode_children(
        &self,
        oid: ObjectId,
        entries: &[TreeEntry],
        codec: &HexKeyCodec,
    ) -> Result<Vec<(Vec<u8>, ObjectId)>, VerifyError> {
        let mut children = Vec::with_capacity(entries.len());
        for entry in entries.iter().skip(1) {
            let separator =
                codec
                    .decode(entry.filename.as_bytes())
                    .map_err(|source| VerifyError::BadKey {
                        node: oid,
                        name: entry.filename.to_vec(),
                        source,
                    })?;
            if entry.mode.kind() != EntryKind::Tree {
                return Err(VerifyError::WrongMode {
                    node: oid,
                    name: entry.filename.to_vec(),
                    expected: "tree",
                    found: mode_name(entry.mode),
                });
            }
            children.push((separator, entry.oid));
        }
        Ok(children)
    }

    /// Decode a leaf node's entries, checking value modes against object kinds.
    fn decode_leaf(
        &self,
        oid: ObjectId,
        entries: &[TreeEntry],
        codec: &HexKeyCodec,
    ) -> Result<Vec<Entry>, VerifyError> {
        let mut decoded = Vec::with_capacity(entries.len());
        for entry in entries {
            let key =
                codec
                    .decode(entry.filename.as_bytes())
                    .map_err(|source| VerifyError::BadKey {
                        node: oid,
                        name: entry.filename.to_vec(),
                        source,
                    })?;
            let kind =
                self.repo()
                    .try_object_kind(entry.oid)?
                    .ok_or(VerifyError::ValueMissing {
                        node: oid,
                        value: entry.oid,
                    })?;
            let expected = match kind {
                Kind::Tree => EntryMode::from(EntryKind::Tree),
                Kind::Blob => EntryMode::from(EntryKind::Blob),
                _ => {
                    return Err(VerifyError::WrongMode {
                        node: oid,
                        name: entry.filename.to_vec(),
                        expected: "tree or blob",
                        found: mode_name(entry.mode),
                    });
                }
            };
            if entry.mode != expected {
                return Err(VerifyError::WrongMode {
                    node: oid,
                    name: entry.filename.to_vec(),
                    expected: mode_name(expected),
                    found: mode_name(entry.mode),
                });
            }
            decoded.push(Entry {
                key,
                value_oid: entry.oid,
                mode: entry.mode,
            });
        }
        Ok(decoded)
    }

    /// Run the bottom-up checks: ordering, separators, and per-level
    /// canonicality against recomputed chunk boundaries.
    fn verify_levels(
        &self,
        root: ObjectId,
        mut levels: Vec<Vec<CollectedNode>>,
    ) -> Result<(), VerifyError> {
        let codec = self.config().key_codec.codec();
        let hash_kind = self.repo().hash_kind();
        let chunker = self.config().chunker();
        let marker_oid = gix::objs::compute_hash(hash_kind, Kind::Blob, INTERNAL_MARKER_CONTENT)
            .map_err(Error::git)?;

        // Bottom level: leaves. Check ordering and value existence, then that
        // the leaves are exactly the chunks the builder would have written.
        let leaves = levels.first().ok_or_else(missing_levels)?;
        let mut actual_leaves = Vec::with_capacity(leaves.len());
        for leaf in leaves {
            let keys: Vec<Vec<u8>> = leaf.entries.iter().map(|entry| entry.key.clone()).collect();
            check_ordered(leaf.oid, &keys)?;
            for entry in &leaf.entries {
                if !self.object_exists(entry.value_oid)? {
                    return Err(VerifyError::ValueMissing {
                        node: leaf.oid,
                        value: entry.value_oid,
                    });
                }
            }
            actual_leaves.push(leaf.oid);
        }
        let all_entries: Vec<Entry> = leaves
            .iter()
            .flat_map(|leaf| leaf.entries.iter().cloned())
            .collect();
        let fingerprints = leaf_fingerprints(&codec, hash_kind, &all_entries)?;
        let expected =
            expected_leaf_nodes(&codec, hash_kind, &chunker, &all_entries, &fingerprints)?;
        compare_grouping(expected, actual_leaves, root)?;

        // Upper levels: internal nodes, bottom-up.
        for depth in 1..levels.len() {
            let (below, above) = levels.split_at_mut(depth);
            let lower = below.last().ok_or_else(missing_levels)?;
            let level = above.first_mut().ok_or_else(missing_levels)?;

            // The children of this level's nodes are exactly the nodes of the
            // level below, in order, each named by its subtree's first key.
            let all_children: Vec<(Vec<u8>, ObjectId)> = lower
                .iter()
                .map(|collected| (collected.first_key.clone(), collected.oid))
                .collect();
            let fingerprints: Vec<u64> = all_children
                .iter()
                .map(|(_, child)| chunk::child_fingerprint(hash_kind, child))
                .collect();
            let expected = expected_internal_nodes(
                &codec,
                hash_kind,
                &chunker,
                marker_oid,
                &all_children,
                &fingerprints,
            )?;
            let actual: Vec<ObjectId> = level.iter().map(|collected| collected.oid).collect();
            compare_grouping(expected, actual, root)?;

            // Per-node checks against the level below, then first keys.
            for collected in level.iter() {
                let separators: Vec<Vec<u8>> = collected
                    .children
                    .iter()
                    .map(|(separator, _)| separator.clone())
                    .collect();
                check_ordered(collected.oid, &separators)?;
                for (slot, (separator, child)) in collected.children.iter().enumerate() {
                    let child_node = collected
                        .child_slots
                        .get(slot)
                        .and_then(|index| lower.get(*index))
                        .ok_or(VerifyError::ChildMissing {
                            node: collected.oid,
                            child: *child,
                        })?;
                    if child_node.oid != *child {
                        return Err(VerifyError::ChildMissing {
                            node: collected.oid,
                            child: *child,
                        });
                    }
                    if *separator != child_node.first_key {
                        return Err(VerifyError::BadSeparator {
                            node: collected.oid,
                            child: *child,
                            expected: child_node.first_key.clone(),
                            found: separator.clone(),
                        });
                    }
                }
            }
            for collected in level.iter_mut() {
                collected.first_key = collected
                    .children
                    .first()
                    .and_then(|(_, child)| {
                        lower
                            .iter()
                            .find(|candidate| candidate.oid == *child)
                            .map(|candidate| candidate.first_key.clone())
                    })
                    .unwrap_or_default();
            }
        }

        // The top level must be exactly the requested root.
        let top = levels
            .last()
            .and_then(|level| level.first())
            .ok_or_else(missing_levels)?;
        if top.oid != root {
            return Err(VerifyError::NonCanonical {
                node: root,
                expected: top.oid,
            });
        }
        Ok(())
    }
}

/// The error for a level structure that failed to collect.
fn missing_levels() -> VerifyError {
    VerifyError::Backend(Error::Git("no levels collected".into()))
}

/// The expected leaf node ids for the chunking of `entries`.
fn expected_leaf_nodes(
    codec: &HexKeyCodec,
    hash_kind: gix::hash::Kind,
    chunker: &crate::chunk::Chunker,
    entries: &[Entry],
    fingerprints: &[u64],
) -> Result<Vec<ObjectId>, VerifyError> {
    let mut expected = Vec::new();
    let mut starts_and_ends = vec![0];
    starts_and_ends.extend(chunker.boundaries(fingerprints));
    for pair in starts_and_ends.windows(2) {
        let (start, end) = match (pair.first().copied(), pair.last().copied()) {
            (Some(start), Some(end)) => (start, end),
            _ => continue,
        };
        let chunk = entries
            .get(start..end)
            .ok_or_else(|| VerifyError::Backend(Error::Git("boundary out of range".into())))?;
        expected.push(
            node::hash_tree(&node::leaf_tree(codec, chunk)?, hash_kind)
                .map_err(VerifyError::Backend)?,
        );
    }
    Ok(expected)
}

/// The expected internal node ids for the chunking of `children`.
fn expected_internal_nodes(
    codec: &HexKeyCodec,
    hash_kind: gix::hash::Kind,
    chunker: &crate::chunk::Chunker,
    marker_oid: ObjectId,
    children: &[(Vec<u8>, ObjectId)],
    fingerprints: &[u64],
) -> Result<Vec<ObjectId>, VerifyError> {
    let mut expected = Vec::new();
    let mut starts_and_ends = vec![0];
    starts_and_ends.extend(chunker.boundaries(fingerprints));
    for pair in starts_and_ends.windows(2) {
        let (start, end) = match (pair.first().copied(), pair.last().copied()) {
            (Some(start), Some(end)) => (start, end),
            _ => continue,
        };
        let chunk = children
            .get(start..end)
            .ok_or_else(|| VerifyError::Backend(Error::Git("boundary out of range".into())))?;
        expected.push(
            node::hash_tree(&node::internal_tree(codec, marker_oid, chunk)?, hash_kind)
                .map_err(VerifyError::Backend)?,
        );
    }
    Ok(expected)
}

/// Leaf-level fingerprints over an entry list.
fn leaf_fingerprints(
    codec: &HexKeyCodec,
    hash_kind: gix::hash::Kind,
    entries: &[Entry],
) -> Result<Vec<u64>, VerifyError> {
    let mut fingerprints = Vec::with_capacity(entries.len());
    for entry in entries {
        let encoded = codec
            .encode(&entry.key)
            .map_err(|source| VerifyError::BadKey {
                node: ObjectId::empty_tree(hash_kind),
                name: entry.key.clone(),
                source,
            })?;
        fingerprints.push(chunk::leaf_fingerprint(
            hash_kind,
            &encoded,
            &entry.value_oid,
        )?);
    }
    Ok(fingerprints)
}

/// Reject a node whose keys are not strictly increasing in stored order.
fn check_ordered(node: ObjectId, keys: &[Vec<u8>]) -> Result<(), VerifyError> {
    for pair in keys.windows(2) {
        let first = pair.first().cloned().unwrap_or_default();
        let second = pair.get(1).cloned().unwrap_or_default();
        if first >= second {
            return Err(VerifyError::Unordered {
                node,
                first,
                second,
            });
        }
    }
    Ok(())
}

/// Compare the actual node ids of one level against the chunker's grouping.
///
/// `expected` holds one node id per recomputed chunk; `actual` holds the
/// collected node ids in order. The lists must be identical, otherwise the
/// level is not what the builder would produce.
fn compare_grouping(
    expected: Vec<ObjectId>,
    actual: Vec<ObjectId>,
    root: ObjectId,
) -> Result<(), VerifyError> {
    for (index, expected_oid) in expected.iter().copied().enumerate() {
        match actual.get(index) {
            Some(actual_oid) if *actual_oid == expected_oid => {}
            Some(actual_oid) => {
                return Err(VerifyError::NonCanonical {
                    node: *actual_oid,
                    expected: expected_oid,
                });
            }
            None => {
                return Err(VerifyError::NonCanonical {
                    node: root,
                    expected: expected_oid,
                });
            }
        }
    }
    if let Some(extra) = actual.get(expected.len()) {
        return Err(VerifyError::NonCanonical {
            node: *extra,
            expected: expected
                .first()
                .copied()
                .unwrap_or_else(|| ObjectId::empty_tree(gix::hash::Kind::Sha1)),
        });
    }
    Ok(())
}

/// The display form of a tree entry mode.
fn mode_name(mode: EntryMode) -> &'static str {
    match mode.kind() {
        EntryKind::Tree => "tree",
        EntryKind::Blob | EntryKind::BlobExecutable => "blob",
        EntryKind::Commit => "commit",
        EntryKind::Link => "link",
    }
}
