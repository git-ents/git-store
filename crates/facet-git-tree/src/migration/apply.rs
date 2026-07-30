//! Read-time upcast: applying a [`Migration`]'s operations to an
//! already-read [`Value`], guided by the source [`Schema`]'s definition
//! structure.
//!
//! `git-attest` binds attestations to object hashes, so migration must never
//! rewrite a stored value: doing so would change its tree hash and silently
//! void every claim made about it. This module takes no object store and
//! performs no writes — it is a pure `Value -> Value` transform, applied
//! after an ordinary schema-driven read.
//!
//! The walk mirrors [`schema::read`](crate::schema::read)'s dispatch exactly,
//! except it runs over a [`Value`] already in hand rather than over Git
//! objects, and it additionally applies [`Migration::ops`] wherever the walk
//! resolves the [`Target`] they name.

use std::collections::BTreeMap;

use core::fmt::Write as _;
use facet_value::{VArray, VNumber, VObject, Value};

use crate::de::MAX_DEPTH;
use crate::error::MigrationError;
use crate::migration::{Change, Constant, Migration, Target};
use crate::schema::{FieldNode, Node, Schema, VariantKind};

/// Upcast a value read against `from` into one conforming to the schema on
/// the far side of `migration`.
///
/// Never writes an object and never mutates stored state.
pub fn apply(value: &Value, from: &Schema, migration: &Migration) -> Result<Value, MigrationError> {
    walk(value, &from.root, from, migration, &Path::ROOT, 0)
}

/// One edge of a chain: the document a value conforms to, and the migration
/// off it.
#[derive(Debug, Clone, Copy)]
pub struct Edge<'a> {
    pub from: &'a Schema,
    pub migration: &'a Migration,
}

/// Apply a chain of edges in series, A->B->C.
///
/// Edges are applied, not composed: each edge's operations are scoped by the
/// definition names of *its own* source document, and two documents in a
/// chain need not agree on those names, so composing the documents would
/// require a name reconciliation that applying in series simply does not
/// need.
pub fn apply_chain(value: &Value, chain: &[Edge<'_>]) -> Result<Value, MigrationError> {
    let Some((first, rest)) = chain.split_first() else {
        return Ok(value.clone());
    };
    let mut current = apply(value, first.from, first.migration)?;
    for edge in rest {
        current = apply(&current, edge.from, edge.migration)?;
    }
    Ok(current)
}

/// A location within the value being walked, threaded through so a mismatch
/// can name exactly where it happened. Mirrors `schema::write`'s `Path`.
struct Path<'a> {
    parent: Option<&'a Path<'a>>,
    seg: Seg<'a>,
}

enum Seg<'a> {
    Root,
    Field(&'a str),
    Index(usize),
}

impl<'a> Path<'a> {
    const ROOT: Path<'static> = Path {
        parent: None,
        seg: Seg::Root,
    };

    fn field<'b>(&'b self, name: &'b str) -> Path<'b> {
        Path {
            parent: Some(self),
            seg: Seg::Field(name),
        }
    }

    fn index<'b>(&'b self, i: usize) -> Path<'b> {
        Path {
            parent: Some(self),
            seg: Seg::Index(i),
        }
    }

    fn show(&self) -> String {
        let mut segs = Vec::new();
        let mut cur = Some(self);
        while let Some(p) = cur {
            segs.push(&p.seg);
            cur = p.parent;
        }
        let mut s = String::from("$");
        for seg in segs.into_iter().rev() {
            match seg {
                Seg::Root => {}
                Seg::Field(name) => {
                    s.push('.');
                    s.push_str(name);
                }
                Seg::Index(i) => {
                    let _ = write!(s, "[{i}]");
                }
            }
        }
        s
    }
}

