//! Self-hosted schemas: [`Schema`]/[`SchemaDoc`] describe how a `Facet` type
//! is encoded, and are themselves ordinary `Facet` values stored through this
//! crate's own tree encoding.
//!
//! [`schema_of`] converts a `Facet` type's shape into a [`SchemaDoc`] by
//! mirroring the encoder's dispatch order, so a schema always describes
//! exactly what serialization writes. With the `value` cargo feature, a
//! schema drives [`deserialize_value_with_schema`](crate::schema::read)'s
//! full-fidelity dynamic reads.
//!
//! The normative rules live in `docs/specification.adoc` under
//! `schema.representation` and `schema.generation`.

#[cfg(feature = "value")]
pub mod read;
#[cfg(feature = "value")]
pub mod write;

use std::collections::{BTreeMap, HashMap};

use facet::{ConstTypeId, Def, Facet, ScalarType, Shape};

use crate::RawTree;
use crate::de::{MAX_DEPTH, collapse_shape};
use crate::error::SchemaError;
use crate::ser::is_byte_seq;

/// A complete, self-contained schema document.
///
/// `root` describes the value itself; `defs` holds the bodies of every named
/// user type (struct or enum) the root reaches, keyed by the deterministic
/// names that [`Schema::Ref`] nodes use. A `BTreeMap` keeps the encoding
/// order-independent of construction, so equal documents share an object id.
#[derive(Debug, Clone, PartialEq, Facet)]
pub struct SchemaDoc {
    /// The schema of the value itself. Named user types appear as
    /// [`Schema::Ref`] nodes resolved through `defs`.
    pub root: Schema,
    /// The definition table for named user types, keyed by assigned name.
    pub defs: BTreeMap<String, Schema>,
}

/// A single schema node: the shape of one value in the encoding.
///
/// Scalar variants are per-width unit variants mirroring [`facet::ScalarType`],
/// so each encodes as a trivially stable single-entry tree. The on-disk form
/// of this type is a public contract (`schema.representation`): changing it is
/// a semver-major break.
#[derive(Debug, Clone, PartialEq, Facet)]
#[repr(u8)]
pub enum Schema {
    /// The unit type `()` or a unit struct: an empty tree.
    Unit,
    /// `bool`: a `true`/`false` blob.
    Bool,
    /// `char`: a blob holding the character's UTF-8 form.
    Char,
    /// A string (`String`, `&str`, `Cow<str>`): a blob of its UTF-8 bytes.
    String,
    /// `i8`.
    I8,
    /// `i16`.
    I16,
    /// `i32`.
    I32,
    /// `i64`.
    I64,
    /// `i128`.
    I128,
    /// `isize`.
    ISize,
    /// `u8`.
    U8,
    /// `u16`.
    U16,
    /// `u32`.
    U32,
    /// `u64`.
    U64,
    /// `u128`.
    U128,
    /// `usize`.
    USize,
    /// `f32`.
    F32,
    /// `f64`.
    F64,
    /// A byte sequence (`Vec<u8>`, `[u8; N]`, `[u8]`): a single blob.
    Bytes,
    /// A named-field struct: a tree with one entry per field.
    Struct(Vec<FieldSchema>),
    /// A tuple or tuple struct: a tree with ordinal-named entries.
    Tuple(Vec<Schema>),
    /// A variable-length sequence (`Vec<T>`, `[T]`): an ordinal-named tree.
    List(Box<Schema>),
    /// A fixed-length array `[T; N]`: an ordinal-named tree of exactly `len`
    /// entries.
    Array {
        /// The element schema.
        elem: Box<Schema>,
        /// The exact element count.
        len: usize,
    },
    /// A map: a name-keyed tree for scalar keys, or ordinal-named `{ k, v }`
    /// pair sub-trees for composite keys.
    Map {
        /// The key schema; whether it is a scalar variant decides the layout.
        key: Box<Schema>,
        /// The value schema.
        value: Box<Schema>,
    },
    /// An `Option<T>`: an empty tree for `None`, a single `some` entry for
    /// `Some`.
    Optional(Box<Schema>),
    /// An enum: a single-entry tree naming the live variant.
    Enum(Vec<VariantSchema>),
    /// A [`RawTree`]: a verbatim tree reference.
    RawTree,
    /// A dynamic value (`facet_value::Value`): shape decided at runtime.
    Dynamic,
    /// A reference to a named definition in the document's `defs` table.
    Ref(String),
}

