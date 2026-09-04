//! Deterministic content-defined chunking.
//!
//! Chunk boundaries are a pure function of the ordered entry fingerprints and
//! the [`ProllyConfig`]: boundary after entry `i` when
//! `fingerprint(i) & mask == 0`, suppressed within `min_entries` of the
//! previous boundary, and forced once a chunk reaches `max_entries`. The same
//! entry sequence and configuration therefore always produce the same
//! boundaries, on every level.
//!
//! Fingerprints are Git object hashes (with the repository's own hash kind) of
//! an explicitly specified canonical tuple, never a hash of an in-memory Rust
//! value:
//!
//! * leaf level: `H(encoded_key || value_object_id)`
//! * parent levels: `H(child_node_oid)`
//!
//! Both are length-unambiguous because `encoded_key` has even length and both
//! sides of the tuple are self-delimiting in the concatenation.
//!
//! This is an entry-granularity rolling-window scheme rather than a
//! byte-level rolling hash (BuzHash): the unit of chunking is the entry, so a
//! per-entry content hash gives the same content-defined behavior with less
//! machinery, and boundary decisions depend only on entry content, never on
//! Rust-side serialization.

use gix::ObjectId;
use gix::objs::Kind;

use crate::error::Error;

/// The number of levels a walk may descend before a structure is rejected.
///
/// Valid trees are logarithmic in any practical entry count; this bound exists
/// to keep hostile object graphs from exhausting the stack.
pub(crate) const MAX_LEVELS: usize = 64;

/// A deterministic boundary decider over ordered entry fingerprints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Chunker {
    bits: u8,
    min_entries: usize,
    max_entries: usize,
}

impl Chunker {
    /// Assemble a chunker from validated configuration parameters.
    pub(crate) const fn new(bits: u8, min_entries: usize, max_entries: usize) -> Self {
        Self {
            bits,
            min_entries,
            max_entries,
        }
    }

    /// The exclusive end offsets of the chunks over `fingerprints`.
    ///
    /// Returns one end per chunk; chunk starts are the previous end (starting
    /// at zero) and the ends are strictly increasing, ending at
    /// `fingerprints.len()`. An empty input produces no chunks.
    pub(crate) fn boundaries(&self, fingerprints: &[u64]) -> Vec<usize> {
        let mask = if self.bits == 0 {
            0
        } else {
            (1_u64 << self.bits) - 1
        };
        let mut ends = Vec::new();
        let mut start = 0;
        for (index, fingerprint) in fingerprints.iter().copied().enumerate() {
            let length = index + 1 - start;
            let boundary = fingerprint & mask == 0;
            if (length >= self.min_entries && boundary) || length == self.max_entries {
                ends.push(index + 1);
                start = index + 1;
            }
        }
        if start < fingerprints.len() {
            ends.push(fingerprints.len());
        }
        ends
    }
}

/// The leaf-level fingerprint of one entry.
///
/// The tuple is `encoded_key || value_object_id`; the value identity used for
/// chunking is the Git object id facet-git-tree produced, never a direct hash
/// of a Rust value.
pub(crate) fn leaf_fingerprint(
    hash_kind: gix::hash::Kind,
    encoded_key: &[u8],
    value_oid: &ObjectId,
) -> Result<u64, Error> {
    let mut data = Vec::with_capacity(encoded_key.len() + hash_kind.len_in_bytes());
    data.extend_from_slice(encoded_key);
    data.extend_from_slice(value_oid.as_bytes());
    fingerprint(hash_kind, &data)
}

/// The parent-level fingerprint of one child node.
///
/// The tuple is the child node's own Git object id: `H(child_oid)`.
pub(crate) fn child_fingerprint(hash_kind: gix::hash::Kind, child_oid: &ObjectId) -> u64 {
    child_fingerprint_result(hash_kind, child_oid)
        .unwrap_or_else(|_| unreachable_fingerprint(hash_kind))
}

/// [`child_fingerprint`] for callers that can propagate errors.
pub(crate) fn child_fingerprint_result(
    hash_kind: gix::hash::Kind,
    child_oid: &ObjectId,
) -> Result<u64, Error> {
    fingerprint(hash_kind, child_oid.as_bytes())
}

/// Hash a canonical tuple with the repository's object hash and compress the
/// digest to a 64-bit fingerprint.
fn fingerprint(hash_kind: gix::hash::Kind, data: &[u8]) -> Result<u64, Error> {
    let digest =
        gix::objs::compute_hash(hash_kind, Kind::Blob, data).map_err(crate::error::Error::git)?;
    let bytes = digest.as_bytes();
    let mut word = [0_u8; 8];
    let head = bytes
        .get(..8)
        .ok_or_else(|| Error::Git("object hash shorter than 8 bytes".into()))?;
    word.copy_from_slice(head);
    Ok(u64::from_le_bytes(word))
}

/// A hash shorter than a fingerprint cannot occur for supported hash kinds;
/// this keeps the infallible parent-level fingerprint total.
fn unreachable_fingerprint(_hash_kind: gix::hash::Kind) -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::Chunker;

    fn chunker(bits: u8, min: usize, max: usize) -> Chunker {
        Chunker::new(bits, min, max)
    }

    fn fingerprints(n: usize, seed: u64) -> Vec<u64> {
        let mut state = seed;
        (0..n)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                (state >> 11) | 1
            })
            .collect()
    }

    #[test]
    fn boundaries_are_deterministic() {
        let fps = fingerprints(1000, 7);
        let first = chunker(4, 4, 64).boundaries(&fps);
        let second = chunker(4, 4, 64).boundaries(&fps);
        assert_eq!(first, second);
        assert!(!first.is_empty());
        assert_eq!(*first.last().expect("non-empty"), fps.len());
    }

    #[test]
    fn boundaries_respect_min_and_max() {
        let fps = fingerprints(2000, 11);
        let ends = chunker(1, 8, 32).boundaries(&fps);
        let mut start = 0;
        let mut last_index = ends.len() - 1;
        for (index, end) in ends.iter().copied().enumerate() {
            let len = end - start;
            assert!(len <= 32, "chunk {index} exceeded max_entries");
            if index < last_index {
                assert!(len >= 8, "non-final chunk {index} below min_entries");
            }
            start = end;
        }
        last_index = ends.len();
        let _ = last_index;
    }

    #[test]
    fn boundaries_cover_the_whole_sequence() {
        for n in [0, 1, 2, 3, 5, 17, 100] {
            let fps = fingerprints(n, 3);
            let ends = chunker(4, 4, 64).boundaries(&fps);
            let mut start = 0;
            for end in &ends {
                assert!(*end > start);
                start = *end;
            }
            assert_eq!(start, n);
        }
    }

    #[test]
    fn max_clamp_bounds_oversized_chunks() {
        let fps = fingerprints(500, 5);
        let ends = chunker(16, 2, 40).boundaries(&fps);
        let mut start = 0;
        for end in &ends {
            assert!(end - start <= 40);
            start = *end;
        }
    }
}
