//! The persisted per-kind materialized entity index.

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

pub(crate) fn read_validated<R, O>(
    store: &Store<R, O>,
    kind: &RefSegment,
    entities: &RefPrefix,
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

    // The index is a cache. Any malformed, non-canonical, or stale object is
    // rejected and the caller uses the entity refs as the source of truth.
    let Ok(entries) = read_tree(store, tree) else {
        return Ok(None);
    };
    if entries == source
        && entries
            .iter()
            .all(|(name, _)| entities.join_path(name).is_under(entities))
    {
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
        .chunks_exact(2)
        .map(|pair| Some(hex_digit(pair[0])? << 4 | hex_digit(pair[1])?))
        .collect()
}

fn invalid_index(message: &'static str) -> Error {
    Error::backend(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message,
    ))
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
