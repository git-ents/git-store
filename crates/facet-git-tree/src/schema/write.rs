//! Schema-directed serialization: encoding a full-fidelity
//! [`facet_value::Value`] as a Git tree guided by a [`SchemaDoc`].
//!
//! The write-side mirror of [`schema::read`](crate::schema::read). A dynamic
//! value parsed from JSON does not, on its own, know which typed encoding a
//! field expects — a bare string cannot tell that its field is an `Option`
//! whose `Some` payload must be wrapped in a `some` entry, and a bare number
//! cannot tell that its field is `f64` rather than an integer. The schema
//! supplies exactly that missing type information, so `serialize_value_with_schema`
//! writes the *same* objects — byte-for-byte, and therefore the same object
//! ids — that the equivalent typed value would produce through
//! [`serialize`](crate::serialize).
//!
//! Encoding is validation: the walk fails with the offending path the moment a
//! value diverges from what its schema node accepts, so there is no separate
//! validation pass to drift out of sync with the encoder. The accepted set is
//! exactly the image of [`deserialize_value_with_schema`](crate::deserialize_value_with_schema)
//! — every `Value` that read could produce round-trips — plus the two
//! deterministic bridges a JSON-authored value forces: a lossless integer into
//! a float field, and a string into a `Bytes` field.
//!
//! The normative mapping lives in `docs/specification.adoc` under
//! `serialization.schema-directed`.

use core::fmt::Write as _;

use facet::Peek;
use facet_value::{VArray, VNumber, Value};
use gix_object::Write;

use crate::de::MAX_DEPTH;
use crate::error::{SchemaWriteError, SerializeError};
use crate::schema::{FieldSchema, Schema, SchemaDoc, VariantKind};
use crate::ser::{float_text, serialize_node, write_leaf_blob};
use crate::{EntryKind, EntryMode, ObjectId, TreeEntry, check_key};

/// Serialize `value` into `store` as the tree `doc` describes, returning the
/// root [`ObjectId`].
///
/// `store` is any `gix` [`Write`] sink, exactly as for
/// [`serialize_into`](crate::serialize_into). The write fails — with the path
/// in `value` at which the mismatch occurred — if the value does not conform
/// to the schema, rather than emitting a lossy or ambiguous encoding.
///
/// ```
/// use facet::Facet;
/// use facet_git_tree::{schema_of, serialize, serialize_value_with_schema};
/// use facet_value::value;
///
/// #[derive(Facet)]
/// struct Point {
///     x: f64,
///     y: f64,
/// }
///
/// let doc = schema_of::<Point>()?;
/// let store = facet_git_tree::ObjectStore::default();
/// let via_schema = serialize_value_with_schema(&value!({ "x": 1.0, "y": 2.0 }), &doc, &store)?;
///
/// // Identical to the typed encoding, down to the object id.
/// let (typed, _) = serialize(&Point { x: 1.0, y: 2.0 })?;
/// assert_eq!(via_schema, typed);
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn serialize_value_with_schema<W: Write + ?Sized>(
    value: &Value,
    doc: &SchemaDoc,
    store: &W,
) -> Result<ObjectId, SchemaWriteError> {
    let (oid, _kind) = write_node(value, &doc.root, doc, store, &Path::ROOT, 0)?;
    Ok(oid)
}

/// A location within the value being written, threaded through the walk so a
/// mismatch can name exactly where it happened.
///
/// Borrowed and stack-linked, so the happy path allocates nothing; only
/// [`Path::show`] materializes a string, at the point an error is built.
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

    /// Render the path from the root as `$.field[0].inner`.
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
                // Writing to a String is infallible.
                Seg::Index(i) => {
                    let _ = write!(s, "[{i}]");
                }
            }
        }
        s
    }
}

