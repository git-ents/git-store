//! Repository-level validation independent of any CLI argument parsing.

use crate::error::Error;

/// The only Git object hash algorithm this build's schema codec is written for.
pub const SUPPORTED_OBJECT_FORMAT: &str = "sha1";

/// The result of a passing [`check_repository`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DoctorReport {
    /// Always [`SUPPORTED_OBJECT_FORMAT`]; carried so a caller can report it
    /// without re-deriving it from the inputs that passed.
    pub object_format: &'static str,
}

/// Validate the repository-level invariants gix-store depends on, namely the
/// object format this build's schema codec assumes.
///
/// `observed` is the repository's object hash algorithm and
/// `configured_sha256` whether its config explicitly names `sha256`.
/// Callers must supply both: a `gix` build without SHA-256 support reports a
/// SHA-256 repository's `object_hash()` as SHA-1, so the raw config setting
/// is the only way to catch it.
pub fn check_repository(
    observed: gix::hash::Kind,
    configured_sha256: bool,
) -> Result<DoctorReport, Error> {
    if observed != gix::hash::Kind::Sha1 || configured_sha256 {
        let observed = if configured_sha256 {
            "sha256".to_owned()
        } else {
            format!("{observed:?}")
        };
        return Err(Error::UnsupportedObjectFormat { observed });
    }
    Ok(DoctorReport {
        object_format: SUPPORTED_OBJECT_FORMAT,
    })
}
