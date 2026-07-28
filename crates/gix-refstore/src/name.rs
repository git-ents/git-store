//! Validated ref-name pieces: single path segments, whole ref names, and
//! namespace prefixes. Validation runs once, at construction, so every other
//! type in the crate can assume its `RefName`/`RefPrefix`/`RefSegment`
//! arguments are already well-formed Git ref-name material.

use std::fmt;

/// Why a ref name, prefix, or segment was rejected. Not an error itself; the
/// error is [`InvalidRefName`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Violation {
    /// Nothing at all, or nothing between two `/`.
    Empty,
    /// A segment bounded by `.`, as in `.hidden` or `trailing.`.
    BoundaryDot,
    /// A segment ending in `.lock`, the suffix git's own lock files claim.
    LockSuffix,
    /// `..` or `@{`, which git's revision syntax would reinterpret.
    AmbiguousSequence,
    /// A segment that is exactly `@`, git's shorthand for `HEAD`.
    LoneAt,
    /// A control character or a space.
    ControlOrSpace,
    /// One of `~^:?*[\`, all meaningful to git's revision or glob syntax.
    ForbiddenCharacter,
    /// A `/` inside what must be a single segment.
    Separator,
    /// A leading or trailing `/`.
    BoundarySlash,
}

impl Violation {
    /// The rule that was broken, phrased to follow the offending value.
    pub fn as_str(self) -> &'static str {
        match self {
            Violation::Empty => "must not be empty",
            Violation::BoundaryDot => "must not begin or end with '.'",
            Violation::LockSuffix => "must not end with '.lock'",
            Violation::AmbiguousSequence => "must not contain '..' or '@{'",
            Violation::LoneAt => "must not be a lone '@'",
            Violation::ControlOrSpace => "must not contain control characters or spaces",
            Violation::ForbiddenCharacter => "must not contain any of ~^:?*[\\",
            Violation::Separator => "must not contain '/'",
            Violation::BoundarySlash => "must not begin or end with '/'",
        }
    }
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A ref name, prefix, or segment that is not usable as Git ref-name
/// material.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid ref name {value:?}: {violation}")]
pub struct InvalidRefName {
    value: String,
    violation: Violation,
}

impl InvalidRefName {
    /// The value that was rejected.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// The rule it broke.
    pub fn violation(&self) -> Violation {
        self.violation
    }
}

/// The per-segment rules shared by every check below: what makes one
/// `/`-free component usable inside a Git ref name.
fn segment_defect(value: &str) -> Option<Violation> {
    if value.is_empty() {
        return Some(Violation::Empty);
    }
    if value.starts_with('.') || value.ends_with('.') {
        return Some(Violation::BoundaryDot);
    }
    if value.ends_with(".lock") {
        return Some(Violation::LockSuffix);
    }
    if value.contains("..") || value.contains("@{") {
        return Some(Violation::AmbiguousSequence);
    }
    if value == "@" {
        return Some(Violation::LoneAt);
    }
    for c in value.chars() {
        if c.is_ascii_control() || c == ' ' {
            return Some(Violation::ControlOrSpace);
        }
        if matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\') {
            return Some(Violation::ForbiddenCharacter);
        }
    }
    None
}

/// Validation shared by [`RefName`] and [`RefPrefix`]: a non-empty,
/// non-`/`-bounded string whose every `/`-separated segment passes
/// [`segment_defect`].
fn check_path(value: &str) -> Result<(), Violation> {
    if value.is_empty() {
        return Err(Violation::Empty);
    }
    if value.starts_with('/') || value.ends_with('/') {
        return Err(Violation::BoundarySlash);
    }
    for segment in value.split('/') {
        if let Some(violation) = segment_defect(segment) {
            return Err(violation);
        }
    }
    Ok(())
}

fn check_segment(value: &str) -> Result<(), Violation> {
    if value.contains('/') {
        return Err(Violation::Separator);
    }
    match segment_defect(value) {
        Some(violation) => Err(violation),
        None => Ok(()),
    }
}

/// One `/`-free ref-name component: a `<kind>` or `<name>` piece of an
/// assembled ref.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RefSegment(String);

impl RefSegment {
    /// Validates `value` against the Git ref-name segment rules.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidRefName> {
        let value = value.into();
        match check_segment(&value) {
            Ok(()) => Ok(Self(value)),
            Err(violation) => Err(InvalidRefName { value, violation }),
        }
    }

    /// The validated segment text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RefSegment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for RefSegment {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for RefSegment {
    type Err = InvalidRefName;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<&str> for RefSegment {
    type Error = InvalidRefName;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for RefSegment {
    type Error = InvalidRefName;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// A whole ref, one or more `/`-separated segments (e.g.
/// `refs/store/recipe/carbonara`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RefName(String);

impl RefName {
    /// Validates `value` against the Git ref-name rules.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidRefName> {
        let value = value.into();
        match check_path(&value) {
            Ok(()) => Ok(Self(value)),
            Err(violation) => Err(InvalidRefName { value, violation }),
        }
    }

    /// The validated ref text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `<self>/<segment>`.
    pub fn join(&self, segment: &RefSegment) -> RefName {
        RefName(format!("{}/{}", self.0, segment.0))
    }

    /// What `self` names under `prefix`, or `None` when it is not under it.
    /// The result may itself contain `/` when the ref is nested deeper.
    pub fn strip_prefix(&self, prefix: &RefPrefix) -> Option<&str> {
        self.0.strip_prefix(prefix.0.as_str())?.strip_prefix('/')
    }
}

impl fmt::Display for RefName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for RefName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for RefName {
    type Err = InvalidRefName;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<&str> for RefName {
    type Error = InvalidRefName;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for RefName {
    type Error = InvalidRefName;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// A ref namespace, one or more `/`-separated segments (e.g.
/// `refs/store/recipe`), used to list what lives under it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RefPrefix(String);

impl RefPrefix {
    /// Validates `value` against the Git ref-name rules.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidRefName> {
        let value = value.into();
        match check_path(&value) {
            Ok(()) => Ok(Self(value)),
            Err(violation) => Err(InvalidRefName { value, violation }),
        }
    }

    /// The validated prefix text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The ref `<self>/<segment>`.
    pub fn join(&self, segment: &RefSegment) -> RefName {
        RefName(format!("{}/{}", self.0, segment.0))
    }

    /// The namespace `<self>/<segment>`, for listing what lives under it.
    pub fn child(&self, segment: &RefSegment) -> RefPrefix {
        RefPrefix(format!("{}/{}", self.0, segment.0))
    }
}

impl fmt::Display for RefPrefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for RefPrefix {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for RefPrefix {
    type Err = InvalidRefName;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<&str> for RefPrefix {
    type Error = InvalidRefName;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for RefPrefix {
    type Error = InvalidRefName;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
