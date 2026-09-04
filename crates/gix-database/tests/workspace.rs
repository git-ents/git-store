//! Workspace tests: working/index/branch state, CAS conflicts, commits,
//! branches, and checkout.

use gix::ObjectId;
use gix::objs::{CommitRef, Find as _, Kind};
use gix_database::{ChangeKind, Database, Error, HEAD_REF, Head, INDEX_REF, WORKING_REF};
use gix_refstore::{RefName, RefStore as _};

fn repo() -> (tempfile::TempDir, gix::Repository) {
    let dir = tempfile::TempDir::new().expect("temp dir");
    test_support::init_repo(dir.path());
    let repo = gix::open(dir.path()).expect("open repo");
    (dir, repo)
}

fn db(repo: &gix::Repository) -> Database<'_> {
    Database::open(repo)
}

fn working_ref() -> RefName {
    RefName::new(WORKING_REF).expect("built-in ref name is valid")
}

fn index_ref() -> RefName {
    RefName::new(INDEX_REF).expect("built-in ref name is valid")
}

fn read_ref(repo: &gix::Repository, name: &RefName) -> Option<ObjectId> {
    GixRefStoreShim::read(repo, name)
}

struct GixRefStoreShim;

impl GixRefStoreShim {
    fn read(repo: &gix::Repository, name: &RefName) -> Option<ObjectId> {
        use gix_refstore::RefStore;
        gix_refstore::GixRefStore::new(repo)
            .read(name)
            .expect("read ref")
    }
}

#[test]
fn init_is_idempotent_and_head_starts_unborn() {
    let (_dir, repo) = repo();
    let database = db(&repo);
    assert_eq!(database.head().expect("head"), Head::Missing);
    database.init().expect("init");
    database.init().expect("init again is a no-op");
    match database.head().expect("head") {
        Head::Unborn { branch } => {
            assert_eq!(branch.as_str(), "refs/db/heads/main");
        }
        other => panic!("expected unborn head, got {other:?}"),
    }
}

#[test]
fn table_lifecycle_round_trips_values() {
    let (_dir, repo) = repo();
    let database = db(&repo);
    database.init().expect("init");
    database.create_table("users").expect("create");
    database
        .create_table("users")
        .expect_err("duplicate create is rejected");
    database.put("users", b"alice", &"42".into()).expect("put");
    database.put("users", b"bob", &"hello".into()).expect("put");
    assert_eq!(
        database.get("users", b"alice").expect("get"),
        Some("42".into())
    );
    let scanned: Vec<_> = database
        .scan("users")
        .expect("scan")
        .map(|row| row.expect("row"))
        .collect();
    assert_eq!(scanned.len(), 2);
    assert_eq!(scanned[0].0, b"alice".to_vec());
    database.remove("users", b"alice").expect("remove");
    assert_eq!(database.get("users", b"alice").expect("get"), None);
    database.drop_table("users").expect("drop");
    assert!(matches!(
        database.get("users", b"bob"),
        Err(Error::TableNotFound(_))
    ));
}

#[test]
fn put_value_object_references_an_existing_object_without_reencoding() {
    let (_dir, repo) = repo();
    let database = db(&repo);
    database.init().expect("init");
    database.create_table("source").expect("create source");
    database
        .put("source", b"k", &"payload".into())
        .expect("put");
    let root = database.table_root("source").expect("root");
    let value_oid = database
        .store()
        .get_oid(root, b"k")
        .expect("oid lookup")
        .expect("value present");
    database.create_table("copy").expect("create copy");
    database
        .put_value_object("copy", b"k", value_oid)
        .expect("oid-in put");
    assert_eq!(
        database.get("copy", b"k").expect("get"),
        Some("payload".into())
    );
}

#[test]
fn status_and_diff_track_staging() {
    let (_dir, repo) = repo();
    let database = db(&repo);
    database.init().expect("init");
    database.create_table("users").expect("create");
    database.put("users", b"alice", &"one".into()).expect("put");

    let status = database.status().expect("status");
    assert_eq!(status.staged.len(), 0, "nothing staged yet");
    assert_eq!(
        status.unstaged.len(),
        1,
        "an untracked table is unstaged work"
    );
    assert_eq!(status.unstaged[0].kind, ChangeKind::Added);
    assert_eq!(status.unstaged[0].rows.len(), 1);

    database.stage("users").expect("stage");
    let status = database.status().expect("status");
    assert_eq!(status.staged.len(), 1);
    assert_eq!(status.unstaged.len(), 0);

    database.put("users", b"bob", &"two".into()).expect("put");
    let status = database.status().expect("status");
    assert_eq!(status.staged.len(), 1);
    assert_eq!(status.unstaged.len(), 1, "the second row is unstaged");
    assert_eq!(status.unstaged[0].rows.len(), 1);
    assert_eq!(status.unstaged[0].rows[0].key, b"bob".to_vec());

    let diff = database.diff_working().expect("diff");
    assert_eq!(diff.len(), 1);
    assert_eq!(diff[0].rows[0].verb(), "added");
}

