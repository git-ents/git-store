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

    let tip = database
        .delete_branch("feature", true)
        .expect("delete feature");
    let branches = database.list_branches().expect("list");
    assert_eq!(branches, vec![("main".to_owned(), tip)]);
    match database.delete_branch("main", false) {
        Err(Error::CurrentBranch(branch)) => assert_eq!(branch, "main"),
        other => panic!("expected CurrentBranch, got {other:?}"),
    }
    match database.delete_branch("ghost", false) {
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

#[test]
fn delete_branch_refuses_unmerged_tips_without_force() {
    let (_dir, repo) = repo();
    let database = db(&repo);
    database.init().expect("init");
    database.create_table("users").expect("create");
    database.put("users", b"alice", &"one".into()).expect("put");
    database.stage("users").expect("stage");
    database.commit("base").expect("commit");
    database.create_branch("feature").expect("create feature");
    database.checkout("feature", false).expect("checkout");
    database.put("users", b"bob", &"two".into()).expect("put");
    database.stage("users").expect("stage");
    database.commit("feature work").expect("commit");
    database.checkout("main", false).expect("checkout main");

    // The feature tip is not reachable from main: -d refuses, -D deletes.
    match database.delete_branch("feature", false) {
        Err(Error::BranchNotMerged(branch)) => assert_eq!(branch, "feature"),
        other => panic!("expected BranchNotMerged, got {other:?}"),
    }
    database
        .delete_branch("feature", true)
        .expect("forced delete");
}

#[test]
fn deleting_a_branch_head_names_restores_the_ref() {
    // The delete-then-recheck design: when `HEAD` names the branch at the
    // post-delete re-check — the state a rival checkout between a pre-check
    // and the delete would leave — the ref is re-created at its old tip and
    // the deletion is refused, never leaving `refs/db/HEAD` dangling.
    let (_dir, repo) = repo();
    let database = db(&repo);
    database.init().expect("init");
    database.commit("base").expect("commit");
    database.create_branch("raced").expect("create raced");
    let tip = {
        let raced = gix_refstore::RefName::new("refs/db/heads/raced").expect("valid");
        gix_refstore::GixRefStore::new(&repo)
            .read(&raced)
            .expect("read raced")
            .expect("raced exists")
    };
    // Point `HEAD` at the branch behind `checkout`'s back: this is exactly
    // the state a rival checkout racing the delete produces.
    let edit = gix::refs::transaction::RefEdit {
        change: gix::refs::transaction::Change::Update {
            log: gix::refs::transaction::LogChange {
                mode: gix::refs::transaction::RefLog::AndReference,
                force_create_reflog: false,
                message: "test: race the delete".into(),
            },
            expected: gix::refs::transaction::PreviousValue::Any,
            new: gix::refs::Target::Symbolic(
                gix::refs::FullName::try_from("refs/db/heads/raced").expect("valid"),
            ),
        },
        name: gix::refs::FullName::try_from(gix_database::HEAD_REF).expect("valid"),
        deref: false,
    };
    repo.edit_reference(edit).expect("retarget head");

    match database.delete_branch("raced", true) {
        Err(Error::CurrentBranch(branch)) => assert_eq!(branch, "raced"),
        other => panic!("expected CurrentBranch, got {other:?}"),
    }
    // The ref is back at its old tip; HEAD resolves cleanly.
    let raced = gix_refstore::RefName::new("refs/db/heads/raced").expect("valid");
    assert_eq!(
        gix_refstore::GixRefStore::new(&repo)
            .read(&raced)
            .expect("read raced"),
        Some(tip),
        "the deleted branch's ref was restored for the dangling HEAD"
    );
    assert_eq!(
        database.head().expect("head").commit(),
        Some(tip),
        "HEAD resolves through the restored ref"
    );
}

#[test]
fn reset_hard_discards_pre_first_commit_state() {
    let (_dir, repo) = repo();
    let database = db(&repo);
    database.init().expect("init");
    database.create_table("drafts").expect("create");
    database.put("drafts", b"k", &"v".into()).expect("put");
    database.stage("drafts").expect("stage");
    database.reset_hard().expect("reset on unborn head");
    let status = database.status().expect("status");
    assert!(status.is_clean(), "unborn reset_hard cleans: {status:?}");
    assert!(
        database.get("drafts", b"k").is_err(),
        "the table is gone from working"
    );
}

#[test]
fn move_table_follows_the_staged_rename() {
    let (_dir, repo) = repo();
    let database = db(&repo);
    database.init().expect("init");
    database.create_table("users").expect("create");
    database.put("users", b"alice", &"one".into()).expect("put");
    database.stage("users").expect("stage");
    database.commit("seed").expect("commit");
    // Staged add of a table under the target name whose working copy is
    // then dropped — the exact tangle the review flagged.
    database.create_table("people").expect("create people");
    database.stage("people").expect("stage people");
    database.drop_table("people").expect("drop working people");

    database.move_table("users", "people").expect("mv");
    // The index followed the rename (the way `git mv` updates the index),
    // so the staged view is exactly "users deleted, people added" — the
    // stale staged add under `people` was overwritten — and nothing is
    // left unstaged.
    let status = database.status().expect("status");
    assert!(status.unstaged.is_empty(), "nothing unstaged: {status:?}");
    let staged: Vec<(&str, &str)> = status
        .staged
        .iter()
        .map(|t| (t.table.as_str(), change_kind(t.kind)))
        .collect();
    assert_eq!(
        staged,
        vec![("people", "new table"), ("users", "deleted table")],
        "the staged rename is coherent: {status:?}"
    );
}

#[test]
fn tags_refuse_commits_no_branch_reaches() {
    let (_dir, repo) = repo();
    let database = db(&repo);
    database.init().expect("init");
    database.create_table("users").expect("create");
    database.put("users", b"alice", &"one".into()).expect("put");
    // The pre-commit working state commit is parented on nothing and no
    // branch will ever reach it once the real commit lands.
    let state = database
        .working_state()
        .expect("working")
        .commit
        .expect("state commit");
    database.stage("users").expect("stage");
    database.commit("seed").expect("commit");
    match database.create_tag("orphan", Some(state)) {
        Err(Error::CommitUnreachable(oid)) => assert_eq!(oid, state),
        other => panic!("expected CommitUnreachable, got {other:?}"),
    }
    database.create_tag("tip", None).expect("tag the tip");
}

#[test]
fn a_branch_needs_a_commit_to_exist() {
    let (_dir, repo) = repo();
    let database = db(&repo);
    database.init().expect("init");
    match database.create_branch("main") {
        Err(Error::EmptyDatabase) => {}
        other => panic!("expected EmptyDatabase, got {other:?}"),
    }
}

/// The Dolt-style verb for a table-level change kind.
fn change_kind(kind: gix_database::ChangeKind) -> &'static str {
    match kind {
        gix_database::ChangeKind::Added => "new table",
        gix_database::ChangeKind::Removed => "deleted table",
        gix_database::ChangeKind::Modified => "modified",
    }
}