/// One field of a [`Schema::Struct`] (or [`VariantKind::Struct`]) node.
#[derive(Debug, Clone, PartialEq, Facet)]
pub struct FieldSchema {
    /// The field name, which is the tree entry name.
    pub name: String,
    /// The field's schema.
    pub schema: Schema,
}

/// One variant of a [`Schema::Enum`] node.
#[derive(Debug, Clone, PartialEq, Facet)]
pub struct VariantSchema {
    /// The variant name, which is the single tree entry's name.
    pub name: String,
    /// The variant's payload shape.
    pub kind: VariantKind,
}

/// The payload shape of one enum variant, mirroring the encoder's four
/// variant layouts.
#[derive(Debug, Clone, PartialEq, Facet)]
#[repr(u8)]
pub enum VariantKind {
    /// No payload: an empty tree.
    Unit,
    /// A single-field tuple variant: the field's own encoding directly.
    Newtype(Box<Schema>),
    /// A multi-field tuple variant: an ordinal-named tree.
    Tuple(Vec<Schema>),
    /// A struct variant: a name-keyed tree.
    Struct(Vec<FieldSchema>),
}

/// Generate the [`SchemaDoc`] describing how `T` is encoded.
///
/// The shorthand for [`SchemaDoc::from_shape`] applied to `T`'s shape.
///
/// ```
/// use facet::Facet;
/// use facet_git_tree::{Schema, schema_of};
///
/// #[derive(Facet)]
/// struct Point {
///     x: f64,
///     y: f64,
/// }
///
/// let doc = schema_of::<Point>()?;
/// assert_eq!(doc.root, Schema::Ref("Point".into()));
/// assert!(doc.defs.contains_key("Point"));
/// # Ok::<(), facet_git_tree::SchemaError>(())
/// ```
pub fn schema_of<T: for<'a> Facet<'a>>() -> Result<SchemaDoc, SchemaError> {
    SchemaDoc::from_shape(<T as Facet>::SHAPE)
}

impl SchemaDoc {
    /// Generate the [`SchemaDoc`] describing how values of `shape` are
    /// encoded.
    ///
    /// The walker mirrors the encoder's dispatch order exactly
    /// (`schema.generation`): transparency collapse, then [`RawTree`], then
    /// dynamic values, then the scalar table, then byte sequences, then
    /// composites. Named user types (structs and enums) are deduplicated into
    /// [`defs`](SchemaDoc::defs) and referenced by [`Schema::Ref`]; names are
    /// assigned deterministically in pre-order, so the same shape always
    /// yields an identical — and identically-encoded — document.
    pub fn from_shape(shape: &'static Shape) -> Result<Self, SchemaError> {
        Self::from_shape_with_limit(shape, MAX_DEPTH)
    }

    /// [`from_shape`](Self::from_shape) with a custom nesting bound in place
    /// of [`MAX_DEPTH`].
    ///
    /// Exists so tests can exercise the depth guard without a pathologically
    /// deep type (whose `SHAPE` evaluation is prohibitively expensive to
    /// compile); not part of the public API.
    #[doc(hidden)]
    pub fn from_shape_with_limit(shape: &'static Shape, limit: usize) -> Result<Self, SchemaError> {
        let mut walker = Walker::new(limit);
        let root = walker.node(shape, 0)?;
        Ok(SchemaDoc {
            root,
            defs: walker.defs,
        })
    }
}

/// The `Shape` → [`Schema`] walker: tracks named-type definitions and the
/// deterministic name assignment while recursing through a shape.
struct Walker {
    /// Finished (or in-progress) definitions, keyed by assigned name.
    defs: BTreeMap<String, Schema>,
    /// Assigned name per user type, keyed by type identity.
    names: HashMap<ConstTypeId, String>,
    /// How many types have claimed each identifier, for `_2`, `_3`, …
    /// disambiguation.
    claimed: HashMap<&'static str, usize>,
    /// The nesting bound `node` enforces — [`MAX_DEPTH`] outside of tests.
    limit: usize,
}

impl Walker {
    fn new(limit: usize) -> Self {
        Walker {
            defs: BTreeMap::new(),
            names: HashMap::new(),
            claimed: HashMap::new(),
            limit,
        }
    }