#[test]
fn commit_advances_the_branch_and_resets_workspace_state() {
    let (_dir, repo) = repo();
    let database = db(&repo);
    database.init().expect("init");
    database.create_table("users").expect("create");
    database.put("users", b"alice", &"one".into()).expect("put");
    database.stage("users").expect("stage");

    let commit = database.commit("seed users").expect("commit");
    let head = database.head().expect("head");
    assert_eq!(head.commit(), Some(commit));
    let status = database.status().expect("status");
    assert!(status.is_clean(), "commit resets working and index");
    let log = database.log().expect("log");
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].commit, commit);
    assert_eq!(log[0].message, "seed users");
    assert_eq!(log[0].parents.len(), 0, "the first commit has no parents");

    // A second commit parents onto the first.
    database.put("users", b"bob", &"two".into()).expect("put");
    database.stage("users").expect("stage");
    let second = database.commit("add bob").expect("commit");
    let log = database.log().expect("log");
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].parents, vec![commit]);
    assert_eq!(second, log[0].commit);
}

#[test]
fn a_rival_working_write_is_not_silently_overwritten() {
    let (_dir, repo) = repo();
    let database = db(&repo);
    database.init().expect("init");
    database.create_table("users").expect("create");
    database.put("users", b"alice", &"one".into()).expect("put");

    // Another writer replaces the working snapshot entirely.
    let stale = read_ref(&repo, &working_ref()).expect("working ref");
    let mut rival = database.working_state().expect("state").snapshot;
    rival.set_table(
        gix_database::TableName::new("users").expect("valid"),
        database.store().empty_root(),
    );
    let tree = rival.write(&repo).expect("tree");
    let rival_commit = repo
        .write_object(&gix::objs::Commit {
            tree,
            parents: Vec::new().into(),
            author: repo
                .committer()
                .expect("committer")
                .expect("signature")
                .into(),
            committer: repo
                .committer()
                .expect("committer")
                .expect("signature")
                .into(),
            encoding: None,
            message: "rival writer".into(),
            extra_headers: vec![],
        })
        .expect("commit")
        .detach();
    gix_refstore::GixRefStore::new(&repo)
        .apply(gix_refstore::RefEdit::Update {
            name: working_ref(),
            expected: stale,
            new: rival_commit,
        })
        .expect("rival write");

    // Our next write reads fresh state, so it lands on the rival's snapshot
    // rather than silently restoring our stale view of the table.
    database.put("users", b"bob", &"two".into()).expect("put");
    let state = database.working_state().expect("state");
    assert_ne!(
        state.commit,
        Some(rival_commit),
        "the working ref advanced past the rival commit"
    );
    assert!(
        state.snapshot.table("users").is_some(),
        "our put built on the rival's snapshot"
    );
    assert_eq!(database.get("users", b"alice").expect("get"), None);
    assert_eq!(
        database.get("users", b"bob").expect("get"),
        Some("two".into())
    );
}

#[test]
fn a_deleted_index_ref_falls_back_to_the_tip() {
    let (_dir, repo) = repo();
    let database = db(&repo);
    database.init().expect("init");
    database.create_table("users").expect("create");
    database.put("users", b"alice", &"one".into()).expect("put");
    database.stage("users").expect("stage");
    let index_before = read_ref(&repo, &index_ref()).expect("index ref");

    gix_refstore::GixRefStore::new(&repo)
        .apply(gix_refstore::RefEdit::Delete {
            name: index_ref(),
            expected: index_before,
        })
        .expect("rival delete");

    let state = database.index_state().expect("index state");
    assert!(
        state.commit.is_none(),
        "a deleted index ref reads as absent"
    );
    assert!(
        state.snapshot.table("users").is_none(),
        "the fallback is the tip, not the deleted staging"
    );
}

