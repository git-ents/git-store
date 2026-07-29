//! Derive a [`Migration`] from a pair of [`SchemaDoc`]s.
//!
//! [`derive`] classifies every difference it can express in the lens
//! vocabulary (`crate::migration`) and reports the rest as [`Divergence`]s —
//! it never fails, since "I cannot classify this edge" is itself the honest
//! answer for a document pair (or hint set) the vocabulary cannot cover. The
//! module and function share a name; call it as
//! `migration::derive::derive(...)`.

use std::collections::{BTreeMap, BTreeSet};

use facet::Facet;

use crate::error::SchemaError;
use crate::migration::{Change, Constant, Hints, Migration, Op, Target};
use crate::schema::{Schema, SchemaDoc, VariantKind};

/// The outcome of [`derive`].
#[derive(Debug, Clone, PartialEq)]
pub enum Derivation {
    /// Every difference between the two documents was classified.
    Complete(Migration),
    /// Some differences have no lens in the vocabulary.
    Partial(Incomplete),
}

/// A derivation that classified only part of an edge.
///
/// Holds no `Migration`: the classified operations are reachable only through
/// [`into_draft`](Incomplete::into_draft), which names the act of taking an
/// incomplete edge as a starting point for hand authoring.
#[derive(Debug, Clone, PartialEq)]
pub struct Incomplete {
    ops: Vec<Op>,
    unclassified: Vec<Divergence>,
}

impl Incomplete {
    /// The differences the vocabulary cannot express.
    pub fn unclassified(&self) -> &[Divergence] {
        &self.unclassified
    }

    /// The operations that *were* classified, as a starting point for hand
    /// authoring — never a substitute for the edge itself.
    pub fn into_draft(self) -> Migration {
        Migration { ops: self.ops }
    }
}

/// A difference between two documents that the lens vocabulary cannot
/// express.
///
/// A report type, not a stored artifact — deliberately does not derive
/// [`Facet`].
#[derive(Debug, Clone, PartialEq)]
pub enum Divergence {
    /// The documents' root schemas differ.
    Root {
        /// The source root.
        from: Schema,
        /// The target root.
        to: Schema,
    },
    /// A definition reachable from one root has no counterpart in the other.
    Unpaired {
        /// The definition's name.
        name: String,
        /// The document it is present in.
        side: Side,
    },
    /// Two paired definitions differ in a way no lens expresses (a kind
    /// change, a variant payload change, …).
    Definition {
        /// The definition's name.
        name: String,
        /// The source body.
        from: Schema,
        /// The target body.
        to: Schema,
    },
    /// A field's schema changed other than by an `Optional` wrap.
    Retyped {
        /// The definition or struct variant holding the field.
        at: Target,
        /// The field's target-side name.
        field: String,
        /// The source schema.
        from: Schema,
        /// The target schema.
        to: Schema,
    },
    /// The target has a field the source lacks, and no default is available.
    Undefaulted {
        /// The definition or struct variant holding the field.
        at: Target,
        /// The added field's name.
        field: String,
    },
    /// The source has an enum variant the target lacks: values holding it
    /// have no image under the edge.
    VariantRemoved {
        /// The enum definition's name.
        def: String,
        /// The variant only the source defines.
        variant: String,
    },
}

/// Which document a [`Divergence::Unpaired`] definition is present in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Only the source document defines it.
    From,
    /// Only the target document defines it.
    To,
}

/// Classify the edge from `from` to `to`, using `hints` for the facts a
/// schema diff cannot contain.
pub fn derive(from: &SchemaDoc, to: &SchemaDoc, hints: &Hints) -> Derivation {
    let mut ops = Vec::new();
    let mut unclassified = Vec::new();

    if from.root != to.root {
        unclassified.push(Divergence::Root {
            from: from.root.clone(),
            to: to.root.clone(),
        });
    }

    let names: BTreeSet<&String> = from.defs.keys().chain(to.defs.keys()).collect();
    for name in names {
        match (from.defs.get(name), to.defs.get(name)) {
            (Some(_), None) => unclassified.push(Divergence::Unpaired {
                name: name.clone(),
                side: Side::From,
            }),
            (None, Some(_)) => unclassified.push(Divergence::Unpaired {
                name: name.clone(),
                side: Side::To,
            }),
            (Some(a), Some(b)) => diff_def(name, a, b, hints, &mut ops, &mut unclassified),
            (None, None) => unreachable!("name drawn from the union of both key sets"),
        }
    }

    if unclassified.is_empty() {
        Derivation::Complete(Migration { ops })
    } else {
        Derivation::Partial(Incomplete { ops, unclassified })
    }
}

/// [`derive`] onto the schema of `T`, taking the rename hints `T`'s
/// `#[facet(migrate::renamed_from = …)]` attributes declare.
pub fn derive_to<T: for<'a> Facet<'a>>(
    from: &SchemaDoc,
    extra: &Hints,
) -> Result<Derivation, SchemaError> {
    let (to, declared) = crate::schema_and_hints_of::<T>()?;
    let hints = declared.merge(extra.clone());
    Ok(derive(from, &to, &hints))
}

