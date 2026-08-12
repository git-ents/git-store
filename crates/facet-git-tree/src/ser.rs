//! Serialization: encoding [`facet::Facet`] values as Git objects.

use facet::{Def, DynDateTimeKind, DynValueKind, Peek};
use gix_object::{Kind, Write};

use crate::check_key;
use crate::classify::{ShapeClass, classify, collapse_shape};
use crate::de::MAX_DEPTH;
use crate::error::SerializeError;
use crate::schema::scalar_node;
use crate::store::ObjectStore;
use crate::{EntryKind, EntryMode, ObjectId, RawBlob, RawTree, TreeEntry};

/// Collapse a `facet` reflection error to [`SerializeError::Reflect`].
///
/// `facet`'s `Peek` operations return their own non-`'static` error types;
/// this collapses them to the dedicated text-carrying variant at the call site
/// without a bespoke closure every time.
fn reflect(e: impl std::fmt::Display) -> SerializeError {
    SerializeError::Reflect(e.to_string())
}

/// Serialize a [`facet::Facet`] value into the given `gix` object `store`.
///
/// Returns the root tree [`ObjectId`].
pub fn serialize_into<T, W>(value: &T, store: &W) -> Result<ObjectId, SerializeError>
where
    T: for<'a> facet::Facet<'a>,
    W: Write + ?Sized,
{
    serialize_root(Peek::new(value), store)
}

/// Serialize a [`facet::Facet`] value into a fresh [`ObjectStore`].
///
/// Dynamic values have no runtime type markers, so their heuristic read is
/// intentionally lossy; typed values round-trip through their supplied shape.
///
/// Returns the root tree [`ObjectId`] and the store containing all reachable objects.
pub fn serialize<T: for<'a> facet::Facet<'a>>(
    value: &T,
) -> Result<(ObjectId, ObjectStore), SerializeError> {
    let store = ObjectStore::default();
    let root = serialize_root(Peek::new(value), &store)?;
    Ok((root, store))
}

/// Serialize an already-constructed [`Peek`] into the given `gix` object `store`.
///
/// Returns the root tree [`ObjectId`].
pub fn serialize_peek_into<W>(peek: Peek<'_, '_>, store: &W) -> Result<ObjectId, SerializeError>
where
    W: Write + ?Sized,
{
    serialize_root(peek, store)
}

/// Serialize an already-constructed [`Peek`] into a fresh [`ObjectStore`].
///
/// Returns the root [`ObjectId`] and the store containing all reachable objects.
pub fn serialize_peek(peek: Peek<'_, '_>) -> Result<(ObjectId, ObjectStore), SerializeError> {
    let store = ObjectStore::default();
    let root = serialize_root(peek, &store)?;
    Ok((root, store))
}

fn serialize_root<W: Write + ?Sized>(
    peek: Peek<'_, '_>,
    store: &W,
) -> Result<ObjectId, SerializeError> {
    let (oid, _kind) = serialize_node(peek, store, 0)?;
    Ok(oid)
}

pub(crate) fn serialize_node<W: Write + ?Sized>(
    peek: Peek<'_, '_>,
    store: &W,
    depth: usize,
) -> Result<(ObjectId, EntryKind), SerializeError> {
    let peek = peek.innermost_peek();
    let shape = peek.shape();

    if depth > MAX_DEPTH {
        return Err(SerializeError::MaxDepth(MAX_DEPTH));
    }

    match classify(shape) {
        ShapeClass::RawTree => {
            let rt = peek.get::<RawTree>().map_err(reflect)?;
            Ok((rt.oid(), EntryKind::Tree))
        }
        ShapeClass::RawBlob => {
            let rb = peek.get::<RawBlob>().map_err(reflect)?;
            Ok((rb.oid(), EntryKind::Blob))
        }
        ShapeClass::Dynamic => serialize_dynamic(peek, store, depth),
        ShapeClass::Scalar => serialize_leaf(peek, store),
        ShapeClass::Bytes => serialize_byte_sequence(peek, store),
        ShapeClass::Struct => serialize_struct(peek, store, depth),
        ShapeClass::Sequence => serialize_sequence_node(peek, store, depth),
        ShapeClass::Map => serialize_map(peek, store, depth),
        ShapeClass::Option => serialize_option(peek, store, depth),
        ShapeClass::Enum => serialize_enum(peek, store, depth),
        ShapeClass::TransparentPointer | ShapeClass::TransparentNewtype => {
            unreachable!("innermost_peek must collapse transparent shapes")
        }
        ShapeClass::Unsupported => Err(SerializeError::Unsupported(shape.type_identifier)),
    }
}

