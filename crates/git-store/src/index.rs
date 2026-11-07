//! The persisted per-kind materialized entity index.
//!
//! The index is a cache, never a source of truth: a reader serves it only when
//! it matches the entity refs exactly, and a writer advances it as a delta on
//! the current index whenever that is readable, so maintaining it does not
//! rescan the kind on every write. Entity refs remain authoritative
//! throughout.
//!
//! Because a write is a delta, a ref moved out-of-band — one that bypasses
//! this crate's writers and so never touches the index — leaves the index
//! stale rather than self-healing on the next write. Readers already reject a
//! stale index and fall back to the refs; the explicit
//! [`Kind::rebuild_index`](crate::Kind::rebuild_index) repairs it.

use std::fmt::Write as _;

use gix::objs::{Find, Write};
use gix_refstore::{ObjectId, RefName, RefPath, RefPrefix, RefSegment, RefStore};

use crate::{error::Error, store::Store};

const CACHE_NAMESPACE: &str = "refs/cache";
const CACHE_NAME: &str = "index-v1";

/// The ref name is injective in the kind name. The cache namespace is private
/// by convention and deliberately separate from both user data and schemas.
pub(crate) fn reference(kind: &RefSegment) -> RefName {
    RefName::new(format!(
        "{CACHE_NAMESPACE}/{}/{}",
        encode_bytes(kind.as_str().as_bytes()),
        CACHE_NAME
    ))
    .expect("encoded index ref is valid")
}

/// Every ref directly under `entities`, named relative to that prefix,
/// ascending by name. Used by [`update`]'s rebuild base, and by
/// [`crate::Transaction`], which stages edits for entities across several
/// kinds at once; [`crate::Kind`]'s own listing shares the same shape through
/// its `entries_in`.
pub(crate) fn source_entries<R, O>(
    store: &Store<R, O>,
    entities: &RefPrefix,
) -> Result<Vec<(RefPath, ObjectId)>, Error>
where
    R: RefStore,
    O: Find,
{
    let mut entries: Vec<_> = store
        .refs()
        .prefixed(entities)
        .map_err(Error::backend)?
        .into_iter()
        .filter_map(|(name, commit)| name.relative_to(entities).map(|name| (name, commit)))
        .collect();
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));
    Ok(entries)
}

/// One change to a kind's materialized index: set an entity's entry to a
/// commit (a publication or a tombstone), or drop its entry.
pub(crate) enum Change {
    /// The named entity's ref now points at this commit.
    Set(RefPath, ObjectId),
    /// The named entity's ref is gone; drop its index entry.
    Remove(RefPath),
}

/// The materialized-index [`RefEdit`](gix_refstore::RefEdit) that advances
/// `kind`'s index (currently at `current`) toward the result of applying
/// `changes`, or `None` when the index already holds that result.
///
/// When `current` names a readable, non-empty index tree it is the base, and
/// `changes` is applied to it as a delta — no ref listing. When the index is
/// absent, unreadable, or empty, the base is a full listing of `entities`, so
/// a missing or corrupt index is rebuilt rather than compounded. Either way
/// the expectation is `current`, so a concurrent writer that moves the index
/// makes this edit lose its compare-and-swap and the caller retries against
/// the winner's index rather than overwriting it.
pub(crate) fn update<R, O>(
    store: &Store<R, O>,
    kind: &RefSegment,
    entities: &RefPrefix,
    current: Option<ObjectId>,
    changes: &[Change],
) -> Result<Option<gix_refstore::RefEdit>, Error>
where
    R: RefStore,
    O: Find + Write,
{
    let base = current.and_then(|tree| read_tree(store, tree).ok());
    let mut entries = match base.filter(|entries| !entries.is_empty()) {
        Some(entries) => entries,
        None => source_entries(store, entities)?,
    };
    for change in changes {
        match change {
            Change::Set(name, commit) => {
                match entries.iter_mut().find(|(existing, _)| existing == name) {
                    Some((_, slot)) => *slot = *commit,
                    None => {
                        entries.push((name.clone(), *commit));
                        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
                    }
                }
            }
            Change::Remove(name) => entries.retain(|(existing, _)| existing != name),
        }
    }
    edit(store, kind, current, &entries)
}

