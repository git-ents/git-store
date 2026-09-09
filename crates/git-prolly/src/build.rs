//! Building trees: bottom-up construction and immutable mutations.
//!
//! Construction is bottom-up over sorted entries. Leaf chunks become Git
//! trees (one entry per key/value), parent chunks become internal Git trees
//! whose edges are ordinary tree entries, and the last remaining node is the
//! root. Given the same ordered entries and the same [`ProllyConfig`], the
//! same root is always produced.
//!
//! Batch construction and mutations preserve canonical roots. Row
//! replacements rewrite only the path from the changed leaf to the root;
//! bulk construction uses canonical bottom-up builds.

use facet_value::Value;
use gix::ObjectId;

use crate::chunk::{self, MAX_LEVELS};
use crate::error::Error;
use crate::key::KeyCodec;
use crate::node::{self, Entry, NodeKind};
use crate::store::ProllyStore;

fn same_boundaries(chunker: &chunk::Chunker, old: &[u64], new: &[u64]) -> bool {
    chunker.boundaries(old) == chunker.boundaries(new)
}

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
        let value_oid = self.write_value_text(value)?;
        self.insert_value_object(root, key, value_oid)
    }

    /// Insert an already-written value object under `key`, returning the new
    /// root.
    ///
    /// This is the oid-in/oid-out path callers with their own value
    /// serialization use: the object must already exist in the repository and
    /// be a tree or a blob; it is referenced as-is, never re-encoded. Passing
    /// `None` builds a fresh tree. When `key` already exists, its value is
    /// replaced; if it is already this object, the root is returned unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] for an invalid key, a missing or unusable value
    /// object, or an invalid configuration.
    pub fn insert_value_object(
        &self,
        root: Option<ObjectId>,
        key: &[u8],
        value_oid: ObjectId,
    ) -> Result<ObjectId, Error> {
        self.config().key_codec.codec().encode(key)?;
        let mode = self.entry_mode_of(value_oid)?;
        if let Some(existing) = root
            && !self.is_empty_root(existing)
            && let Some(root) = self.replace_existing(existing, key, value_oid, mode)?
        {
            return Ok(root);
        }
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

    fn replace_existing(
        &self,
        root: ObjectId,
        key: &[u8],
        value_oid: ObjectId,
        mode: gix::objs::tree::EntryMode,
    ) -> Result<Option<ObjectId>, Error> {
        let entries = self.read_tree(root)?;
        let codec = self.config().key_codec.codec();
        match node::node_kind(&entries) {
            NodeKind::Leaf => {
                let decoded = node::decode_entry_keys(&entries, &codec)?;
                let Ok(index) =
                    decoded.binary_search_by(|(entry_key, _, _)| entry_key.as_slice().cmp(key))
                else {
                    return Ok(None);
                };
                let mut replacement = decoded
                    .iter()
                    .map(|(entry_key, oid, entry_mode)| Entry {
                        key: entry_key.clone(),
                        value_oid: *oid,
                        mode: crate::store::mode_of_kind(*entry_mode),
                    })
                    .collect::<Vec<_>>();
                let entry = replacement
                    .get_mut(index)
                    .ok_or_else(|| Error::Git("binary search index out of range".into()))?;
                if entry.value_oid == value_oid && entry.mode == mode {
                    return Ok(Some(root));
                }
                entry.value_oid = value_oid;
                entry.mode = mode;
                if !same_boundaries(
                    &self.config().chunker(),
                    &decoded
                        .iter()
                        .map(|(entry_key, oid, _)| {
                            chunk::leaf_fingerprint(
                                self.hash_kind(),
                                &codec.encode(entry_key)?,
                                oid,
                            )
                        })
                        .collect::<Result<Vec<_>, Error>>()?,
                    &replacement
                        .iter()
                        .map(|entry| {
                            let encoded = codec.encode(&entry.key)?;
                            chunk::leaf_fingerprint(self.hash_kind(), &encoded, &entry.value_oid)
                        })
                        .collect::<Result<Vec<_>, Error>>()?,
                ) {
                    return Ok(None);
                }
                Ok(Some(
                    self.write_tree(&node::leaf_tree(&codec, &replacement)?)?,
                ))
            }
            NodeKind::Internal => {
                let children = node::decode_entry_keys(&entries, &codec)?;
                let index = children
                    .iter()
                    .rposition(|(separator, _, _)| separator.as_slice() <= key);
                let Some(index) = index else { return Ok(None) };
                let (_, child, _) = children
                    .get(index)
                    .ok_or_else(|| Error::Git("child index out of range".into()))?;
                let Some(replaced) = self.replace_existing(*child, key, value_oid, mode)? else {
                    return Ok(None);
                };
                if replaced == *child {
                    return Ok(Some(root));
                }
                let mut replacement = children
                    .iter()
                    .map(|(separator, child, _)| (separator.clone(), *child))
                    .collect::<Vec<_>>();
                let child = replacement
                    .get_mut(index)
                    .ok_or_else(|| Error::Git("child index out of range".into()))?;
                child.1 = replaced;
                let marker = self.internal_marker_oid()?;
                Ok(Some(self.write_tree(&node::internal_tree(
                    &codec,
                    marker,
                    &replacement,
                )?)?))
            }
        }
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
            let value_oid = self.write_value_text(&value)?;
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
            let fingerprints: Vec<u64> = level
                .iter()
                .map(|(separator, _)| chunk::child_fingerprint(hash_kind, separator))
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