/// Migrate one schema node's value.
///
/// `def_name` is `Some` exactly when `schema` is the body of the definition
/// just resolved through a [`Node::Ref`] one hop up — the only condition
/// under which a [`Target`] can name what is being walked. Every other
/// caller passes `None`, since a [`Node::Struct`]/[`Node::Enum`] reached
/// any other way (only possible inside a definition body, or as a
/// hand-authored document's bare root) has no name a `Target` could address.
///
/// `depth` counts every hop — including `Ref` resolution — against
/// [`MAX_DEPTH`], exactly as `schema::read` does, so a `Ref`-to-`Ref` cycle
/// fails rather than recursing unboundedly.
fn walk_named(
    value: &Value,
    schema: &Node,
    def_name: Option<&str>,
    doc: &Schema,
    migration: &Migration,
    path: &Path,
    depth: usize,
) -> Result<Value, MigrationError> {
    if depth > MAX_DEPTH {
        return Err(MigrationError::MaxDepth {
            path: path.show(),
            depth: MAX_DEPTH,
        });
    }
    match schema {
        Node::Ref(name) => {
            let target = doc
                .defs
                .get(name)
                .ok_or_else(|| MigrationError::UnknownRef {
                    path: path.show(),
                    name: name.clone(),
                })?;
            walk_named(value, target, Some(name), doc, migration, path, depth + 1)
        }
        Node::Struct(fields) => {
            let obj = as_object(value, path)?;
            let mut result = walk_struct(obj, fields, doc, migration, path, depth)?;
            if let Some(name) = def_name {
                apply_ops(&mut result, migration, |t| matches_def(t, name));
            }
            Ok(result.into())
        }
        Node::Enum(variants) => {
            walk_enum(value, def_name, variants, doc, migration, path, depth).map(Into::into)
        }
        Node::Tuple(elems) => {
            walk_tuple(as_array(value, path)?, elems, doc, migration, path, depth)
        }
        Node::List(elem) => walk_seq(as_array(value, path)?, elem, doc, migration, path, depth),
        Node::Array { elem, len } => {
            let arr = as_array(value, path)?;
            expect_len(path, *len, arr.as_slice().len())?;
            walk_seq(arr, elem, doc, migration, path, depth)
        }
        Node::Map { key, value: val } => walk_map(value, key, val, doc, migration, path, depth),
        Node::Optional(inner) => {
            if value.is_null() {
                Ok(Value::NULL)
            } else {
                walk(value, inner, doc, migration, path, depth + 1)
            }
        }
        // Every scalar, plus Bytes, Unit, RawTree, and Dynamic: none of these
        // carry a Target a migration could address, and none nest further.
        _ => Ok(value.clone()),
    }
}

/// [`walk_named`] with no definition name in scope.
fn walk(
    value: &Value,
    schema: &Node,
    doc: &Schema,
    migration: &Migration,
    path: &Path,
    depth: usize,
) -> Result<Value, MigrationError> {
    walk_named(value, schema, None, doc, migration, path, depth)
}

/// Migrate a name-keyed object field by field, skipping fields absent from
/// the value: application walks an already-read value, not a tree, so a
/// field's absence there (a defaulted field the source writer omitted, or one
/// a prior edge already dropped) is not rechecked against the source schema.
fn walk_struct<T: FieldNode>(
    obj: &VObject,
    fields: &BTreeMap<String, T>,
    doc: &Schema,
    migration: &Migration,
    path: &Path,
    depth: usize,
) -> Result<VObject, MigrationError> {
    let mut result = VObject::new();
    for (name, field) in fields {
        if let Some(v) = obj.get(name.as_str()) {
            let migrated = walk(
                v,
                field.node(),
                doc,
                migration,
                &path.field(name),
                depth + 1,
            )?;
            result.insert(name.clone(), migrated);
        }
    }
    Ok(result)
}

