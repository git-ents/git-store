//! The database snapshot: a canonical Git tree of table roots plus one
//! reserved metadata blob.

use std::collections::BTreeMap;

use git_prolly::ProllyConfig;
use gix::ObjectId;
use gix::bstr::ByteSlice;
use gix::objs::tree::{Entry as TreeEntry, EntryKind, EntryMode};
use gix::objs::{Kind, Tree, Write as _};

use crate::error::{SnapshotError, TableNameError};
use crate::format::{METADATA_NAME, SNAPSHOT_FORMAT_LINE, TableName, encode_config, parse_config};

/// A database snapshot: the table-name-to-root map of one moment.
///
/// Two snapshots with the same tables and the same configuration always write
/// to the same tree object, so snapshot identity is object identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    config: ProllyConfig,
    tables: BTreeMap<TableName, ObjectId>,
}

impl Snapshot {
    /// A snapshot with no tables.
    #[must_use]
    pub fn empty(config: ProllyConfig) -> Self {
        Self {
            config,
            tables: BTreeMap::new(),
        }
    }

    /// The Prolly configuration this snapshot's table roots were built with.
    #[must_use]
    pub const fn config(&self) -> &ProllyConfig {
        &self.config
    }

    /// Every table, ascending by name.
    #[must_use]
    pub fn tables(&self) -> &BTreeMap<TableName, ObjectId> {
        &self.tables
    }

    /// One table's root, or `None` when the table does not exist.
    #[must_use]
    pub fn table(&self, name: &str) -> Option<ObjectId> {
        self.tables.get(name).copied()
    }

    /// Set or replace a table's root.
    pub fn set_table(&mut self, name: TableName, root: ObjectId) {
        self.tables.insert(name, root);
    }

    /// Remove a table, returning its previous root when it existed.
    pub fn remove_table(&mut self, name: &str) -> Option<ObjectId> {
        self.tables.remove(name)
    }

    /// The number of tables.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tables.len()
    }

    /// Whether the snapshot has no tables.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    /// Read a snapshot tree.
    ///
    /// When `expected` is given, a snapshot pinned to a different
    /// [git_prolly::ProllyConfig] is rejected rather than silently mixed with trees
    /// built under another configuration.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError`] for any tree that is not a readable
    /// snapshot: missing or malformed metadata, invalid table names, wrong
    /// object kinds, or absent objects.
    pub fn read(
        repo: &gix::Repository,
        tree: ObjectId,
        expected: Option<ProllyConfig>,
    ) -> Result<Self, SnapshotError> {
        let empty = ObjectId::empty_tree(repo.object_hash());
        if tree == empty {
            return Err(SnapshotError::MetadataMissing {
                tree,
                name: METADATA_NAME,
            });
        }
        let entries = read_tree(repo, tree)?;
        let metadata = entries
            .iter()
            .find(|entry| entry.filename == METADATA_NAME)
            .ok_or(SnapshotError::MetadataMissing {
                tree,
                name: METADATA_NAME,
            })?;
        if metadata.mode.kind() != EntryKind::Blob {
            return Err(SnapshotError::MetadataNotBlob {
                tree,
                name: METADATA_NAME,
                kind: kind_name_of(metadata.mode),
            });
        }
        let config = parse_metadata(repo, metadata.oid)?;
        if let Some(expected) = expected
            && expected != config
        {
            return Err(SnapshotError::ConfigMismatch {
                found: crate::error::config_summary(&config),
                expected: crate::error::config_summary(&expected),
            });
        }
        let mut tables = BTreeMap::new();
        for entry in entries {
            if entry.filename == METADATA_NAME {
                continue;
            }
            let name = match entry.filename.to_str() {
                Ok(name) => TableName::new(name),
                Err(_) => Err(TableNameError::not_utf8()),
            }
            .map_err(|source| SnapshotError::TableName { tree, source })?;
            if entry.mode.kind() != EntryKind::Tree {
                return Err(SnapshotError::TableNotTree {
                    tree,
                    table: name.to_string(),
                    kind: kind_name_of(entry.mode),
                });
            }
            let header = repo
                .try_find_header(entry.oid)
                .map_err(SnapshotError::git)?
                .ok_or(SnapshotError::ObjectNotFound { oid: entry.oid })?;
            if header.kind() != Kind::Tree {
                return Err(SnapshotError::TableNotTree {
                    tree,
                    table: name.to_string(),
                    kind: kind_name_of_tree_kind(header.kind()),
                });
            }
            tables.insert(name, entry.oid);
        }
        Ok(Self { config, tables })
    }

    /// Write this snapshot as a canonical Git tree and return its id.
    ///
    /// Writing the same logical snapshot twice is a no-op at the object
    /// layer; the returned id is a pure function of the tables and the
    /// configuration.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError`] when a table root is absent or not a tree,
    /// or when the metadata blob or snapshot tree cannot be written.
    pub fn write(&self, repo: &gix::Repository) -> Result<ObjectId, SnapshotError> {
        self.config
            .validate()
            .map_err(|error| SnapshotError::InvalidConfig(error.to_string()))?;
        let metadata = repo
            .write_buf(Kind::Blob, &metadata_bytes(self.config))
            .map_err(SnapshotError::git)?;
        let mut entries = Vec::with_capacity(self.tables.len() + 1);
        entries.push(TreeEntry {
            mode: EntryMode::from(EntryKind::Blob),
            filename: METADATA_NAME.into(),
            oid: metadata,
        });
        for (name, root) in &self.tables {
            let empty = ObjectId::empty_tree(repo.object_hash());
            if *root != empty {
                let header = repo
                    .try_find_header(*root)
                    .map_err(SnapshotError::git)?
                    .ok_or(SnapshotError::ObjectNotFound { oid: *root })?;
                if header.kind() != Kind::Tree {
                    return Err(SnapshotError::TableNotTree {
                        tree: *root,
                        table: name.to_string(),
                        kind: kind_name_of_tree_kind(header.kind()),
                    });
                }
            }
            entries.push(TreeEntry {
                mode: EntryMode::from(EntryKind::Tree),
                filename: name.as_str().into(),
                oid: *root,
            });
        }
        entries.sort_unstable();
        repo.write_object(&Tree { entries })
            .map(|id| id.detach())
            .map_err(SnapshotError::git)
    }
}

