//! Repository-level validation independent of any CLI argument parsing.

use facet_git_tree::{ObjectId, doctor};

use crate::error::Error;

/// The Git object hash algorithm this build's plumbing writes in.
pub const SUPPORTED_OBJECT_FORMAT: &str = "sha1";

/// The result of a passing [`check_repository`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DoctorReport {
    /// Always [`SUPPORTED_OBJECT_FORMAT`]; carried so a caller can report the
    /// repository format without re-deriving it from the inputs that passed.
    pub object_format: &'static str,
    /// The verified SHA-1 id of `S0`, the schema describing `Schema` itself.
    pub s0_sha1: ObjectId,
    /// The verified SHA-256 id of `S0`, the same value under the other format.
    pub s0_sha256: ObjectId,
}

/// Validate the invariants `git-store` depends on, and prove the codec's own
/// fixed point.
///
/// Two checks run in order. First, the repository's object format must be one
/// this build's plumbing can write: `observed` is the repository's object hash
/// algorithm and `configured_sha256` whether its config explicitly names
/// `sha256`. Callers must supply both, because a `gix` build without SHA-256
/// support reports a SHA-256 repository's `object_hash()` as SHA-1, and the raw
/// config setting is then the only way to catch it.
///
/// Second, the schema-of-schemas fixed point is verified through
/// [`facet_git_tree::doctor::check`]: the compiled-in `S0` must encode to its
/// pinned id and decode and re-encode to itself, in *both* object formats.
/// That check runs in an in-memory object store, so it is independent of the
/// repository being inspected.
pub fn check_repository(
    observed: gix::hash::Kind,
    configured_sha256: bool,
) -> Result<DoctorReport, Error> {
    if observed != gix::hash::Kind::Sha1 || configured_sha256 {
        let observed = if configured_sha256 {
            "sha256".to_owned()
        } else {
            // `Kind`'s debug spelling is `Sha1`/`Sha256`; the error reads like
            // the config value it stands for.
            format!("{observed:?}").to_ascii_lowercase()
        };
        return Err(Error::UnsupportedObjectFormat { observed });
    }
    let fixed_point = doctor::check()?;
    Ok(DoctorReport {
        object_format: SUPPORTED_OBJECT_FORMAT,
        s0_sha1: fixed_point.sha1,
        s0_sha256: fixed_point.sha256,
    })
}
