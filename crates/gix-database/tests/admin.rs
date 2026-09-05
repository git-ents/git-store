//! Branch/tag administration and working/index recovery: delete, rename,
//! restore, unstage, hard reset, and table moves.

use gix::ObjectId;
use gix_database::{Database, Error, TAGS_PREFIX, WriteStateError};
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

fn head_branch(repo: &gix::Repository) -> Option<ObjectId> {
    use gix_database::HEAD_REF;
    gix_refstore::GixRefStore::new(repo)
        .read(&RefName::new(HEAD_REF).expect("built-in ref name is valid"))
        .expect("read HEAD")
}

#[test]
fn delete_branch_refuses_the_current_branch() {
    let (_dir, repo) = repo();
    let database = db(&repo);
    database.init().expect("init");
    database.create_table("users").expect("create");
    database.commit("seed").expect("commit");
    database.create_branch("feature").expect("create feature");

    let tip = database.delete_branch("feature").expect("delete feature");
    let branches = database.list_branches().expect("list");
    assert_eq!(branches, vec![("main".to_owned(), tip)]);
    match database.delete_branch("main") {
        Err(Error::CurrentBranch(branch)) => assert_eq!(branch, "main"),
        other => panic!("expected CurrentBranch, got {other:?}"),
    }
    match database.delete_branch("ghost") {
        Err(Error::BranchNotFound(branch)) => assert_eq!(branch, "ghost"),
        other => panic!("expected BranchNotFound, got {other:?}"),
    }
}

#[test]
fn rename_branch_moves_the_tip_and_retargets_head() {
    let (_dir, repo) = repo();
    let database = db(&repo);
    database.init().expect("init");
    database.create_table("users").expect("create");
    let commit = database.commit("seed").expect("commit");

    let tip = database.rename_branch("main", "trunk").expect("rename");
    assert_eq!(tip, commit);
    assert!(database.list_branches().expect("list").len() == 1);
    // HEAD followed the rename: refs/db/HEAD still resolves to the tip.
    assert_eq!(head_branch(&repo), Some(commit));

    database.create_branch("other").expect("create other");
    match database.rename_branch("other", "trunk") {
        Err(Error::BranchExists(branch)) => assert_eq!(branch, "trunk"),
        other => panic!("expected BranchExists, got {other:?}"),
    }
    match database.rename_branch("ghost", "anything") {
        Err(Error::BranchNotFound(branch)) => assert_eq!(branch, "ghost"),
        other => panic!("expected BranchNotFound, got {other:?}"),
    }
}

#[test]
fn rename_branch_without_a_head_follow_leaves_head_alone() {
    let (_dir, repo) = repo();
    let database = db(&repo);
    database.init().expect("init");
    database.commit("seed").expect("commit");
    database.create_branch("feature").expect("create");
    let main = head_branch(&repo);
    database.checkout("feature", false).expect("checkout");
    database.rename_branch("main", "trunk").expect("rename");
    // HEAD still names feature; main's tip moved to trunk.
    let branches = database.list_branches().expect("list");
    assert_eq!(branches.len(), 2);
    assert_eq!(head_branch(&repo), main);
}

#[test]
fn tags_create_list_and_delete() {
    let (_dir, repo) = repo();
    let database = db(&repo);
    database.init().expect("init");
    database.create_table("users").expect("create");
    let commit = database.commit("seed").expect("commit");

    let tagged = database.create_tag("v1", None).expect("tag head");
    assert_eq!(tagged, commit);
    let explicit = database
        .create_tag("seed", Some(commit))
        .expect("explicit tag");
    assert_eq!(explicit, commit);
    assert_eq!(
        database.list_tags().expect("list"),
        vec![("seed".to_owned(), commit), ("v1".to_owned(), commit)]
    );
    match database.create_tag("v1", None) {
        Err(Error::TagExists(tag)) => assert_eq!(tag, "v1"),
        other => panic!("expected TagExists, got {other:?}"),
    }
    assert_eq!(
        database.delete_tag("v1").expect("delete"),
        commit,
        "deleting returns the commit the tag held"
    );
    match database.delete_tag("v1") {
        Err(Error::TagNotFound(tag)) => assert_eq!(tag, "v1"),
        other => panic!("expected TagNotFound, got {other:?}"),
    }
    // Tags live under refs/db/tags, reachable by ordinary git plumbing.
    let tag_ref = RefName::new(format!("{TAGS_PREFIX}/seed")).expect("built-in tag name is valid");
    assert_eq!(
        gix_refstore::GixRefStore::new(&repo)
            .read(&tag_ref)
            .expect("read tag ref"),
        Some(commit)
    );
}

