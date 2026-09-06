//! Snapshot format tests: round-trip, convergence, Git inspectability, and
//! rejection of malformed or foreign trees.

use facet_value::Value;
use git_prolly::{ProllyConfig, ProllyStore};
use gix::ObjectId;
use gix::objs::tree::{Entry as TreeEntry, EntryKind, EntryMode};
use gix::objs::{Kind, Tree, Write as _};
use gix_database::{METADATA_NAME, Snapshot};

fn repo() -> (tempfile::TempDir, gix::Repository) {
    let dir = tempfile::TempDir::new().expect("temp dir");
    test_support::init_repo(dir.path());
    let repo = gix::open(dir.path()).expect("open repo");
    (dir, repo)
}

fn repo_with_hash(dir: &tempfile::TempDir, hash: gix::hash::Kind) -> gix::Repository {
    let options = gix::create::Options {
        object_hash: Some(hash),
        ..Default::default()
    };
    gix::ThreadSafeRepository::init(dir.path(), gix::create::Kind::WithWorktree, options)
        .expect("init repo")
        .to_thread_local()
}

/// A snapshot with one table holding one row, written twice.
fn populated(store: &ProllyStore<'_>) -> (Snapshot, ObjectId) {
    let root = store
        .insert(None, b"alice", &Value::from(1))
        .expect("insert");
    let mut snapshot = Snapshot::empty(ProllyConfig::default());
    snapshot.set_table(gix_database::TableName::new("users").expect("valid"), root);
    let tree = snapshot.write(store.repo()).expect("write");
    (snapshot, tree)
}

fn write_tree(repo: &gix::Repository, entries: Vec<TreeEntry>) -> ObjectId {
    repo.write_object(&Tree { entries })
        .expect("write tree")
        .detach()
}

fn blob(repo: &gix::Repository, data: &[u8]) -> ObjectId {
    repo.write_buf(Kind::Blob, data).expect("write blob")
}

#[test]
fn round_trip_preserves_table_roots() {
    let (_dir, repo) = repo();
    let store = ProllyStore::open(&repo);
    let (snapshot, tree) = populated(&store);
    let read = Snapshot::read(&repo, tree, Some(ProllyConfig::default())).expect("read");
    assert_eq!(&read, &snapshot);
    assert_eq!(read.len(), 1);
}

#[test]
fn identical_snapshots_converge_to_one_object() {
    let (_dir, repo) = repo();
    let store = ProllyStore::open(&repo);
    let (_, first) = populated(&store);
    // A second, independently built snapshot of the same logical contents.
    let (_, second) = populated(&store);
    assert_eq!(first, second);
    // Rewriting the same snapshot is a no-op at the object layer.
    let read = Snapshot::read(&repo, first, Some(ProllyConfig::default())).expect("read");
    assert_eq!(read.write(&repo).expect("rewrite"), first);
}

#[test]
fn unchanged_table_roots_keep_their_object_ids() {
    let (_dir, repo) = repo();
    let store = ProllyStore::open(&repo);
    let users = store
        .insert(None, b"alice", &Value::from(1))
        .expect("insert");
    let mut before = Snapshot::empty(ProllyConfig::default());
    before.set_table(gix_database::TableName::new("users").expect("valid"), users);
    before.set_table(
        gix_database::TableName::new("items").expect("valid"),
        store.empty_root(),
    );
    let first_tree = before.write(&repo).expect("write");

    let mut after = before.clone();
    let users2 = store
        .insert(Some(users), b"bob", &Value::from(2))
        .expect("insert");
    after.set_table(
        gix_database::TableName::new("users").expect("valid"),
        users2,
    );
    let second_tree = after.write(&repo).expect("write");

    let older = Snapshot::read(&repo, first_tree, None).expect("read older");
    let newer = Snapshot::read(&repo, second_tree, None).expect("read newer");
    assert_eq!(older.table("items"), newer.table("items"));
    assert_eq!(older.table("users"), Some(users));
    assert_eq!(newer.table("users"), Some(users2));
}

#[test]
fn snapshot_is_inspectable_with_git_plumbing() {
    let (_dir, repo) = repo();
    let store = ProllyStore::open(&repo);
    let (_, tree) = populated(&store);
    let tree_obj = repo.find_tree(tree).expect("tree");
    let decoded = tree_obj.decode().expect("decode");
    let metadata = decoded
        .entries
        .iter()
        .find(|entry| entry.filename == METADATA_NAME)
        .expect("metadata entry");
    assert_eq!(metadata.mode, EntryMode::from(EntryKind::Blob));
    let blob = repo.find_blob(metadata.oid).expect("metadata blob");
    let text = std::str::from_utf8(&blob.data).expect("utf-8 metadata");
    assert_eq!(
        text.lines().next(),
        Some("git-store-database v2"),
        "the format line is plain text, readable with git cat-file"
    );
}

