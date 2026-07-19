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
pub(crate) const MAX_DEPTH: usize = 32;

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
    deserialize_at_depth(root, store, 0)
}

/// [`deserialize`], starting the recursion depth budget at `depth` instead of
/// `0`.
///
/// For a caller that is itself already some number of levels deep in a larger
/// deserialization — schema-driven reads route a [`Schema::Dynamic`]
/// (`crate::schema::Schema`) node back through this crate's own typed
/// [`deserialize`], and must hand off the depth already spent so the combined
/// recursion still respects [`MAX_DEPTH`] rather than resetting the budget.
/// [`deserialize`] is this with `depth` fixed at `0`.
pub(crate) fn deserialize_at_depth<T: for<'a> facet::Facet<'a>>(
    root: &ObjectId,
    store: &(impl Find + ?Sized),
    depth: usize,
) -> Result<T, DeserializeError> {
    let partial = Partial::alloc::<T>()
        .map_err(|e| DeserializeError::Reflect(format!("alloc failed: {e}")))?;
    let partial = deser_into(partial, root, store, depth)?;
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

pub(crate) fn find_object<'a, F: Find + ?Sized>(
    id: &ObjectId,
    buf: &'a mut Vec<u8>,
    store: &F,
) -> Result<Data<'a>, DeserializeError> {
    store
        .try_find(id, buf)
        .map_err(DeserializeError::Backend)?
        .ok_or_else(|| DeserializeError::NotFound(*id))
}

