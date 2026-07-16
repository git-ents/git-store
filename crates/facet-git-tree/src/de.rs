//! Deserialization: decoding Git trees back into [`facet::Facet`] values.

use facet::{Def, Partial};
use gix_hash::Kind as HashKind;
use gix_object::{Data, Find, Kind};

use crate::error::{DeserializeError, KeyError};
use crate::ser::is_byte_seq;
use crate::{EntryKind, ObjectId, RawTree};

/// Collapse a `facet` reflection error to [`DeserializeError::Reflect`].
///
/// `facet`'s `Partial` operations return their own non-`'static` error types;
/// this collapses them to the dedicated text-carrying variant at the call site
/// without a bespoke closure every time.
fn reflect(e: impl std::fmt::Display) -> DeserializeError {
    DeserializeError::Reflect(e.to_string())
}

/// Maximum tree nesting depth accepted on deserialization.
///
/// Bounds recursion in [`deser_into`] so a hostile or corrupt tree cannot
/// overflow the stack. The limit must stay well under what a default thread
/// stack can hold: [`deser_into`] is a large recursive frame (a debug build is
/// tens of KB per level), so a 2 MiB stack — the standard library's default for
/// spawned threads — only holds a few dozen levels before overflowing. The
/// guard exists precisely to forestall that overflow, so it is kept low enough
/// to fire first with margin to spare. Still far deeper than any
/// practically-encoded value nests.
const MAX_DEPTH: usize = 32;

/// Validate a user-supplied key for use as a Git tree entry name.
///
/// Keys become tree entry names, which double as path segments, so a key may not
/// contain the path separator `/` ([`KeyError`]). Serialization is required to
/// apply this to every dynamic key (such as map keys) before emitting its entry,
/// so a `/`-bearing name can never be written as data.
pub fn check_key(key: &str) -> Result<(), KeyError> {
    if key.contains('/') {
        return Err(KeyError {
            key: key.to_owned(),
        });
    }
    Ok(())
}

/// Deserialize a [`facet::Facet`] value from a root tree stored in `store`.
///
/// `store` is any `gix` [`Find`] source — a real repository, an in-memory odb,
/// or an [`ObjectStore`](crate::ObjectStore) — the read side of the backend
/// contract documented on [`serialize_into`](crate::serialize_into). `?Sized`
/// is permitted so a `&dyn Find` may be passed.
pub fn deserialize<T: for<'a> facet::Facet<'a>>(
    root: &ObjectId,
    store: &(impl Find + ?Sized),
) -> Result<T, DeserializeError> {
    let partial = Partial::alloc::<T>()
        .map_err(|e| DeserializeError::Reflect(format!("alloc failed: {e}")))?;
    let partial = deser_into(partial, root, store, 0)?;
    let heap = partial
        .build()
        .map_err(|e| DeserializeError::Reflect(format!("build failed: {e}")))?;
    heap.materialize::<T>()
        .map_err(|e| DeserializeError::Reflect(format!("materialize failed: {e}")))
}

/// Deserialize the tree at `root` into an existing [`Partial`].
///
/// The into-existing-[`Partial`] entry point, mirroring `facet`'s `*_into`
/// convention: the caller owns allocation and `build`, so a value read from a
/// Git tree can be slotted into a larger reflected construction. [`deserialize`]
/// is this applied to a freshly-allocated `Partial`, then built and materialized.
///
/// `store` is any `gix` [`Find`] source, exactly as for [`deserialize`].
pub fn deserialize_into<'facet>(
    partial: Partial<'facet, true>,
    root: &ObjectId,
    store: &(impl Find + ?Sized),
) -> Result<Partial<'facet, true>, DeserializeError> {
    deser_into(partial, root, store, 0)
}

fn find_object<'a, F: Find + ?Sized>(
    id: &ObjectId,
    buf: &'a mut Vec<u8>,
    store: &F,
) -> Result<Data<'a>, DeserializeError> {
    store
        .try_find(id, buf)
        .map_err(DeserializeError::Backend)?
        .ok_or_else(|| DeserializeError::NotFound(*id))
}

