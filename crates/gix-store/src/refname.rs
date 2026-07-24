//! Validation of the `<kind>` and `<name>` components that become Git ref
//! path segments.
//!
//! Both are single ref-name components, so the check is stricter than
//! [`facet_git_tree::check_key`]'s (which only forbids `/`): the git
//! ref-format rules that make a segment usable in `refs/store/<kind>/<name>`
//! are enforced here, rejecting rather than escaping — the same posture the
//! rest of the stack takes at its boundaries. `gix` re-validates the assembled
//! ref when it writes, so this is the friendly first line, not the only one.

use crate::Error;

/// Reject a `<kind>` or `<name>` that cannot be a Git ref-name component.
///
/// `what` labels the component for the error message (`"kind"` / `"name"`).
pub(crate) fn check_component(what: &'static str, value: &str) -> Result<(), Error> {
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
    if value.contains('/') {
        return reject("must not contain '/'");
    }
    if value.starts_with('.') || value.ends_with('.') {
        return reject("must not begin or end with '.'");
    }
    if value.ends_with(".lock") {
        return reject("must not end with '.lock'");
    }
    if value.contains("..") || value.contains("@{") {
        return reject("must not contain '..' or '@{'");
    }
    if value == "@" {
        return reject("must not be a lone '@'");
    }
    for c in value.chars() {
        if c.is_ascii_control() || c == ' ' {
            return reject("must not contain control characters or spaces");
        }
        if matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\') {
            return reject("must not contain any of ~^:?*[\\");
        }
    }
    Ok(())
}
