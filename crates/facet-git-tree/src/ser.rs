//! Serialization: encoding [`facet::Facet`] values as Git objects.

use facet::{Def, DynDateTimeKind, DynValueKind, Peek};
use gix_object::{Kind, Write};

use crate::check_key;
use crate::de::collapse_shape;
use crate::error::SerializeError;
use crate::store::ObjectStore;
use crate::{EntryKind, EntryMode, ObjectId, RawTree, TreeEntry};

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
/// Writes all blobs and sub-trees reachable from `value` and returns the root
/// tree [`ObjectId`]. This is the generic core; [`serialize`] is a convenience
/// wrapper that allocates a fresh [`ObjectStore`].
///
/// `store` is the backend contract: any `gix` [`Write`] sink works — a real
/// `gix` repository, an in-memory odb proxy, or the bundled [`ObjectStore`]. The
/// bound is `&self` (never `&mut`) because `gix`'s `Write` is; that is what lets
/// one backend be shared while objects stream into it. `?Sized` is permitted so
/// a `&dyn Write` may be passed for runtime backend selection.
pub fn serialize_into<T, W>(value: &T, store: &W) -> Result<ObjectId, SerializeError>
where
    T: for<'a> facet::Facet<'a>,
    W: Write + ?Sized,
{
    let peek = Peek::new(value);
    serialize_peek_into(peek, store)
}

/// Serialize a [`facet::Facet`] value into a set of Git objects.
///
/// Returns the root [`ObjectId`] (a tree) and an [`ObjectStore`] containing
/// all blobs and sub-trees reachable from that root.
pub fn serialize<T: for<'a> facet::Facet<'a>>(
    value: &T,
) -> Result<(ObjectId, ObjectStore), SerializeError> {
    let store = ObjectStore::default();
    let root = serialize_into(value, &store)?;
    Ok((root, store))
}

/// Serialize an already-constructed [`Peek`] into the given `gix` object `store`.
///
/// The [`Peek`] entry point for callers that have a reflected handle rather than
/// a concrete `T` — for example, when relaying a value obtained from another
/// `facet` operation. This is the [`Peek`]-based mirror of [`serialize_into`];
/// `serialize_into` is just `serialize_peek_into` applied to `Peek::new(value)`.
pub fn serialize_peek_into<W>(peek: Peek<'_, '_>, store: &W) -> Result<ObjectId, SerializeError>
where
    W: Write + ?Sized,
{
    let (oid, _kind) = serialize_node(peek, store)?;
    Ok(oid)
}

/// Serialize an already-constructed [`Peek`] into a fresh set of Git objects.
///
/// The [`Peek`]-based mirror of [`serialize`]: returns the root [`ObjectId`] and
/// an [`ObjectStore`] containing every object reachable from it.
pub fn serialize_peek(peek: Peek<'_, '_>) -> Result<(ObjectId, ObjectStore), SerializeError> {
    let store = ObjectStore::default();
    let root = serialize_peek_into(peek, &store)?;
    Ok((root, store))
}

/// The element shape of a `Vec`/array/slice, or `None` for any other type.
pub(crate) fn seq_elem(shape: &facet::Shape) -> Option<&'static facet::Shape> {
    match shape.def {
        Def::List(d) => Some(d.t),
        Def::Array(d) => Some(d.t),
        Def::Slice(d) => Some(d.t),
        _ => None,
    }
}

/// Whether `shape` is a sequence of `u8` (`Vec<u8>`, `[u8; N]`, `[u8]`). Such a
/// sequence is stored as one blob rather than a per-element tree.
pub(crate) fn is_byte_seq(shape: &facet::Shape) -> bool {
    seq_elem(shape).is_some_and(|t| t.is_type::<u8>())
}