fn find_tree_entries<F: Find + ?Sized>(
    id: &ObjectId,
    store: &F,
) -> Result<Vec<(String, ObjectId, EntryKind)>, DeserializeError> {
    let mut buf = Vec::new();
    let data = find_object(id, &mut buf, store)?;
    if data.kind != Kind::Tree {
        return Err(DeserializeError::NotATree(*id));
    }
    let tree_ref = gix_object::TreeRef::from_bytes(data.data, HashKind::Sha1)
        .map_err(|source| DeserializeError::Decode { oid: *id, source })?;
    let mut result = Vec::new();
    for entry in &tree_ref.entries {
        let name = std::str::from_utf8(entry.filename).map_err(|_| {
            DeserializeError::NonUtf8Name(String::from_utf8_lossy(entry.filename).into_owned())
        })?;
        result.push((name.to_owned(), entry.oid.to_owned(), entry.mode.kind()));
    }
    Ok(result)
}

fn find_blob_bytes<F: Find + ?Sized>(
    id: &ObjectId,
    store: &F,
) -> Result<Vec<u8>, DeserializeError> {
    let mut buf = Vec::new();
    let data = find_object(id, &mut buf, store)?;
    if data.kind != Kind::Blob {
        return Err(DeserializeError::NotABlob(*id));
    }
    Ok(data.data.to_owned())
}

/// Sort sequence entries into ascending ordinal order, rejecting any entry whose
/// name is not a decimal index.
///
/// Sequence elements are named by zero-based decimal index, so the order must be
/// recovered numerically rather than lexically (`10000` sorts before `9999`). A
/// non-numeric name can only come from a foreign tree and is reported as
/// [`DeserializeError::InvalidOrdinal`].
fn sort_by_ordinal(entries: &mut [(String, ObjectId, EntryKind)]) -> Result<(), DeserializeError> {
    // Validate up front so the infallible sort key below cannot misorder entries.
    for (name, _, _) in entries.iter() {
        name.parse::<usize>()
            .map_err(|_| DeserializeError::InvalidOrdinal(name.clone()))?;
    }
    entries
        .sort_by_cached_key(|(name, _, _)| name.parse::<usize>().expect("ordinal validated above"));
    Ok(())
}