/// Write one schema node's value from `value`.
///
/// Returns the encoded object's id and its entry kind (blob vs. tree), so a
/// caller embedding it in a parent tree can set the entry mode exactly as the
/// typed encoder does.
///
/// `depth` counts every hop — including [`Schema::Ref`] resolution — against
/// the same [`MAX_DEPTH`] limit that bounds deserialization, so a `Ref`-to-`Ref`
/// cycle in the schema fails rather than recursing unboundedly.
fn write_node<W: Write + ?Sized>(
    value: &Value,
    schema: &Schema,
    doc: &SchemaDoc,
    store: &W,
    path: &Path,
    depth: usize,
) -> Result<(ObjectId, EntryKind), SchemaWriteError> {
    if depth > MAX_DEPTH {
        return Err(SchemaWriteError::MaxDepth {
            path: path.show(),
            depth: MAX_DEPTH,
        });
    }
    match schema {
        Schema::Unit => {
            if value.is_null() {
                tree(store, vec![])
            } else {
                Err(expected(path, "null", value))
            }
        }
        Schema::Bool => match value.as_bool() {
            Some(true) => blob(store, b"true"),
            Some(false) => blob(store, b"false"),
            None => Err(expected(path, "bool", value)),
        },
        Schema::Char => {
            let mut buf = [0u8; 4];
            if let Some(c) = value.as_char() {
                blob(store, c.encode_utf8(&mut buf).as_bytes())
            } else if let Some(c) = value_str(value).and_then(single_char) {
                blob(store, c.encode_utf8(&mut buf).as_bytes())
            } else {
                Err(expected(path, "char or single-character string", value))
            }
        }
        Schema::String => match value_str(value) {
            Some(s) => blob(store, s.as_bytes()),
            None => Err(expected(path, "string", value)),
        },
        Schema::I8 => signed_blob::<i8, W>(value, "I8", path, store),
        Schema::I16 => signed_blob::<i16, W>(value, "I16", path, store),
        Schema::I32 => signed_blob::<i32, W>(value, "I32", path, store),
        Schema::I64 => signed_blob::<i64, W>(value, "I64", path, store),
        Schema::I128 => signed_blob::<i128, W>(value, "I128", path, store),
        Schema::ISize => signed_blob::<isize, W>(value, "ISize", path, store),
        Schema::U8 => unsigned_blob::<u8, W>(value, "U8", path, store),
        Schema::U16 => unsigned_blob::<u16, W>(value, "U16", path, store),
        Schema::U32 => unsigned_blob::<u32, W>(value, "U32", path, store),
        Schema::U64 => unsigned_blob::<u64, W>(value, "U64", path, store),
        Schema::U128 => unsigned_blob::<u128, W>(value, "U128", path, store),
        Schema::USize => unsigned_blob::<usize, W>(value, "USize", path, store),
        Schema::F64 => {
            let n = as_number(value, path)?;
            // A float-backed number is rendered at any magnitude; an
            // integer-backed one must be exactly representable, else refused.
            let f = if n.is_float() {
                n.to_f64_lossy()
            } else {
                n.to_f64().ok_or_else(|| unrepresentable(path, "F64"))?
            };
            blob(store, &float_text(f))
        }
        Schema::F32 => {
            let n = as_number(value, path)?;
            let f: f32 = if n.is_float() {
                n.to_f64_lossy() as f32
            } else {
                n.to_f32().ok_or_else(|| unrepresentable(path, "F32"))?
            };
            blob(store, &float_text(f))
        }
        // `Bytes` reads back as raw bytes; a JSON author who cannot express a
        // byte string writes an ordinary string, whose UTF-8 bytes become the
        // blob — the same blob a typed `Vec<u8>` of those bytes produces.
        Schema::Bytes => {
            if let Some(b) = value.as_bytes() {
                blob(store, b.as_slice())
            } else if let Some(s) = value_str(value) {
                blob(store, s.as_bytes())
            } else {
                Err(expected(path, "bytes or string", value))
            }
        }
        Schema::Struct(fields) => write_named_tree(value, fields, doc, store, path, depth),
        Schema::Tuple(elems) => {
            let arr = as_array(value, path)?;
            if arr.len() != elems.len() {
                return Err(length_mismatch(path, elems.len(), arr.len()));
            }
            write_seq(arr, |i| &elems[i], doc, store, path, depth, false)
        }
        Schema::List(elem) => {
            let arr = as_array(value, path)?;
            write_seq(arr, |_| elem, doc, store, path, depth, true)
        }
        Schema::Array { elem, len } => {
            let arr = as_array(value, path)?;
            if arr.len() != *len {
                return Err(length_mismatch(path, *len, arr.len()));
            }
            write_seq(arr, |_| elem, doc, store, path, depth, true)
        }
        // The key schema decides the layout, exactly as it does on read: a
        // scalar key is a name-keyed object; a composite key is an array of
        // `{ k, v }` pairs. An empty map (either layout) writes the presence
        // marker instead of a literal empty tree, per `crate::marker`.
        Schema::Map { key, value: val } => {
            if is_scalar_schema(key) {
                let obj = as_object(value, path)?;
                let mut entries = Vec::with_capacity(obj.len());
                for (k, v) in obj.iter() {
                    let k = k.as_str();
                    check_key(k).map_err(SerializeError::from)?;
                    let (oid, kind) = write_node(v, val, doc, store, &path.field(k), depth + 1)?;
                    entries.push(TreeEntry {
                        mode: EntryMode::from(kind),
                        filename: k.into(),
                        oid,
                    });
                }
                entries.sort();
                if entries.is_empty() {
                    return marker_tree(store);
                }
                return tree(store, entries);
            }
            write_composite_map(value, key, val, doc, store, path, depth)
        }
        Schema::Optional(inner) => {
            if value.is_null() {
                // None: the presence marker, not a literal empty tree — see
                // `crate::marker`.
                return marker_tree(store);
            }
            let (oid, kind) = write_node(value, inner, doc, store, &path.field("some"), depth + 1)?;
            tree(
                store,
                vec![TreeEntry {
                    mode: EntryMode::from(kind),
                    filename: "some".into(),
                    oid,
                }],
            )
        }
        Schema::Enum(variants) => write_enum(value, variants, doc, store, path, depth),
        // A raw tree is opaque: the value is the referenced object id in hex,
        // handed straight back as a tree reference with no write — mirroring
        // the typed `RawTree` encoding, which likewise emits no object.
        Schema::RawTree => {
            let s = value_str(value).ok_or_else(|| expected(path, "object-id string", value))?;
            let oid =
                ObjectId::from_hex(s.as_bytes()).map_err(|_| SchemaWriteError::InvalidRawTree {
                    path: path.show(),
                    text: s.to_owned(),
                })?;
            Ok((oid, EntryKind::Tree))
        }
        // A dynamic node carries no schema, so the bare heuristic write — the
        // same one `serialize` applies to a `Value` field — is exactly right.
        // Serialization is not depth-bounded (a `Value` is a finite tree), so
        // no budget need be threaded across the boundary as reads must.
        Schema::Dynamic => Ok(serialize_node(Peek::new(value), store)?),
        Schema::Ref(name) => {
            let target = doc
                .defs
                .get(name)
                .ok_or_else(|| SchemaWriteError::UnknownRef {
                    path: path.show(),
                    name: name.clone(),
                })?;
            write_node(value, target, doc, store, path, depth + 1)
        }
    }
}