/// Migrate an enum value: a single-member object tagged by the live
/// variant's name, exactly as `schema::read` produces it. `def_name` gates
/// whether [`Target::Variant`] ops apply, per [`walk_named`]'s contract.
fn walk_enum(
    value: &Value,
    def_name: Option<&str>,
    variants: &BTreeMap<String, VariantKind>,
    doc: &Schema,
    migration: &Migration,
    path: &Path,
    depth: usize,
) -> Result<VObject, MigrationError> {
    let obj = as_object(value, path)?;
    if obj.len() != 1 {
        let found = if obj.is_empty() {
            "empty object"
        } else {
            "multi-member object"
        };
        return Err(mismatch_kind(path, "single-member object", found));
    }
    let (variant_name, payload) = obj.iter().next().expect("length checked to be 1 above");
    let variant_name = variant_name.as_str();
    let kind = variants
        .get(variant_name)
        .ok_or_else(|| mismatch_kind(path, "known enum variant", "unknown variant"))?;
    let vpath = path.field(variant_name);
    let migrated_payload = match kind {
        VariantKind::Unit => payload.clone(),
        VariantKind::Newtype(inner) => walk(payload, inner, doc, migration, &vpath, depth + 1)?,
        VariantKind::Tuple(elems) => walk_tuple(
            as_array(payload, &vpath)?,
            elems,
            doc,
            migration,
            &vpath,
            depth + 1,
        )?,
        VariantKind::Struct(fields) => {
            let payload_obj = as_object(payload, &vpath)?;
            let mut result = walk_struct(payload_obj, fields, doc, migration, &vpath, depth + 1)?;
            if let Some(def) = def_name {
                apply_ops(&mut result, migration, |t| {
                    matches_variant(t, def, variant_name)
                });
            }
            result.into()
        }
    };
    let mut out = VObject::new();
    out.insert(variant_name, migrated_payload);
    Ok(out)
}

/// Migrate a fixed-length sequence (a [`Node::Tuple`], or a tuple enum
/// variant's payload) element-wise with `elems`.
fn walk_tuple(
    arr: &VArray,
    elems: &[Node],
    doc: &Schema,
    migration: &Migration,
    path: &Path,
    depth: usize,
) -> Result<Value, MigrationError> {
    expect_len(path, elems.len(), arr.as_slice().len())?;
    let mut out = VArray::new();
    for (i, (item, schema)) in arr.as_slice().iter().zip(elems).enumerate() {
        out.push(walk(
            item,
            schema,
            doc,
            migration,
            &path.index(i),
            depth + 1,
        )?);
    }
    Ok(out.into())
}

/// Migrate a variable-length sequence ([`Node::List`] or [`Node::Array`])
/// element-wise with a single element schema.
fn walk_seq(
    arr: &VArray,
    elem: &Node,
    doc: &Schema,
    migration: &Migration,
    path: &Path,
    depth: usize,
) -> Result<Value, MigrationError> {
    let mut out = VArray::new();
    for (i, item) in arr.as_slice().iter().enumerate() {
        out.push(walk(item, elem, doc, migration, &path.index(i), depth + 1)?);
    }
    Ok(out.into())
}

/// Migrate a map: a name-keyed object for a scalar key, or an array of
/// `{ "k": ..., "v": ... }` pairs for a composite key — the same layout
/// `schema::read` produces.
fn walk_map(
    value: &Value,
    key: &Node,
    val: &Node,
    doc: &Schema,
    migration: &Migration,
    path: &Path,
    depth: usize,
) -> Result<Value, MigrationError> {
    if is_scalar_schema(key) {
        let obj = as_object(value, path)?;
        let mut out = VObject::new();
        for (k, v) in obj.iter() {
            let migrated = walk(v, val, doc, migration, &path.field(k.as_str()), depth + 1)?;
            out.insert(k.clone(), migrated);
        }
        return Ok(out.into());
    }
    let arr = as_array(value, path)?;
    let mut out = VArray::new();
    for (i, item) in arr.as_slice().iter().enumerate() {
        let ipath = path.index(i);
        let pair = as_object(item, &ipath)?;
        let k = pair
            .get("k")
            .ok_or_else(|| mismatch(&ipath, "object with \"k\" and \"v\"", item))?;
        let v = pair
            .get("v")
            .ok_or_else(|| mismatch(&ipath, "object with \"k\" and \"v\"", item))?;
        let mut pair_out = VObject::new();
        pair_out.insert(
            "k",
            walk(k, key, doc, migration, &ipath.field("k"), depth + 1)?,
        );
        pair_out.insert(
            "v",
            walk(v, val, doc, migration, &ipath.field("v"), depth + 1)?,
        );
        out.push(pair_out);
    }
    Ok(out.into())
}

