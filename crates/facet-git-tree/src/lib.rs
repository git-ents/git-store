//! Serialize [`facet::Facet`] values to, and deserialize them from, Git trees.
//!
//! A value is encoded as a graph of Git objects — scalars and strings as blobs,
//! structs, enums, and collections as trees — addressed by SHA-1 exactly as Git
//! would compute them. The bundled [`ObjectStore`] is an in-memory backend, but
//! the entry points are generic over `gix`'s `Find` and `Write` traits, so a
//! real `gix` repository or object database works just as well.
//!
//! The normative encoding rules live in `docs/specification.adoc`.
#![forbid(unsafe_code)]

use std::io::Read;

pub use gix_hash::ObjectId;
pub use gix_object::Object as GitObject;
pub use gix_object::tree::{Entry as TreeEntry, EntryKind, EntryMode};

use gix_hash::Kind as HashKind;
use gix_object::{Data, Find, Kind, ObjectRef, Write};

use facet::Def;
use facet::Facet;
use facet::{Partial, Peek};

/// A content-addressed store of Git objects produced by [`serialize`].
///
/// This is a thin wrapper around [`gix_odb::memory::Proxy`], gitoxide's own
/// in-memory object database, so the `Find`/`Write` buffer handling lives in
/// `gix` rather than being reimplemented here. The accessors return owned values
/// because the proxy is `RefCell`-backed (required since [`Write::write_stream`]
/// takes `&self`).
///
/// This type is only a convenience default for callers that lack a backend of
/// their own. The actual contract is the generic `gix` [`Find`]/[`Write`] bounds
/// on [`serialize_into`] and [`deserialize`], which a real `gix` repository or
/// odb satisfies just as well. Like `gix`'s in-memory store it is `!Sync`;
/// cross-thread sharing is the job of the on-disk backends, not of this type.
pub struct ObjectStore(gix_odb::memory::Proxy<NoBackend>);

impl Default for ObjectStore {
    fn default() -> Self {
        Self(gix_odb::memory::Proxy::new(NoBackend, HashKind::Sha1))
    }
}

impl std::fmt::Debug for ObjectStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectStore")
            .field("objects", &self.0.num_objects_in_memory())
            .finish()
    }
}

impl ObjectStore {
    /// Decode and return the object stored under `id`, if present.
    pub fn get(&self, id: &ObjectId) -> Option<GitObject> {
        let mut buf = Vec::new();
        let data = self.0.try_find(id, &mut buf).ok().flatten()?;
        ObjectRef::from_bytes(data.data, data.kind, HashKind::Sha1)
            .ok()?
            .into_owned()
            .ok()
    }

    /// Return the entries of the tree stored under `id`, if it is a tree.
    pub fn get_tree(&self, id: &ObjectId) -> Option<Vec<TreeEntry>> {
        match self.get(id)? {
            GitObject::Tree(tree) => Some(tree.entries),
            _ => None,
        }
    }

    /// Return the raw bytes of the blob stored under `id`, if it is a blob.
    pub fn get_blob(&self, id: &ObjectId) -> Option<Vec<u8>> {
        match self.get(id)? {
            GitObject::Blob(blob) => Some(blob.data),
            _ => None,
        }
    }
}

impl Find for ObjectStore {
    fn try_find<'a>(
        &self,
        id: &gix_hash::oid,
        buffer: &'a mut Vec<u8>,
    ) -> Result<Option<Data<'a>>, gix_object::find::Error> {
        self.0.try_find(id, buffer)
    }
}

impl Write for ObjectStore {
    fn write_stream(
        &self,
        kind: Kind,
        size: u64,
        from: &mut dyn Read,
    ) -> Result<ObjectId, gix_object::write::Error> {
        self.0.write_stream(kind, size, from)
    }
}

/// Inert backing database for [`ObjectStore`]'s in-memory [`Proxy`].
///
/// [`gix_odb::memory::Proxy`] is generic over an inner object database it falls
/// back to, but [`ObjectStore`] keeps everything in the proxy's in-memory map, so
/// the inner is never read from or written to. gitoxide ships no type that is
/// both [`Find`] and [`Write`] while doing nothing, so this supplies one.
#[derive(Debug, Default)]
struct NoBackend;