/// Encode an object as a name-keyed tree, shared by [`Schema::Struct`] and
/// struct enum variants.
///
/// A field absent from the object is skipped, matching the read path's
/// leniency (and the partial trees it can produce); a key the schema does not
/// define is rejected, since the read path never emits one.
fn write_named_tree<W: Write + ?Sized>(
    value: &Value,
    fields: &[FieldSchema],
    doc: &SchemaDoc,
    store: &W,
    path: &Path,
    depth: usize,
) -> Result<(ObjectId, EntryKind), SchemaWriteError> {
    let obj = as_object(value, path)?;
    for k in obj.keys() {
        if !fields.iter().any(|f| f.name == k.as_str()) {
            return Err(SchemaWriteError::UnknownField {
                path: path.show(),
                field: k.as_str().to_owned(),
            });
        }
    }
    let mut entries = Vec::with_capacity(fields.len());
    for field in fields {
        // A `SchemaDoc` is data — `git store schema put` ingests one from
        // hand-authored JSON — so a field name is untrusted input here, unlike
        // a `#[derive(Facet)]` name which is always a Rust identifier. Without
        // this, a field named exactly `crate::marker::MARKER_KEY` would encode
        // to the very tree that means "empty", and read back as empty.
        check_key(&field.name).map_err(SerializeError::from)?;
        if let Some(fv) = obj.get(&field.name) {
            let (oid, kind) = write_node(
                fv,
                &field.schema,
                doc,
                store,
                &path.field(&field.name),
                depth + 1,
            )?;
            entries.push(TreeEntry {
                mode: EntryMode::from(kind),
                filename: field.name.as_str().into(),
                oid,
            });
        }
    }
    entries.sort();
    tree(store, entries)
}

