//! Self-hosted schemas: [`Node`]/[`Schema`] describe how a `Facet` type
//! is encoded, and are themselves ordinary `Facet` values stored through this
//! crate's own tree encoding.
//!
//! [`schema_of`] converts a `Facet` type's shape into a [`Schema`] by
//! mirroring the encoder's dispatch order, so a schema always describes
//! exactly what serialization writes. With the `value` cargo feature, a
//! schema drives [`deserialize_value_with_schema`](crate::schema::read)'s
//! full-fidelity dynamic reads.
//!
//! The normative rules live in `docs/specification.adoc` under
//! `schema.representation` and `schema.generation`.

pub mod codec;
pub mod pin;
#[cfg(feature = "value")]
pub mod read;
#[cfg(feature = "value")]
pub mod write;

use std::collections::{BTreeMap, HashMap};

use facet::{ConstTypeId, Def, Facet, ScalarType, Shape};

use crate::RawTree;
use crate::attr;
use crate::de::{MAX_DEPTH, collapse_shape};
use crate::error::SchemaError;
use crate::migration::{Hints, Target};
use crate::normal_form::IDENTITY_DEF_PREFIX;
use crate::ser::is_byte_seq;

/// A complete, self-contained schema document.
///
/// `root` describes the value itself; `defs` holds the bodies of every named
/// user type (struct or enum) the root reaches, keyed by the deterministic
/// names that [`Node::Ref`] nodes use. A `BTreeMap` keeps the encoding
/// order-independent of construction, so equal documents share an object id.
///
/// This type carries no format-version field: a stored document instead pins
/// the schema-schema tree it was written against as a `schema` entry spliced
/// onto the tree at write time — see [`pin`](crate::schema::pin) — which is a
/// storage-layer concern, not part of this Rust type.
#[derive(Debug, Clone, PartialEq, Facet)]
pub struct Schema {
    /// The schema of the value itself. Named user types appear as
    /// [`Node::Ref`] nodes resolved through `defs`.
    pub root: Node,
    /// The definition table for named user types, keyed by assigned name.
    pub defs: BTreeMap<String, Node>,
}

/// A single schema node: the shape of one value in the encoding.
///
/// Scalar variants are per-width unit variants mirroring [`facet::ScalarType`],
/// so each encodes as a trivially stable single-entry tree. The on-disk form
/// of this type is a public contract (`schema.representation`): changing it is
/// a semver-major break.
#[derive(Debug, Clone, PartialEq, Facet)]
#[repr(u8)]
pub enum Node {
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
    /// A named-field struct: a tree with one entry per field, keyed by field
    /// name — the map key *is* the tree entry name (`schema.representation`).
    /// A field whose [`StructField::has_default`] is set may have no entry
    /// at all: a write may omit it, and a read finds it simply absent.
    Struct(BTreeMap<String, StructField>),
    /// A tuple or tuple struct: a tree with ordinal-named entries.
    Tuple(Vec<Node>),
    /// A variable-length sequence (`Vec<T>`, `[T]`): an ordinal-named tree.
    List(Box<Node>),
    /// A fixed-length array `[T; N]`: an ordinal-named tree of exactly `len`
    /// entries.
    Array {
        /// The element schema.
        elem: Box<Node>,
        /// The exact element count.
        len: usize,
    },
    /// A map: a name-keyed tree for scalar keys, or ordinal-named `{ k, v }`
    /// pair sub-trees for composite keys.
    Map {
        /// The key schema; whether it is a scalar variant decides the layout.
        key: Box<Node>,
        /// The value schema.
        value: Box<Node>,
    },
    /// An `Option<T>`: the presence-marker tree (`crate::marker`) for `None`,
    /// a single `some` entry for `Some`.
    Optional(Box<Node>),
    /// An enum: externally tagged by the live variant's name — a bare blob
    /// holding that name for a unit variant ([`VariantKind::Unit`]), a
    /// single-entry tree naming it for every other variant. Keyed by variant
    /// name, which is also the tag.
    Enum(BTreeMap<String, VariantKind>),
    /// A [`RawTree`]: a verbatim tree reference.
    RawTree,
    /// A dynamic value (`facet_value::Value`): shape decided at runtime.
    Dynamic,
    /// A reference to a named definition in the document's `defs` table.
    Ref(String),
}