/// The materialized-index [`RefEdit`](gix_refstore::RefEdit) that brings
/// `kind`'s index ref (currently at `current`) to reflect the complete set
/// `entries`, or `None` when it already does.
fn edit<R, O>(
    store: &Store<R, O>,
    kind: &RefSegment,
    current: Option<ObjectId>,
    entries: &[(RefPath, ObjectId)],
) -> Result<Option<gix_refstore::RefEdit>, Error>
where
    R: RefStore,
    O: Find + Write,
{
    if entries.is_empty() {
        return Ok(current.map(|expected| gix_refstore::RefEdit::Delete {
            name: reference(kind),
            expected,
        }));
    }
    let tree = write(store, entries)?;
    Ok(Some(match current {
        Some(expected) if expected == tree => return Ok(None),
        Some(expected) => gix_refstore::RefEdit::Update {
            name: reference(kind),
            expected,
            new: tree,
        },
        None => gix_refstore::RefEdit::Create {
            name: reference(kind),
            new: tree,
        },
    }))
}

/// The index entries for `kind`, validated against `source` — the kind's own
/// entity refs, which the caller has already listed — or `None` when the index
/// is missing, malformed, or stale.
///
/// Validation is exactly "the decoded index equals `source`": the index is a
/// cache, so anything that does not match the authoritative ref listing is
/// rejected and the caller falls back to `source`. This costs the caller a
/// listing, a tree read, and the decode of every index entry, so the index can
/// never make a read cheaper than the listing it must be checked against; its
/// only remaining job is to be maintained cheaply on the write side.
pub(crate) fn read_validated<R, O>(
    store: &Store<R, O>,
    kind: &RefSegment,
    source: &[(RefPath, ObjectId)],
) -> Result<Option<Vec<(RefPath, ObjectId)>>, Error>
where
    R: RefStore,
    O: Find,
{
    let index_ref = reference(kind);
    let Some(tree) = store.refs().read(&index_ref).map_err(Error::backend)? else {
        return Ok(None);
    };

    // Any malformed, non-canonical, or stale object is rejected and the
    // caller uses the entity refs as the source of truth.
    let Ok(entries) = read_tree(store, tree) else {
        return Ok(None);
    };
    if entries == source {
        Ok(Some(entries))
    } else {
        Ok(None)
    }
}

pub(crate) fn write<R, O>(
    store: &Store<R, O>,
    entries: &[(RefPath, ObjectId)],
) -> Result<ObjectId, Error>
where
    R: RefStore,
    O: Find + Write,
{
    let mode = gix::objs::tree::EntryMode::from(gix::objs::tree::EntryKind::Commit);
    let mut tree_entries = entries
        .iter()
        .map(|(name, commit)| gix::objs::tree::Entry {
            mode,
            filename: encode_path(name).into(),
            oid: *commit,
        })
        .collect::<Vec<_>>();
    tree_entries.sort();
    store
        .objects()
        .write(&gix::objs::Tree {
            entries: tree_entries,
        })
        .map_err(Error::backend)
}

fn read_tree<R, O>(store: &Store<R, O>, tree: ObjectId) -> Result<Vec<(RefPath, ObjectId)>, Error>
where
    R: RefStore,
    O: Find,
{
    let entries = store.tree_entries(tree)?;
    if !entries
        .windows(2)
        .all(|pair| pair[0].filename < pair[1].filename)
    {
        return Err(invalid_index("index tree is not canonically ordered"));
    }

    let mut decoded = Vec::with_capacity(entries.len());
    for entry in entries {
        if entry.mode.kind() != gix::objs::tree::EntryKind::Commit {
            return Err(invalid_index("index tree contains a non-commit target"));
        }
        let name = decode_path(entry.filename.as_ref())
            .ok_or_else(|| invalid_index("index tree contains an invalid path"))?;
        decoded.push((name, entry.oid));
    }
    decoded.sort_by(|(a, _), (b, _)| a.cmp(b));
    if decoded.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(invalid_index("index tree contains duplicate paths"));
    }
    Ok(decoded)
}

fn encode_path(path: &RefPath) -> String {
    let mut encoded = String::from("p");
    for segment in path.segments() {
        let bytes = segment.as_str().as_bytes();
        let length = u32::try_from(bytes.len()).expect("ref segment is too large to index");
        write!(&mut encoded, "{length:08x}").expect("writing to String cannot fail");
        encoded.push_str(&encode_hex(bytes));
    }
    encoded
}

fn decode_path(bytes: &[u8]) -> Option<RefPath> {
    if bytes.first().copied() != Some(b'p') {
        return None;
    }
    let mut cursor = 1;
    let mut segments = Vec::new();
    while cursor < bytes.len() {
        let header_end = cursor.checked_add(8)?;
        if header_end > bytes.len() {
            return None;
        }
        let length = parse_hex(&bytes[cursor..header_end])?;
        cursor = header_end;
        let end = cursor.checked_add(length.checked_mul(2)?)?;
        if end > bytes.len() {
            return None;
        }
        let decoded = decode_hex(&bytes[cursor..end])?;
        segments.push(String::from_utf8(decoded).ok()?);
        cursor = end;
    }
    if segments.is_empty() {
        return None;
    }
    RefPath::new(segments.join("/")).ok()
}