#[test]
fn restore_table_discards_unstaged_changes() {
    let (_dir, repo) = repo();
    let database = db(&repo);
    database.init().expect("init");
    database.create_table("users").expect("create");
    database.put("users", b"alice", &"one".into()).expect("put");
    database.stage("users").expect("stage");
    database.commit("seed").expect("commit");

    database.put("users", b"alice", &"two".into()).expect("put");
    database.restore_table("users").expect("restore");
    assert_eq!(
        database.get("users", b"alice").expect("get"),
        Some("one".into()),
        "working is back to the staged value"
    );

    // A table created after staging disappears again on restore.
    database.create_table("drafts").expect("create drafts");
    database.put("drafts", b"k", &"v".into()).expect("put");
    database.restore_table("drafts").expect("restore drafts");
    assert!(
        database.get("drafts", b"k").is_err(),
        "the unstaged table is gone from working"
    );
    match database.restore_table("ghost") {
        Err(Error::TableNotFound(name)) => assert_eq!(name.as_str(), "ghost"),
        other => panic!("expected TableNotFound, got {other:?}"),
    }
}

#[test]
fn unstage_restores_the_index_to_the_tip() {
    let (_dir, repo) = repo();
    let database = db(&repo);
    database.init().expect("init");
    database.create_table("users").expect("create");
    database.put("users", b"alice", &"one".into()).expect("put");
    database.stage("users").expect("stage");
    database.commit("seed").expect("commit");

    database.put("users", b"alice", &"two".into()).expect("put");
    database.stage("users").expect("stage");
    database.unstage("users").expect("unstage");
    // The tip still holds "one"; staging semantics mean the index matches it.
    database.restore_table("users").expect("restore to staged");
    assert_eq!(
        database.get("users", b"alice").expect("get"),
        Some("one".into())
    );
    match database.unstage("ghost") {
        Err(Error::TableNotFound(name)) => assert_eq!(name.as_str(), "ghost"),
        other => panic!("expected TableNotFound, got {other:?}"),
    }
}

#[test]
fn reset_hard_returns_working_and_index_to_the_tip() {
    let (_dir, repo) = repo();
    let database = db(&repo);
    database.init().expect("init");
    database.create_table("users").expect("create");
    database.put("users", b"alice", &"one".into()).expect("put");
    database.stage("users").expect("stage");
    let commit = database.commit("seed").expect("commit");

    database.put("users", b"alice", &"two".into()).expect("put");
    database.create_table("drafts").expect("create drafts");
    database.stage("drafts").expect("stage drafts");
    let reset = database.reset_hard().expect("reset hard");
    assert_eq!(reset, commit);
    assert_eq!(
        database.get("users", b"alice").expect("get"),
        Some("one".into())
    );
    assert!(database.get("drafts", b"nope").is_err());
    let status = database.status().expect("status");
    assert!(status.is_clean(), "hard reset leaves a clean status");
}

