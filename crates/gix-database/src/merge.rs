//! Three-way snapshot merge: exact key/value behavior, no value decoding.
//!
//! For every table, each side is diffed against the merge base as Prolly row
//! changes; rows changed on one side only are taken, rows changed on both
//! sides must agree or the merge is refused with explicit conflicts. Tables
//! dropped on one side and changed on the other conflict at the table level.
//! A refused merge writes nothing.

use std::collections::{BTreeMap, BTreeSet};

use git_prolly::{DiffEntry, ProllyStore};
use gix::ObjectId;

use crate::error::{ConflictEntry, ConflictKind};
use crate::format::TableName;
use crate::snapshot::Snapshot;

/// The outcome of comparing base, ours, and theirs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merged {
    /// The merged snapshot; identical to `ours` when conflicts remain, in
    /// which case it must not be written.
    pub snapshot: Snapshot,
    /// Every conflicting table entry, in table then key order. Empty when
    /// the merge is clean.
    pub conflicts: Vec<ConflictEntry>,
}

/// Merge `theirs` into `ours` against `base`, row by row.
///
/// # Errors
///
/// Returns [`git_prolly::Error`] when a table root or node cannot be read.
pub fn merge_snapshots(
    store: &ProllyStore<'_>,
    base: &Snapshot,
    ours: &Snapshot,
    theirs: &Snapshot,
) -> Result<Merged, git_prolly::Error> {
    let empty = store.empty_root();
    let mut merged = ours.clone();
    let mut conflicts = Vec::new();

    let names: BTreeSet<&str> = base
        .tables()
        .keys()
        .chain(ours.tables().keys())
        .chain(theirs.tables().keys())
        .map(TableName::as_str)
        .collect();
    for name in names {
        let table = TableName::new(name).expect("names from snapshots are valid");
        let b = base.table(name);
        let o = ours.table(name);
        let t = theirs.table(name);
        if o == t {
            continue;
        }
        let base_root = b.unwrap_or(empty);
        let ours_root = o.unwrap_or(empty);
        let theirs_root = t.unwrap_or(empty);
        let ours_changes = change_map(store, base_root, ours_root)?;
        let theirs_changes = change_map(store, base_root, theirs_root)?;
        let ours_dropped = b.is_some() && o.is_none();
        let theirs_dropped = b.is_some() && t.is_none();

        if ours_dropped != theirs_dropped {
            let (survivor_changes, ours_survives) = if ours_dropped {
                (&theirs_changes, false)
            } else {
                (&ours_changes, true)
            };
            for (key, value) in survivor_changes {
                let Some(value) = value else { continue };
                let kind = if ours_survives {
                    ConflictKind::TheirsDeleted { ours: *value }
                } else {
                    ConflictKind::OursDeleted { theirs: *value }
                };
                conflicts.push(ConflictEntry {
                    table: table.clone(),
                    key: key.clone(),
                    kind,
                });
            }
            if survivor_changes.is_empty() {
                merged.remove_table(name);
            }
            continue;
        }

        let root = merge_rows(
            store,
            ours_root,
            &ours_changes,
            &theirs_changes,
            &mut conflicts,
            &table,
        )?;
        merged.set_table(table, root);
    }
    conflicts.sort();
    Ok(Merged {
        snapshot: merged,
        conflicts,
    })
}

/// Diff one side against the base as a key → new-value-object map. A key
/// mapping to `None` was deleted; a missing key was untouched.
fn change_map(
    store: &ProllyStore<'_>,
    base_root: ObjectId,
    side_root: ObjectId,
) -> Result<BTreeMap<Vec<u8>, Option<ObjectId>>, git_prolly::Error> {
    let mut map = BTreeMap::new();
    for entry in store.diff(base_root, side_root)? {
        match entry {
            DiffEntry::Insert { key, new } => {
                map.insert(key, Some(new));
            }
            DiffEntry::Delete { key, .. } => {
                map.insert(key, None);
            }
            DiffEntry::Modify { key, new, .. } => {
                map.insert(key, Some(new));
            }
        }
    }
    Ok(map)
}

/// Reconcile the two sides' changes to one table, starting from `ours_root`.
///
/// Rows changed on one side only are applied; rows changed on both sides must
/// agree. Every disagreement is recorded in `conflicts` and the merge is
/// refused by the caller; the returned root is then meaningless.
fn merge_rows(
    store: &ProllyStore<'_>,
    ours_root: ObjectId,
    ours_changes: &BTreeMap<Vec<u8>, Option<ObjectId>>,
    theirs_changes: &BTreeMap<Vec<u8>, Option<ObjectId>>,
    conflicts: &mut Vec<ConflictEntry>,
    table: &TableName,
) -> Result<ObjectId, git_prolly::Error> {
    let mut root = ours_root;
    let keys: BTreeSet<&Vec<u8>> = ours_changes.keys().chain(theirs_changes.keys()).collect();
    for key in keys {
        let ours_changed = ours_changes.contains_key(key);
        let theirs_changed = theirs_changes.contains_key(key);
        let theirs_new = theirs_changes.get(key).copied().flatten();
        match (ours_changed, theirs_changed) {
            (false, false) => continue,
            // Only ours touched the key, and `root` already holds ours.
            (true, false) => {}
            // Only theirs touched the key: take their action.
            (false, true) => {
                if let Some(new) = theirs_new {
                    root = store.insert_value_object(Some(root), key, new)?;
                } else {
                    root = store.remove(root, key)?;
                }
            }
            (true, true) => {
                let ours_new = ours_changes.get(key).copied().flatten();
                if ours_new == theirs_new {
                    continue;
                }
                let kind = match (ours_new, theirs_new) {
                    (None, Some(theirs)) => ConflictKind::OursDeleted { theirs },
                    (Some(ours), None) => ConflictKind::TheirsDeleted { ours },
                    (Some(ours), Some(theirs)) => ConflictKind::DifferentValues { ours, theirs },
                    (None, None) => continue,
                };
                conflicts.push(ConflictEntry {
                    table: table.clone(),
                    key: key.clone(),
                    kind,
                });
            }
        }
    }
    Ok(root)
}