fn encode_bytes(bytes: &[u8]) -> String {
    let mut encoded = String::from("k");
    encoded.push_str(&encode_hex(bytes));
    encoded
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

fn parse_hex(bytes: &[u8]) -> Option<usize> {
    let mut value = 0usize;
    for byte in bytes {
        value = value
            .checked_mul(16)?
            .checked_add(hex_digit(*byte)? as usize)?;
    }
    Some(value)
}

fn decode_hex(bytes: &[u8]) -> Option<Vec<u8>> {
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&[high, low]| Some(hex_digit(high)? << 4 | hex_digit(low)?))
        .collect()
}

fn invalid_index(message: &'static str) -> Error {
    Error::backend(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message,
    ))
}

/// The value of one canonical (lowercase) hex digit.
///
/// Uppercase is refused deliberately: [`encode_hex`] only ever writes
/// lowercase, so accepting uppercase would let a hand-written index tree carry
/// a byte-for-byte different spelling of the same path — more than one
/// encoding for one ref name. A tree that uses any other spelling is
/// non-canonical and is rejected, so the caller rebuilds it.
fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use facet_git_tree::ObjectStore;

    #[test]
    fn decode_hex_accepts_only_lowercase() {
        assert_eq!(decode_hex(b"00ff"), Some(vec![0x00, 0xff]));
        assert_eq!(decode_hex(b"c3a9"), Some(vec![0xc3, 0xa9]));
        assert_eq!(
            decode_hex(b"00FF"),
            None,
            "uppercase hex is non-canonical and must be refused",
        );
        assert_eq!(decode_hex(b"C3A9"), None, "uppercase hex is non-canonical");
    }

    /// The length header before each segment is hex too, and must be canonical.
    #[test]
    fn parse_hex_accepts_only_lowercase() {
        assert_eq!(parse_hex(b"0000000a"), Some(10));
        assert_eq!(
            parse_hex(b"0000000A"),
            None,
            "an uppercase length header is non-canonical",
        );
    }

    #[test]
    fn decode_path_round_trips_canonical_and_refuses_uppercase() {
        let path = RefPath::new("é").expect("legal non-ASCII segment");
        let canonical = encode_path(&path);
        assert_eq!(decode_path(canonical.as_bytes()), Some(path.clone()));

        // "é" is two bytes (c3 a9), so the canonical encoding is
        // `p` + the 8-digit length + the hex payload `c3a9`. Upper-casing the
        // payload leaves the `p` marker and every other byte canonical, so
        // only the hex strictness can reject it.
        assert_eq!(decode_path(b"p00000002c3a9"), Some(path.clone()));
        assert_eq!(
            decode_path(b"p00000002C3A9"),
            None,
            "an uppercase hex payload must be refused, not decoded",
        );
        // Same for an uppercase length header.
        assert_eq!(
            decode_path(b"p0000000A6162636465666768"),
            None,
            "an uppercase length header must be refused",
        );
    }

    fn store() -> Store<crate::MemoryRefStore, ObjectStore> {
        Store::new(crate::MemoryRefStore::new(), ObjectStore::default())
    }

    /// A one-entry tree carrying `filename`, whose commit target is a real
    /// blob so the object store can hold it.
    fn tree_with(store: &Store<crate::MemoryRefStore, ObjectStore>, filename: &str) -> ObjectId {
        use gix::objs::Write;
        let mode = gix::objs::tree::EntryMode::from(gix::objs::tree::EntryKind::Commit);
        let target = Write::write(
            store.objects(),
            &gix::objs::Blob {
                data: b"c".to_vec(),
            },
        )
        .expect("write blob");
        Write::write(
            store.objects(),
            &gix::objs::Tree {
                entries: vec![gix::objs::tree::Entry {
                    mode,
                    filename: filename.into(),
                    oid: target,
                }],
            },
        )
        .expect("write tree")
    }

    /// The end-to-end shape of the defect: a hand-built index tree whose path
    /// entry is non-canonical must be rejected by the tree reader — which is
    /// what makes `read_validated` fall back to the ref listing — rather than
    /// silently decoded.
    #[test]
    fn read_tree_rejects_uppercase_hex_entry() {
        let store = store();
        assert!(
            read_tree(&store, tree_with(&store, "p00000002c3a9")).is_ok(),
            "a canonical entry must decode",
        );
        assert!(
            read_tree(&store, tree_with(&store, "p00000002C3A9")).is_err(),
            "a non-canonical entry must be refused",
        );
    }
}