fn serialize_leaf<W: Write + ?Sized>(
    peek: Peek<'_, '_>,
    store: &W,
) -> Result<(ObjectId, EntryKind), SerializeError> {
    let oid = write_leaf_blob(store, &scalar_bytes(peek)?)?;
    Ok((oid, EntryKind::Blob))
}

fn serialize_byte_sequence<W: Write + ?Sized>(
    peek: Peek<'_, '_>,
    store: &W,
) -> Result<(ObjectId, EntryKind), SerializeError> {
    let seq = peek.into_list_like().map_err(reflect)?;
    let mut bytes = Vec::new();
    for item in seq.iter() {
        bytes.push(*item.get::<u8>().map_err(reflect)?);
    }
    let oid = write_leaf_blob(store, &bytes)?;
    Ok((oid, EntryKind::Blob))
}

fn serialize_struct<W: Write + ?Sized>(
    peek: Peek<'_, '_>,
    store: &W,
    depth: usize,
) -> Result<(ObjectId, EntryKind), SerializeError> {
    let facet::Type::User(facet::UserType::Struct(st)) = peek.shape().ty else {
        unreachable!()
    };
    let positional = matches!(
        st.kind,
        facet::StructKind::Tuple | facet::StructKind::TupleStruct
    );
    let ps = peek.into_struct().map_err(reflect)?;
    let mut entries = Vec::with_capacity(st.fields.len());
    for (i, field) in st.fields.iter().enumerate() {
        let (oid, kind) = serialize_node(ps.field(i).map_err(reflect)?, store, depth + 1)?;
        let filename: gix_object::bstr::BString = if positional {
            format!("{i:04}").into()
        } else {
            field.name.into()
        };
        entries.push(TreeEntry {
            mode: EntryMode::from(kind),
            filename,
            oid,
        });
    }
    Ok((write_sorted_tree(store, entries)?, EntryKind::Tree))
}

fn serialize_sequence_node<W: Write + ?Sized>(
    peek: Peek<'_, '_>,
    store: &W,
    depth: usize,
) -> Result<(ObjectId, EntryKind), SerializeError> {
    let entries = serialize_sequence(peek, store, depth)?;
    Ok((
        write_tree_or_presence_marker(store, entries)?,
        EntryKind::Tree,
    ))
}

