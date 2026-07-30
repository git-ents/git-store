//! The signing seam: [`Signer`] produces opaque [`SignatureBytes`] a store
//! carries on its writes and never interprets.
//!
//! Adjacent to, and deliberately distinct from, [`Committer`](crate::Committer):
//! a committer supplies the git author/committer identity stamped on an object,
//! which is metadata anyone may write; a signer supplies the bytes that make a
//! ref transition authoritative. Verification is not here and never will be —
//! this crate has no notion of a key, a signature format, or validity. It moves
//! bytes.

/// A signature, as bytes and nothing else.
///
/// The type is deliberately featureless: there is no accessor that could imply
/// a format, a key, or a verdict, because whichever of those the bytes encode is
/// the signer's and the verifier's business, not the store's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureBytes(Vec<u8>);

impl SignatureBytes {
    /// The bytes, exactly as the [`Signer`] produced them.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// How many bytes the signature holds.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the signature holds no bytes at all.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The bytes, moved out.
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

impl From<Vec<u8>> for SignatureBytes {
    fn from(bytes: Vec<u8>) -> Self {
        SignatureBytes(bytes)
    }
}

/// Produces the signature bytes covering a write.
///
/// The contract is symmetric and total: the caller passes the exact bytes the
/// signature is to cover, the signer returns bytes covering exactly those, and
/// neither side interprets the other's. A store passes the canonical bytes of
/// the object it is about to write, as it would serialize it with no signature
/// present, and stores the result verbatim.
pub trait Signer {
    /// A failure to produce a signature — an absent key, an unreachable agent,
    /// a declined prompt.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Sign `bytes`.
    fn sign(&self, bytes: &[u8]) -> Result<SignatureBytes, Self::Error>;
}

/// A [`Signer`] with its error type erased.
///
/// What lets a store hold *some* signer — configured once, at construction —
/// without every type and call site downstream of it becoming generic over
/// which one. The blanket implementation covers every [`Signer`], so a caller
/// never names this trait.
pub trait ErasedSigner {
    /// [`Signer::sign`], with the error boxed.
    fn sign_erased(
        &self,
        bytes: &[u8],
    ) -> Result<SignatureBytes, Box<dyn std::error::Error + Send + Sync + 'static>>;
}

impl<S: Signer> ErasedSigner for S {
    fn sign_erased(
        &self,
        bytes: &[u8],
    ) -> Result<SignatureBytes, Box<dyn std::error::Error + Send + Sync + 'static>> {
        Signer::sign(self, bytes).map_err(Into::into)
    }
}