/// Apply every op in `migration.ops` whose target `matches`, in document
/// order — each a total transformation of `object`, so conflicts between ops
/// are well-defined rather than an error.
fn apply_ops(object: &mut VObject, migration: &Migration, matches: impl Fn(&Target) -> bool) {
    for op in &migration.ops {
        if matches(&op.at) {
            apply_change(object, &op.change);
        }
    }
}

fn apply_change(object: &mut VObject, change: &Change) {
    match change {
        Change::Rename { from, to } => {
            if let Some(v) = object.remove(from.as_str()) {
                object.insert(to.clone(), v);
            }
        }
        Change::Remove { field } => {
            object.remove(field.as_str());
        }
        Change::Add { field, default } => {
            object.insert(field.clone(), Value::from(default));
        }
        // `Node::Optional` reads as `null` or the inner value directly, so
        // `Some(x)` and `x` are the same `Value` — `Wrap` is the identity here.
        Change::Wrap { .. } => {}
    }
}

fn matches_def(target: &Target, name: &str) -> bool {
    matches!(target, Target::Def(n) if n == name)
}

fn matches_variant(target: &Target, def: &str, variant: &str) -> bool {
    matches!(target, Target::Variant { def: d, variant: v } if d == def && v == variant)
}

impl From<&Constant> for Value {
    fn from(c: &Constant) -> Self {
        match c {
            Constant::Null => Value::NULL,
            Constant::Bool(b) => Value::from(*b),
            Constant::Integer(i) => VNumber::from_i128(*i as i128).into(),
            Constant::Float(f) => Value::from(*f),
            Constant::Text(s) => Value::from(s.as_str()),
            Constant::List(items) => items.iter().map(Value::from).collect::<VArray>().into(),
            Constant::Object(map) => map
                .iter()
                .map(|(k, v)| (k.clone(), Value::from(v)))
                .collect::<VObject>()
                .into(),
        }
    }
}

fn as_object<'v>(value: &'v Value, path: &Path) -> Result<&'v VObject, MigrationError> {
    value
        .as_object()
        .ok_or_else(|| mismatch(path, "object", value))
}

fn as_array<'v>(value: &'v Value, path: &Path) -> Result<&'v VArray, MigrationError> {
    value
        .as_array()
        .ok_or_else(|| mismatch(path, "array", value))
}

/// Refuse a fixed-length sequence whose element count the source schema does
/// not describe, rather than truncating it against the shorter of the two.
fn expect_len(path: &Path, expected: usize, found: usize) -> Result<(), MigrationError> {
    if expected == found {
        return Ok(());
    }
    Err(MigrationError::LengthMismatch {
        path: path.show(),
        expected,
        found,
    })
}

fn mismatch(path: &Path, expected: &'static str, value: &Value) -> MigrationError {
    mismatch_kind(path, expected, value_kind(value))
}

fn mismatch_kind(path: &Path, expected: &'static str, found: &'static str) -> MigrationError {
    MigrationError::Mismatch {
        path: path.show(),
        expected,
        found,
    }
}

/// A value's runtime kind, for [`MigrationError::Mismatch`] diagnostics.
fn value_kind(v: &Value) -> &'static str {
    if v.is_null() {
        "null"
    } else if v.is_bool() {
        "bool"
    } else if v.is_number() {
        "number"
    } else if v.is_string() {
        "string"
    } else if v.is_bytes() {
        "bytes"
    } else if v.is_array() {
        "array"
    } else if v.is_object() {
        "object"
    } else {
        "value"
    }
}

/// Whether `schema` decides scalar-keyed map layout. Mirrors the read path's
/// `is_scalar_schema` (`schema/read.rs`), duplicated here since that one is
/// private to its module.
fn is_scalar_schema(schema: &Node) -> bool {
    matches!(
        schema,
        Node::Bool
            | Node::Char
            | Node::String
            | Node::I8
            | Node::I16
            | Node::I32
            | Node::I64
            | Node::I128
            | Node::ISize
            | Node::U8
            | Node::U16
            | Node::U32
            | Node::U64
            | Node::U128
            | Node::USize
            | Node::F32
            | Node::F64
    )
}