fn serialize_map<W: Write + ?Sized>(
    peek: Peek<'_, '_>,
    store: &W,
    depth: usize,
) -> Result<(ObjectId, EntryKind), SerializeError> {
    let Def::Map(md) = peek.shape().def else {
        unreachable!()
    };
    let pm = peek.into_map().map_err(reflect)?;
    let scalar_keys = matches!(collapse_shape(md.k).def, Def::Scalar);
    let mut entries = Vec::new();
    if scalar_keys {
        for (k, v) in pm.iter() {
            let key_bytes = scalar_bytes(k)?;
            let key_str =
                std::str::from_utf8(&key_bytes).map_err(|_| SerializeError::NonUtf8MapKey)?;
            check_key(key_str)?;
            let (oid, kind) = serialize_node(v, store, depth + 1)?;
            entries.push(TreeEntry {
                mode: EntryMode::from(kind),
                filename: key_str.into(),
                oid,
            });
        }
    } else {
        let mut pair_oids = Vec::new();
        for (k, v) in pm.iter() {
            let (k_oid, k_kind) = serialize_node(k, store, depth + 1)?;
            let (v_oid, v_kind) = serialize_node(v, store, depth + 1)?;
            let pair = vec![
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
            pair_oids.push(write_sorted_tree(store, pair)?);
        }
        pair_oids.sort();
        for (i, pair_oid) in pair_oids.into_iter().enumerate() {
            entries.push(TreeEntry {
                mode: EntryMode::from(EntryKind::Tree),
                filename: format!("{i:04}").into(),
                oid: pair_oid,
            });
        }
    }
    Ok((
        write_tree_or_presence_marker(store, entries)?,
        EntryKind::Tree,
    ))
}

fn serialize_option<W: Write + ?Sized>(
    peek: Peek<'_, '_>,
    store: &W,
    depth: usize,
) -> Result<(ObjectId, EntryKind), SerializeError> {
    let po = peek.into_option().map_err(reflect)?;
    let Some(inner) = po.value() else {
        return Ok((
            write_tree_or_presence_marker(store, Vec::new())?,
            EntryKind::Tree,
        ));
    };
    let (oid, kind) = serialize_node(inner, store, depth + 1)?;
    let entries = vec![TreeEntry {
        mode: EntryMode::from(kind),
        filename: "some".into(),
        oid,
    }];
    Ok((write_sorted_tree(store, entries)?, EntryKind::Tree))
}

fn serialize_enum<W: Write + ?Sized>(
    peek: Peek<'_, '_>,
    store: &W,
    depth: usize,
) -> Result<(ObjectId, EntryKind), SerializeError> {
    let pe = peek.into_enum().map_err(reflect)?;
    let variant = pe.active_variant().map_err(reflect)?;
    let variant_name = pe.variant_name_active().map_err(reflect)?;
    if variant.data.fields.is_empty() {
        return Ok((
            write_leaf_blob(store, variant_name.as_bytes())?,
            EntryKind::Blob,
        ));
    }

    let positional = matches!(variant.data.kind, facet::StructKind::TupleStruct);
    let newtype = positional && variant.data.fields.len() == 1;
    let (inner_oid, inner_kind) = if newtype {
        let child = pe
            .field(0)
            .map_err(reflect)?
            .ok_or_else(|| SerializeError::Reflect("variant field 0 missing".into()))?;
        serialize_node(child, store, depth + 1)?
    } else {
        let mut entries = Vec::new();
        for (i, field) in variant.data.fields.iter().enumerate() {
            let child = pe
                .field(i)
                .map_err(reflect)?
                .ok_or_else(|| SerializeError::Reflect(format!("variant field {i} missing")))?;
            let (oid, kind) = serialize_node(child, store, depth + 1)?;
            let filename: gix_object::bstr::BString = if positional {
                format!("{i:04}").into()
            } else {
                field.name.into()
            };
            entries.push(TreeEntry {
                mode: EntryMode::from(kind),
                filename,
                oid,
            });
        }
        (write_sorted_tree(store, entries)?, EntryKind::Tree)
    };
    let entries = vec![TreeEntry {
        mode: EntryMode::from(inner_kind),
        filename: variant_name.into(),
        oid: inner_oid,
    }];
    Ok((write_sorted_tree(store, entries)?, EntryKind::Tree))
}

fn write_sorted_tree<W: Write + ?Sized>(
    store: &W,
    mut entries: Vec<TreeEntry>,
) -> Result<ObjectId, SerializeError> {
    entries.sort();
    store
        .write(&gix_object::Tree { entries })
        .map_err(SerializeError::Backend)
}

fn write_tree_or_presence_marker<W: Write + ?Sized>(
    store: &W,
    entries: Vec<TreeEntry>,
) -> Result<ObjectId, SerializeError> {
    if entries.is_empty() {
        crate::marker::write_marker_tree(store)
    } else {
        write_sorted_tree(store, entries)
    }
}

/// Serialize a dynamic value (`Def::DynamicValue`, e.g. `facet_value::Value`)
/// by dispatching on its runtime kind.
///
/// Dynamic kinds use the same encodings as equivalent typed values whenever
/// they can be rendered. Empty dynamic containers and `Null` use the presence
/// marker so Git tooling can observe their presence instead of treating them
/// as absent empty trees.
///
/// If the runtime interface cannot recover an exact representation—especially
/// for out-of-range integers—the value is rejected rather than serialized
/// lossily, which would change its object id.
fn serialize_dynamic<W: Write + ?Sized>(
    peek: Peek<'_, '_>,
    store: &W,
    depth: usize,
) -> Result<(ObjectId, EntryKind), SerializeError> {
    let dv = peek.into_dynamic_value().map_err(reflect)?;

    let blob = |bytes: &[u8]| -> Result<(ObjectId, EntryKind), SerializeError> {
        let oid = write_leaf_blob(store, bytes)?;
        Ok((oid, EntryKind::Blob))
    };
    let tree_or_marker =
        |entries: Vec<TreeEntry>| -> Result<(ObjectId, EntryKind), SerializeError> {
            Ok((
                write_tree_or_presence_marker(store, entries)?,
                EntryKind::Tree,
            ))
        };

    match dv.kind() {
        DynValueKind::Null => tree_or_marker(vec![]),
        DynValueKind::Bool => {
            let b = dv
                .as_bool()
                .ok_or_else(|| reflect("dynamic bool unreadable"))?;
            blob(if b { "true" } else { "false" }.as_bytes())
        }
        // Dynamic chars are surfaced as their UTF-8 string representation.
        DynValueKind::String => {
            let s = dv
                .as_str()
                .ok_or_else(|| reflect("dynamic string unreadable"))?;
            blob(s.as_bytes())
        }
        DynValueKind::Bytes => {
            let b = dv
                .as_bytes()
                .ok_or_else(|| reflect("dynamic bytes unreadable"))?;
            blob(b)
        }
        DynValueKind::Number => {
            // Resolve values beyond the generic vtable's 64-bit accessors
            // without changing the encoding of values those accessors handle.
            #[cfg(feature = "value")]
            {
                if peek.shape().is_type::<facet_value::Value>()
                    && dv.as_i64().is_none()
                    && dv.as_u64().is_none()
                {
                    let v = peek.get::<facet_value::Value>().map_err(reflect)?;
                    if let Some(n) = v.as_number() {
                        // Preserve float-backed values as floats: integer
                        // accessors can expose an exact integer while changing
                        // the shortest-round-tripping decimal representation.
                        if n.is_float() {
                            return blob(&float_text(n.to_f64_lossy()));
                        }
                        // `VNumber` canonicalizes integer representations, so
                        // the range-appropriate accessor is exact.
                        if let Some(i) = n.to_i128() {
                            return blob(i.to_string().as_bytes());
                        }
                        if let Some(u) = n.to_u128() {
                            return blob(u.to_string().as_bytes());
                        }
                    }
                }
            }
            if let Some(i) = dv.as_i64() {
                return blob(i.to_string().as_bytes());
            }
            if let Some(u) = dv.as_u64() {
                return blob(u.to_string().as_bytes());
            }
            if let Some(f) = dv.as_f64() {
                // A whole value here may be a lossy image of an out-of-range
                // integer; reject it rather than change the object id.
                if f.is_finite() && f.trunc() == f {
                    return Err(SerializeError::UnrepresentableNumber);
                }
                return blob(&float_text(f));
            }
            Err(SerializeError::UnrepresentableNumber)
        }
        DynValueKind::Array => {
            let iter = dv
                .array_iter()
                .ok_or_else(|| reflect("dynamic array unreadable"))?;
            let mut entries: Vec<TreeEntry> = Vec::with_capacity(iter.len());
            for (i, item) in iter.enumerate() {
                let (oid, kind) = serialize_node(item, store, depth + 1)?;
                entries.push(TreeEntry {
                    mode: EntryMode::from(kind),
                    filename: format!("{i:04}").into(),
                    oid,
                });
            }
            tree_or_marker(entries)
        }
        DynValueKind::Object => {
            let iter = dv
                .object_iter()
                .ok_or_else(|| reflect("dynamic object unreadable"))?;
            let mut entries: Vec<TreeEntry> = Vec::with_capacity(iter.len());
            for (key, value) in iter {
                check_key(key)?;
                let (oid, kind) = serialize_node(value, store, depth + 1)?;
                entries.push(TreeEntry {
                    mode: EntryMode::from(kind),
                    filename: key.into(),
                    oid,
                });
            }
            tree_or_marker(entries)
        }
        DynValueKind::DateTime => {
            let parts = dv
                .as_datetime()
                .ok_or_else(|| reflect("dynamic datetime unreadable"))?;
            blob(datetime_text(parts)?.as_bytes())
        }
        // QName and UUID text requires the `value` feature's downcast.
        kind @ (DynValueKind::QName | DynValueKind::Uuid) => {
            #[cfg(feature = "value")]
            {
                if let Some(text) = value_special_text(peek)? {
                    return blob(text.as_bytes());
                }
            }
            Err(SerializeError::UnsupportedDynamicKind(format!("{kind:?}")))
        }
        // Refuse future kinds rather than guess an encoding.
        other => Err(SerializeError::UnsupportedDynamicKind(format!("{other:?}"))),
    }
}

/// Render a dynamic datetime as RFC 3339-style text.
///
/// Render the vtable's datetime tuple in the format specified for dynamic
/// datetimes. Negative years pad the magnitude separately so `-5` becomes
/// `-0005`; years at least `10000` are not truncated.
#[allow(clippy::type_complexity)] // the vtable's datetime tuple, taken as-is
fn datetime_text(
    parts: (i32, u8, u8, u8, u8, u8, u32, DynDateTimeKind),
) -> Result<String, SerializeError> {
    let (year, month, day, hour, minute, second, nanos, kind) = parts;
    let year_text = if year < 0 {
        format!("-{:04}", year.unsigned_abs())
    } else {
        format!("{year:04}")
    };
    let date = format!("{year_text}-{month:02}-{day:02}");
    let mut time = format!("{hour:02}:{minute:02}:{second:02}");
    if nanos > 0 {
        let mut frac = format!("{nanos:09}");
        while frac.ends_with('0') {
            frac.pop();
        }
        time.push('.');
        time.push_str(&frac);
    }
    Ok(match kind {
        DynDateTimeKind::Offset { offset_minutes: 0 } => format!("{date}T{time}Z"),
        DynDateTimeKind::Offset { offset_minutes } => {
            let sign = if offset_minutes < 0 { '-' } else { '+' };
            let mag = offset_minutes.unsigned_abs();
            format!("{date}T{time}{sign}{:02}:{:02}", mag / 60, mag % 60)
        }
        DynDateTimeKind::LocalDateTime => format!("{date}T{time}"),
        DynDateTimeKind::LocalDate => date,
        DynDateTimeKind::LocalTime => time,
        // Refuse future kinds rather than emit ambiguous text.
        other => {
            return Err(SerializeError::UnsupportedDynamicKind(format!(
                "DateTime({other:?})"
            )));
        }
    })
}

/// The textual form of a `facet_value::Value` QName or UUID, or `None` when
/// `peek` is not a `facet_value::Value` holding one of those kinds.
///
/// UUIDs use canonical lowercase hyphenated hex; QNames use Clark notation
/// only for non-empty namespaces. Empty and absent namespaces therefore hash
/// to the same blob.
#[cfg(feature = "value")]
fn value_special_text(peek: Peek<'_, '_>) -> Result<Option<String>, SerializeError> {
    use std::fmt::Write as _;

    if !peek.shape().is_type::<facet_value::Value>() {
        return Ok(None);
    }
    let v = peek.get::<facet_value::Value>().map_err(reflect)?;
    if let Some(u) = v.as_uuid() {
        let mut s = String::with_capacity(36);
        for (i, byte) in u.as_bytes().iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                s.push('-');
            }
            let _ = write!(s, "{byte:02x}");
        }
        return Ok(Some(s));
    }
    if let Some(q) = v.as_qname() {
        let local = q
            .local_name()
            .as_string()
            .map(|s| s.as_str().to_owned())
            .ok_or_else(|| reflect("qname local name is not a string"))?;
        return Ok(Some(match q.namespace().and_then(|n| n.as_string()) {
            Some(ns) if !ns.as_str().is_empty() => format!("{{{}}}{local}", ns.as_str()),
            _ => local,
        }));
    }
    Ok(None)
}

