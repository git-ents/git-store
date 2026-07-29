//! The lens vocabulary: the data describing one schema edge (schema A -> schema B).
//!
//! A migration is DATA, never Rust code: it is an ordinary [`Facet`] value
//! storable through this crate's own tree encoding, exactly as
//! [`SchemaDoc`](crate::SchemaDoc) is self-hosted. The vocabulary is
//! deliberately tiny — every operator here is semantics every consumer, in
//! every language, must implement forever. Migration is read-time upcast,
//! never rewrite: nothing here ever produces a new stored value tree from an
//! old one.

#[cfg(feature = "value")]
pub mod apply;
pub mod attr;
pub mod derive;
pub mod pin;

use std::collections::BTreeMap;

use facet::Facet;

/// An edge from one schema to another: the operations applied, in document
/// order, at each occurrence of their target.
#[derive(Debug, Clone, Default, PartialEq, Facet)]
pub struct Migration {
    /// The operations, in application order.
    pub ops: Vec<Op>,
}

/// One operation, addressed at a definition.
#[derive(Debug, Clone, PartialEq, Facet)]
pub struct Op {
    /// The definition or struct variant this operation applies to.
    pub at: Target,
    /// The edit itself.
    pub change: Change,
}

/// Where an [`Op`] applies: a definition, not a root-relative path.
///
/// A type used in two places, or a recursive type, changes at every
/// occurrence, which a root path cannot express.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Facet)]
#[repr(u8)]
pub enum Target {
    /// A named definition in the document's `defs` table.
    Def(String),
    /// A struct variant of an enum definition.
    Variant { def: String, variant: String },
}

/// One field-level edit at a [`Target`].
///
/// [`Migration::ops`] is an ordered sequence applied in document order at
/// each occurrence of its target, so composing two edges is concatenating
/// their operation lists, and conflicting operations are well-defined rather
/// than an error.
#[derive(Debug, Clone, PartialEq, Facet)]
#[repr(u8)]
pub enum Change {
    /// The target's field `to` holds what the source's field `from` held.
    Rename { from: String, to: String },
    /// The target has a field the source lacks; every upcast value takes
    /// `default`.
    Add { field: String, default: Constant },
    /// The source has a field the target lacks; the upcast drops it.
    Remove { field: String },
    /// The target's field schema is `Optional` of the source's.
    ///
    /// This is the identity on a dynamic value: `Schema::Optional` reads as
    /// `null` or the inner value directly (see `schema/read.rs`), so
    /// `Some(x)` and `x` are the same `Value`. The operator exists anyway
    /// because it records the *encoding* change — a `some/` tree entry
    /// appears — for consumers working at the tree altitude, and because
    /// without it the single most common schema evolution after
    /// add/remove/rename would be unclassifiable by derivation.
    Wrap { field: String },
}

/// The closed JSON data model, so a default is interpretable by a consumer
/// with no Rust type in hand.
///
/// `Integer` is `i64`; defaults outside that range are out of scope.
#[derive(Debug, Clone, PartialEq, Facet)]
#[repr(u8)]
pub enum Constant {
    /// The absent value: `None`, or a `Unit`.
    Null,
    /// A boolean.
    Bool(bool),
    /// An integer, at any width the schema node accepts.
    Integer(i64),
    /// A floating-point number.
    Float(f64),
    /// A string, or any scalar with a textual form.
    Text(String),
    /// A sequence.
    List(Vec<Constant>),
    /// A name-keyed composite: a struct, or a scalar-keyed map.
    Object(BTreeMap<String, Constant>),
}

/// Author-supplied facts a schema diff cannot contain: which {removed, added}
/// field pair is a rename, and what an added field defaults to.
///
/// Input to derivation, not a stored artifact — deliberately does not derive
/// [`Facet`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hints {
    /// Target-side field name -> source-side field name, per target.
    renames: BTreeMap<Target, BTreeMap<String, String>>,
    /// Target-side field name -> default, per target.
    defaults: BTreeMap<Target, BTreeMap<String, Constant>>,
}

impl Hints {
    /// An empty hint set.
    pub fn new() -> Self {
        Self::default()
    }

    /// The target's field `to` holds what its field `from` held.
    pub fn renamed(mut self, at: Target, from: impl Into<String>, to: impl Into<String>) -> Self {
        self.record_rename(at, from, to);
        self
    }

    pub(crate) fn record_rename(
        &mut self,
        at: Target,
        from: impl Into<String>,
        to: impl Into<String>,
    ) {
        self.renames
            .entry(at)
            .or_default()
            .insert(to.into(), from.into());
    }

    /// The target's added field `field` takes `default` in every upcast value.
    pub fn defaulted(mut self, at: Target, field: impl Into<String>, default: Constant) -> Self {
        self.defaults
            .entry(at)
            .or_default()
            .insert(field.into(), default);
        self
    }

    /// The source field the target's `field` was renamed from, if any.
    pub fn rename_of(&self, at: &Target, field: &str) -> Option<&str> {
        self.renames.get(at)?.get(field).map(String::as_str)
    }

    /// The constant the target's added `field` takes, if the author supplied one.
    pub fn default_of(&self, at: &Target, field: &str) -> Option<&Constant> {
        self.defaults.get(at)?.get(field)
    }

    /// Merge `other`'s hints into these, `other` winning on collision.
    pub fn merge(mut self, other: Hints) -> Self {
        for (target, fields) in other.renames {
            self.renames.entry(target).or_default().extend(fields);
        }
        for (target, fields) in other.defaults {
            self.defaults.entry(target).or_default().extend(fields);
        }
        self
    }
}