impl Find for NoBackend {
    fn try_find<'a>(
        &self,
        _id: &gix_hash::oid,
        _buffer: &'a mut Vec<u8>,
    ) -> Result<Option<Data<'a>>, gix_object::find::Error> {
        Ok(None)
    }
}

impl Write for NoBackend {
    fn write_stream(
        &self,
        _kind: Kind,
        _size: u64,
        _from: &mut dyn Read,
    ) -> Result<ObjectId, gix_object::write::Error> {
        // The enclosing `Proxy` always has its in-memory store enabled, so writes
        // are intercepted before reaching this inner database.
        Err("NoBackend: writes are handled by the in-memory proxy".into())
    }
}

/// An error produced by serialization or deserialization.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A facet key cannot be represented as a Git tree entry name.
    ///
    /// Tree entry names double as path segments, so a key may not contain the
    /// path separator `/`.
    #[error("invalid key {0:?}: must not contain '/'")]
    InvalidKey(String),
    /// A tree entry name (path segment) is not valid UTF-8.
    ///
    /// Holds the lossily-decoded name for diagnostics. Write-side names are
    /// always UTF-8, so this can only arise from an externally-produced tree.
    #[error("tree entry name {0:?} is not valid UTF-8")]
    NonUtf8Name(String),
    /// A referenced object was not present in its backing store.
    #[error("object {0} not found")]
    NotFound(ObjectId),
    /// An object was expected to be a tree but was of another kind.
    #[error("object {0} is not a tree")]
    NotATree(ObjectId),
    /// An object was expected to be a blob (a scalar leaf) but was of another kind.
    #[error("object {0} is not a blob")]
    NotABlob(ObjectId),
    /// An error from the underlying `gix` object backend.
    ///
    /// Wraps the backend's own error (from [`Find`]/[`Write`]) as the source
    /// rather than flattening it into a string.
    #[error("git object backend error")]
    Backend(#[source] gix_object::write::Error),
    /// Deserialization exceeded the maximum supported nesting depth.
    ///
    /// A guard against unbounded recursion — and thus stack overflow — when
    /// reading a deeply nested, possibly externally-produced tree. The bundled
    /// encoder never approaches this depth for ordinary values.
    #[error("maximum nesting depth ({0}) exceeded while deserializing")]
    MaxDepth(usize),
    /// A sequence entry name is not a valid decimal ordinal.
    ///
    /// Sequence (`Vec`/array) entries are named by their zero-based decimal index
    /// on write, so a non-numeric name can only arise from an externally-produced
    /// tree.
    #[error("invalid sequence ordinal {0:?}")]
    InvalidOrdinal(String),
    /// A general serialization or deserialization failure.
    #[error("{0}")]
    Message(String),
}

/// A Git tree already written into the backing store, embedded by object id
/// rather than walked field-by-field.
///
/// [`serialize_node`] and [`deser_into`] special-case this type ahead of the
/// generic struct branch: a `RawTree` field passes its wrapped object id
/// straight through as a tree entry (no recursion, no write), and reading one
/// back captures the child entry's object id without decoding its contents.
/// This lets a `Facet`-derived struct embed an arbitrarily-shaped subtree — a
/// directory with no fixed layout, such as an imported toolchain's `bin/` —
/// next to ordinarily-encoded fields.
///
/// The wrapped tree must already exist in the store the caller serializes
/// into; `RawTree` carries no content of its own to write. Sha-1 only, like
/// the rest of this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Facet)]
pub struct RawTree {
    // Opaque to the normal field-by-field encoding: `serialize_node`/
    // `deser_into` intercept `RawTree` by shape identity before this field is
    // ever visited.
    hash: [u8; 20],
}

impl RawTree {
    /// Wrap a tree's object id for embedding as a passthrough field.
    pub fn new(oid: ObjectId) -> Self {
        let mut hash = [0u8; 20];
        hash.copy_from_slice(oid.as_slice());
        Self { hash }
    }

    /// The wrapped tree's object id.
    pub fn oid(&self) -> ObjectId {
        ObjectId::from_bytes_or_panic(&self.hash)
    }
}