#[test]
fn empty_database_is_a_metadata_only_tree() {
    let (_dir, repo) = repo();
    let snapshot = Snapshot::empty(ProllyConfig::default());
    let tree = snapshot.write(&repo).expect("write");
    let tree_obj = repo.find_tree(tree).expect("tree");
    let decoded = tree_obj.decode().expect("decode");
    assert_eq!(decoded.entries.len(), 1);
    assert_eq!(decoded.entries[0].filename, METADATA_NAME);
    let read = Snapshot::read(&repo, tree, None).expect("read");
    assert!(read.is_empty());
}

#[test]
fn rejects_trees_without_metadata() {
    let (_dir, repo) = repo();
    let bare = write_tree(
        &repo,
        vec![TreeEntry {
            mode: EntryMode::from(EntryKind::Blob),
            filename: "x".into(),
            oid: blob(&repo, b"content"),
        }],
    );
    match Snapshot::read(&repo, bare, None) {
        Err(gix_database::SnapshotError::MetadataMissing { tree, name }) => {
            assert_eq!(tree, bare);
            assert_eq!(name, METADATA_NAME);
        }
        other => panic!("expected MetadataMissing, got {other:?}"),
    }
}

#[test]
fn rejects_metadata_that_is_not_a_blob() {
    let (_dir, repo) = repo();
    let inner = write_tree(&repo, vec![]);
    let metadata = write_tree(&repo, vec![]);
    let tree = write_tree(
        &repo,
        vec![
            TreeEntry {
                mode: EntryMode::from(EntryKind::Tree),
                filename: METADATA_NAME.into(),
                oid: metadata,
            },
            TreeEntry {
                mode: EntryMode::from(EntryKind::Tree),
                filename: "users".into(),
                oid: inner,
            },
        ],
    );
    match Snapshot::read(&repo, tree, None) {
        Err(gix_database::SnapshotError::MetadataNotBlob { name, kind, .. }) => {
            assert_eq!(name, METADATA_NAME);
            assert_eq!(kind, "tree");
        }
        other => panic!("expected MetadataNotBlob, got {other:?}"),
    }
}

#[test]
fn rejects_foreign_format_lines() {
    let (_dir, repo) = repo();
    let metadata = blob(
        &repo,
        b"other-db v1\nprolly bits=4 min_entries=4 max_entries=64 key_codec=hex\n",
    );
    let tree = write_tree(
        &repo,
        vec![TreeEntry {
            mode: EntryMode::from(EntryKind::Blob),
            filename: METADATA_NAME.into(),
            oid: metadata,
        }],
    );
    match Snapshot::read(&repo, tree, None) {
        Err(gix_database::SnapshotError::UnknownFormat(line)) => {
            assert_eq!(line, b"other-db v1");
        }
        other => panic!("expected UnknownFormat, got {other:?}"),
    }
}

#[test]
fn rejects_tables_that_are_not_trees() {
    let (_dir, repo) = repo();
    let metadata = blob(
        &repo,
        gix_database::metadata_bytes(ProllyConfig::default()).as_slice(),
    );
    let tree = write_tree(
        &repo,
        vec![
            TreeEntry {
                mode: EntryMode::from(EntryKind::Blob),
                filename: METADATA_NAME.into(),
                oid: metadata,
            },
            TreeEntry {
                mode: EntryMode::from(EntryKind::Blob),
                filename: "users".into(),
                oid: blob(&repo, b"not a table"),
            },
        ],
    );
    match Snapshot::read(&repo, tree, None) {
        Err(gix_database::SnapshotError::TableNotTree { table, kind, .. }) => {
            assert_eq!(table, "users");
            assert_eq!(kind, "blob");
        }
        other => panic!("expected TableNotTree, got {other:?}"),
    }
}

#[test]
fn rejects_reserved_table_names() {
    let (_dir, repo) = repo();
    let metadata = blob(
        &repo,
        gix_database::metadata_bytes(ProllyConfig::default()).as_slice(),
    );
    for name in ["!sneaky", ".."] {
        let tree = write_tree(
            &repo,
            vec![
                TreeEntry {
                    mode: EntryMode::from(EntryKind::Blob),
                    filename: METADATA_NAME.into(),
                    oid: metadata,
                },
                TreeEntry {
                    mode: EntryMode::from(EntryKind::Tree),
                    filename: name.into(),
                    oid: repo
                        .find_tree(
                            gix_database::Snapshot::empty(ProllyConfig::default())
                                .write(&repo)
                                .expect("empty tree"),
                        )
                        .expect("tree")
                        .id()
                        .detach(),
                },
            ],
        );
        match Snapshot::read(&repo, tree, None) {
            Err(gix_database::SnapshotError::TableName { source, .. }) => {
                assert!(
                    source.to_string().contains("reserved"),
                    "expected a reserved-name rejection for {name:?}, got {source}"
                );
            }
            other => panic!("expected TableName rejection for {name:?}, got {other:?}"),
        }
    }
}