fn serialize_node<W: Write + ?Sized>(
    peek: Peek<'_, '_>,
    store: &W,
) -> Result<(ObjectId, EntryKind), SerializeError> {
    // Smart pointer (`Box`/`Arc`/`Rc`, including `Arc<[T]>`) and transparent
    // newtype (`#[facet(transparent)]`, `NonZero<T>`, path wrappers) → the
    // inner value's own encoding. Neither carries information Git needs to
    // record, so both are transparent: `Arc<[u8]>` serializes exactly as
    // `[u8]` would, and a transparent `Hex(String)` exactly as `String` would.
    let peek = peek.innermost_peek();
    let shape = peek.shape();

    // RawTree → its wrapped object id, straight through as a tree entry. No
    // write happens here: the referenced tree must already be present in
    // `store`, from a write the caller made directly beforehand.
    if shape.is_type::<RawTree>() {
        let rt = peek.get::<RawTree>().map_err(reflect)?;
        return Ok((rt.oid(), EntryKind::Tree));
    }

    // Dynamic value (`facet_value::Value` and friends) → the encoding of its
    // runtime kind, dispatched in serialize_dynamic.
    if let Def::DynamicValue(_) = shape.def {
        return serialize_dynamic(peek, store);
    }

    // Scalar leaf → blob
    if matches!(shape.def, Def::Scalar) {
        let bytes = scalar_bytes(peek)?;
        let oid = store
            .write_buf(Kind::Blob, &bytes)
            .map_err(SerializeError::Backend)?;
        return Ok((oid, EntryKind::Blob));
    }

    // Byte sequence (`Vec<u8>`, `[u8; N]`, `[u8]`) → a single blob. This is the
    // Git-native representation; a per-byte tree would be wasteful and would
    // defeat blob-level deduplication of identical buffers.
    if is_byte_seq(shape) {
        let seq = peek.into_list_like().map_err(reflect)?;
        let mut bytes = Vec::new();
        for item in seq.iter() {
            bytes.push(*item.get::<u8>().map_err(reflect)?);
        }
        let oid = store
            .write_buf(Kind::Blob, &bytes)
            .map_err(SerializeError::Backend)?;
        return Ok((oid, EntryKind::Blob));
    }

    // Struct or tuple → tree. A named struct keys entries by field name; a tuple
    // or tuple struct keys them by zero-padded positional ordinal (facet models
    // all of these as `UserType::Struct`, distinguished by `StructKind`).
    if let facet::Type::User(facet::UserType::Struct(st)) = shape.ty {
        let positional = matches!(
            st.kind,
            facet::StructKind::Tuple | facet::StructKind::TupleStruct
        );
        let ps = peek.into_struct().map_err(reflect)?;
        let mut entries: Vec<TreeEntry> = Vec::with_capacity(st.fields.len());
        for (i, field) in st.fields.iter().enumerate() {
            let child = ps.field(i).map_err(reflect)?;
            let (oid, kind) = serialize_node(child, store)?;
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
        entries.sort();
        let oid = store
            .write(&gix_object::Tree { entries })
            .map_err(SerializeError::Backend)?;
        return Ok((oid, EntryKind::Tree));
    }

    // Vec / Array / slice → tree with ordinal keys
    if matches!(shape.def, Def::List(_) | Def::Array(_) | Def::Slice(_)) {
        let entries = serialize_sequence(peek, store)?;
        let oid = store
            .write(&gix_object::Tree { entries })
            .map_err(SerializeError::Backend)?;
        return Ok((oid, EntryKind::Tree));
    }

    // Map → tree. A map with scalar keys names each entry by the textual form of
    // its key (the readable, JSON-like form). A map with composite keys (structs,
    // tuples, enums, ...) — which have no faithful textual form — instead stores
    // each pair as an ordinal-named two-entry sub-tree `{ k, v }`, both children
    // recursing through the normal encoding. The two layouts are distinguished by
    // the key shape *after* transparency collapse (`collapse_shape`), so no
    // on-disk marker is needed and a smart-pointer or transparent-newtype key
    // (`Arc<str>`, a `#[facet(transparent)]` wrapper, ...) is named exactly as
    // its collapsed scalar shape would be.
    if let Def::Map(md) = shape.def {
        let pm = peek.into_map().map_err(reflect)?;
        let scalar_keys = matches!(collapse_shape(md.k).def, Def::Scalar);
        let mut entries: Vec<TreeEntry> = Vec::new();
        if scalar_keys {
            for (k, v) in pm.iter() {
                let key_bytes = scalar_bytes(k)?;
                let key_str =
                    std::str::from_utf8(&key_bytes).map_err(|_| SerializeError::NonUtf8MapKey)?;
                check_key(key_str)?;
                let (oid, kind) = serialize_node(v, store)?;
                entries.push(TreeEntry {
                    mode: EntryMode::from(kind),
                    filename: key_str.into(),
                    oid,
                });
            }
        } else {
            // Each pair becomes a `{ k, v }` sub-tree; the outer entries are named
            // by ordinal. To keep the map content-addressed (insertion-order
            // independent), the ordinals are assigned after sorting the pairs by
            // their sub-tree object id.
            let mut pair_oids: Vec<ObjectId> = Vec::new();
            for (k, v) in pm.iter() {
                let (k_oid, k_kind) = serialize_node(k, store)?;
                let (v_oid, v_kind) = serialize_node(v, store)?;
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
                let pair_oid = store
                    .write(&gix_object::Tree { entries: pair })
                    .map_err(SerializeError::Backend)?;
                pair_oids.push(pair_oid);
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
        entries.sort();
        let oid = store
            .write(&gix_object::Tree { entries })
            .map_err(SerializeError::Backend)?;
        return Ok((oid, EntryKind::Tree));
    }

    // Option
    if matches!(shape.def, Def::Option(_)) {
        let po = peek.into_option().map_err(reflect)?;
        if let Some(inner) = po.value() {
            let (oid, kind) = serialize_node(inner, store)?;
            // Some: wrap in a tree with a single "some" entry
            let entries = vec![TreeEntry {
                mode: EntryMode::from(kind),
                filename: "some".into(),
                oid,
            }];
            let oid = store
                .write(&gix_object::Tree { entries })
                .map_err(SerializeError::Backend)?;
            return Ok((oid, EntryKind::Tree));
        } else {
            // None: empty tree
            let oid = store
                .write(&gix_object::Tree { entries: vec![] })
                .map_err(SerializeError::Backend)?;
            return Ok((oid, EntryKind::Tree));
        }
    }

    // Enum → single-entry tree: variant name → variant contents
    if let facet::Type::User(facet::UserType::Enum(_)) = shape.ty {
        let pe = peek.into_enum().map_err(reflect)?;
        let variant = pe.active_variant().map_err(reflect)?;
        let variant_name = pe.variant_name_active().map_err(reflect)?;

        // Encode the variant's payload (unit → empty tree, newtype → the field's
        // own encoding directly, tuple → ordinal-keyed tree, struct → name-keyed
        // tree). A tuple variant is `StructKind::TupleStruct`; a struct variant is
        // `StructKind::Struct`.
        let positional = matches!(variant.data.kind, facet::StructKind::TupleStruct);
        let newtype = positional && variant.data.fields.len() == 1;
        let (inner_oid, inner_kind) = if variant.data.fields.is_empty() {
            let oid = store
                .write(&gix_object::Tree { entries: vec![] })
                .map_err(SerializeError::Backend)?;
            (oid, EntryKind::Tree)
        } else if newtype {
            // Newtype variant: resolves directly to the encoding of its one field.
            let child = pe
                .field(0)
                .map_err(reflect)?
                .ok_or_else(|| SerializeError::Reflect("variant field 0 missing".into()))?;
            serialize_node(child, store)?
        } else {
            let mut inner_entries: Vec<TreeEntry> = Vec::new();
            for (i, field) in variant.data.fields.iter().enumerate() {
                let child = pe
                    .field(i)
                    .map_err(reflect)?
                    .ok_or_else(|| SerializeError::Reflect(format!("variant field {i} missing")))?;
                let (oid, kind) = serialize_node(child, store)?;
                let name: gix_object::bstr::BString = if positional {
                    format!("{i:04}").into()
                } else {
                    field.name.into()
                };
                inner_entries.push(TreeEntry {
                    mode: EntryMode::from(kind),
                    filename: name,
                    oid,
                });
            }
            inner_entries.sort();
            let oid = store
                .write(&gix_object::Tree {
                    entries: inner_entries,
                })
                .map_err(SerializeError::Backend)?;
            (oid, EntryKind::Tree)
        };

        let entries = vec![TreeEntry {
            mode: EntryMode::from(inner_kind),
            filename: variant_name.into(),
            oid: inner_oid,
        }];
        let oid = store
            .write(&gix_object::Tree { entries })
            .map_err(SerializeError::Backend)?;
        return Ok((oid, EntryKind::Tree));
    }

    Err(SerializeError::Unsupported(shape.type_identifier))
}

/// Serialize a dynamic value (`Def::DynamicValue`, e.g. `facet_value::Value`)
/// by dispatching on its runtime kind.
///
/// Each kind maps onto the encoding its typed counterpart uses, so a dynamic
/// value and the equivalent typed value produce identical objects *whenever
/// the dynamic value can be rendered at all*: strings are their UTF-8 bytes,
/// bytes a raw blob, booleans and numbers their textual form, arrays
/// ordinal-keyed trees, and objects name-keyed trees (each key validated by
/// [`check_key`]). Null is the empty tree — an empty blob would collide with
/// `""` and empty bytes, which are far more common than null.
///
/// The generic vtable cannot render every kind exactly: integers beyond 64
/// bits, QNames, and UUIDs need the `value`-feature downcast to
/// `facet_value::Value`, which also resolves numbers the generic 64-bit reads
/// leave ambiguous (see the `Number` case below). Without that downcast —
/// or when it is available but still cannot tell an out-of-range whole value's
/// exact representation apart from a truncated one — rendering is refused
/// rather than writing a lossy form that would change the value's object id,
/// with [`SerializeError::UnrepresentableNumber`] or
/// [`SerializeError::UnsupportedDynamicKind`].
fn serialize_dynamic<W: Write + ?Sized>(
    peek: Peek<'_, '_>,
    store: &W,
) -> Result<(ObjectId, EntryKind), SerializeError> {
    let dv = peek.into_dynamic_value().map_err(reflect)?;

    let blob = |bytes: &[u8]| -> Result<(ObjectId, EntryKind), SerializeError> {
        let oid = store
            .write_buf(Kind::Blob, bytes)
            .map_err(SerializeError::Backend)?;
        Ok((oid, EntryKind::Blob))
    };
    let tree = |entries: Vec<TreeEntry>| -> Result<(ObjectId, EntryKind), SerializeError> {
        let oid = store
            .write(&gix_object::Tree { entries })
            .map_err(SerializeError::Backend)?;
        Ok((oid, EntryKind::Tree))
    };

    match dv.kind() {
        DynValueKind::Null => tree(vec![]),
        DynValueKind::Bool => {
            let b = dv
                .as_bool()
                .ok_or_else(|| reflect("dynamic bool unreadable"))?;
            blob(if b { "true" } else { "false" }.as_bytes())
        }
        // Also covers char values: `facet_value` surfaces a char as a String
        // (its UTF-8 form), which matches the typed `char` encoding.
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
            // Exact fast path (`value` feature): the generic vtable only
            // surfaces 64-bit reads, so anything outside that range is
            // resolved through `facet_value`'s own accessors instead.
            // Numbers that fit the generic reads below never take this path,
            // keeping the emitted bytes identical with and without the
            // feature.
            #[cfg(feature = "value")]
            {
                if peek.shape().is_type::<facet_value::Value>()
                    && dv.as_i64().is_none()
                    && dv.as_u64().is_none()
                {
                    let v = peek.get::<facet_value::Value>().map_err(reflect)?;
                    if let Some(n) = v.as_number() {
                        // Dispatch on the number's actual backing
                        // (`VNumber::is_float`) rather than on whether
                        // `to_i128`/`to_u128` succeed: those *do* round-trip
                        // a whole float within range (e.g. `6.022e23`), but
                        // they read back its exact binary value, which is
                        // not the same decimal as the *shortest*
                        // round-tripping text `float_text`/the typed `f64`
                        // path renders for those same bits (`f64::to_string`
                        // is shortest-round-trip, not exact-value). Using
                        // the integer accessors for a float-backed number
                        // would therefore silently diverge from the typed
                        // encoding — and from a different `f64` whose exact
                        // bits happen to share that same shortest decimal.
                        if n.is_float() {
                            return blob(&float_text(n.to_f64_lossy()));
                        }
                        // Not float-backed: `VNumber` canonicalizes every
                        // integer repr (`I64`/`U64`/`I128`/`U128`) so that
                        // whichever of `to_i128`/`to_u128` is
                        // signed-appropriate for its range always succeeds.
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
                // A finite whole f64 only reaches here when the value did not
                // fit the 64-bit reads above, the `value`-feature fast path
                // did not already resolve it (either the feature is off, or
                // this dynamic value is not a `facet_value::Value`), and it
                // may therefore be the lossy image of a >64-bit integer.
                // Refuse rather than write an approximation that would
                // silently change the object id.
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
                let (oid, kind) = serialize_node(item, store)?;
                entries.push(TreeEntry {
                    mode: EntryMode::from(kind),
                    filename: format!("{i:04}").into(),
                    oid,
                });
            }
            entries.sort();
            tree(entries)
        }
        DynValueKind::Object => {
            let iter = dv
                .object_iter()
                .ok_or_else(|| reflect("dynamic object unreadable"))?;
            let mut entries: Vec<TreeEntry> = Vec::with_capacity(iter.len());
            for (key, value) in iter {
                check_key(key)?;
                let (oid, kind) = serialize_node(value, store)?;
                entries.push(TreeEntry {
                    mode: EntryMode::from(kind),
                    filename: key.into(),
                    oid,
                });
            }
            entries.sort();
            tree(entries)
        }
        DynValueKind::DateTime => {
            let parts = dv
                .as_datetime()
                .ok_or_else(|| reflect("dynamic datetime unreadable"))?;
            blob(datetime_text(parts)?.as_bytes())
        }
        // Neither QName nor UUID has a generic textual read on the vtable;
        // only the `value` feature's downcast to `facet_value::Value` can
        // render them.
        kind @ (DynValueKind::QName | DynValueKind::Uuid) => {
            #[cfg(feature = "value")]
            {
                if let Some(text) = value_special_text(peek)? {
                    return blob(text.as_bytes());
                }
            }
            Err(SerializeError::UnsupportedDynamicKind(format!("{kind:?}")))
        }
        // `DynValueKind` is non_exhaustive: refuse kinds this crate does not
        // know rather than guess an encoding for them.
        other => Err(SerializeError::UnsupportedDynamicKind(format!("{other:?}"))),
    }
}

/// Render a dynamic datetime as RFC 3339-style text.
///
/// `parts` is the `(year, month, day, hour, minute, second, nanos, kind)`
/// tuple surfaced by the dynamic-value vtable. Date and time fields are
/// zero-padded; fractional seconds appear only when `nanos` is non-zero, with
/// trailing zeros trimmed; a zero UTC offset renders as `Z`, any other as
/// `±HH:MM`. The local kinds drop the parts they do not carry: no offset
/// suffix for a local date-time, date only for a local date, time only for a
/// local time.
///
/// A negative (BCE, proleptic Gregorian) year renders as a `-` followed by
/// its magnitude, zero-padded to four digits (`-5` → `-0005`) — `{year:04}`
/// alone would zero-pad the *whole* signed value including the sign
/// character, producing a three-digit magnitude (`-005`). A year at or
/// beyond `10000` simply prints all its digits, as the zero-padding is a
/// minimum width, not a truncation.
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
        // `DynDateTimeKind` is non_exhaustive: refuse unknown kinds rather
        // than emit text whose meaning this crate cannot vouch for.
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
/// A UUID renders as canonical hyphenated lowercase hex (8-4-4-4-12). A QName
/// renders in Clark notation — `{namespace}local` — or as the bare local name
/// when it has no namespace. An empty-string namespace is treated the same as
/// no namespace: Clark notation reserves the empty-braces form for "no
/// namespace", so `VQName::new("", local)` and `VQName::new_local(local)`
/// must render — and therefore hash to — the same blob.
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
            // Writing to a String is infallible.
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
) -> Result<Vec<TreeEntry>, SerializeError> {
    let seq = peek.into_list_like().map_err(reflect)?;
    let mut entries: Vec<TreeEntry> = Vec::new();
    for (i, item) in seq.iter().enumerate() {
        let (oid, kind) = serialize_node(item, store)?;
        entries.push(TreeEntry {
            mode: EntryMode::from(kind),
            filename: format!("{i:04}").into(),
            oid,
        });
    }
    entries.sort();
    Ok(entries)
}

/// A float type [`float_text`] can canonicalize (`f32`, `f64`).
trait FloatScalar: Copy + PartialEq + ToString {
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
/// Every NaN payload collapses to `nan` and negative zero to positive zero, so
/// numerically equal values always produce byte-identical blobs — and thus
/// equal object ids. Shared by the typed scalar path ([`scalar_bytes`]) and
/// the dynamic number path ([`serialize_dynamic`]).
fn float_text<F: FloatScalar>(v: F) -> Vec<u8> {
    if v.is_nan() {
        return b"nan".to_vec();
    }
    let v = if v == F::ZERO { F::ZERO } else { v };
    v.to_string().into_bytes()
}

fn scalar_bytes(peek: Peek<'_, '_>) -> Result<Vec<u8>, SerializeError> {
    // Strings: verbatim UTF-8 bytes
    if let Some(s) = peek.as_str() {
        return Ok(s.as_bytes().to_vec());
    }

    // Use Display for everything else, with special float/bool/char handling
    let shape = peek.shape();
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
                // handled above by as_str(); shouldn't reach here
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
                // Every integer width is its `Display` form, which `Peek` forwards
                // to the underlying value. Dispatching on layout size and calling
                // `get::<iN>()` would reject `isize`/`usize`, which share a size
                // with `i64`/`u64` but are a distinct type to `Peek::get`.
                return Ok(peek.to_string().into_bytes());
            }
            _ => {}
        }
    }

    Err(SerializeError::UnsupportedScalar(shape.type_identifier))
}