/// Parse an already-fetched object's [`Data`] as a tree's entries.
///
/// Factored out of [`find_tree_entries`] so a caller that must inspect an
/// object's kind before deciding how to read it — [`deser_dynamic`], which
/// branches on blob-vs-tree — can parse the same fetched buffer instead of
/// fetching `id` from `store` a second time.
pub(crate) fn tree_entries_from_data(
    data: &Data<'_>,
    id: &ObjectId,
) -> Result<Vec<(String, ObjectId, EntryKind)>, DeserializeError> {
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

pub(crate) fn find_tree_entries<F: Find + ?Sized>(
    id: &ObjectId,
    store: &F,
) -> Result<Vec<(String, ObjectId, EntryKind)>, DeserializeError> {
    let mut buf = Vec::new();
    let data = find_object(id, &mut buf, store)?;
    tree_entries_from_data(&data, id)
}

pub(crate) fn find_blob_bytes<F: Find + ?Sized>(
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
/// name is not a decimal index or that repeats another entry's index.
///
/// Sequence elements are named by zero-based decimal index, so the order must be
/// recovered numerically rather than lexically (`10000` sorts before `9999`). A
/// non-numeric name can only come from a foreign tree and is reported as
/// [`DeserializeError::InvalidOrdinal`]. Two entries naming the same index —
/// e.g. `"0"` and `"0000"`, distinct strings that parse to the same
/// number — leave the element order ambiguous and are reported as
/// [`DeserializeError::DuplicateOrdinal`]; this also hardens the dynamic-value
/// heuristic's all-ordinal classification, which shares this function.
pub(crate) fn sort_by_ordinal(
    entries: &mut [(String, ObjectId, EntryKind)],
) -> Result<(), DeserializeError> {
    // Validate up front so the infallible sort key below cannot misorder entries.
    for (name, _, _) in entries.iter() {
        name.parse::<usize>()
            .map_err(|_| DeserializeError::InvalidOrdinal(name.clone()))?;
    }
    entries
        .sort_by_cached_key(|(name, _, _)| name.parse::<usize>().expect("ordinal validated above"));
    // Duplicates are now adjacent, so a single pass over sorted windows finds
    // any pair of entries claiming the same index.
    for pair in entries.windows(2) {
        let ordinal = |name: &str| name.parse::<usize>().expect("ordinal validated above");
        let (a, b) = (ordinal(&pair[0].0), ordinal(&pair[1].0));
        if a == b {
            return Err(DeserializeError::DuplicateOrdinal(a));
        }
    }
    Ok(())
}

/// Collapse a shape's smart-pointer and transparent-newtype layers to the
/// shape actually written on disk, iterating to a fixed point.
///
/// Mirrors `Peek::innermost_peek` on the write side: a smart pointer
/// (`Def::Pointer`) is transparent to its pointee, and a transparent newtype
/// (`#[facet(transparent)]`, `NonZero<T>`, path wrappers) is transparent to
/// its inner shape — gated on `has_try_borrow_inner`, not just
/// `shape.inner.is_some()`, since plain collections like `Vec<T>` also carry
/// an `inner` shape (for variance) but were never unwrapped on write. Shared
/// by every caller that must classify a *shape* — not a concrete value — by
/// what it actually encodes to: the map-key scalar-vs-composite layout check
/// ([`ser`](crate::ser)'s and this module's `Def::Map` branches) and the
/// schema walker's own collapse (`crate::schema::collapse`).
///
/// A pointer shape with no pointee (an opaque pointer shape) cannot be
/// collapsed further; the loop simply stops there and returns it as-is, since
/// it is not a scalar or any other shape a caller here would special-case.
/// [`crate::schema::collapse`] treats that case as an error, since a schema
/// document has no way to describe an indescribable shape.
pub(crate) fn collapse_shape(mut shape: &'static facet::Shape) -> &'static facet::Shape {
    loop {
        if let Def::Pointer(pd) = shape.def {
            match pd.pointee {
                Some(pointee) => {
                    shape = pointee;
                    continue;
                }
                None => return shape,
            }
        }
        if shape.inner.is_some() && shape.vtable.has_try_borrow_inner() {
            shape = shape.inner.expect("checked is_some above");
            continue;
        }
        return shape;
    }
}

/// The `k`/`v` object ids of a composite-key map pair sub-tree.
///
/// Shared by this module's and [`crate::schema::read`]'s `Def::Map`/
/// `Schema::Map` branches, which decode the same `{ k, v }` layout that
/// [`crate::ser`] writes for composite (non-scalar) map keys.
pub(crate) fn map_pair_entries(
    pair: &[(String, ObjectId, EntryKind)],
) -> Result<(ObjectId, ObjectId), DeserializeError> {
    let find = |want: &'static str| {
        pair.iter()
            .find(|(n, _, _)| n == want)
            .map(|(_, o, _)| *o)
            .ok_or(DeserializeError::MissingMapPairEntry { entry: want })
    };
    Ok((find("k")?, find("v")?))
}

/// Validate an `Option` tree's entries, returning the `some` entry's object
/// id, or `None` for an empty (`None`-valued) tree.
///
/// Shared by this module's and [`crate::schema::read`]'s `Def::Option`/
/// `Schema::Optional` branches: `Some` is written as exactly one entry named
/// `some` and `None` as an empty tree, so any other arity or naming is a
/// malformed (necessarily foreign) tree rather than a value to guess at.
pub(crate) fn validate_option_entries(
    entries: &[(String, ObjectId, EntryKind)],
) -> Result<Option<ObjectId>, DeserializeError> {
    if entries.is_empty() {
        return Ok(None);
    }
    let [(name, inner_oid, _)] = entries else {
        return Err(DeserializeError::MalformedOption {
            found: entries.len(),
        });
    };
    if name != "some" {
        return Err(DeserializeError::MislabeledOption { name: name.clone() });
    }
    Ok(Some(*inner_oid))
}

/// Extract an enum tree's single (variant-name, payload object id) entry.
///
/// Shared by this module's and [`crate::schema::read`]'s enum branches: a
/// variant tree holds exactly one externally-tagged entry, and any other
/// arity is a malformed (necessarily foreign) tree.
pub(crate) fn extract_enum_entry(
    entries: Vec<(String, ObjectId, EntryKind)>,
) -> Result<(String, ObjectId), DeserializeError> {
    if entries.len() != 1 {
        return Err(DeserializeError::MalformedEnum {
            found: entries.len(),
        });
    }
    let (name, oid, _) = entries
        .into_iter()
        .next()
        .expect("length checked to be 1 above");
    Ok((name, oid))
}

/// Build a scalar-keyed map's key value from its entry name's textual form.
///
/// [`collapse_shape`] may classify a key as scalar even though the key's own
/// shape is still wrapped in a smart pointer or transparent newtype — an
/// `Arc<str>` key, for instance, collapses to `str` for the scalar-vs-composite
/// decision and is written under that collapsed scalar's textual form, but the
/// `Partial` frame `begin_key()` opens is still shaped `Arc<str>`, whose own
/// vtable has no parse function to call directly. This unwraps the same
/// smart-pointer (`begin_smart_ptr`) and transparent-newtype (`begin_inner`)
/// layers [`deser_into`]'s own `Def::Pointer` and inner-shape branches do,
/// bottoming out in `parse_from_str` on the fully collapsed frame — the map
/// analogue of those branches, except there is no separate key *object* to
/// fetch: the entry's name already is the key's textual form.
fn parse_key_from_str<'facet>(
    partial: Partial<'facet, true>,
    text: &str,
) -> Result<Partial<'facet, true>, DeserializeError> {
    let shape = partial.shape();
    if let Def::Pointer(_) = shape.def {
        let partial = partial.begin_smart_ptr().map_err(reflect)?;
        let partial = parse_key_from_str(partial, text)?;
        return partial.end().map_err(reflect);
    }
    if shape.inner.is_some() && shape.vtable.has_try_borrow_inner() {
        let partial = partial.begin_inner().map_err(reflect)?;
        let partial = parse_key_from_str(partial, text)?;
        return partial.end().map_err(reflect);
    }
    partial
        .parse_from_str(text)
        .map_err(|e| DeserializeError::Parse {
            shape: shape.type_identifier,
            text: text.to_owned(),
            reason: e.to_string(),
        })
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

    // Dynamic value (`Def::DynamicValue`, e.g. `facet_value::Value`): the
    // encoding writes no type marker, so the value's shape is recovered
    // heuristically from the object graph itself.
    if let Def::DynamicValue(_) = shape.def {
        return deser_dynamic(partial, oid, store, depth);
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
    // The scalar-vs-composite classification is decided from the key shape
    // *after* transparency collapse (`collapse_shape`), matching what the
    // encoder actually wrote: a smart-pointer or transparent-newtype key
    // (`Arc<str>`, a `#[facet(transparent)]` wrapper, ...) is written
    // name-keyed exactly as its collapsed scalar shape would be.
    if let Def::Map(md) = shape.def {
        let entries = find_tree_entries(oid, store)?;
        let scalar_keys = matches!(collapse_shape(md.k).def, Def::Scalar);
        let mut partial = partial.init_map().map_err(reflect)?;
        if scalar_keys {
            for (key, child_oid, _) in entries {
                partial = partial.begin_key().map_err(reflect)?;
                partial = parse_key_from_str(partial, &key)?;
                partial = partial.end().map_err(reflect)?;
                partial = partial.begin_value().map_err(reflect)?;
                partial = deser_into(partial, &child_oid, store, depth + 1)?;
                partial = partial.end().map_err(reflect)?;
            }
        } else {
            for (_, pair_oid, _) in entries {
                let pair = find_tree_entries(&pair_oid, store)?;
                let (k_oid, v_oid) = map_pair_entries(&pair)?;
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
        let Some(inner_oid) = validate_option_entries(&entries)? else {
            // None — the partial already holds the default None.
            return Ok(partial);
        };
        let partial = partial.begin_some().map_err(reflect)?;
        let partial = deser_into(partial, &inner_oid, store, depth + 1)?;
        return partial.end().map_err(reflect);
    }

    // Enum: single-entry tree → variant name → variant contents
    if let facet::Type::User(facet::UserType::Enum(et)) = shape.ty {
        let entries = find_tree_entries(oid, store)?;
        let (variant_name, inner_oid) = extract_enum_entry(entries)?;

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

/// Decode the object at `oid` into a dynamic value (`Def::DynamicValue`).
///
/// The encoding writes no type markers, so the value's shape is recovered by
/// a normative — and documented lossy — heuristic:
///
/// - a blob is a String when its bytes are valid UTF-8, otherwise Bytes;
/// - a non-empty tree whose entry names are all decimal ordinals is an Array;
/// - any other tree — including the empty tree — is an Object.
///
/// Scalar encodings that are not self-evident from the object alone (bool,
/// numbers, char, datetime, ...) therefore come back as Strings of their
/// textual form, and null (written as the empty tree) as an empty Object.
///
/// The caller ([`deser_into`]) has already applied the [`MAX_DEPTH`] guard for
/// this level; children recurse through `deser_into` at `depth + 1`, so the
/// guard bounds heuristic recursion exactly as it bounds typed recursion.
///
/// Fetches `oid` exactly once: the blob-vs-tree classification and the tree
/// parse both read the same fetched [`Data`], via [`tree_entries_from_data`],
/// rather than fetching the object again to re-derive the entries a second
/// `find_tree_entries` call would otherwise perform.
fn deser_dynamic<'facet, F: Find + ?Sized>(
    partial: Partial<'facet, true>,
    oid: &ObjectId,
    store: &F,
    depth: usize,
) -> Result<Partial<'facet, true>, DeserializeError> {
    let mut buf = Vec::new();
    let data = find_object(oid, &mut buf, store)?;

    // Blob → String or Bytes, decided by UTF-8 validity.
    if data.kind == Kind::Blob {
        return match String::from_utf8(data.data.to_owned()) {
            Ok(s) => partial.set::<String>(s).map_err(reflect),
            Err(e) => partial.set::<Vec<u8>>(e.into_bytes()).map_err(reflect),
        };
    }

    let mut entries = tree_entries_from_data(&data, oid)?;

    // Non-empty + all-ordinal names → Array. The empty tree reads as an
    // Object: it is what both null and the empty Object serialize to, and an
    // empty Object is the less lossy of the two readings.
    let all_ordinal = !entries.is_empty()
        && entries
            .iter()
            .all(|(name, _, _)| name.parse::<usize>().is_ok());
    if all_ordinal {
        sort_by_ordinal(&mut entries)?;
        let mut partial = partial.init_list().map_err(reflect)?;
        for (_, child_oid, _) in entries {
            partial = partial.begin_list_item().map_err(reflect)?;
            partial = deser_into(partial, &child_oid, store, depth + 1)?;
            partial = partial.end().map_err(reflect)?;
        }
        return Ok(partial);
    }

    let mut partial = partial.init_map().map_err(reflect)?;
    for (name, child_oid, _) in entries {
        partial = partial.begin_object_entry(&name).map_err(reflect)?;
        partial = deser_into(partial, &child_oid, store, depth + 1)?;
        partial = partial.end().map_err(reflect)?;
    }
    Ok(partial)
}