/// The payload shape of one enum variant, mirroring the encoder's four
/// variant layouts.
#[derive(Debug, Clone, PartialEq, Facet)]
#[repr(u8)]
pub enum VariantKind {
    /// No payload: the variant's entire encoding is a bare blob holding its
    /// name (see `serialization.design.trees.variants`), not a tree — so it
    /// appears as ordinary content to git's blob-oriented diff and ls-tree
    /// tooling instead of vanishing as a tree-entry rename with no blob
    /// content on either side.
    Unit,
    /// A single-field tuple variant: the field's own encoding directly.
    Newtype(Box<Node>),
    /// A multi-field tuple variant: an ordinal-named tree.
    Tuple(Vec<Node>),
    /// A struct variant: a name-keyed tree.
    Struct(BTreeMap<String, Node>),
}

/// One field of a [`Node::Struct`]: its own schema, plus whether a write may
/// leave it unset because `T`'s `facet_core::Field` supplies a default.
///
/// Struct enum variant fields ([`VariantKind::Struct`]) carry no such marker
/// and stay bare [`Node`]s: `facet`'s per-field default metadata is read only
/// where [`schema_of`] walks a named struct's own fields, not a variant's.
#[derive(Debug, Clone, PartialEq, Facet)]
pub struct StructField {
    /// The field's own schema.
    pub node: Node,
    /// Whether a write may omit this field's tree entry, per
    /// `facet_core::Field::has_default()`. A schema-driven read finds an
    /// omitted defaulted field simply absent from the result — a schema
    /// carries no snapshot of the default *value*, since one (`created_at`
    /// wanting `now_nanos()`) can be computed only at write time. A typed
    /// read is unaffected: it recovers `T`'s real default through `facet`'s
    /// own reflection machinery, independent of this marker.
    pub has_default: bool,
}

/// A [`Node::Struct`] field, or a struct enum variant's plain [`Node`] field:
/// one implementation lets the read, write, and migration walks handle both
/// without duplicating themselves.
pub(crate) trait FieldNode {
    /// The field's own schema.
    fn node(&self) -> &Node;
}

impl FieldNode for Node {
    fn node(&self) -> &Node {
        self
    }
}

impl FieldNode for StructField {
    fn node(&self) -> &Node {
        &self.node
    }
}

#[cfg(feature = "value")]
pub(crate) trait DefaultFieldNode: FieldNode {
    /// Whether the field's tree entry may be absent.
    fn has_default(&self) -> bool;
}

#[cfg(feature = "value")]
impl DefaultFieldNode for Node {
    fn has_default(&self) -> bool {
        false
    }
}

#[cfg(feature = "value")]
impl DefaultFieldNode for StructField {
    fn has_default(&self) -> bool {
        self.has_default
    }
}

/// Generate the [`Schema`] describing how `T` is encoded.
///
/// The shorthand for [`Schema::from_shape`] applied to `T`'s shape.
///
/// ```
/// use facet::Facet;
/// use facet_git_tree::{Node, schema_of};
///
/// #[derive(Facet)]
/// struct Point {
///     x: f64,
///     y: f64,
/// }
///
/// let doc = schema_of::<Point>()?;
/// assert_eq!(doc.root, Node::Ref("Point".into()));
/// assert!(doc.defs.contains_key("Point"));
/// # Ok::<(), facet_git_tree::SchemaError>(())
/// ```
pub fn schema_of<T: for<'a> Facet<'a>>() -> Result<Schema, SchemaError> {
    Schema::from_shape(<T as Facet>::SHAPE)
}

/// [`schema_of`] together with the rename hints `T`'s
/// `#[facet(migrate::renamed_from = …)]` attributes declare.
pub fn schema_and_hints_of<T: for<'a> Facet<'a>>() -> Result<(Schema, Hints), SchemaError> {
    Schema::from_shape_with_hints(<T as Facet>::SHAPE)
}

impl Schema {
    /// Generate the [`Schema`] describing how values of `shape` are
    /// encoded.
    ///
    /// The walker mirrors the encoder's dispatch order exactly
    /// (`schema.generation`): transparency collapse, then [`RawTree`], then
    /// dynamic values, then the scalar table, then byte sequences, then
    /// composites. Named user types (structs and enums) are deduplicated into
    /// [`defs`](Schema::defs) and referenced by [`Node::Ref`]; names are
    /// assigned deterministically in pre-order, so the same shape always
    /// yields an identical — and identically-encoded — document.
    pub fn from_shape(shape: &'static Shape) -> Result<Self, SchemaError> {
        Self::from_shape_with_limit(shape, MAX_DEPTH).map(|(doc, _hints)| doc)
    }

