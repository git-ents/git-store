//! Branch/tag administration and working/index recovery: delete, rename,
//! restore, unstage, hard reset, and table moves.

use gix::ObjectId;
use gix_database::{Database, Error, TAGS_PREFIX};
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