/// Encode an array as an ordinal-named tree, one entry per element, drawing
/// each element's schema from `schema_for`.
///
/// `marker_empty` says whether an empty result takes the presence marker
/// instead of a literal empty tree, per `crate::marker`. It is true for the
/// variable-length sequences — [`Schema::List`], [`Schema::Array`] — whose
/// emptiness is a property of the *value* and so is worth seeing in a diff.
/// It is false for [`Schema::Tuple`], whose length is fixed by the schema:
/// a zero-element tuple encodes identically for every value, so there is
/// nothing to diff, and marking it would both diverge from the typed encoder
/// (which writes the empty tree for a zero-field tuple struct) and produce a
/// tree [`read_tuple`](super::read) refuses to read back.
fn write_seq<'s, W: Write + ?Sized>(
    arr: &VArray,
    schema_for: impl Fn(usize) -> &'s Schema,
    doc: &SchemaDoc,
    store: &W,
    path: &Path,
    depth: usize,
    marker_empty: bool,
) -> Result<(ObjectId, EntryKind), SchemaWriteError> {
    let mut entries = Vec::with_capacity(arr.len());
    for (i, item) in arr.as_slice().iter().enumerate() {
        let (oid, kind) = write_node(item, schema_for(i), doc, store, &path.index(i), depth + 1)?;
        entries.push(TreeEntry {
            mode: EntryMode::from(kind),
            filename: format!("{i:04}").into(),
            oid,
        });
    }
    entries.sort();
    if marker_empty && entries.is_empty() {
        return marker_tree(store);
    }
    tree(store, entries)
}

/// Encode a composite-key map as ordinal-named `{ k, v }` pair sub-trees.
///
/// The value is the pair array the read path produces: an [`Array`] of
/// two-member objects `{ "k": …, "v": … }`. Pair sub-trees are sorted by their
/// own object id before ordinal assignment, exactly as the typed encoder does,
/// so the map stays content-addressed independent of array order.
///
/// [`Array`]: facet_value::Value::as_array
fn write_composite_map<W: Write + ?Sized>(
    value: &Value,
    key: &Schema,
    val: &Schema,
    doc: &SchemaDoc,
    store: &W,
    path: &Path,
    depth: usize,
) -> Result<(ObjectId, EntryKind), SchemaWriteError> {
    let arr = as_array(value, path)?;
    let mut pair_oids: Vec<ObjectId> = Vec::with_capacity(arr.len());
    for (i, item) in arr.as_slice().iter().enumerate() {
        let ipath = path.index(i);
        let obj = as_object(item, &ipath)?;
        let k = obj
            .get("k")
            .ok_or_else(|| expected(&ipath, "object with \"k\" and \"v\"", item))?;
        let v = obj
            .get("v")
            .ok_or_else(|| expected(&ipath, "object with \"k\" and \"v\"", item))?;
        let (k_oid, k_kind) = write_node(k, key, doc, store, &ipath.field("k"), depth + 1)?;
        let (v_oid, v_kind) = write_node(v, val, doc, store, &ipath.field("v"), depth + 1)?;
        let mut pair = vec![
            TreeEntry {
                mode: EntryMode::from(k_kind),
                filename: "k".into(),
                oid: k_oid,
            },
            TreeEntry {
                mode: EntryMode::from(v_kind),
                filename: "v".into(),
                oid: v_oid,
            },
        ];
        pair.sort();
        let (pair_oid, _) = tree(store, pair)?;
        pair_oids.push(pair_oid);
    }
    pair_oids.sort();
    let mut entries = Vec::with_capacity(pair_oids.len());
    for (i, pair_oid) in pair_oids.into_iter().enumerate() {
        entries.push(TreeEntry {
            mode: EntryMode::from(EntryKind::Tree),
            filename: format!("{i:04}").into(),
            oid: pair_oid,
        });
    }
    entries.sort();
    if entries.is_empty() {
        return marker_tree(store);
    }
    tree(store, entries)
}

