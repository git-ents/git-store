//! Schema-driven deserialization: reading a tree into a full-fidelity
//! [`facet_value::Value`] guided by a [`SchemaDoc`].
//!
//! Where the bare heuristic read is documented lossy (numbers come back as
//! strings, enums as plain objects), a schema supplies the type information
//! the schemaless encoding leaves off disk, so `deserialize_value_with_schema`
//! recovers numbers as numbers, bools as bools, and enums as tagged objects.
//!
//! The normative mapping lives in `docs/specification.adoc` under
//! `deserialization.schema-driven`.

use facet_value::{VArray, VNumber, VObject, Value};
use gix_object::{Find, Kind};

use crate::de::{
    MAX_DEPTH, deserialize_at_depth, extract_enum_entry, find_blob_bytes, find_object,
    find_tree_entries, map_pair_entries, sort_by_ordinal, validate_option_entries,
};
use crate::error::{DeserializeError, SchemaReadError};
use crate::schema::{Schema, SchemaDoc, VariantKind};
use crate::{EntryKind, ObjectId};

/// Deserialize the tree at `root` into a full-fidelity [`Value`], guided by
/// `doc`.
///
/// `store` is any `gix` [`Find`] source, exactly as for
/// [`deserialize`](crate::deserialize). The read fails if the object graph
/// does not match the schema — a wrong scalar form, a missing definition, a
/// fixed-length mismatch — rather than guessing.
///
/// ```
/// use facet::Facet;
/// use facet_git_tree::{deserialize_value_with_schema, schema_of, serialize};
/// use facet_value::value;
///
/// #[derive(Facet)]
/// struct Point {
///     x: f64,
///     y: f64,
/// }
///
/// let (oid, store) = serialize(&Point { x: 1.0, y: 2.0 })?;
/// let doc = schema_of::<Point>()?;
/// let v = deserialize_value_with_schema(&oid, &doc, &store)?;
/// assert_eq!(v, value!({ "x": 1.0, "y": 2.0 }));
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn deserialize_value_with_schema<F: Find + ?Sized>(
    root: &ObjectId,
    doc: &SchemaDoc,
    store: &F,
) -> Result<Value, SchemaReadError> {
    read_node(root, &doc.root, doc, store, 0)
}

/// Check that the tree at `root` conforms to `doc` without keeping the value.
///
/// Exactly [`deserialize_value_with_schema`] with the result discarded.
pub fn validate_with_schema<F: Find + ?Sized>(
    root: &ObjectId,
    doc: &SchemaDoc,
    store: &F,
) -> Result<(), SchemaReadError> {
    deserialize_value_with_schema(root, doc, store).map(|_| ())
}

/// A tree's entries, shared by every tree-shaped schema node.
type Entries = Vec<(String, ObjectId, EntryKind)>;