#[test]
fn move_table_renames_and_refuses_collisions() {
    let (_dir, repo) = repo();
    let database = db(&repo);
    database.init().expect("init");
    database.create_table("users").expect("create");
    database.put("users", b"alice", &"one".into()).expect("put");
    database.create_table("logs").expect("create logs");

    let root = database.move_table("users", "people").expect("move");
    assert!(database.get("users", b"alice").is_err());
    assert_eq!(
        database.get("people", b"alice").expect("get"),
        Some("one".into())
    );
    assert_eq!(
        database.table_root("people").expect("root"),
        root,
        "the rows move under the same root"
    );
    match database.move_table("logs", "people") {
        Err(Error::TableExists(name)) => assert_eq!(name.as_str(), "people"),
        other => panic!("expected TableExists, got {other:?}"),
    }
    match database.move_table("ghost", "anything") {
        Err(Error::TableNotFound(name)) => assert_eq!(name.as_str(), "ghost"),
        other => panic!("expected TableNotFound, got {other:?}"),
    }
}

#[test]
fn a_racing_put_never_reports_success_while_losing_its_write() {
    // Reproduce the stale-CAS window: two writers build their snapshots
    // from the same base ref value, then publish. The loser must surface a
    // conflict — never a success whose write silently vanished.
    for _ in 0..25 {
        let (dir, repo) = repo();
        let database = db(&repo);
        database.init().expect("init");
        database.create_table("t").expect("create");
        database.put("t", b"base", &"0".into()).expect("put");
        database.stage("t").expect("stage");
        database.commit("base").expect("commit");

        let path = dir.path().to_owned();
        let results: [Result<ObjectId, Error>; 2] = std::thread::scope(|scope| {
            let put = |key: &'static [u8]| {
                let path = path.clone();
                scope.spawn(move || {
                    let repo = gix::open(&path).expect("open repo");
                    Database::open(&repo).put("t", key, &"v".into())
                })
            };
            let a = put(b"k1");
            let b = put(b"k2");
            [a.join().expect("thread a"), b.join().expect("thread b")]
        });

        // Whatever the schedule, the surviving ref must contain every key
        // whose write reported success.
        let ok_keys: Vec<&[u8]> = results
            .iter()
            .enumerate()
            .filter_map(|(i, r)| match r {
                Ok(_) => Some(if i == 0 { &b"k1"[..] } else { &b"k2"[..] }),
                Err(Error::Write(WriteStateError::Conflict(_))) => None,
                Err(error) => panic!("unexpected racing error: {error}"),
            })
            .collect();
        let read = |key: &[u8]| {
            let repo = gix::open(&path).expect("open repo");
            Database::open(&repo).get("t", key).expect("get")
        };
        for key in ok_keys {
            assert_eq!(
                read(key),
                Some("v".into()),
                "a put that reported success lost its write"
            );
        }
        assert!(
            !results.iter().all(|r| r.is_err()),
            "two CAS races cannot both lose: one writer always publishes"
        );
    }
}

#[test]
fn a_racing_stage_never_reports_success_while_losing_its_write() {
    let (dir, repo) = repo();
    let database = db(&repo);
    database.init().expect("init");
    database.create_table("a").expect("create a");
    database.create_table("b").expect("create b");
    database.put("a", b"k", &"1".into()).expect("put a");
    database.put("b", b"k", &"1".into()).expect("put b");

    let path = dir.path().to_owned();
    let results: [Result<ObjectId, Error>; 2] = std::thread::scope(|scope| {
        let stage = |table: &'static str| {
            let path = path.clone();
            scope.spawn(move || {
                let repo = gix::open(&path).expect("open repo");
                Database::open(&repo).stage(table)
            })
        };
        let a = stage("a");
        let b = stage("b");
        [a.join().expect("thread a"), b.join().expect("thread b")]
    });
    let successes = results.iter().filter(|r| r.is_ok()).count();
    let conflicts = results
        .iter()
        .filter(|r| matches!(r, Err(Error::Write(WriteStateError::Conflict(_)))))
        .count();
    assert_eq!(
        successes + conflicts,
        2,
        "every stage either publishes or reports a conflict, got {results:?}"
    );
}