    /// [`from_shape`](Self::from_shape), additionally returning the rename
    /// [`Hints`] collected from `#[facet(migrate::renamed_from = …)]`
    /// attributes on named struct fields and struct enum variant fields.
    pub fn from_shape_with_hints(shape: &'static Shape) -> Result<(Self, Hints), SchemaError> {
        Self::from_shape_with_limit(shape, MAX_DEPTH)
    }

    /// [`from_shape`](Self::from_shape) with a custom nesting bound in place
    /// of [`MAX_DEPTH`], also returning the collected [`Hints`].
    ///
    /// Exists so tests can exercise the depth guard without a pathologically
    /// deep type (whose `SHAPE` evaluation is prohibitively expensive to
    /// compile); not part of the public API.
    #[doc(hidden)]
    pub fn from_shape_with_limit(
        shape: &'static Shape,
        limit: usize,
    ) -> Result<(Self, Hints), SchemaError> {
        let mut walker = Walker::new(limit);
        let root = walker.node(shape, 0)?;
        Ok((
            Schema {
                root,
                defs: walker.defs,
            },
            walker.hints,
        ))
    }
}

/// The `Shape` → [`Node`] walker: tracks named-type definitions and the
/// deterministic name assignment while recursing through a shape.
struct Walker {
    /// Finished (or in-progress) definitions, keyed by assigned name.
    defs: BTreeMap<String, Node>,
    /// Assigned name per user type, keyed by type identity.
    names: HashMap<ConstTypeId, String>,
    /// How many types have claimed each identifier, for `_2`, `_3`, …
    /// disambiguation.
    claimed: HashMap<&'static str, usize>,
    /// The nesting bound `node` enforces — [`MAX_DEPTH`] outside of tests.
    limit: usize,
    /// Rename hints collected from named-field structs' and struct enum
    /// variants' fields as they are visited.
    hints: Hints,
}

impl Walker {
    fn new(limit: usize) -> Self {
        Walker {
            defs: BTreeMap::new(),
            names: HashMap::new(),
            claimed: HashMap::new(),
            limit,
            hints: Hints::default(),
        }
    }

    /// The schema of one shape, with `#[facet(identity::key)]` on the type
    /// compiled into the reserved definition that marks an identity- or
    /// key-bearing subtree (see [`crate::normal_form`]).
    fn node(&mut self, shape: &'static Shape, depth: usize) -> Result<Node, SchemaError> {
        let marked = attr::is_identity_key(shape.attributes);
        let node = self.unmarked_node(shape, depth)?;
        Ok(self.mark_identity(node, marked))
    }

    /// Register `node` under a reserved [`IDENTITY_DEF_PREFIX`] definition
    /// when `marked`, returning the [`Node::Ref`] that stands in for it — a
    /// reference adds no tree level, so a marked subtree encodes exactly as
    /// an unmarked one does. Bodies are deduplicated, and an already-marked
    /// node is returned untouched, so marking is idempotent.
    fn mark_identity(&mut self, node: Node, marked: bool) -> Node {
        if !marked {
            return node;
        }
        if let Node::Ref(name) = &node
            && name.starts_with(IDENTITY_DEF_PREFIX)
        {
            return node;
        }
        if let Some((name, _)) = self
            .defs
            .iter()
            .find(|(name, body)| name.starts_with(IDENTITY_DEF_PREFIX) && **body == node)
        {
            return Node::Ref(name.clone());
        }
        let name = format!(
            "{IDENTITY_DEF_PREFIX}{}",
            self.defs
                .keys()
                .filter(|name| name.starts_with(IDENTITY_DEF_PREFIX))
                .count()
        );
        self.defs.insert(name.clone(), node);
        Node::Ref(name)
    }

