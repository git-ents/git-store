//! Reads: point lookup, typed lookup, and ordered iteration.
//!
//! Lookup descends the Git tree without materializing unrelated subtrees. At
//! each internal node it finds the greatest separator key at or below the
//! requested key and follows that child; at the leaf it finds the exact key.
//! A lookup touches `root → … → leaf → requested value` and nothing else, and
//! deserializes only the requested value.

use facet_value::Value;
use gix::ObjectId;
use gix::bstr::ByteSlice;
use gix::objs::tree::Entry as TreeEntry;

use crate::chunk::MAX_LEVELS;
use crate::error::Error;
use crate::key::KeyCodec;
use crate::node::{self, INTERNAL_MARKER_NAME, NodeKind};
use crate::store::ProllyStore;

impl ProllyStore<'_> {
    /// Look up the value object id stored under `key`, without reading it.
    ///
    /// This is the deserialization-free read everything else layers onto.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] when a node cannot be read or a key cannot be encoded.
    pub fn get_oid(&self, root: ObjectId, key: &[u8]) -> Result<Option<ObjectId>, Error> {
        let encoded = self.config().key_codec.codec().encode(key)?;
        let empty = self.empty_root();
        let mut node = root;
        let mut depth = 0;
        loop {
            if node == empty {
                return Ok(None);
            }
            depth += 1;
            if depth > MAX_LEVELS {
                return Err(Error::TooDeep {
                    root,
                    max: MAX_LEVELS,
                });
            }
            let entries = self.read_tree(node)?;
            match node::node_kind(&entries) {
                NodeKind::Internal => {
                    let children = entries.get(1..).unwrap_or_default();
                    // Names are hexadecimal, so bytewise name order is key
                    // order: the greatest separator at or below the encoded
                    // key owns the key's range.
                    let index = children
                        .partition_point(|entry| entry.filename.as_bytes() <= encoded.as_slice());
                    match index.checked_sub(1).and_then(|index| children.get(index)) {
                        Some(entry) => node = entry.oid,
                        None => return Ok(None),
                    }
                }
                NodeKind::Leaf => {
                    let index = entries
                        .partition_point(|entry| entry.filename.as_bytes() < encoded.as_slice());
                    return Ok(entries.get(index).and_then(|entry| {
                        (entry.filename.as_bytes() == encoded.as_slice()).then_some(entry.oid)
                    }));
                }
            }
        }
    }

    /// Look up the value stored under `key` as a dynamic [`Value`].
    ///
    /// Only the requested value's object graph is deserialized.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] on tree-walk or facet-git-tree deserialization
    /// failures.
    pub fn get(&self, root: ObjectId, key: &[u8]) -> Result<Option<Value>, Error> {
        match self.get_oid(root, key)? {
            None => Ok(None),
            Some(value_oid) => self.read_value_text(value_oid).map(Some),
        }
    }

    /// Look up the value stored under `key`, decoded into a `Facet` type.
    ///
    /// The value's JSON text leaf is decoded directly into `T` through
    /// Facet reflection.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] on tree-walk or decoding failures, including when
    /// the stored value does not match `T`.
    pub fn get_as<T>(&self, root: ObjectId, key: &[u8]) -> Result<Option<T>, Error>
    where
        T: for<'a> facet::Facet<'a>,
    {
        match self.get_oid(root, key)? {
            None => Ok(None),
            Some(value_oid) => {
                let value = self.read_value_text(value_oid)?;
                let text = facet_json::to_string(&value)
                    .map_err(|error| Error::ValueJson(error.to_string()))?;
                facet_json::from_str(&text)
                    .map(Some)
                    .map_err(|error| Error::ValueJson(error.to_string()))
            }
        }
    }

    /// Iterate every `(key, value)` pair in key order.
    ///
    /// Values are deserialized lazily, one at a time, as the iterator advances.
    /// A read error terminates the iterator after yielding the error.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] when the root cannot be read.
    #[expect(
        clippy::iter_not_returning_iterator,
        reason = "the crate API mandates the name `iter`; the Result wrapper is part of it"
    )]
    pub fn iter(&self, root: ObjectId) -> Result<ProllyIter<'_, '_>, Error> {
        let mut iter = ProllyIter {
            store: self,
            stack: Vec::new(),
            done: false,
        };
        if !self.is_empty_root(root) {
            iter.push_frame(root)?;
        }
        Ok(iter)
    }
}

/// An owning-walk iterator over a tree's key/value pairs in key order.
///
/// The stack holds one frame per level: the node's entries plus the index of
/// the next entry to consume. Leaf frames start at their first entry;
/// internal frames start past the marker, and internal frame indices address
/// `entries` directly (so index 1 is the first child, never the marker).
#[derive(Debug)]
pub struct ProllyIter<'a, 'repo> {
    store: &'a ProllyStore<'repo>,
    stack: Vec<Frame>,
    done: bool,
}

#[derive(Debug)]
struct Frame {
    entries: Vec<TreeEntry>,
    index: usize,
}

impl ProllyIter<'_, '_> {
    /// Push `oid`'s frame onto the stack.
    fn push_frame(&mut self, oid: ObjectId) -> Result<(), Error> {
        let entries = self.store.read_tree(oid)?;
        let index = usize::from(node::node_kind(&entries) == NodeKind::Internal);
        self.stack.push(Frame { entries, index });
        Ok(())
    }
}

impl Iterator for ProllyIter<'_, '_> {
    type Item = Result<(Vec<u8>, Value), Error>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.done {
                return None;
            }
            // Position the walk: descend internal frames and pop exhausted
            // frames until a leaf frame with a remaining entry is on top.
            loop {
                let top = match self.stack.last_mut() {
                    None => {
                        self.done = true;
                        return None;
                    }
                    Some(top) => top,
                };
                match node::node_kind(&top.entries) {
                    NodeKind::Leaf => {
                        if top.index < top.entries.len() {
                            break;
                        }
                        self.stack.pop();
                    }
                    NodeKind::Internal => {
                        // Frame indices address `entries` directly; internal
                        // frames start at 1 so the marker is never consumed.
                        if top.index < top.entries.len() {
                            let Some(child) = top.entries.get(top.index).map(|entry| entry.oid)
                            else {
                                self.done = true;
                                return Some(Err(Error::Git("child index out of range".into())));
                            };
                            top.index += 1;
                            if let Err(error) = self.push_frame(child) {
                                self.done = true;
                                return Some(Err(error));
                            }
                        } else {
                            self.stack.pop();
                        }
                    }
                }
            }
            // Consume one entry of the leaf on top.
            let (name, oid) = match self.stack.last_mut() {
                Some(frame) => match frame.entries.get(frame.index) {
                    Some(entry) => {
                        let name = entry.filename.clone();
                        let oid = entry.oid;
                        frame.index += 1;
                        (name, oid)
                    }
                    None => {
                        self.done = true;
                        return Some(Err(Error::Git("leaf index out of range".into())));
                    }
                },
                None => {
                    self.done = true;
                    return None;
                }
            };
            if name.as_bytes() == INTERNAL_MARKER_NAME {
                continue;
            }
            let codec = self.store.config().key_codec.codec();
            let key = match codec.decode(name.as_bytes()) {
                Ok(key) => key,
                Err(error) => {
                    self.done = true;
                    return Some(Err(Error::Key(error)));
                }
            };
            let value = self.store.read_value_text(oid);
            return match value {
                Ok(value) => Some(Ok((key, value))),
                Err(error) => {
                    self.done = true;
                    Some(Err(error))
                }
            };
        }
    }
}