fn serialize_sequence<W: Write + ?Sized>(
    peek: Peek<'_, '_>,
    store: &W,
    depth: usize,
) -> Result<Vec<TreeEntry>, SerializeError> {
    let seq = peek.into_list_like().map_err(reflect)?;
    let mut entries: Vec<TreeEntry> = Vec::new();
    for (i, item) in seq.iter().enumerate() {
        let (oid, kind) = serialize_node(item, store, depth + 1)?;
        entries.push(TreeEntry {
            mode: EntryMode::from(kind),
            filename: format!("{i:04}").into(),
            oid,
        });
    }
    Ok(entries)
}

/// A float type [`float_text`] can canonicalize (`f32`, `f64`).
pub(crate) trait FloatScalar: Copy + PartialEq + ToString {
    /// Positive zero, used to collapse negative zero.
    const ZERO: Self;
    /// Whether the value is a NaN of any payload.
    fn is_nan(self) -> bool;
}

impl FloatScalar for f32 {
    const ZERO: Self = 0.0;
    fn is_nan(self) -> bool {
        f32::is_nan(self)
    }
}

impl FloatScalar for f64 {
    const ZERO: Self = 0.0;
    fn is_nan(self) -> bool {
        f64::is_nan(self)
    }
}

/// Canonical blob text of a float.
///
/// Normalizes NaN and negative zero so equivalent values share one blob and
/// object id. Used by typed scalars and dynamic numbers.
pub(crate) fn float_text<F: FloatScalar>(v: F) -> Vec<u8> {
    if v.is_nan() {
        return b"nan".to_vec();
    }
    let v = if v == F::ZERO { F::ZERO } else { v };
    v.to_string().into_bytes()
}