/// Wrap any displayable backend or reflection error as [`Error::Message`].
///
/// `facet`'s `Partial`/`Peek` operations and `gix`'s tree decoding each return
/// their own error types; this collapses them to the catch-all variant at the
/// call site without a bespoke closure every time.
fn msg(e: impl std::fmt::Display) -> Error {
    Error::Message(e.to_string())
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
/// contain the path separator `/` ([`Error::InvalidKey`]). Serialization is
/// required to apply this to every dynamic key (such as map keys) before emitting
/// its entry, so a `/`-bearing name can never be written as data.
pub fn check_key(key: &str) -> Result<(), Error> {
    if key.contains('/') {
        return Err(Error::InvalidKey(key.to_owned()));
    }
    Ok(())
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
pub fn serialize_into<T, W>(value: &T, store: &W) -> Result<ObjectId, Error>
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
pub fn serialize<T: for<'a> facet::Facet<'a>>(value: &T) -> Result<(ObjectId, ObjectStore), Error> {
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
pub fn serialize_peek_into<W>(peek: Peek<'_, '_>, store: &W) -> Result<ObjectId, Error>
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
pub fn serialize_peek(peek: Peek<'_, '_>) -> Result<(ObjectId, ObjectStore), Error> {
    let store = ObjectStore::default();
    let root = serialize_peek_into(peek, &store)?;
    Ok((root, store))
}

/// Deserialize a [`facet::Facet`] value from a root tree stored in `store`.
///
/// `store` is any `gix` [`Find`] source — a real repository, an in-memory odb,
/// or an [`ObjectStore`] — the read side of the backend contract documented on
/// [`serialize_into`]. `?Sized` is permitted so a `&dyn Find` may be passed.
pub fn deserialize<T: for<'a> facet::Facet<'a>>(
    root: &ObjectId,
    store: &(impl Find + ?Sized),
) -> Result<T, Error> {
    let partial =
        Partial::alloc::<T>().map_err(|e| Error::Message(format!("alloc failed: {e}")))?;
    let partial = deser_into(partial, root, store, 0)?;
    let heap = partial
        .build()
        .map_err(|e| Error::Message(format!("build failed: {e}")))?;
    heap.materialize::<T>()
        .map_err(|e| Error::Message(format!("materialize failed: {e}")))
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
) -> Result<Partial<'facet, true>, Error> {
    deser_into(partial, root, store, 0)
}

// --- serialization internals ---

/// The element shape of a `Vec`/array/slice, or `None` for any other type.
fn seq_elem(shape: &facet::Shape) -> Option<&'static facet::Shape> {
    match shape.def {
        Def::List(d) => Some(d.t),
        Def::Array(d) => Some(d.t),
        Def::Slice(d) => Some(d.t),
        _ => None,
    }
}

/// Whether `shape` is a sequence of `u8` (`Vec<u8>`, `[u8; N]`, `[u8]`). Such a
/// sequence is stored as one blob rather than a per-element tree.
fn is_byte_seq(shape: &facet::Shape) -> bool {
    seq_elem(shape).is_some_and(|t| t.is_type::<u8>())
}

fn serialize_node<W: Write + ?Sized>(
    peek: Peek<'_, '_>,
    store: &W,
) -> Result<(ObjectId, EntryKind), Error> {
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
        let rt = peek.get::<RawTree>().map_err(msg)?;
        return Ok((rt.oid(), EntryKind::Tree));
    }

    // Scalar leaf → blob
    if matches!(shape.def, Def::Scalar) {
        let bytes = scalar_bytes(peek)?;
        let oid = store
            .write_buf(Kind::Blob, &bytes)
            .map_err(Error::Backend)?;
        return Ok((oid, EntryKind::Blob));
    }

    // Byte sequence (`Vec<u8>`, `[u8; N]`, `[u8]`) → a single blob. This is the
    // Git-native representation; a per-byte tree would be wasteful and would
    // defeat blob-level deduplication of identical buffers.
    if is_byte_seq(shape) {
        let seq = peek.into_list_like().map_err(msg)?;
        let mut bytes = Vec::new();
        for item in seq.iter() {
            bytes.push(*item.get::<u8>().map_err(msg)?);
        }
        let oid = store
            .write_buf(Kind::Blob, &bytes)
            .map_err(Error::Backend)?;
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
        let ps = peek.into_struct().map_err(msg)?;
        let mut entries: Vec<TreeEntry> = Vec::with_capacity(st.fields.len());
        for (i, field) in st.fields.iter().enumerate() {
            let child = ps.field(i).map_err(msg)?;
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
            .map_err(Error::Backend)?;
        return Ok((oid, EntryKind::Tree));
    }

    // Vec / Array / slice → tree with ordinal keys
    if matches!(shape.def, Def::List(_) | Def::Array(_) | Def::Slice(_)) {
        let entries = serialize_sequence(peek, store)?;
        let oid = store
            .write(&gix_object::Tree { entries })
            .map_err(Error::Backend)?;
        return Ok((oid, EntryKind::Tree));
    }

    // Map → tree. A map with scalar keys names each entry by the textual form of
    // its key (the readable, JSON-like form). A map with composite keys (structs,
    // tuples, enums, ...) — which have no faithful textual form — instead stores
    // each pair as an ordinal-named two-entry sub-tree `{ k, v }`, both children
    // recursing through the normal encoding. The two layouts are distinguished by
    // the static key shape, so no on-disk marker is needed.
    if let Def::Map(md) = shape.def {
        let pm = peek.into_map().map_err(msg)?;
        let scalar_keys = matches!(md.k.def, Def::Scalar);
        let mut entries: Vec<TreeEntry> = Vec::new();
        if scalar_keys {
            for (k, v) in pm.iter() {
                let key_bytes = scalar_bytes(k)?;
                let key_str = std::str::from_utf8(&key_bytes)
                    .map_err(|_| Error::Message("map key is not valid UTF-8".into()))?;
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
                    .map_err(Error::Backend)?;
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
            .map_err(Error::Backend)?;
        return Ok((oid, EntryKind::Tree));
    }

    // Option
    if matches!(shape.def, Def::Option(_)) {
        let po = peek.into_option().map_err(msg)?;
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
                .map_err(Error::Backend)?;
            return Ok((oid, EntryKind::Tree));
        } else {
            // None: empty tree
            let oid = store
                .write(&gix_object::Tree { entries: vec![] })
                .map_err(Error::Backend)?;
            return Ok((oid, EntryKind::Tree));
        }
    }

    // Enum → single-entry tree: variant name → variant contents
    if let facet::Type::User(facet::UserType::Enum(_)) = shape.ty {
        let pe = peek.into_enum().map_err(msg)?;
        let variant = pe.active_variant().map_err(msg)?;
        let variant_name = pe.variant_name_active().map_err(msg)?;

        // Encode the variant's payload (unit → empty tree, newtype → the field's
        // own encoding directly, tuple → ordinal-keyed tree, struct → name-keyed
        // tree). A tuple variant is `StructKind::TupleStruct`; a struct variant is
        // `StructKind::Struct`.
        let positional = matches!(variant.data.kind, facet::StructKind::TupleStruct);
        let newtype = positional && variant.data.fields.len() == 1;
        let (inner_oid, inner_kind) = if variant.data.fields.is_empty() {
            let oid = store
                .write(&gix_object::Tree { entries: vec![] })
                .map_err(Error::Backend)?;
            (oid, EntryKind::Tree)
        } else if newtype {
            // Newtype variant: resolves directly to the encoding of its one field.
            let child = pe
                .field(0)
                .map_err(msg)?
                .ok_or_else(|| Error::Message("variant field 0 missing".into()))?;
            serialize_node(child, store)?
        } else {
            let mut inner_entries: Vec<TreeEntry> = Vec::new();
            for (i, field) in variant.data.fields.iter().enumerate() {
                let child = pe
                    .field(i)
                    .map_err(msg)?
                    .ok_or_else(|| Error::Message(format!("variant field {i} missing")))?;
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
                .map_err(Error::Backend)?;
            (oid, EntryKind::Tree)
        };

        let entries = vec![TreeEntry {
            mode: EntryMode::from(inner_kind),
            filename: variant_name.into(),
            oid: inner_oid,
        }];
        let oid = store
            .write(&gix_object::Tree { entries })
            .map_err(Error::Backend)?;
        return Ok((oid, EntryKind::Tree));
    }

    Err(Error::Message(format!(
        "unsupported type for serialization: {}",
        shape.type_identifier
    )))
}

fn serialize_sequence<W: Write + ?Sized>(
    peek: Peek<'_, '_>,
    store: &W,
) -> Result<Vec<TreeEntry>, Error> {
    let seq = peek.into_list_like().map_err(msg)?;
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

fn scalar_bytes(peek: Peek<'_, '_>) -> Result<Vec<u8>, Error> {
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
                let v = *peek.get::<bool>().map_err(msg)?;
                return Ok(v.to_string().into_bytes());
            }
            PrimitiveType::Textual(TextualType::Char) => {
                let v = *peek.get::<char>().map_err(msg)?;
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
                    let v = *peek.get::<f32>().map_err(msg)?;
                    if v.is_nan() {
                        return Ok(b"nan".to_vec());
                    }
                    let v = if v == 0.0f32 { 0.0f32 } else { v };
                    return Ok(v.to_string().into_bytes());
                } else {
                    let v = *peek.get::<f64>().map_err(msg)?;
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

    Err(Error::Message(format!(
        "unsupported scalar type: {}",
        shape.type_identifier
    )))
}

// --- deserialization internals ---

fn find_object<'a, F: Find + ?Sized>(
    id: &ObjectId,
    buf: &'a mut Vec<u8>,
    store: &F,
) -> Result<Data<'a>, Error> {
    store
        .try_find(id, buf)
        .map_err(msg)?
        .ok_or_else(|| Error::NotFound(*id))
}

fn find_tree_entries<F: Find + ?Sized>(
    id: &ObjectId,
    store: &F,
) -> Result<Vec<(String, ObjectId, EntryKind)>, Error> {
    let mut buf = Vec::new();
    let data = find_object(id, &mut buf, store)?;
    if data.kind != Kind::Tree {
        return Err(Error::NotATree(*id));
    }
    let tree_ref = gix_object::TreeRef::from_bytes(data.data, HashKind::Sha1).map_err(msg)?;
    let mut result = Vec::new();
    for entry in &tree_ref.entries {
        let name = std::str::from_utf8(entry.filename).map_err(|_| {
            Error::NonUtf8Name(String::from_utf8_lossy(entry.filename).into_owned())
        })?;
        result.push((name.to_owned(), entry.oid.to_owned(), entry.mode.kind()));
    }
    Ok(result)
}

fn find_blob_bytes<F: Find + ?Sized>(id: &ObjectId, store: &F) -> Result<Vec<u8>, Error> {
    let mut buf = Vec::new();
    let data = find_object(id, &mut buf, store)?;
    if data.kind != Kind::Blob {
        return Err(Error::NotABlob(*id));
    }
    Ok(data.data.to_owned())
}

/// Sort sequence entries into ascending ordinal order, rejecting any entry whose
/// name is not a decimal index.
///
/// Sequence elements are named by zero-based decimal index, so the order must be
/// recovered numerically rather than lexically (`10000` sorts before `9999`). A
/// non-numeric name can only come from a foreign tree and is reported as
/// [`Error::InvalidOrdinal`].
fn sort_by_ordinal(entries: &mut [(String, ObjectId, EntryKind)]) -> Result<(), Error> {
    // Validate up front so the infallible sort key below cannot misorder entries.
    for (name, _, _) in entries.iter() {
        name.parse::<usize>()
            .map_err(|_| Error::InvalidOrdinal(name.clone()))?;
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
) -> Result<Partial<'facet, true>, Error> {
    if depth > MAX_DEPTH {
        return Err(Error::MaxDepth(MAX_DEPTH));
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
            return Err(Error::NotATree(*oid));
        }
        return partial.set(RawTree::new(*oid)).map_err(msg);
    }

    // Scalar leaf: read blob, parse from str
    if matches!(shape.def, Def::Scalar) {
        let bytes = find_blob_bytes(oid, store)?;
        let s = std::str::from_utf8(&bytes)
            .map_err(|_| Error::Message("blob is not valid UTF-8".into()))?;
        return partial
            .parse_from_str(s)
            .map_err(|e| Error::Message(format!("parse failed: {e}")));
    }

    // Byte sequence (`Vec<u8>`, `[u8; N]`): read the single blob and fill the
    // collection one byte at a time, mirroring the serializer's blob encoding.
    if is_byte_seq(shape) {
        let bytes = find_blob_bytes(oid, store)?;
        if matches!(shape.def, Def::Array(_)) {
            let mut partial = partial.init_array().map_err(msg)?;
            for (i, b) in bytes.iter().enumerate() {
                partial = partial.begin_nth_field(i).map_err(msg)?;
                partial = partial.set::<u8>(*b).map_err(msg)?;
                partial = partial.end().map_err(msg)?;
            }
            return Ok(partial);
        }
        let mut partial = partial.init_list().map_err(msg)?;
        for b in bytes {
            partial = partial.begin_list_item().map_err(msg)?;
            partial = partial.set::<u8>(b).map_err(msg)?;
            partial = partial.end().map_err(msg)?;
        }
        return Ok(partial);
    }

    // Smart pointer (`Box`/`Arc`/`Rc`). For a slice pointee (`Arc<[T]>`) facet
    // hands back a slice builder we feed item by item; its element type decides
    // blob-vs-tree exactly as for an owned sequence. For a sized pointee the
    // pointee shares this node's encoding, so we recurse on the same object.
    if let Def::Pointer(pd) = shape.def {
        let mut partial = partial.begin_smart_ptr().map_err(msg)?;
        if partial.is_building_smart_ptr_slice() {
            if pd.pointee.is_some_and(is_byte_seq) {
                let bytes = find_blob_bytes(oid, store)?;
                for b in bytes {
                    partial = partial.begin_list_item().map_err(msg)?;
                    partial = partial.set::<u8>(b).map_err(msg)?;
                    partial = partial.end().map_err(msg)?;
                }
            } else {
                let mut entries = find_tree_entries(oid, store)?;
                sort_by_ordinal(&mut entries)?;
                for (_, child_oid, _) in entries {
                    partial = partial.begin_list_item().map_err(msg)?;
                    partial = deser_into(partial, &child_oid, store, depth + 1)?;
                    partial = partial.end().map_err(msg)?;
                }
            }
            return partial.end().map_err(msg);
        }
        partial = deser_into(partial, oid, store, depth + 1)?;
        return partial.end().map_err(msg);
    }

    // Transparent newtype (`#[facet(transparent)]`, `NonZero<T>`, path
    // wrappers): the object was written as the inner value's own encoding (via
    // `Peek::innermost_peek`, which unwraps exactly when `try_borrow_inner` is
    // present), so build that and let `begin_inner` reassemble the wrapper.
    // Gated on `has_try_borrow_inner`, not just `shape.inner.is_some()`: plain
    // collections like `Vec<T>` also carry an `inner` shape (for variance) but
    // were never unwrapped on serialization, so must not be routed here.
    if shape.inner.is_some() && shape.vtable.has_try_borrow_inner() {
        let partial = partial.begin_inner().map_err(msg)?;
        let partial = deser_into(partial, oid, store, depth + 1)?;
        return partial.end().map_err(msg);
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
                partial = partial
                    .begin_field(field.name)
                    .map_err(|e| Error::Message(format!("begin_field {}: {e}", field.name)))?;
                partial = deser_into(partial, &child_oid, store, depth + 1)?;
                partial = partial
                    .end()
                    .map_err(|e| Error::Message(format!("end field {}: {e}", field.name)))?;
            }
        }
        return Ok(partial);
    }

    // List (Vec): read tree with ordinal keys, sort numerically, push items
    if matches!(shape.def, Def::List(_)) {
        let mut entries = find_tree_entries(oid, store)?;
        sort_by_ordinal(&mut entries)?;
        let mut partial = partial.init_list().map_err(msg)?;
        for (_, child_oid, _) in entries {
            partial = partial.begin_list_item().map_err(msg)?;
            partial = deser_into(partial, &child_oid, store, depth + 1)?;
            partial = partial.end().map_err(msg)?;
        }
        return Ok(partial);
    }

    // Array: same as List but init_array
    if matches!(shape.def, Def::Array(_)) {
        let mut entries = find_tree_entries(oid, store)?;
        sort_by_ordinal(&mut entries)?;
        let mut partial = partial.init_array().map_err(msg)?;
        for (name, child_oid, _) in entries {
            let idx = name
                .parse::<usize>()
                .expect("ordinal validated by sort_by_ordinal");
            partial = partial.begin_nth_field(idx).map_err(msg)?;
            partial = deser_into(partial, &child_oid, store, depth + 1)?;
            partial = partial.end().map_err(msg)?;
        }
        return Ok(partial);
    }

    // Map: mirror serialization. Scalar-keyed maps name each entry by the key's
    // textual form (parsed back via `parse_from_str`); composite-keyed maps store
    // each pair as a `{ k, v }` sub-tree, both children recovered by recursing.
    if let Def::Map(md) = shape.def {
        let entries = find_tree_entries(oid, store)?;
        let scalar_keys = matches!(md.k.def, Def::Scalar);
        let mut partial = partial.init_map().map_err(msg)?;
        if scalar_keys {
            for (key, child_oid, _) in entries {
                partial = partial.begin_key().map_err(msg)?;
                partial = partial.parse_from_str(&key).map_err(msg)?;
                partial = partial.end().map_err(msg)?;
                partial = partial.begin_value().map_err(msg)?;
                partial = deser_into(partial, &child_oid, store, depth + 1)?;
                partial = partial.end().map_err(msg)?;
            }
        } else {
            for (_, pair_oid, _) in entries {
                let pair = find_tree_entries(&pair_oid, store)?;
                let find = |want: &str| {
                    pair.iter()
                        .find(|(n, _, _)| n == want)
                        .map(|(_, o, _)| *o)
                        .ok_or_else(|| {
                            Error::Message(format!("map pair sub-tree missing {want:?} entry"))
                        })
                };
                let k_oid = find("k")?;
                let v_oid = find("v")?;
                partial = partial.begin_key().map_err(msg)?;
                partial = deser_into(partial, &k_oid, store, depth + 1)?;
                partial = partial.end().map_err(msg)?;
                partial = partial.begin_value().map_err(msg)?;
                partial = deser_into(partial, &v_oid, store, depth + 1)?;
                partial = partial.end().map_err(msg)?;
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
            return Err(Error::Message(format!(
                "malformed Option tree: expected a single \"some\" entry, found {}",
                entries.len()
            )));
        };
        if name != "some" {
            return Err(Error::Message(format!(
                "malformed Option tree: entry must be named \"some\", found {name:?}"
            )));
        }
        let inner_oid = *inner_oid;
        let partial = partial.begin_some().map_err(msg)?;
        let partial = deser_into(partial, &inner_oid, store, depth + 1)?;
        return partial.end().map_err(msg);
    }

    // Enum: single-entry tree → variant name → variant contents
    if let facet::Type::User(facet::UserType::Enum(et)) = shape.ty {
        let entries = find_tree_entries(oid, store)?;
        if entries.len() != 1 {
            return Err(Error::Message(format!(
                "malformed enum tree: expected exactly one entry, found {}",
                entries.len()
            )));
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

        let mut partial = partial
            .select_variant_named(&variant_name)
            .map_err(|e| Error::Message(format!("select variant {variant_name}: {e}")))?;

        if newtype {
            partial = partial.begin_nth_field(0).map_err(msg)?;
            partial = deser_into(partial, &inner_oid, store, depth + 1)?;
            return partial.end().map_err(msg);
        }

        let inner_entries = find_tree_entries(&inner_oid, store)?;
        for (name, child_oid, _) in inner_entries {
            if positional {
                let idx = name
                    .parse::<usize>()
                    .map_err(|_| Error::InvalidOrdinal(name.clone()))?;
                partial = partial.begin_nth_field(idx).map_err(msg)?;
            } else {
                partial = partial.begin_field(&name).map_err(msg)?;
            }
            partial = deser_into(partial, &child_oid, store, depth + 1)?;
            partial = partial.end().map_err(msg)?;
        }
        return Ok(partial);
    }

    Err(Error::Message(format!(
        "unsupported type for deserialization: {}",
        shape.type_identifier
    )))
}
