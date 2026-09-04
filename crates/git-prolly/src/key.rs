//! Order-preserving key encoding.
//!
//! Git tree entry names are compared bytewise, with directory entries compared
//! as if suffixed by `/`. Leaf nodes mix both entry kinds (a value may be a
//! tree or a blob), so the key encoding must make bytewise name order equal
//! logical key order **and** keep mixed file/tree comparisons unambiguous.
//! Lowercase hexadecimal satisfies both: every hex digit is above `/` (0x2F),
//! so for any two hex names a plain bytewise comparison and Git's
//! directory-aware comparison produce the same order, and the encoding is
//! monotonic in the raw key.
//!
//! Hexadecimal names also sit entirely above the reserved internal-marker name
//! `!`, so user keys can never collide with format metadata.

use crate::error::KeyError;

/// The maximum accepted encoded key length in bytes.
///
/// Git tree entry names are practically bounded by path-length limits. Longer
/// keys are rejected explicitly; a longer-key format extension must be designed
/// as one rather than introduced as an implicit lossy encoding.
pub const MAX_ENCODED_KEY_LEN: usize = 4096;

/// The maximum accepted raw key length implied by [`MAX_ENCODED_KEY_LEN`].
pub const MAX_KEY_LEN: usize = MAX_ENCODED_KEY_LEN / 2;

/// An order-preserving mapping between raw keys and Git tree entry names.
///
/// Implementations must be injective and order-preserving: for keys `a < b`
/// (bytewise), `encode(a) < encode(b)` under both plain bytewise ordering and
/// Git's tree-entry ordering, and `decode(encode(k)) == k`.
pub trait KeyCodec {
    /// Encode a raw key into a Git tree entry name.
    ///
    /// # Errors
    ///
    /// Returns [`KeyError`] for keys that cannot be represented.
    fn encode(&self, key: &[u8]) -> Result<Vec<u8>, KeyError>;

    /// Decode a Git tree entry name back into a raw key.
    ///
    /// # Errors
    ///
    /// Returns [`KeyError`] for names this codec cannot have produced.
    fn decode(&self, encoded: &[u8]) -> Result<Vec<u8>, KeyError>;
}

/// The key codec a [`ProllyConfig`](crate::ProllyConfig) selects.
///
/// The initial format ships exactly one codec. The variant exists so the
/// configuration can name the encoding explicitly — the encoding participates
/// in canonical identity — and so future codecs (for example base32hex, once
/// proven order-preserving for Git's directory-aware comparison) can be added
/// without changing the configuration shape.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum KeyCodecKind {
    /// Lowercase hexadecimal; the initial format's codec.
    #[default]
    Hex,
}

impl KeyCodecKind {
    /// The codec implementation this kind selects.
    #[must_use]
    pub const fn codec(self) -> HexKeyCodec {
        HexKeyCodec
    }
}

/// Lowercase hexadecimal key encoding.
///
/// Empty keys are rejected (Git tree entry names must be non-empty), keys are
/// rejected above [`MAX_KEY_LEN`], and decoding validates the hexadecimal
/// alphabet and length. No key is ever truncated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HexKeyCodec;

/// The lowercase hexadecimal digit for a nibble in `0..16`.
const fn hex_char(nibble: u8) -> u8 {
    match nibble {
        0..=9 => b'0' + nibble,
        10..=15 => b'a' + nibble - 10,
        _ => b'0',
    }
}

impl KeyCodec for HexKeyCodec {
    fn encode(&self, key: &[u8]) -> Result<Vec<u8>, KeyError> {
        if key.is_empty() {
            return Err(KeyError::Empty);
        }
        let encoded_len = key.len() * 2;
        if encoded_len > MAX_ENCODED_KEY_LEN {
            return Err(KeyError::TooLong {
                len: key.len(),
                encoded: encoded_len,
                max: MAX_ENCODED_KEY_LEN,
            });
        }
        let mut out = Vec::with_capacity(encoded_len);
        for byte in key {
            out.push(hex_char(byte >> 4));
            out.push(hex_char(byte & 0x0f));
        }
        Ok(out)
    }

    fn decode(&self, encoded: &[u8]) -> Result<Vec<u8>, KeyError> {
        if encoded.is_empty() {
            return Err(KeyError::Empty);
        }
        if !encoded.len().is_multiple_of(2) {
            return Err(KeyError::OddLength(encoded.len()));
        }
        if encoded.len() > MAX_ENCODED_KEY_LEN {
            return Err(KeyError::TooLong {
                len: encoded.len() / 2,
                encoded: encoded.len(),
                max: MAX_ENCODED_KEY_LEN,
            });
        }
        let mut out = Vec::with_capacity(encoded.len() / 2);
        for pair in encoded.chunks_exact(2) {
            let high = hex_digit(pair.first().copied().unwrap_or_default())
                .ok_or_else(|| KeyError::InvalidEncoding(encoded.to_vec()))?;
            let low = hex_digit(pair.last().copied().unwrap_or_default())
                .ok_or_else(|| KeyError::InvalidEncoding(encoded.to_vec()))?;
            out.push((high << 4) | low);
        }
        Ok(out)
    }
}

/// The value of one lowercase hexadecimal digit, or `None` if not a hex digit.
fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::indexing_slicing,
        clippy::assertions_on_result_states,
        reason = "unit tests index and assert on results deliberately"
    )]

    use super::{HexKeyCodec, KeyCodec, MAX_ENCODED_KEY_LEN, MAX_KEY_LEN};

    #[test]
    fn hex_encode_decode_round_trips() {
        let codec = HexKeyCodec;
        for key in [
            &[0x00_u8][..],
            &[0x0f],
            &[0xff],
            b"alice",
            &[0x00, 0x01, 0xfe, 0xff],
        ] {
            let encoded = codec.encode(key).expect("encode");
            assert_eq!(encoded.len(), key.len() * 2);
            assert_eq!(codec.decode(&encoded).expect("decode"), key);
        }
    }

    #[test]
    fn hex_encoding_preserves_order() {
        let codec = HexKeyCodec;
        let keys: Vec<Vec<u8>> = vec![
            vec![0x00],
            vec![0x00, 0x00],
            vec![0x00, 0x01],
            vec![0x0e, 0xff],
            vec![0x0f, 0x00],
            vec![0x10],
            vec![0xab, 0xcd],
            vec![0xab, 0xcd, 0x00],
            vec![0xff, 0xff],
        ];
        for pair in keys.windows(2) {
            let a = codec.encode(&pair[0]).expect("encode a");
            let b = codec.encode(&pair[1]).expect("encode b");
            assert!(a < b, "{a:?} must sort below {b:?}");
        }
    }

    #[test]
    fn rejects_empty_and_oversized_keys() {
        let codec = HexKeyCodec;
        assert!(codec.encode(b"").is_err());
        assert!(codec.encode(&vec![0x00; MAX_KEY_LEN + 1]).is_err());
        let at_limit = codec.encode(&vec![0x61; MAX_KEY_LEN]).expect("encode");
        assert_eq!(at_limit.len(), MAX_ENCODED_KEY_LEN);
    }

    #[test]
    fn rejects_invalid_decodings() {
        let codec = HexKeyCodec;
        assert!(codec.decode(b"").is_err());
        assert!(codec.decode(b"abc").is_err());
        assert!(codec.decode(b"zz").is_err());
        assert!(codec.decode(b"AB").is_err(), "uppercase is rejected");
        assert!(codec.decode(b"00ab").is_ok());
    }
}