/// Compare two paired definitions, dispatching on their shared kind.
fn diff_def(
    name: &str,
    a: &Schema,
    b: &Schema,
    hints: &Hints,
    ops: &mut Vec<Op>,
    unclassified: &mut Vec<Divergence>,
) {
    match (a, b) {
        (Schema::Struct(fa), Schema::Struct(fb)) => {
            diff_fields(
                &Target::Def(name.to_owned()),
                fa,
                fb,
                hints,
                ops,
                unclassified,
            );
        }
        (Schema::Enum(va), Schema::Enum(vb)) => {
            diff_enum(name, va, vb, hints, ops, unclassified);
        }
        _ if a == b => {}
        _ => unclassified.push(Divergence::Definition {
            name: name.to_owned(),
            from: a.clone(),
            to: b.clone(),
        }),
    }
}

/// Compare two paired enum definitions, variant by variant.
///
/// A variant present only in `b` is a pure widening — no value of the old
/// type holds it — so it needs no lens and contributes nothing.
fn diff_enum(
    name: &str,
    a: &BTreeMap<String, VariantKind>,
    b: &BTreeMap<String, VariantKind>,
    hints: &Hints,
    ops: &mut Vec<Op>,
    unclassified: &mut Vec<Divergence>,
) {
    let mut kind_mismatch = false;
    for (variant, kind_a) in a {
        match b.get(variant) {
            None => unclassified.push(Divergence::VariantRemoved {
                def: name.to_owned(),
                variant: variant.clone(),
            }),
            Some(kind_b) => match (kind_a, kind_b) {
                (VariantKind::Struct(fa), VariantKind::Struct(fb)) => diff_fields(
                    &Target::Variant {
                        def: name.to_owned(),
                        variant: variant.clone(),
                    },
                    fa,
                    fb,
                    hints,
                    ops,
                    unclassified,
                ),
                _ if kind_a == kind_b => {}
                _ => kind_mismatch = true,
            },
        }
    }
    if kind_mismatch {
        unclassified.push(Divergence::Definition {
            name: name.to_owned(),
            from: Schema::Enum(a.clone()),
            to: Schema::Enum(b.clone()),
        });
    }
}

/// Classify a field whose source and target schemas differ: an `Optional`
/// wrap is the only change the vocabulary expresses, and anything else is a
/// retype no lens covers.
fn classify_retype(
    at: &Target,
    field: &str,
    from: &Schema,
    to: &Schema,
    wraps: &mut Vec<String>,
    unclassified: &mut Vec<Divergence>,
) {
    if matches!(to, Schema::Optional(inner) if **inner == *from) {
        wraps.push(field.to_owned());
        return;
    }
    unclassified.push(Divergence::Retyped {
        at: at.clone(),
        field: field.to_owned(),
        from: from.clone(),
        to: to.clone(),
    });
}

/// Compare the fields of a struct or struct variant at `at`, emitting ops in
/// `Rename`, `Remove`, `Wrap`, `Add` order — `Rename` first so later phases
/// address target-side names, and a `Wrap` on a renamed field therefore names
/// the *target* field.
fn diff_fields(
    at: &Target,
    a: &BTreeMap<String, Schema>,
    b: &BTreeMap<String, Schema>,
    hints: &Hints,
    ops: &mut Vec<Op>,
    unclassified: &mut Vec<Divergence>,
) {
    let mut removed: BTreeSet<String> = a.keys().filter(|k| !b.contains_key(*k)).cloned().collect();
    let added: BTreeSet<String> = b.keys().filter(|k| !a.contains_key(*k)).cloned().collect();
    let common: BTreeSet<String> = a.keys().filter(|k| b.contains_key(*k)).cloned().collect();

    let mut renames: Vec<(String, String)> = Vec::new();
    let mut wraps: Vec<String> = Vec::new();
    let mut adds: Vec<(String, Constant)> = Vec::new();

    for field in &added {
        // A hint naming a source field that was not removed is stale: the
        // addition is treated as ordinary, and the named field left alone.
        if let Some(src) = hints
            .rename_of(at, field)
            .filter(|src| removed.contains(*src))
            .map(str::to_owned)
        {
            removed.remove(&src);
            let from_schema = a.get(&src).expect("a removed field is present in `a`");
            let to_schema = &b[field];
            renames.push((src, field.clone()));
            if from_schema != to_schema {
                classify_retype(at, field, from_schema, to_schema, &mut wraps, unclassified);
            }
            continue;
        }

        if let Some(default) = hints.default_of(at, field) {
            adds.push((field.clone(), default.clone()));
        } else if matches!(&b[field], Schema::Optional(_)) {
            adds.push((field.clone(), Constant::Null));
        } else {
            unclassified.push(Divergence::Undefaulted {
                at: at.clone(),
                field: field.clone(),
            });
        }
    }

    for field in &common {
        let (fa, fb) = (&a[field], &b[field]);
        if fa != fb {
            classify_retype(at, field, fa, fb, &mut wraps, unclassified);
        }
    }

    for (source, target) in renames {
        ops.push(Op {
            at: at.clone(),
            change: Change::Rename {
                from: source,
                to: target,
            },
        });
    }
    for field in removed {
        ops.push(Op {
            at: at.clone(),
            change: Change::Remove { field },
        });
    }
    for field in wraps {
        ops.push(Op {
            at: at.clone(),
            change: Change::Wrap { field },
        });
    }
    for (field, default) in adds {
        ops.push(Op {
            at: at.clone(),
            change: Change::Add { field, default },
        });
    }
}