fn deser_into<'facet, F: Find + ?Sized>(
    partial: Partial<'facet, true>,
    oid: &ObjectId,
    store: &F,
    depth: usize,
) -> Result<Partial<'facet, true>, DeserializeError> {
    if depth > MAX_DEPTH {
        return Err(DeserializeError::MaxDepth(MAX_DEPTH));
    }
    let shape = partial.shape();

    // RawTree: capture the child entry's object id without decoding its
    // contents — the caller walks it separately, by whatever means it was
    // originally written. Still verified to be a tree, not a blob, so a
    // malformed or foreign tree fails fast with `NotATree` rather than
    // silently handing back a bogus tree id.
    if shape.is_type::<RawTree>() {
        let mut buf = Vec::new();
        let data = find_object(oid, &mut buf, store)?;
        if data.kind != Kind::Tree {
            return Err(DeserializeError::NotATree(*oid));
        }
        return partial.set(RawTree::new(*oid)).map_err(reflect);
    }

    // Scalar leaf: read blob, parse from str
    if matches!(shape.def, Def::Scalar) {
        let bytes = find_blob_bytes(oid, store)?;
        let s = std::str::from_utf8(&bytes).map_err(|_| DeserializeError::NonUtf8Blob(*oid))?;
        return partial
            .parse_from_str(s)
            .map_err(|e| DeserializeError::Parse {
                shape: shape.type_identifier,
                text: s.to_owned(),
                reason: e.to_string(),
            });
    }

    // Byte sequence (`Vec<u8>`, `[u8; N]`): read the single blob and fill the
    // collection one byte at a time, mirroring the serializer's blob encoding.
    if is_byte_seq(shape) {
        let bytes = find_blob_bytes(oid, store)?;
        if matches!(shape.def, Def::Array(_)) {
            let mut partial = partial.init_array().map_err(reflect)?;
            for (i, b) in bytes.iter().enumerate() {
                partial = partial.begin_nth_field(i).map_err(reflect)?;
                partial = partial.set::<u8>(*b).map_err(reflect)?;
                partial = partial.end().map_err(reflect)?;
            }
            return Ok(partial);
        }
        let mut partial = partial.init_list().map_err(reflect)?;
        for b in bytes {
            partial = partial.begin_list_item().map_err(reflect)?;
            partial = partial.set::<u8>(b).map_err(reflect)?;
            partial = partial.end().map_err(reflect)?;
        }
        return Ok(partial);
    }

    // Smart pointer (`Box`/`Arc`/`Rc`). For a slice pointee (`Arc<[T]>`) facet
    // hands back a slice builder we feed item by item; its element type decides
    // blob-vs-tree exactly as for an owned sequence. For a sized pointee the
    // pointee shares this node's encoding, so we recurse on the same object.
    if let Def::Pointer(pd) = shape.def {
        let mut partial = partial.begin_smart_ptr().map_err(reflect)?;
        if partial.is_building_smart_ptr_slice() {
            if pd.pointee.is_some_and(is_byte_seq) {
                let bytes = find_blob_bytes(oid, store)?;
                for b in bytes {
                    partial = partial.begin_list_item().map_err(reflect)?;
                    partial = partial.set::<u8>(b).map_err(reflect)?;
                    partial = partial.end().map_err(reflect)?;
                }
            } else {
                let mut entries = find_tree_entries(oid, store)?;
                sort_by_ordinal(&mut entries)?;
                for (_, child_oid, _) in entries {
                    partial = partial.begin_list_item().map_err(reflect)?;
                    partial = deser_into(partial, &child_oid, store, depth + 1)?;
                    partial = partial.end().map_err(reflect)?;
                }
            }
            return partial.end().map_err(reflect);
        }
        partial = deser_into(partial, oid, store, depth + 1)?;
        return partial.end().map_err(reflect);
    }

    // Transparent newtype (`#[facet(transparent)]`, `NonZero<T>`, path
    // wrappers): the object was written as the inner value's own encoding (via
    // `Peek::innermost_peek`, which unwraps exactly when `try_borrow_inner` is
    // present), so build that and let `begin_inner` reassemble the wrapper.
    // Gated on `has_try_borrow_inner`, not just `shape.inner.is_some()`: plain
    // collections like `Vec<T>` also carry an `inner` shape (for variance) but
    // were never unwrapped on serialization, so must not be routed here.
    if shape.inner.is_some() && shape.vtable.has_try_borrow_inner() {
        let partial = partial.begin_inner().map_err(reflect)?;
        let partial = deser_into(partial, oid, store, depth + 1)?;
        return partial.end().map_err(reflect);
    }

    // Struct: read tree, fill fields by name. Tuples and tuple structs key their
    // entries by zero-padded positional ordinal (mirroring serialization).
    if let facet::Type::User(facet::UserType::Struct(st)) = shape.ty {
        let positional = matches!(
            st.kind,
            facet::StructKind::Tuple | facet::StructKind::TupleStruct
        );
        let entries = find_tree_entries(oid, store)?;
        let mut partial = partial;
        for (i, field) in st.fields.iter().enumerate() {
            // Find this field's entry in the tree
            let entry_name = if positional {
                format!("{i:04}")
            } else {
                field.name.to_string()
            };
            let entry = entries.iter().find(|(name, _, _)| *name == entry_name);
            if let Some((_, child_oid, _)) = entry {
                let child_oid = *child_oid;
                partial = partial.begin_field(field.name).map_err(|e| {
                    DeserializeError::Reflect(format!("begin_field {}: {e}", field.name))
                })?;
                partial = deser_into(partial, &child_oid, store, depth + 1)?;
                partial = partial.end().map_err(|e| {
                    DeserializeError::Reflect(format!("end field {}: {e}", field.name))
                })?;
            }
        }
        return Ok(partial);
    }

    // List (Vec): read tree with ordinal keys, sort numerically, push items
    if matches!(shape.def, Def::List(_)) {
        let mut entries = find_tree_entries(oid, store)?;
        sort_by_ordinal(&mut entries)?;
        let mut partial = partial.init_list().map_err(reflect)?;
        for (_, child_oid, _) in entries {
            partial = partial.begin_list_item().map_err(reflect)?;
            partial = deser_into(partial, &child_oid, store, depth + 1)?;
            partial = partial.end().map_err(reflect)?;
        }
        return Ok(partial);
    }

    // Array: same as List but init_array
    if matches!(shape.def, Def::Array(_)) {
        let mut entries = find_tree_entries(oid, store)?;
        sort_by_ordinal(&mut entries)?;
        let mut partial = partial.init_array().map_err(reflect)?;
        for (name, child_oid, _) in entries {
            let idx = name
                .parse::<usize>()
                .expect("ordinal validated by sort_by_ordinal");
            partial = partial.begin_nth_field(idx).map_err(reflect)?;
            partial = deser_into(partial, &child_oid, store, depth + 1)?;
            partial = partial.end().map_err(reflect)?;
        }
        return Ok(partial);
    }

    // Map: mirror serialization. Scalar-keyed maps name each entry by the key's
    // textual form (parsed back via `parse_from_str`); composite-keyed maps store
    // each pair as a `{ k, v }` sub-tree, both children recovered by recursing.
    if let Def::Map(md) = shape.def {
        let entries = find_tree_entries(oid, store)?;
        let scalar_keys = matches!(md.k.def, Def::Scalar);
        let mut partial = partial.init_map().map_err(reflect)?;
        if scalar_keys {
            for (key, child_oid, _) in entries {
                partial = partial.begin_key().map_err(reflect)?;
                partial = partial
                    .parse_from_str(&key)
                    .map_err(|e| DeserializeError::Parse {
                        shape: md.k.type_identifier,
                        text: key.clone(),
                        reason: e.to_string(),
                    })?;
                partial = partial.end().map_err(reflect)?;
                partial = partial.begin_value().map_err(reflect)?;
                partial = deser_into(partial, &child_oid, store, depth + 1)?;
                partial = partial.end().map_err(reflect)?;
            }
        } else {
            for (_, pair_oid, _) in entries {
                let pair = find_tree_entries(&pair_oid, store)?;
                let find = |want: &'static str| {
                    pair.iter()
                        .find(|(n, _, _)| n == want)
                        .map(|(_, o, _)| *o)
                        .ok_or(DeserializeError::MissingMapPairEntry { entry: want })
                };
                let k_oid = find("k")?;
                let v_oid = find("v")?;
                partial = partial.begin_key().map_err(reflect)?;
                partial = deser_into(partial, &k_oid, store, depth + 1)?;
                partial = partial.end().map_err(reflect)?;
                partial = partial.begin_value().map_err(reflect)?;
                partial = deser_into(partial, &v_oid, store, depth + 1)?;
                partial = partial.end().map_err(reflect)?;
            }
        }
        return Ok(partial);
    }

    // Option: empty tree → None, single "some"-named entry → Some(inner).
    if matches!(shape.def, Def::Option(_)) {
        let entries = find_tree_entries(oid, store)?;
        if entries.is_empty() {
            // None — the partial already holds the default None.
            return Ok(partial);
        }
        // Some is written as exactly one entry named "some"; anything else is a
        // malformed (necessarily foreign) tree rather than a value to guess at.
        let [(name, inner_oid, _)] = entries.as_slice() else {
            return Err(DeserializeError::MalformedOption {
                found: entries.len(),
            });
        };
        if name != "some" {
            return Err(DeserializeError::MislabeledOption { name: name.clone() });
        }
        let inner_oid = *inner_oid;
        let partial = partial.begin_some().map_err(reflect)?;
        let partial = deser_into(partial, &inner_oid, store, depth + 1)?;
        return partial.end().map_err(reflect);
    }

    // Enum: single-entry tree → variant name → variant contents
    if let facet::Type::User(facet::UserType::Enum(et)) = shape.ty {
        let entries = find_tree_entries(oid, store)?;
        if entries.len() != 1 {
            return Err(DeserializeError::MalformedEnum {
                found: entries.len(),
            });
        }
        let (variant_name, inner_oid, _) = entries
            .into_iter()
            .next()
            .expect("length checked to be 1 above");

        // The variant's field layout comes from the type, not the tree: a tuple
        // variant (`TupleStruct`) keys by ordinal, a struct variant by name, and a
        // newtype (single-field tuple) variant resolves directly to its field.
        let variant = et.variants.iter().find(|v| v.name == variant_name);
        let positional =
            variant.is_some_and(|v| matches!(v.data.kind, facet::StructKind::TupleStruct));
        let newtype = positional && variant.is_some_and(|v| v.data.fields.len() == 1);

        let mut partial = partial.select_variant_named(&variant_name).map_err(|e| {
            DeserializeError::Reflect(format!("select variant {variant_name}: {e}"))
        })?;

        if newtype {
            partial = partial.begin_nth_field(0).map_err(reflect)?;
            partial = deser_into(partial, &inner_oid, store, depth + 1)?;
            return partial.end().map_err(reflect);
        }

        let inner_entries = find_tree_entries(&inner_oid, store)?;
        for (name, child_oid, _) in inner_entries {
            if positional {
                let idx = name
                    .parse::<usize>()
                    .map_err(|_| DeserializeError::InvalidOrdinal(name.clone()))?;
                partial = partial.begin_nth_field(idx).map_err(reflect)?;
            } else {
                partial = partial.begin_field(&name).map_err(reflect)?;
            }
            partial = deser_into(partial, &child_oid, store, depth + 1)?;
            partial = partial.end().map_err(reflect)?;
        }
        return Ok(partial);
    }

    Err(DeserializeError::Unsupported(shape.type_identifier))
}
