//! Validation of the ref-name pieces the store assembles into full refnames:
//! the `<kind>`/`<name>` components, and the data/schema namespace prefixes
//! those components are nested under.
//!
//! The check is stricter than [`facet_git_tree::check_key`]'s (which only
//! forbids `/`): the git ref-format rules that make a segment usable in an
//! assembled ref like `refs/store/<kind>/<name>` are enforced here, rejecting
//! rather than escaping — the same posture the rest of the stack takes at its
//! boundaries. `gix` re-validates the assembled ref when it writes, so this is
//! the friendly first line, not the only one.

use crate::Error;

/// Why a single `/`-free ref-name segment is unusable, or `None` if it is
/// fine. Shared by [`check_component`] (one segment) and [`check_prefix`]
/// (every segment of a multi-segment prefix).
fn segment_defect(value: &str) -> Option<&'static str> {
    if value.is_empty() {
        return Some("must not be empty");
    }
    if value.starts_with('.') || value.ends_with('.') {
        return Some("must not begin or end with '.'");
    }
    if value.ends_with(".lock") {
        return Some("must not end with '.lock'");
    }
    if value.contains("..") || value.contains("@{") {
        return Some("must not contain '..' or '@{'");
    }
    if value == "@" {
        return Some("must not be a lone '@'");
    }
    for c in value.chars() {
        if c.is_ascii_control() || c == ' ' {
            return Some("must not contain control characters or spaces");
        }
        if matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\') {
            return Some("must not contain any of ~^:?*[\\");
        }
    }
    None
}

/// Reject a `<kind>` or `<name>` that cannot be a Git ref-name component.
///
/// `what` labels the component for the error message (`"kind"` / `"name"`).
pub(crate) fn check_component(what: &'static str, value: &str) -> Result<(), Error> {
    if value.contains('/') {
        return Err(Error::InvalidName {
            what,
            value: value.to_owned(),
            reason: "must not contain '/'",
        });
    }
    match segment_defect(value) {
        Some(reason) => Err(Error::InvalidName {
            what,
            value: value.to_owned(),
            reason,
        }),
        None => Ok(()),
    }
}

/// Reject a ref-namespace prefix (e.g. `refs/meta/rules`, in place of the
/// default `refs/store`) that cannot anchor an assembled
/// `<prefix>/<kind>/<name>` ref: no leading or trailing `/`, no empty
/// segment (so no `//`), and every `/`-separated segment individually valid
/// by [`check_component`]'s own rules.
///
/// `what` labels the prefix for the error message (`"data prefix"` /
/// `"schema prefix"`).
pub(crate) fn check_prefix(what: &'static str, value: &str) -> Result<(), Error> {
    let reject = |reason: &'static str| {
        Err(Error::InvalidName {
            what,
            value: value.to_owned(),
            reason,
        })
    };

    if value.is_empty() {
        return reject("must not be empty");
    }
    if value.starts_with('/') || value.ends_with('/') {
        return reject("must not begin or end with '/'");
    }
    for segment in value.split('/') {
        if let Some(reason) = segment_defect(segment) {
            return reject(reason);
        }
    }
    Ok(())
}