/// Read one schema node's value from the object at `oid`.
///
/// `depth` counts every hop — including [`Schema::Ref`] resolution — against
/// the same [`MAX_DEPTH`] limit that bounds typed deserialization, so
/// `Ref`-to-`Ref` chains cannot recurse unboundedly.
fn read_node<F: Find + ?Sized>(
    oid: &ObjectId,
    schema: &Schema,
    doc: &SchemaDoc,
    store: &F,
    depth: usize,
) -> Result<Value, SchemaReadError> {
    if depth > MAX_DEPTH {
        return Err(DeserializeError::MaxDepth(MAX_DEPTH).into());
    }
    match schema {
        Schema::Unit => {
            expect_empty_tree(oid, store)?;
            Ok(Value::NULL)
        }
        Schema::Bool => match blob_text(oid, store)?.as_str() {
            "true" => Ok(Value::from(true)),
            "false" => Ok(Value::from(false)),
            other => Err(invalid_scalar("Bool", other)),
        },
        Schema::Char => {
            let text = blob_text(oid, store)?;
            let mut chars = text.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Ok(Value::from(c)),
                _ => Err(invalid_scalar("Char", &text)),
            }
        }
        Schema::String => Ok(Value::from(blob_text(oid, store)?)),
        Schema::I8 => int_value::<i8, F>(oid, store, "I8"),
        Schema::I16 => int_value::<i16, F>(oid, store, "I16"),
        Schema::I32 => int_value::<i32, F>(oid, store, "I32"),
        Schema::I64 => int_value::<i64, F>(oid, store, "I64"),
        Schema::I128 => int_value::<i128, F>(oid, store, "I128"),
        // `isize`/`usize` have no `From` into the 128-bit widths (their size
        // is platform-defined), but are at most 64 bits on every supported
        // platform, so the widening cast is lossless.
        Schema::ISize => {
            let text = blob_text(oid, store)?;
            let v: isize = text.parse().map_err(|_| invalid_scalar("ISize", &text))?;
            Ok(VNumber::from_i128(v as i128).into())
        }
        Schema::U8 => uint_value::<u8, F>(oid, store, "U8"),
        Schema::U16 => uint_value::<u16, F>(oid, store, "U16"),
        Schema::U32 => uint_value::<u32, F>(oid, store, "U32"),
        Schema::U64 => uint_value::<u64, F>(oid, store, "U64"),
        Schema::U128 => uint_value::<u128, F>(oid, store, "U128"),
        Schema::USize => {
            let text = blob_text(oid, store)?;
            let v: usize = text.parse().map_err(|_| invalid_scalar("USize", &text))?;
            Ok(VNumber::from_u128(v as u128).into())
        }
        Schema::F32 => {
            let text = blob_text(oid, store)?;
            let v: f32 = text.parse().map_err(|_| invalid_scalar("F32", &text))?;
            Ok(Value::from(v))
        }
        Schema::F64 => {
            let text = blob_text(oid, store)?;
            let v: f64 = text.parse().map_err(|_| invalid_scalar("F64", &text))?;
            Ok(Value::from(v))
        }
        Schema::Bytes => Ok(Value::from(find_blob_bytes(oid, store)?)),
        Schema::Struct(fields) => {
            let entries = find_tree_entries(oid, store)?;
            Ok(read_struct(&entries, fields, doc, store, depth)?.into())
        }
        Schema::Tuple(elems) => {
            let entries = find_tree_entries(oid, store)?;
            Ok(read_tuple(entries, elems, doc, store, depth)?.into())
        }
        Schema::List(elem) => {
            let mut entries = find_tree_entries(oid, store)?;
            if crate::marker::is_marker(&entries) {
                entries.clear();
            }
            sort_by_ordinal(&mut entries)?;
            let mut array = VArray::new();
            for (_, child_oid, _) in entries {
                array.push(read_node(&child_oid, elem, doc, store, depth + 1)?);
            }
            Ok(array.into())
        }
        Schema::Array { elem, len } => {
            let mut entries = find_tree_entries(oid, store)?;
            if crate::marker::is_marker(&entries) {
                entries.clear();
            }
            if entries.len() != *len {
                return Err(SchemaReadError::ArrayLenMismatch {
                    expected: *len,
                    found: entries.len(),
                });
            }
            sort_by_ordinal(&mut entries)?;
            let mut array = VArray::new();
            for (_, child_oid, _) in entries {
                array.push(read_node(&child_oid, elem, doc, store, depth + 1)?);
            }
            Ok(array.into())
        }
        // The key schema decides the layout, exactly as the static key shape
        // does on write: scalar keys name the entries directly; composite keys
        // store ordinal-named `{ k, v }` pair sub-trees. The marker tree
        // written for an empty map (either layout) is stripped up front.
        Schema::Map { key, value } => {
            let mut entries = find_tree_entries(oid, store)?;
            if crate::marker::is_marker(&entries) {
                entries.clear();
            }
            if is_scalar_schema(key) {
                let mut object = VObject::new();
                for (name, child_oid, _) in entries {
                    let v = read_node(&child_oid, value, doc, store, depth + 1)?;
                    object.insert(name, v);
                }
                return Ok(object.into());
            }
            let mut array = VArray::new();
            for (_, pair_oid, _) in entries {
                let pair = find_tree_entries(&pair_oid, store)?;
                let (k_oid, v_oid) = map_pair_entries(&pair)?;
                let mut object = VObject::new();
                object.insert("k", read_node(&k_oid, key, doc, store, depth + 1)?);
                object.insert("v", read_node(&v_oid, value, doc, store, depth + 1)?);
                array.push(object);
            }
            Ok(array.into())
        }
        Schema::Optional(inner) => {
            let entries = find_tree_entries(oid, store)?;
            let Some(inner_oid) = validate_option_entries(&entries)? else {
                return Ok(Value::NULL);
            };
            read_node(&inner_oid, inner, doc, store, depth + 1)
        }
        Schema::Enum(variants) => {
            let (variant_name, inner_oid) = extract_enum_entry(oid, store)?;
            let Some(variant) = variants.iter().find(|v| v.name == variant_name) else {
                return Err(SchemaReadError::UnknownVariant {
                    variant: variant_name,
                    expected: variants.iter().map(|v| v.name.clone()).collect(),
                });
            };
            let payload = match (&variant.kind, inner_oid) {
                // Unit variant, tagged with a blob (the normal case): the
                // variant name is the payload's entire content.
                (VariantKind::Unit, None) => Value::NULL,
                (VariantKind::Unit, Some(_)) => {
                    return Err(DeserializeError::UnitVariantIsTree {
                        variant: variant_name,
                    }
                    .into());
                }
                (VariantKind::Newtype(inner), Some(inner_oid)) => {
                    read_node(&inner_oid, inner, doc, store, depth + 1)?
                }
                (VariantKind::Tuple(elems), Some(inner_oid)) => {
                    let inner_entries = find_tree_entries(&inner_oid, store)?;
                    read_tuple(inner_entries, elems, doc, store, depth + 1)?.into()
                }
                (VariantKind::Struct(fields), Some(inner_oid)) => {
                    let inner_entries = find_tree_entries(&inner_oid, store)?;
                    read_struct(&inner_entries, fields, doc, store, depth + 1)?.into()
                }
                (_, None) => {
                    return Err(DeserializeError::VariantPayloadIsBlob {
                        variant: variant_name,
                    }
                    .into());
                }
            };
            let mut object = VObject::new();
            object.insert(variant.name.clone(), payload);
            Ok(object.into())
        }
        // A raw tree is opaque to the schema: surface the reference itself,
        // as 40-character lowercase hex, after verifying it is a tree.
        Schema::RawTree => {
            let mut buf = Vec::new();
            let data = find_object(oid, &mut buf, store)?;
            if data.kind != Kind::Tree {
                return Err(DeserializeError::NotATree(*oid).into());
            }
            Ok(Value::from(oid.to_string()))
        }
        // A dynamic node carries no schema information by construction, so
        // the bare heuristic read is exactly what applies here. Routed
        // through `deserialize_at_depth` rather than the public
        // `deserialize` (which always starts at depth 0): this read is
        // already `depth` levels into the schema-driven walk, so the typed
        // read underneath it must keep spending from that same budget rather
        // than resetting it — otherwise a `Dynamic` node nested near
        // `MAX_DEPTH` could recurse further than an ordinary typed read of
        // the same effective depth ever could.
        Schema::Dynamic => Ok(deserialize_at_depth::<Value>(oid, store, depth)?),
        Schema::Ref(name) => {
            let Some(target) = doc.defs.get(name) else {
                return Err(SchemaReadError::UnknownRef(name.clone()));
            };
            read_node(oid, target, doc, store, depth + 1)
        }
    }
}