    /// The schema of one shape, mirroring `serialize_node`'s dispatch order.
    ///
    /// `depth` counts nesting levels against [`MAX_DEPTH`], the same bound
    /// typed deserialization enforces on reads: a shape nested deeper than
    /// that could never be read back regardless of what schema described it,
    /// so generation is refused here rather than recursing unboundedly on a
    /// pathological (or adversarially deep) type.
    fn node(&mut self, shape: &'static Shape, depth: usize) -> Result<Schema, SchemaError> {
        if depth > self.limit {
            return Err(SchemaError::MaxDepth(self.limit));
        }

        // Transparency collapse (smart pointers and transparent newtypes),
        // mirroring `Peek::innermost_peek` on the write side: neither carries
        // information the encoding records, so neither appears in a schema.
        let shape = collapse(shape)?;

        // RawTree → a verbatim tree reference.
        if shape.is_type::<RawTree>() {
            return Ok(Schema::RawTree);
        }

        // Dynamic value → shape decided at runtime, not describable further.
        if let Def::DynamicValue(_) = shape.def {
            return Ok(Schema::Dynamic);
        }

        // Scalar leaf → the per-width scalar table.
        if matches!(shape.def, Def::Scalar) {
            return scalar_schema(shape);
        }

        // Byte sequence → a single blob, before generic sequence handling,
        // exactly as the encoder special-cases it.
        if is_byte_seq(shape) {
            return Ok(Schema::Bytes);
        }

        // Struct or tuple. An anonymous tuple `(A, B)` has no user name and is
        // inlined; unit structs, tuple structs, and named-field structs are
        // named user types and live in `defs`.
        if let facet::Type::User(facet::UserType::Struct(st)) = shape.ty {
            if matches!(st.kind, facet::StructKind::Tuple) {
                return Ok(Schema::Tuple(self.field_schemas(st.fields, depth + 1)?));
            }
            return self.define(shape, |walker| match st.kind {
                facet::StructKind::Unit => Ok(Schema::Unit),
                facet::StructKind::TupleStruct => {
                    Ok(Schema::Tuple(walker.field_schemas(st.fields, depth + 1)?))
                }
                _ => Ok(Schema::Struct(
                    walker.named_field_schemas(st.fields, depth + 1)?,
                )),
            });
        }

        // Sequences, maps, options — the same order the encoder checks them.
        match shape.def {
            Def::List(d) => return Ok(Schema::List(Box::new(self.node(d.t, depth + 1)?))),
            Def::Slice(d) => return Ok(Schema::List(Box::new(self.node(d.t, depth + 1)?))),
            Def::Array(d) => {
                return Ok(Schema::Array {
                    elem: Box::new(self.node(d.t, depth + 1)?),
                    len: d.n,
                });
            }
            Def::Map(d) => {
                return Ok(Schema::Map {
                    key: Box::new(self.node(d.k, depth + 1)?),
                    value: Box::new(self.node(d.v, depth + 1)?),
                });
            }
            Def::Option(d) => return Ok(Schema::Optional(Box::new(self.node(d.t, depth + 1)?))),
            _ => {}
        }

        // Enum → a named user type in `defs`, with each variant's payload
        // classified exactly as the encoder classifies it.
        if let facet::Type::User(facet::UserType::Enum(et)) = shape.ty {
            return self.define(shape, |walker| {
                let mut variants = Vec::with_capacity(et.variants.len());
                for variant in et.variants {
                    let positional = matches!(variant.data.kind, facet::StructKind::TupleStruct);
                    let newtype = positional && variant.data.fields.len() == 1;
                    let kind = if variant.data.fields.is_empty() {
                        VariantKind::Unit
                    } else if newtype {
                        VariantKind::Newtype(Box::new(
                            walker.node(variant.data.fields[0].shape(), depth + 1)?,
                        ))
                    } else if positional {
                        VariantKind::Tuple(walker.field_schemas(variant.data.fields, depth + 1)?)
                    } else {
                        VariantKind::Struct(
                            walker.named_field_schemas(variant.data.fields, depth + 1)?,
                        )
                    };
                    variants.push(VariantSchema {
                        name: variant.name.to_owned(),
                        kind,
                    });
                }
                Ok(Schema::Enum(variants))
            });
        }

        Err(SchemaError::UnsupportedShape(shape.type_identifier))
    }

    /// The schemas of positional fields, in declaration order.
    fn field_schemas(
        &mut self,
        fields: &'static [facet::Field],
        depth: usize,
    ) -> Result<Vec<Schema>, SchemaError> {
        fields.iter().map(|f| self.node(f.shape(), depth)).collect()
    }

    /// The named schemas of struct fields, in declaration order.
    fn named_field_schemas(
        &mut self,
        fields: &'static [facet::Field],
        depth: usize,
    ) -> Result<Vec<FieldSchema>, SchemaError> {
        fields
            .iter()
            .map(|f| {
                Ok(FieldSchema {
                    name: f.name.to_owned(),
                    schema: self.node(f.shape(), depth)?,
                })
            })
            .collect()
    }

