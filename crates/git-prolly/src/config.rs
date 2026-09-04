//! Chunking and key-encoding configuration.

use crate::chunk::Chunker;
use crate::error::ConfigError;
use crate::key::KeyCodecKind;

/// The chunking and key-encoding parameters of a Prolly tree.
///
/// All fields participate in canonical identity: two trees are only comparable
/// when built with the same configuration, and
/// [`ProllyStore::verify`](crate::ProllyStore::verify) checks a tree against
/// the configuration it is verified with.
///
/// The default (`bits = 4`, `min_entries = 4`, `max_entries = 64`) targets an
/// average of 16 entries per node; these defaults are provisional until the
/// benchmarks in `benches/` justify different values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProllyConfig {
    /// Chunk boundaries form where the entry fingerprint has its low `bits`
    /// bits all zero, giving an expected chunk size of `2^bits` entries.
    pub bits: u8,
    /// The fewest entries a non-final chunk may hold. Boundaries inside this
    /// distance of the previous boundary are suppressed, so intermediate
    /// chunks are never smaller than this.
    pub min_entries: usize,
    /// The most entries a chunk may hold. A chunk reaching this size is cut
    /// regardless of its fingerprint, bounding node size.
    pub max_entries: usize,
    /// The key codec; participates in canonical identity.
    pub key_codec: KeyCodecKind,
}

impl Default for ProllyConfig {
    fn default() -> Self {
        Self {
            bits: 4,
            min_entries: 4,
            max_entries: 64,
            key_codec: KeyCodecKind::default(),
        }
    }
}

impl ProllyConfig {
    /// Reject configurations whose parameters cannot produce a valid tree.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when `bits` exceeds the 31-bit fingerprint
    /// mask, `min_entries` is zero, or `max_entries` is below `min_entries`.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.bits > 31 {
            return Err(ConfigError::BitsTooLarge(self.bits));
        }
        if self.min_entries < 1 {
            return Err(ConfigError::MinTooSmall(self.min_entries));
        }
        if self.max_entries < self.min_entries {
            return Err(ConfigError::MaxBelowMin {
                max: self.max_entries,
                min: self.min_entries,
            });
        }
        Ok(())
    }

    /// The chunker this configuration describes.
    pub(crate) const fn chunker(&self) -> Chunker {
        Chunker::new(self.bits, self.min_entries, self.max_entries)
    }
}