/// Read a name-keyed tree as a [`VObject`], skipping fields whose entry is
/// missing — the same leniency the typed decoder applies.
fn read_struct<F: Find + ?Sized>(
    entries: &Entries,
    fields: &[crate::schema::FieldSchema],
    doc: &SchemaDoc,
    store: &F,
    depth: usize,
) -> Result<VObject, SchemaReadError> {
    let mut object = VObject::new();
    for field in fields {
        let entry = entries.iter().find(|(name, _, _)| *name == field.name);
        if let Some((_, child_oid, _)) = entry {
            let v = read_node(child_oid, &field.schema, doc, store, depth + 1)?;
            object.insert(field.name.clone(), v);
        }
    }
    Ok(object)
}

/// Read an ordinal-named tree as a [`VArray`] paired element-wise with
/// `elems`, requiring the counts to match.
fn read_tuple<F: Find + ?Sized>(
    mut entries: Entries,
    elems: &[Schema],
    doc: &SchemaDoc,
    store: &F,
    depth: usize,
) -> Result<VArray, SchemaReadError> {
    if entries.len() != elems.len() {
        return Err(SchemaReadError::ArrayLenMismatch {
            expected: elems.len(),
            found: entries.len(),
        });
    }
    sort_by_ordinal(&mut entries)?;
    let mut array = VArray::new();
    for ((_, child_oid, _), elem) in entries.iter().zip(elems) {
        array.push(read_node(child_oid, elem, doc, store, depth + 1)?);
    }
    Ok(array)
}