/// Encode an enum: externally tagged by the live variant's name. A unit
/// variant collapses to a bare blob holding that name (its entire
/// information content); every other variant is a single-member object whose
/// value follows the variant's [`VariantKind`] layout, wrapped in a
/// single-entry tree keyed by the name.
fn write_enum<W: Write + ?Sized>(
    value: &Value,
    variants: &[crate::schema::VariantSchema],
    doc: &SchemaDoc,
    store: &W,
    path: &Path,
    depth: usize,
) -> Result<(ObjectId, EntryKind), SchemaWriteError> {
    let obj = as_object(value, path)?;
    if obj.len() != 1 {
        return Err(SchemaWriteError::MalformedEnum {
            path: path.show(),
            found: obj.len(),
        });
    }
    let (name, payload) = obj.iter().next().expect("length checked to be 1 above");
    let name = name.as_str();
    let variant = variants.iter().find(|v| v.name == name).ok_or_else(|| {
        SchemaWriteError::UnknownVariant {
            path: path.show(),
            variant: name.to_owned(),
            expected: variants.iter().map(|v| v.name.clone()).collect(),
        }
    })?;
    let vpath = path.field(name);

    if let VariantKind::Unit = &variant.kind {
        if !payload.is_null() {
            return Err(expected(&vpath, "null", payload));
        }
        return blob(store, name.as_bytes());
    }

    let (inner_oid, inner_kind) = match &variant.kind {
        VariantKind::Unit => unreachable!("handled above"),
        VariantKind::Newtype(inner) => write_node(payload, inner, doc, store, &vpath, depth + 1)?,
        VariantKind::Tuple(elems) => {
            let arr = as_array(payload, &vpath)?;
            if arr.len() != elems.len() {
                return Err(length_mismatch(&vpath, elems.len(), arr.len()));
            }
            write_seq(arr, |i| &elems[i], doc, store, &vpath, depth + 1, false)?
        }
        VariantKind::Struct(fields) => {
            write_named_tree(payload, fields, doc, store, &vpath, depth + 1)?
        }
    };
    tree(
        store,
        vec![TreeEntry {
            mode: EntryMode::from(inner_kind),
            filename: name.into(),
            oid: inner_oid,
        }],
    )
}

// --- scalar helpers ---

/// Signed integers of any width: the value must be an in-range integer (a
/// float-backed number is refused — the read path never emits one for an
/// integer node).
fn signed_blob<T, W>(
    value: &Value,
    schema: &'static str,
    path: &Path,
    store: &W,
) -> Result<(ObjectId, EntryKind), SchemaWriteError>
where
    T: TryFrom<i128> + core::fmt::Display,
    W: Write + ?Sized,
{
    let n = as_integer(value, schema, path)?;
    let i = n.to_i128().ok_or_else(|| out_of_range(path, schema, n))?;
    let t = T::try_from(i).map_err(|_| out_of_range(path, schema, n))?;
    blob(store, t.to_string().as_bytes())
}

/// Unsigned integers of any width. `to_u128` yields `None` for a negative
/// value, which is how a negative number is rejected for an unsigned node.
fn unsigned_blob<T, W>(
    value: &Value,
    schema: &'static str,
    path: &Path,
    store: &W,
) -> Result<(ObjectId, EntryKind), SchemaWriteError>
where
    T: TryFrom<u128> + core::fmt::Display,
    W: Write + ?Sized,
{
    let n = as_integer(value, schema, path)?;
    let u = n.to_u128().ok_or_else(|| out_of_range(path, schema, n))?;
    let t = T::try_from(u).map_err(|_| out_of_range(path, schema, n))?;
    blob(store, t.to_string().as_bytes())
}