/// The metadata blob's bytes: the format line, then the configuration line.
#[must_use]
pub fn metadata_bytes(config: ProllyConfig) -> Vec<u8> {
    let mut bytes = SNAPSHOT_FORMAT_LINE.to_vec();
    bytes.push(b'\n');
    bytes.extend_from_slice(encode_config(config).as_bytes());
    bytes.push(b'\n');
    bytes
}

fn parse_metadata(repo: &gix::Repository, blob: ObjectId) -> Result<ProllyConfig, SnapshotError> {
    let data = repo
        .find_blob(blob)
        .map_err(SnapshotError::git)?
        .data
        .to_vec();
    let mut lines = data.split(|byte| *byte == b'\n');
    let format = lines
        .next()
        .ok_or_else(|| SnapshotError::UnknownFormat(data.clone()))?;
    if format == b"git-store-database v1" {
        return Err(SnapshotError::LegacyFormat);
    }
    if format != SNAPSHOT_FORMAT_LINE {
        return Err(SnapshotError::UnknownFormat(format.to_vec()));
    }
    let config_line = lines.next().ok_or_else(|| {
        SnapshotError::MalformedMetadata(String::from_utf8_lossy(&data).into_owned())
    })?;
    let config_line = std::str::from_utf8(config_line).map_err(|_| {
        SnapshotError::MalformedMetadata(String::from_utf8_lossy(&data).into_owned())
    })?;
    if config_line.is_empty() {
        return Err(SnapshotError::MalformedMetadata(
            String::from_utf8_lossy(&data).into_owned(),
        ));
    }
    let config = parse_config(config_line)?;
    if lines.next().is_some_and(|rest| !rest.is_empty()) {
        return Err(SnapshotError::MalformedMetadata(
            String::from_utf8_lossy(&data).into_owned(),
        ));
    }
    Ok(config)
}

/// Decode a tree's entries, failing when absent or not a tree.
pub(crate) fn read_tree(
    repo: &gix::Repository,
    oid: ObjectId,
) -> Result<Vec<TreeEntry>, SnapshotError> {
    let tree = repo
        .find_tree(oid)
        .map_err(|error| match tree_absent(repo, oid) {
            Some(absent) => absent,
            None => SnapshotError::git(error),
        })?;
    let decoded = tree.decode().map_err(SnapshotError::git)?;
    Ok(decoded
        .entries
        .iter()
        .map(|entry| TreeEntry {
            mode: entry.mode,
            filename: entry.filename.into(),
            oid: entry.oid.to_owned(),
        })
        .collect())
}

/// Distinguish "the object is gone" from "the object is not a tree".
fn tree_absent(repo: &gix::Repository, oid: ObjectId) -> Option<SnapshotError> {
    repo.try_find_header(oid)
        .ok()
        .flatten()
        .is_none()
        .then_some(SnapshotError::ObjectNotFound { oid })
}

fn kind_name_of(mode: EntryMode) -> &'static str {
    match mode.kind() {
        EntryKind::Tree => "tree",
        EntryKind::Blob | EntryKind::BlobExecutable => "blob",
        EntryKind::Commit => "commit",
        EntryKind::Link => "symlink",
    }
}

fn kind_name_of_tree_kind(kind: Kind) -> &'static str {
    match kind {
        Kind::Tree => "tree",
        Kind::Blob => "blob",
        Kind::Commit => "commit",
        Kind::Tag => "tag",
    }
}