#[test]
fn branches_and_checkout() {
    let (_dir, repo) = repo();
    let database = db(&repo);
    database.init().expect("init");
    database.create_table("users").expect("create");
    database.put("users", b"alice", &"one".into()).expect("put");
    database.stage("users").expect("stage");
    database.commit("base").expect("commit");

    database.create_branch("feature").expect("branch");
    database
        .create_branch("feature")
        .expect_err("duplicate branch is rejected");
    database.checkout("feature", false).expect("checkout");

    // Diverge: feature adds a row, main stays put.
    database
        .put("users", b"carol", &"three".into())
        .expect("put");
    database.stage("users").expect("stage");
    let feature_tip = database.commit("add carol").expect("commit");

    database.checkout("main", false).expect("back to main");
    assert_eq!(
        database.get("users", b"carol").expect("get"),
        None,
        "main does not see feature's rows"
    );
    database
        .checkout("feature", false)
        .expect("back to feature");
    assert_eq!(database.head().expect("head").commit(), Some(feature_tip));
    assert_eq!(
        database.get("users", b"carol").expect("get"),
        Some("three".into())
    );

    // A dirty working snapshot blocks checkout.
    database.put("users", b"dave", &"four".into()).expect("put");
    match database.checkout("main", false) {
        Err(Error::Checkout(gix_database::CheckoutError::Dirty { branch })) => {
            assert_eq!(branch, "main");
        }
        other => panic!("expected a dirty refusal, got {other:?}"),
    }
    database.checkout("main", true).expect("forced checkout");
    assert_eq!(
        database.get("users", b"dave").expect("get"),
        None,
        "force discards the dirty working snapshot"
    );

    match database.checkout("nope", false) {
        Err(Error::Checkout(gix_database::CheckoutError::UnknownBranch(_))) => {}
        other => panic!("expected an unknown-branch refusal, got {other:?}"),
    }
}

#[test]
fn unborn_head_holds_the_empty_database() {
    let (_dir, repo) = repo();
    let database = db(&repo);
    database.init().expect("init");
    let snapshot = database.head_snapshot().expect("head snapshot");
    assert!(snapshot.is_empty());
    assert!(database.log().expect("log").is_empty());
    let status = database.status().expect("status");
    assert!(status.is_clean());
}

#[test]
fn commit_refuses_a_detached_head() {
    let (_dir, repo) = repo();
    let database = db(&repo);
    database.init().expect("init");
    database.create_table("t").expect("create");
    database.put("t", b"k", &"one".into()).expect("put");
    database.stage("t").expect("stage");
    let commit = database.commit("first").expect("commit");

    // Detach refs/db/HEAD at the commit.
    repo.edit_reference(gix::refs::transaction::RefEdit {
        change: gix::refs::transaction::Change::Update {
            log: gix::refs::transaction::LogChange {
                mode: gix::refs::transaction::RefLog::AndReference,
                force_create_reflog: false,
                message: "detach".into(),
            },
            expected: gix::refs::transaction::PreviousValue::Any,
            new: gix::refs::Target::Object(commit),
        },
        name: gix::refs::FullName::try_from(HEAD_REF).expect("valid"),
        deref: false,
    })
    .expect("detach");
    assert!(matches!(
        database.head().expect("head"),
        Head::Detached { .. }
    ));
    assert!(matches!(
        database.commit("x"),
        Err(gix_database::CommitError::Detached)
    ));
}

#[test]
fn snapshots_of_commits_are_readable_and_config_checked() {
    let (_dir, repo) = repo();
    let database = db(&repo);
    database.init().expect("init");
    database.create_table("t").expect("create");
    database.put("t", b"k", &"seven".into()).expect("put");
    database.stage("t").expect("stage");
    let commit = database.commit("c").expect("commit");
    let snapshot = database.snapshot_at(commit).expect("snapshot");
    assert_eq!(snapshot.len(), 1);
    let mut buf = Vec::new();
    let data = repo
        .try_find(&commit, &mut buf)
        .expect("object")
        .expect("commit present");
    assert_eq!(data.kind, Kind::Commit);
    let tree = CommitRef::from_bytes(data.data, data.object_hash)
        .expect("commit")
        .tree();
    let read = gix_database::Snapshot::read(&repo, tree, None)
        .expect("a committed snapshot is an ordinary readable Git tree");
    assert_eq!(read, snapshot);
}