    /// The schema of one shape, mirroring `serialize_node`'s dispatch order.
    ///
    /// `depth` counts nesting levels against [`MAX_DEPTH`], the same bound
    /// typed deserialization enforces on reads: a shape nested deeper than
    /// that could never be read back regardless of what schema described it,
    /// so generation is refused here rather than recursing unboundedly on a
    /// pathological (or adversarially deep) type.
    fn unmarked_node(&mut self, shape: &'static Shape, depth: usize) -> Result<Node, SchemaError> {
        if depth > self.limit {
            return Err(SchemaError::MaxDepth(self.limit));
        }

        // Transparency collapse (smart pointers and transparent newtypes),
        // mirroring `Peek::innermost_peek` on the write side: neither carries
        // information the encoding records, so neither appears in a schema.
        let shape = collapse(shape)?;

        // RawTree → a verbatim tree reference.
        if shape.is_type::<RawTree>() {
            return Ok(Node::RawTree);
        }

        // Dynamic value → shape decided at runtime, not describable further.
        if let Def::DynamicValue(_) = shape.def {
            return Ok(Node::Dynamic);
        }

        // Scalar leaf → the per-width scalar table.
        if matches!(shape.def, Def::Scalar) {
            return scalar_schema(shape);
        }

        // Byte sequence → a single blob, before generic sequence handling,
        // exactly as the encoder special-cases it.
        if is_byte_seq(shape) {
            return Ok(Node::Bytes);
        }

        // Struct or tuple. An anonymous tuple `(A, B)` has no user name and is
        // inlined; unit structs, tuple structs, and named-field structs are
        // named user types and live in `defs`.
        if let facet::Type::User(facet::UserType::Struct(st)) = shape.ty {
            if matches!(st.kind, facet::StructKind::Tuple) {
                return Ok(Node::Tuple(self.field_schemas(st.fields, depth + 1)?));
            }
            return self.define(shape, |walker, name| match st.kind {
                facet::StructKind::Unit => Ok(Node::Unit),
                facet::StructKind::TupleStruct => {
                    Ok(Node::Tuple(walker.field_schemas(st.fields, depth + 1)?))
                }
                _ => {
                    walker.record_rename_hints(&Target::Def(name.to_owned()), st.fields);
                    Ok(Node::Struct(
                        walker.struct_field_schemas(st.fields, depth + 1)?,
                    ))
                }
            });
        }

        // Sequences, maps, options — the same order the encoder checks them.
        match shape.def {
            Def::List(d) => return Ok(Node::List(Box::new(self.node(d.t, depth + 1)?))),
            Def::Slice(d) => return Ok(Node::List(Box::new(self.node(d.t, depth + 1)?))),
            Def::Array(d) => {
                return Ok(Node::Array {
                    elem: Box::new(self.node(d.t, depth + 1)?),
                    len: d.n,
                });
            }
            Def::Map(d) => {
                return Ok(Node::Map {
                    key: Box::new(self.node(d.k, depth + 1)?),
                    value: Box::new(self.node(d.v, depth + 1)?),
                });
            }
            Def::Option(d) => return Ok(Node::Optional(Box::new(self.node(d.t, depth + 1)?))),
            _ => {}
        }

        // Enum → a named user type in `defs`, with each variant's payload
        // classified exactly as the encoder classifies it.
        if let facet::Type::User(facet::UserType::Enum(et)) = shape.ty {
            return self.define(shape, |walker, def_name| {
                let mut variants = BTreeMap::new();
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
                        walker.record_rename_hints(
                            &Target::Variant {
                                def: def_name.to_owned(),
                                variant: variant.name.to_owned(),
                            },
                            variant.data.fields,
                        );
                        VariantKind::Struct(
                            walker.named_field_schemas(variant.data.fields, depth + 1)?,
                        )
                    };
                    variants.insert(variant.name.to_owned(), kind);
                }
                Ok(Node::Enum(variants))
            });
        }

        Err(SchemaError::UnsupportedShape(shape.type_identifier))
    }

    /// The schemas of positional fields, in declaration order.
    fn field_schemas(
        &mut self,
        fields: &'static [facet::Field],
        depth: usize,
    ) -> Result<Vec<Node>, SchemaError> {
        fields.iter().map(|f| self.node(f.shape(), depth)).collect()
    }

    /// The schemas of struct fields, keyed by field name.
    fn named_field_schemas(
        &mut self,
        fields: &'static [facet::Field],
        depth: usize,
    ) -> Result<BTreeMap<String, Node>, SchemaError> {
        fields
            .iter()
            .map(|f| {
                let node = self.node(f.shape(), depth)?;
                Ok((
                    f.name.to_owned(),
                    self.mark_identity(node, attr::is_identity_key(f.attributes)),
                ))
            })
            .collect()
    }

    /// The schemas of a plain (non-variant) struct's fields, keyed by field
    /// name, each carrying [`Field::has_default()`](facet::Field::has_default)
    /// as its [`StructField::has_default`] marker.
    fn struct_field_schemas(
        &mut self,
        fields: &'static [facet::Field],
        depth: usize,
    ) -> Result<BTreeMap<String, StructField>, SchemaError> {
        fields
            .iter()
            .map(|f| {
                let node = self.node(f.shape(), depth)?;
                Ok((
                    f.name.to_owned(),
                    StructField {
                        node: self.mark_identity(node, attr::is_identity_key(f.attributes)),
                        has_default: f.has_default(),
                    },
                ))
            })
            .collect()
    }

    /// Register `shape` as a named definition and return the [`Node::Ref`]
    /// to it, computing the body via `body` on first encounter.
    ///
    /// The name is claimed *before* the body is computed, so a recursive type
    /// (`struct Node { children: Vec<Node> }`) resolves its own occurrences to
    /// the already-assigned `Ref` instead of recursing forever. Distinct types
    /// sharing an identifier get `_2`, `_3`, … suffixes in pre-order, keeping
    /// name assignment deterministic. `body` receives the assigned name, so it
    /// can address its own fields as migration [`Target`]s.
    fn define(
        &mut self,
        shape: &'static Shape,
        body: impl FnOnce(&mut Self, &str) -> Result<Node, SchemaError>,
    ) -> Result<Node, SchemaError> {
        if let Some(name) = self.names.get(&shape.id) {
            return Ok(Node::Ref(name.clone()));
        }
        let claimed = self.claimed.entry(shape.type_identifier).or_insert(0);
        *claimed += 1;
        let name = if *claimed == 1 {
            shape.type_identifier.to_owned()
        } else {
            format!("{}_{claimed}", shape.type_identifier)
        };
        self.names.insert(shape.id, name.clone());
        let schema = body(self, &name)?;
        self.defs.insert(name.clone(), schema);
        Ok(Node::Ref(name))
    }

    /// Record each of `fields`' `#[facet(migrate::renamed_from = …)]` hint,
    /// if any, against `target`.
    ///
    /// Only meaningful for named fields (named-field structs and struct enum
    /// variants); positional fields have no name for a rename to target, so
    /// callers must not invoke this for them.
    fn record_rename_hints(&mut self, target: &Target, fields: &'static [facet::Field]) {
        for field in fields {
            if let Some(from) = attr::renamed_from(field) {
                self.hints.record_rename(target.clone(), from, field.name);
            }
        }
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
/// Every textual scalar (`str`, `String`, `Cow<str>`) is [`Node::String`];
/// every numeric width maps 1:1. Scalars outside the table (network address
/// types, `ConstTypeId`, future additions) are unsupported, exactly as the
/// encoder's `scalar_bytes` refuses them. `()` (`ScalarType::Unit`) is
/// likewise refused here: `scalar_bytes` has no textual rendering for it
/// either — `Display`/`FromStr` on `()` is not implemented — so it cannot
/// reach a leaf blob. [`Node::Unit`] remains reachable, but only for a unit
/// struct or a unit enum variant, whose *composite* (not scalar) encoding is
/// the empty tree those actually write.
fn scalar_schema(shape: &'static Shape) -> Result<Node, SchemaError> {
    let Some(scalar) = shape.scalar_type() else {
        return Err(SchemaError::UnsupportedScalar(shape.type_identifier));
    };
    Ok(match scalar {
        ScalarType::Unit => return Err(SchemaError::UnsupportedScalar(shape.type_identifier)),
        ScalarType::Bool => Node::Bool,
        ScalarType::Char => Node::Char,
        ScalarType::Str | ScalarType::String | ScalarType::CowStr => Node::String,
        ScalarType::F32 => Node::F32,
        ScalarType::F64 => Node::F64,
        ScalarType::U8 => Node::U8,
        ScalarType::U16 => Node::U16,
        ScalarType::U32 => Node::U32,
        ScalarType::U64 => Node::U64,
        ScalarType::U128 => Node::U128,
        ScalarType::USize => Node::USize,
        ScalarType::I8 => Node::I8,
        ScalarType::I16 => Node::I16,
        ScalarType::I32 => Node::I32,
        ScalarType::I64 => Node::I64,
        ScalarType::I128 => Node::I128,
        ScalarType::ISize => Node::ISize,
        _ => return Err(SchemaError::UnsupportedScalar(shape.type_identifier)),
    })
}
