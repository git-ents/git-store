//! Building trees: bottom-up construction and immutable mutations.
//!
//! Construction is bottom-up over sorted entries. Leaf chunks become Git
//! trees (one entry per key/value), parent chunks become internal Git trees
//! whose edges are ordinary tree entries, and the last remaining node is the
//! root. Given the same ordered entries and the same [`ProllyConfig`], the
//! same root is always produced.
//!
//! Mutations (`insert`, `remove`) rebuild from the merged sorted entry list,
//! so a mutation result is always identical to a batch construction of the
//! same logical contents — the determinism invariant this crate commits to.
//! Rebuilds never rewrite unchanged trees: identical chunks hash to identical
//! ObjectIds and gitoxide skips writes of objects that already exist.
//! Path-local rebuilding that provably matches batch construction is future
//! work; the current implementation is O(total entries) per mutation by
//! design, and the benchmarks make that cost visible.

use facet_value::Value;
use gix::ObjectId;

use crate::chunk::{self, MAX_LEVELS};
use crate::error::Error;
use crate::key::KeyCodec;
use crate::node::{self, Entry, NodeKind};
use crate::store::ProllyStore;

impl ProllyStore<'_> {
    /// Insert `value` under `key`, returning the new root.
    ///
    /// Passing `None` builds a fresh tree. When `key` already exists, its
    /// value is replaced. If the value is already the stored one, the root is
    /// returned unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] for an invalid key, a serialization failure, a
    /// missing referenced object, or an invalid configuration.
    pub fn insert(
        &self,
        root: Option<ObjectId>,
        key: &[u8],
        value: &Value,
    ) -> Result<ObjectId, Error> {
        self.config().key_codec.codec().encode(key)?;
        let value_oid =
            facet_git_tree::serialize_into(value, self.repo()).map_err(Error::Serialize)?;
        let mode = self.entry_mode_of(value_oid)?;
        let mut entries = match root {
            None => Vec::new(),
            Some(existing) if self.is_empty_root(existing) => Vec::new(),
            Some(existing) => self.collect_entries(existing)?,
        };
        match entries.binary_search_by(|entry| entry.key.as_slice().cmp(key)) {
            Ok(index) => {
                let existing = entries
                    .get(index)
                    .ok_or_else(|| Error::Git("binary search index out of range".into()))?;
                if existing.value_oid == value_oid {
                    return match root {
                        Some(existing_root) => Ok(existing_root),
                        None => Ok(self.empty_root()),
                    };
                }
                let slot = entries
                    .get_mut(index)
                    .ok_or_else(|| Error::Git("binary search index out of range".into()))?;
                *slot = Entry {
                    key: key.to_vec(),
                    value_oid,
                    mode,
                };
            }
            Err(index) => entries.insert(
                index,
                Entry {
                    key: key.to_vec(),
                    value_oid,
                    mode,
                },
            ),
        }
        self.rebuild(entries)
    }

    /// Remove `key` from the tree rooted at `root`, returning the new root.
    ///
    /// # Errors
    ///
    /// Returns [`Error::KeyNotFound`] when the key is absent.
    pub fn remove(&self, root: ObjectId, key: &[u8]) -> Result<ObjectId, Error> {
        if self.is_empty_root(root) {
            return Err(Error::KeyNotFound(key.to_vec()));
        }
        let mut entries = self.collect_entries(root)?;
        match entries.binary_search_by(|entry| entry.key.as_slice().cmp(key)) {
            Ok(index) => {
                entries.remove(index);
            }
            Err(_) => return Err(Error::KeyNotFound(key.to_vec())),
        }
        self.rebuild(entries)
    }

    /// Build a fresh tree from `(key, value)` pairs.
    ///
    /// Duplicate keys are rejected rather than silently resolved.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] for invalid keys, duplicate keys, or serialization
    /// failures.
    pub fn build<I, K>(&self, entries: I) -> Result<ObjectId, Error>
    where
        I: IntoIterator<Item = (K, Value)>,
        K: Into<Vec<u8>>,
    {
        let mut built: Vec<Entry> = Vec::new();
        for (key, value) in entries {
            let key: Vec<u8> = key.into();
            self.config().key_codec.codec().encode(&key)?;
            let value_oid =
                facet_git_tree::serialize_into(&value, self.repo()).map_err(Error::Serialize)?;
            let mode = self.entry_mode_of(value_oid)?;
            built.push(Entry {
                key,
                value_oid,
                mode,
            });
        }
        built.sort_by(|a, b| a.key.cmp(&b.key));
        let duplicate = built.windows(2).find(|pair| {
            pair.first()
                .zip(pair.last())
                .is_some_and(|(a, b)| a.key == b.key)
        });
        if let Some(pair) = duplicate.and_then(|pair| pair.first()) {
            return Err(Error::DuplicateKey(pair.key.clone()));
        }
        self.rebuild(built)
    }

    /// Construct a tree bottom-up from sorted, unique entries.
    pub(crate) fn rebuild(&self, entries: Vec<Entry>) -> Result<ObjectId, Error> {
        if entries.is_empty() {
            return Ok(self.empty_root());
        }
        let codec = self.config().key_codec.codec();
        let hash_kind = self.hash_kind();
        let chunker = self.config().chunker();

        let mut fingerprints: Vec<u64> = Vec::with_capacity(entries.len());
        {
            let mut encoded_keys = Vec::with_capacity(entries.len());
            for entry in &entries {
                encoded_keys.push(codec.encode(&entry.key)?);
            }
            for (entry, encoded) in entries.iter().zip(&encoded_keys) {
                fingerprints.push(chunk::leaf_fingerprint(
                    hash_kind,
                    encoded,
                    &entry.value_oid,
                )?);
            }
        }

        // Level 0: leaf chunks.
        let mut level: Vec<(Vec<u8>, ObjectId)> = Vec::new();
        let mut start = 0;
        for end in chunker.boundaries(&fingerprints) {
            let chunk = entries
                .get(start..end)
                .ok_or_else(|| Error::Git("chunker returned an out-of-range boundary".into()))?;
            let tree = node::leaf_tree(&codec, chunk)?;
            let oid = self.write_tree(&tree)?;
            let first_key = chunk
                .first()
                .map(|entry| entry.key.clone())
                .unwrap_or_default();
            level.push((first_key, oid));
            start = end;
        }

        // Parent levels: chunk child nodes until one remains.
        let mut levels = 1;
        while level.len() > 1 {
            levels += 1;
            if levels > MAX_LEVELS {
                return Err(Error::TooDeep {
                    root: ObjectId::empty_tree(hash_kind),
                    max: MAX_LEVELS,
                });
            }
            let children: Vec<ObjectId> = level.iter().map(|(_, oid)| *oid).collect();
            let fingerprints: Vec<u64> = children
                .iter()
                .map(|oid| chunk::child_fingerprint(hash_kind, oid))
                .collect();
            let mut next: Vec<(Vec<u8>, ObjectId)> = Vec::new();
            let marker = self.internal_marker_oid()?;
            let mut start = 0;
            for end in chunker.boundaries(&fingerprints) {
                let chunk = level.get(start..end).ok_or_else(|| {
                    Error::Git("chunker returned an out-of-range boundary".into())
                })?;
                let tree = node::internal_tree(&codec, marker, chunk)?;
                let oid = self.write_tree(&tree)?;
                let first_key = chunk
                    .first()
                    .map(|(key, _)| key.clone())
                    .unwrap_or_default();
                next.push((first_key, oid));
                start = end;
            }
            level = next;
        }

        level
            .first()
            .map(|(_, oid)| *oid)
            .ok_or_else(|| Error::Git("no chunks were produced from a non-empty entry list".into()))
    }

    /// Walk a tree's leaves and collect its sorted logical entries.
    ///
    /// Only node trees are read; value objects are never fetched and values
    /// are never deserialized.
    pub(crate) fn collect_entries(&self, root: ObjectId) -> Result<Vec<Entry>, Error> {
        let mut entries = Vec::new();
        self.collect_node(root, &mut entries, 0)?;
        Ok(entries)
    }

    fn collect_node(&self, oid: ObjectId, out: &mut Vec<Entry>, depth: usize) -> Result<(), Error> {
        if depth > MAX_LEVELS {
            return Err(Error::TooDeep {
                root: oid,
                max: MAX_LEVELS,
            });
        }
        if self.is_empty_root(oid) {
            return Ok(());
        }
        let tree_entries = self.read_tree(oid)?;
        let codec = self.config().key_codec.codec();
        match node::node_kind(&tree_entries) {
            NodeKind::Leaf => {
                for (key, value_oid, kind) in node::decode_entry_keys(&tree_entries, &codec)? {
                    out.push(Entry {
                        key,
                        value_oid,
                        mode: crate::store::mode_of_kind(kind),
                    });
                }
            }
            NodeKind::Internal => {
                let decoded = node::decode_entry_keys(&tree_entries, &codec)?;
                for (_, child_oid, _) in decoded {
                    self.collect_node(child_oid, out, depth + 1)?;
                }
            }
        }
        Ok(())
    }
}