/// Whether `schema` is a scalar node, deciding the map layout exactly as
/// `Def::Scalar` does on the write side.
fn is_scalar_schema(schema: &Schema) -> bool {
    matches!(
        schema,
        Schema::Bool
            | Schema::Char
            | Schema::String
            | Schema::I8
            | Schema::I16
            | Schema::I32
            | Schema::I64
            | Schema::I128
            | Schema::ISize
            | Schema::U8
            | Schema::U16
            | Schema::U32
            | Schema::U64
            | Schema::U128
            | Schema::USize
            | Schema::F32
            | Schema::F64
    )
}

/// Verify that `oid` is an empty tree (a `Unit` value or unit variant
/// payload).
fn expect_empty_tree<F: Find + ?Sized>(oid: &ObjectId, store: &F) -> Result<(), SchemaReadError> {
    let entries = find_tree_entries(oid, store)?;
    if !entries.is_empty() {
        return Err(SchemaReadError::MalformedUnit {
            found: entries.len(),
        });
    }
    Ok(())
}

/// A scalar blob's UTF-8 text.
fn blob_text<F: Find + ?Sized>(oid: &ObjectId, store: &F) -> Result<String, SchemaReadError> {
    let bytes = find_blob_bytes(oid, store)?;
    String::from_utf8(bytes).map_err(|_| DeserializeError::NonUtf8Blob(*oid).into())
}

/// Signed integers of any width, parsed exactly and widened to `i128` — never
/// routed through floating point.
fn int_value<T, F>(
    oid: &ObjectId,
    store: &F,
    schema: &'static str,
) -> Result<Value, SchemaReadError>
where
    T: std::str::FromStr + Into<i128>,
    F: Find + ?Sized,
{
    let text = blob_text(oid, store)?;
    let v: T = text.parse().map_err(|_| invalid_scalar(schema, &text))?;
    Ok(VNumber::from_i128(v.into()).into())
}

/// Unsigned integers of any width, parsed exactly and widened to `u128` —
/// never routed through floating point.
fn uint_value<T, F>(
    oid: &ObjectId,
    store: &F,
    schema: &'static str,
) -> Result<Value, SchemaReadError>
where
    T: std::str::FromStr + Into<u128>,
    F: Find + ?Sized,
{
    let text = blob_text(oid, store)?;
    let v: T = text.parse().map_err(|_| invalid_scalar(schema, &text))?;
    Ok(VNumber::from_u128(v.into()).into())
}

/// Shorthand for [`SchemaReadError::InvalidScalar`].
fn invalid_scalar(schema: &'static str, text: &str) -> SchemaReadError {
    SchemaReadError::InvalidScalar {
        schema,
        text: text.to_owned(),
    }
}
