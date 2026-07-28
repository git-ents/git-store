//! The compare-and-swap edit primitive every [`RefStore`](crate::RefStore)
//! applies.

use std::fmt;

use gix_hash::ObjectId;

use crate::name::RefName;

/// What a ref must currently hold for an edit to apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expectation {
    /// The ref must not exist.
    Absent,
    /// The ref must exist and point at this object.
    Exactly(ObjectId),
}

impl fmt::Display for Expectation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expectation::Absent => f.write_str("absent"),
            Expectation::Exactly(id) => write!(f, "{id}"),
        }
    }
}

/// A compare-and-swap on one ref — the only mutating primitive, so every
/// write states the value it is displacing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefEdit {
    /// Point a ref that does not exist at `new`.
    Create {
        /// The ref to create.
        name: RefName,
        /// The object it must come to point at.
        new: ObjectId,
    },
    /// Move a ref from `expected` to `new`.
    Update {
        /// The ref to move.
        name: RefName,
        /// The object it must currently point at.
        expected: ObjectId,
        /// The object it must come to point at.
        new: ObjectId,
    },
    /// Remove a ref that currently holds `expected`.
    Delete {
        /// The ref to remove.
        name: RefName,
        /// The object it must currently point at.
        expected: ObjectId,
    },
}

impl RefEdit {
    /// The ref this edit touches.
    pub fn name(&self) -> &RefName {
        match self {
            RefEdit::Create { name, .. } => name,
            RefEdit::Update { name, .. } => name,
            RefEdit::Delete { name, .. } => name,
        }
    }

    /// The precondition the backend must verify.
    pub fn expectation(&self) -> Expectation {
        match self {
            RefEdit::Create { .. } => Expectation::Absent,
            RefEdit::Update { expected, .. } | RefEdit::Delete { expected, .. } => {
                Expectation::Exactly(*expected)
            }
        }
    }
}
