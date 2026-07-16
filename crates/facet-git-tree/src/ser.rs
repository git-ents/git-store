//! Serialization: encoding [`facet::Facet`] values as Git objects.

use facet::{Def, Peek};
use gix_object::{Kind, Write};

use crate::check_key;
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
    // the static key shape, so no on-disk marker is needed.
    if let Def::Map(md) = shape.def {
        let pm = peek.into_map().map_err(reflect)?;
        let scalar_keys = matches!(md.k.def, Def::Scalar);
        let mut entries: Vec<TreeEntry> = Vec::new();
        if scalar_keys {
            for (k, v) in pm.iter() {
                let key_bytes = scalar_bytes(k)?;
                let key_str = std::str::from_utf8(&key_bytes)
                    .map_err(|_| SerializeError::NonUtf8MapKey)?;
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
                let child = pe.field(i).map_err(reflect)?.ok_or_else(|| {
                    SerializeError::Reflect(format!("variant field {i} missing"))
                })?;
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
                    if v.is_nan() {
                        return Ok(b"nan".to_vec());
                    }
                    let v = if v == 0.0f32 { 0.0f32 } else { v };
                    return Ok(v.to_string().into_bytes());
                } else {
                    let v = *peek.get::<f64>().map_err(reflect)?;
                    if v.is_nan() {
                        return Ok(b"nan".to_vec());
                    }
                    let v = if v == 0.0f64 { 0.0f64 } else { v };
                    return Ok(v.to_string().into_bytes());
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