    /// Register `shape` as a named definition and return the [`Schema::Ref`]
    /// to it, computing the body via `body` on first encounter.
    ///
    /// The name is claimed *before* the body is computed, so a recursive type
    /// (`struct Node { children: Vec<Node> }`) resolves its own occurrences to
    /// the already-assigned `Ref` instead of recursing forever. Distinct types
    /// sharing an identifier get `_2`, `_3`, … suffixes in pre-order, keeping
    /// name assignment deterministic.
    fn define(
        &mut self,
        shape: &'static Shape,
        body: impl FnOnce(&mut Self) -> Result<Schema, SchemaError>,
    ) -> Result<Schema, SchemaError> {
        if let Some(name) = self.names.get(&shape.id) {
            return Ok(Schema::Ref(name.clone()));
        }
        let claimed = self.claimed.entry(shape.type_identifier).or_insert(0);
        *claimed += 1;
        let name = if *claimed == 1 {
            shape.type_identifier.to_owned()
        } else {
            format!("{}_{claimed}", shape.type_identifier)
        };
        self.names.insert(shape.id, name.clone());
        let schema = body(self)?;
        self.defs.insert(name.clone(), schema);
        Ok(Schema::Ref(name))
    }
}

/// Collapse smart pointers and transparent newtypes to the shape the encoder
/// actually writes.
///
/// Delegates the walk itself to [`collapse_shape`], the same shape-level
/// transparency collapse [`crate::de`] and [`crate::ser`]'s map-key layout
/// checks use, so all three altitudes agree on what a shape collapses to.
/// This wrapper adds what only a full schema needs: an opaque pointer shape
/// (no pointee to collapse to) is not just "not a scalar" here as it is for a
/// map-key check — a schema document has no way to describe an indescribable
/// shape at all, so it is a hard [`SchemaError::MissingPointee`].
fn collapse(shape: &'static Shape) -> Result<&'static Shape, SchemaError> {
    let shape = collapse_shape(shape);
    if let Def::Pointer(pd) = shape.def {
        // `collapse_shape` only stops on a `Def::Pointer` when it has no
        // pointee to continue through.
        debug_assert!(pd.pointee.is_none());
        return Err(SchemaError::MissingPointee(shape.type_identifier));
    }
    Ok(shape)
}

/// The per-width scalar table, mirroring [`facet::ScalarType`].
///
/// Every textual scalar (`str`, `String`, `Cow<str>`) is [`Schema::String`];
/// every numeric width maps 1:1. Scalars outside the table (network address
/// types, `ConstTypeId`, future additions) are unsupported, exactly as the
/// encoder's `scalar_bytes` refuses them. `()` (`ScalarType::Unit`) is
/// likewise refused here: `scalar_bytes` has no textual rendering for it
/// either — `Display`/`FromStr` on `()` is not implemented — so it cannot
/// reach a leaf blob. [`Schema::Unit`] remains reachable, but only for a unit
/// struct or a unit enum variant, whose *composite* (not scalar) encoding is
/// the empty tree those actually write.
fn scalar_schema(shape: &'static Shape) -> Result<Schema, SchemaError> {
    let Some(scalar) = shape.scalar_type() else {
        return Err(SchemaError::UnsupportedScalar(shape.type_identifier));
    };
    Ok(match scalar {
        ScalarType::Unit => return Err(SchemaError::UnsupportedScalar(shape.type_identifier)),
        ScalarType::Bool => Schema::Bool,
        ScalarType::Char => Schema::Char,
        ScalarType::Str | ScalarType::String | ScalarType::CowStr => Schema::String,
        ScalarType::F32 => Schema::F32,
        ScalarType::F64 => Schema::F64,
        ScalarType::U8 => Schema::U8,
        ScalarType::U16 => Schema::U16,
        ScalarType::U32 => Schema::U32,
        ScalarType::U64 => Schema::U64,
        ScalarType::U128 => Schema::U128,
        ScalarType::USize => Schema::USize,
        ScalarType::I8 => Schema::I8,
        ScalarType::I16 => Schema::I16,
        ScalarType::I32 => Schema::I32,
        ScalarType::I64 => Schema::I64,
        ScalarType::I128 => Schema::I128,
        ScalarType::ISize => Schema::ISize,
        _ => return Err(SchemaError::UnsupportedScalar(shape.type_identifier)),
    })
}