// --- accessors and error builders ---

fn value_str(v: &Value) -> Option<&str> {
    v.as_string().map(|s| s.as_str())
}

fn single_char(s: &str) -> Option<char> {
    let mut it = s.chars();
    match (it.next(), it.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    }
}

fn as_number<'v>(value: &'v Value, path: &Path) -> Result<&'v VNumber, SchemaWriteError> {
    value
        .as_number()
        .ok_or_else(|| expected(path, "number", value))
}

/// A number that is integer-backed. A float-backed number is refused for an
/// integer schema node: the read path only ever emits integer-backed numbers
/// there, and float-to-integer is not one of the accepted JSON bridges.
fn as_integer<'v>(
    value: &'v Value,
    schema: &'static str,
    path: &Path,
) -> Result<&'v VNumber, SchemaWriteError> {
    let n = as_number(value, path)?;
    if n.is_float() {
        return Err(SchemaWriteError::Expected {
            path: path.show(),
            expected: schema,
            found: "float",
        });
    }
    Ok(n)
}

fn as_array<'v>(value: &'v Value, path: &Path) -> Result<&'v VArray, SchemaWriteError> {
    value
        .as_array()
        .ok_or_else(|| expected(path, "array", value))
}

fn as_object<'v>(
    value: &'v Value,
    path: &Path,
) -> Result<&'v facet_value::VObject, SchemaWriteError> {
    value
        .as_object()
        .ok_or_else(|| expected(path, "object", value))
}

fn blob<W: Write + ?Sized>(
    store: &W,
    bytes: &[u8],
) -> Result<(ObjectId, EntryKind), SchemaWriteError> {
    let oid = write_leaf_blob(store, bytes)?;
    Ok((oid, EntryKind::Blob))
}

fn tree<W: Write + ?Sized>(
    store: &W,
    entries: Vec<TreeEntry>,
) -> Result<(ObjectId, EntryKind), SchemaWriteError> {
    let oid = store
        .write(&gix_object::Tree { entries })
        .map_err(SerializeError::Backend)?;
    Ok((oid, EntryKind::Tree))
}

/// Write the presence-marker tree (`crate::marker`) in place of a literal
/// empty tree, for `None` and an empty `List`/`Array`/`Tuple`/`Map`.
fn marker_tree<W: Write + ?Sized>(store: &W) -> Result<(ObjectId, EntryKind), SchemaWriteError> {
    let oid = crate::marker::write_marker_tree(store)?;
    Ok((oid, EntryKind::Tree))
}

fn expected(path: &Path, expected: &'static str, found: &Value) -> SchemaWriteError {
    SchemaWriteError::Expected {
        path: path.show(),
        expected,
        found: value_kind(found),
    }
}

fn out_of_range(path: &Path, schema: &'static str, n: &VNumber) -> SchemaWriteError {
    SchemaWriteError::NumberOutOfRange {
        path: path.show(),
        schema,
        value: number_text(n),
    }
}

fn length_mismatch(path: &Path, expected: usize, found: usize) -> SchemaWriteError {
    SchemaWriteError::LengthMismatch {
        path: path.show(),
        expected,
        found,
    }
}

fn unrepresentable(path: &Path, schema: &'static str) -> SchemaWriteError {
    SchemaWriteError::UnrepresentableNumber {
        path: path.show(),
        schema,
    }
}

/// A number's value as text, for diagnostics only.
fn number_text(n: &VNumber) -> String {
    if let Some(i) = n.to_i128() {
        i.to_string()
    } else if let Some(u) = n.to_u128() {
        u.to_string()
    } else {
        n.to_f64_lossy().to_string()
    }
}

/// A value's runtime kind, for mismatch messages.
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
    } else if v.is_char() {
        "char"
    } else if v.is_datetime() {
        "datetime"
    } else if v.is_qname() {
        "qname"
    } else if v.is_uuid() {
        "uuid"
    } else {
        "value"
    }
}

/// Whether `schema` is a scalar node — the same classification that decides
/// map layout on read.
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