/// Write `content` as a leaf blob, with exactly one trailing `\n` appended.
///
/// Appends exactly one newline to every value leaf, unconditionally. This
/// preserves the distinction between content ending in `\n` and content that
/// does not; the structural presence marker is intentionally excluded and
/// remains the empty blob.
pub(crate) fn write_leaf_blob<W: Write + ?Sized>(
    store: &W,
    content: &[u8],
) -> Result<ObjectId, SerializeError> {
    let mut bytes = Vec::with_capacity(content.len() + 1);
    bytes.extend_from_slice(content);
    bytes.push(b'\n');
    store
        .write_buf(Kind::Blob, &bytes)
        .map_err(SerializeError::Backend)
}

fn scalar_bytes(peek: Peek<'_, '_>) -> Result<Vec<u8>, SerializeError> {
    let shape = peek.shape();
    let scalar_shape = collapse_shape(shape);
    if scalar_shape
        .scalar_type()
        .is_none_or(|scalar| scalar_node(scalar).is_none())
    {
        return Err(SerializeError::UnsupportedScalar(shape.type_identifier));
    }

    if let Some(s) = peek.as_str() {
        return Ok(s.as_bytes().to_vec());
    }

    if let facet::Type::Primitive(pt) = shape.ty {
        use facet::{NumericType, PrimitiveType, TextualType};
        match pt {
            PrimitiveType::Boolean => {
                let v = *peek.get::<bool>().map_err(reflect)?;
                return Ok(v.to_string().into_bytes());
            }
            PrimitiveType::Textual(TextualType::Char) => {
                let v = *peek.get::<char>().map_err(reflect)?;
                let mut buf = [0u8; 4];
                return Ok(v.encode_utf8(&mut buf).as_bytes().to_vec());
            }
            PrimitiveType::Textual(TextualType::Str) => {
                if let Some(s) = peek.as_str() {
                    return Ok(s.as_bytes().to_vec());
                }
            }
            PrimitiveType::Numeric(NumericType::Float) => {
                let layout_size = shape.layout.sized_layout().map(|l| l.size()).unwrap_or(8);
                if layout_size == 4 {
                    let v = *peek.get::<f32>().map_err(reflect)?;
                    return Ok(float_text(v));
                } else {
                    let v = *peek.get::<f64>().map_err(reflect)?;
                    return Ok(float_text(v));
                }
            }
            PrimitiveType::Numeric(NumericType::Integer { .. }) => {
                // Display also handles `isize`/`usize`, which are distinct from
                // same-sized fixed-width types to `Peek::get`.
                return Ok(peek.to_string().into_bytes());
            }
            _ => {}
        }
    }

    Err(SerializeError::UnsupportedScalar(shape.type_identifier))
}