#[test]
fn rejects_absent_table_roots() {
    let (_dir, repo) = repo();
    let metadata = blob(
        &repo,
        gix_database::metadata_bytes(ProllyConfig::default()).as_slice(),
    );
    let dangling: ObjectId = "0"
        .repeat(repo.object_hash().len_in_bytes() * 2)
        .parse()
        .expect("zero oid");
    let tree = write_tree(
        &repo,
        vec![
            TreeEntry {
                mode: EntryMode::from(EntryKind::Blob),
                filename: METADATA_NAME.into(),
                oid: metadata,
            },
            TreeEntry {
                mode: EntryMode::from(EntryKind::Tree),
                filename: "users".into(),
                oid: dangling,
            },
        ],
    );
    match Snapshot::read(&repo, tree, None) {
        Err(gix_database::SnapshotError::ObjectNotFound { oid }) => assert_eq!(oid, dangling),
        other => panic!("expected ObjectNotFound, got {other:?}"),
    }
}

#[test]
fn rejects_mismatched_prolly_configuration() {
    let (_dir, repo) = repo();
    let store = ProllyStore::open(&repo);
    let (_, tree) = populated(&store);
    let other = ProllyConfig {
        bits: 5,
        ..ProllyConfig::default()
    };
    match Snapshot::read(&repo, tree, Some(other)) {
        Err(gix_database::SnapshotError::ConfigMismatch { .. }) => {}
        other => panic!("expected ConfigMismatch, got {other:?}"),
    }
    Snapshot::read(&repo, tree, Some(ProllyConfig::default())).expect("matching config reads");
}

#[test]
fn write_rejects_table_roots_that_are_not_trees() {
    let (_dir, repo) = repo();
    let mut snapshot = Snapshot::empty(ProllyConfig::default());
    snapshot.set_table(
        gix_database::TableName::new("users").expect("valid"),
        blob(&repo, b"scalar"),
    );
    match snapshot.write(&repo) {
        Err(gix_database::SnapshotError::TableNotTree { .. }) => {}
        other => panic!("expected TableNotTree, got {other:?}"),
    }
}

#[test]
fn table_name_rules() {
    for name in ["users", "user_events", "Ünicode", "0"] {
        gix_database::TableName::new(name).expect("valid name");
    }
    for name in ["", "!meta", ".", "..", "a/b", "a b", "a\tb"] {
        gix_database::TableName::new(name).expect_err("invalid name");
    }
    gix_database::TableName::new(&"x".repeat(256)).expect_err("too long");
}

#[test]
fn sha256_repositories_round_trip() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let repo = repo_with_hash(&dir, gix::hash::Kind::Sha256);
    assert_eq!(repo.object_hash(), gix::hash::Kind::Sha256);
    let store = ProllyStore::open(&repo);
    let (snapshot, tree) = populated(&store);
    assert_eq!(tree.to_string().len(), 64);
    let read = Snapshot::read(&repo, tree, Some(ProllyConfig::default())).expect("read");
    assert_eq!(read, snapshot);
}

#[test]
fn metadata_round_trips_every_config_field() {
    let config = ProllyConfig {
        bits: 6,
        min_entries: 3,
        max_entries: 128,
        key_codec: git_prolly::KeyCodecKind::Hex,
    };
    let (_dir, repo) = repo();
    let snapshot = Snapshot::empty(config);
    let tree = snapshot.write(&repo).expect("write");
    let read = Snapshot::read(&repo, tree, Some(config)).expect("read");
    assert_eq!(read.config(), &config);
}

#[test]
fn tables_are_sorted_and_complete() {
    let (_dir, repo) = repo();
    let store = ProllyStore::open(&repo);
    let mut snapshot = Snapshot::empty(ProllyConfig::default());
    for name in ["zeta", "alpha", "mid"] {
        snapshot.set_table(
            gix_database::TableName::new(name).expect("valid"),
            store.empty_root(),
        );
    }
    let tree = snapshot.write(&repo).expect("write");
    let read = Snapshot::read(&repo, tree, None).expect("read");
    let names: Vec<&str> = read
        .tables()
        .keys()
        .map(gix_database::TableName::as_str)
        .collect();
    assert_eq!(names, vec!["alpha", "mid", "zeta"]);
}
