//! Three-way merge tests: disjoint row merges, same-key conflicts, and
//! table-level drop/modify behavior.

use gix::ObjectId;
use gix_database::{ConflictKind, Database, Error, MergeError, Snapshot};

fn fresh_repo() -> (tempfile::TempDir, gix::Repository) {
    let dir = tempfile::TempDir::new().expect("temp dir");
    test_support::init_repo(dir.path());
    let repo = gix::open(dir.path()).expect("open repo");
    (dir, repo)
}

/// Set up `main` with a seeded table and create `feature` at the tip.
fn seeded() -> (tempfile::TempDir, gix::Repository) {
    let (dir, repo) = fresh_repo();
    let database = Database::open(&repo);
    database.init().expect("init");
    database.create_table("users").expect("create");
    database.put("users", b"alice", &"one".into()).expect("put");
    database.put("users", b"bob", &"two".into()).expect("put");
    database.stage("users").expect("stage");
    database.commit("base").expect("commit");
    database.create_branch("feature").expect("branch");
    (dir, repo)
}

#[test]
fn disjoint_row_changes_merge_automatically() {
    let (_dir, repo) = seeded();
    let database = Database::open(&repo);
    database.checkout("feature", false).expect("checkout");
    database
        .put("users", b"carol", &"three".into())
        .expect("put");
    database.stage("users").expect("stage");
    database.commit("add carol on feature").expect("commit");

    database.checkout("main", false).expect("checkout");
    database.remove("users", b"bob").expect("remove");
    database.stage("users").expect("stage");
    database.commit("remove bob on main").expect("commit");

    let merge_commit = database.merge("feature").expect("merge");
    let log = database.log().expect("log");
    assert_eq!(log[0].commit, merge_commit);
    assert_eq!(
        log[0].parents.len(),
        2,
        "a merge is an ordinary two-parent commit"
    );

    assert_eq!(
        database.get("users", b"alice").expect("get"),
        Some("one".into()),
        "untouched rows carry over"
    );
    assert_eq!(
        database.get("users", b"carol").expect("get"),
        Some("three".into()),
        "their-side insert lands"
    );
    assert_eq!(
        database.get("users", b"bob").expect("get"),
        None,
        "our-side delete lands"
    );
}

#[test]
fn same_key_different_values_refuse_with_conflicts() {
    let (_dir, repo) = seeded();
    let database = Database::open(&repo);
    database.checkout("feature", false).expect("checkout");
    database
        .put("users", b"alice", &"theirs".into())
        .expect("put");
    database.stage("users").expect("stage");
    database.commit("theirs alice").expect("commit");

    database.checkout("main", false).expect("checkout");
    database
        .put("users", b"alice", &"ours".into())
        .expect("put");
    database.stage("users").expect("stage");
    let ours_tip = database.commit("ours alice").expect("commit");

    let conflicts = match database.merge("feature") {
        Err(MergeError::Conflicts { conflicts }) => conflicts,
        other => panic!("expected conflicts, got {other:?}"),
    };
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].table.as_str(), "users");
    assert_eq!(conflicts[0].key, b"alice".to_vec());
    assert!(matches!(
        conflicts[0].kind,
        ConflictKind::DifferentValues { .. }
    ));

    // Nothing was written: the branch still points at our tip.
    assert_eq!(database.head().expect("head").commit(), Some(ours_tip));
    assert_eq!(database.log().expect("log").len(), 2);
}

#[test]
fn delete_modify_conflicts() {
    let (_dir, repo) = seeded();
    let database = Database::open(&repo);

    // They delete alice; we modify alice.
    database.checkout("feature", false).expect("checkout");
    database.remove("users", b"alice").expect("remove");
    database.stage("users").expect("stage");
    database.commit("delete alice on feature").expect("commit");

    database.checkout("main", false).expect("checkout");
    database
        .put("users", b"alice", &"changed".into())
        .expect("put");
    database.stage("users").expect("stage");
    database.commit("modify alice on main").expect("commit");

    let conflicts = match database.merge("feature") {
        Err(MergeError::Conflicts { conflicts }) => conflicts,
        other => panic!("expected a delete/modify conflict, got {other:?}"),
    };
    assert_eq!(conflicts.len(), 1);
    assert!(
        matches!(conflicts[0].kind, ConflictKind::TheirsDeleted { .. }),
        "they deleted, we modified"
    );
    assert!(conflicts[0].kind.ours_oid().is_some());

    // The reverse direction: we delete, they modify.
    let (dir2, repo2) = fresh_repo();
    let other = Database::open(&repo2);
    other.init().expect("init");
    other.create_table("t").expect("create");
    other.put("t", b"k", &"v".into()).expect("put");
    other.stage("t").expect("stage");
    other.commit("base").expect("commit");
    other.create_branch("feature").expect("branch");
    other.checkout("feature", false).expect("checkout");
    other.put("t", b"k", &"theirs".into()).expect("put");
    other.stage("t").expect("stage");
    other.commit("theirs k").expect("commit");
    other.checkout("main", false).expect("checkout");
    other.remove("t", b"k").expect("remove");
    other.stage("t").expect("stage");
    other.commit("ours delete").expect("commit");
    let conflicts = match other.merge("feature") {
        Err(MergeError::Conflicts { conflicts }) => conflicts,
        other => panic!("expected an ours-deleted conflict, got {other:?}"),
    };
    assert!(matches!(
        conflicts[0].kind,
        ConflictKind::OursDeleted { .. }
    ));
    drop((dir2, repo2));
}

#[test]
fn same_key_set_to_the_same_value_converges() {
    let (_dir, repo) = seeded();
    let database = Database::open(&repo);
    database.checkout("feature", false).expect("checkout");
    database.put("users", b"carol", &"new".into()).expect("put");
    database.stage("users").expect("stage");
    database.commit("theirs carol").expect("commit");

    database.checkout("main", false).expect("checkout");
    database.put("users", b"carol", &"new".into()).expect("put");
    database.stage("users").expect("stage");
    database.commit("ours carol").expect("commit");

    database
        .merge("feature")
        .expect("identical adds converge without conflict");
    assert_eq!(
        database.get("users", b"carol").expect("get"),
        Some("new".into())
    );
}

#[test]
fn tables_added_on_both_sides_merge_row_by_row() {
    let (_dir, repo) = fresh_repo();
    let database = Database::open(&repo);
    database.init().expect("init");
    database.create_table("users").expect("create");
    database.put("users", b"alice", &"one".into()).expect("put");
    database.stage("users").expect("stage");
    database.commit("base").expect("commit");
    database.create_branch("feature").expect("branch");

    database.checkout("feature", false).expect("checkout");
    database.create_table("events").expect("create");
    database
        .put("events", b"signup", &"them".into())
        .expect("put");
    database.stage("events").expect("stage");
    database.commit("feature events").expect("commit");

    database.checkout("main", false).expect("checkout");
    database.create_table("events").expect("create");
    database.put("events", b"login", &"us".into()).expect("put");
    database.stage("events").expect("stage");
    database.commit("main events").expect("commit");

    database
        .merge("feature")
        .expect("add/add merges disjoint rows");
    assert_eq!(
        database.get("events", b"signup").expect("get"),
        Some("them".into())
    );
    assert_eq!(
        database.get("events", b"login").expect("get"),
        Some("us".into())
    );
}

#[test]
fn dropped_table_vs_modified_table_conflicts() {
    let (_dir, repo) = fresh_repo();
    let database = Database::open(&repo);
    database.init().expect("init");
    database.create_table("users").expect("create");
    database.put("users", b"alice", &"one".into()).expect("put");
    database.stage("users").expect("stage");
    database.commit("base").expect("commit");
    database.create_branch("feature").expect("branch");

    // They drop the table; we modify a row in it.
    database.checkout("feature", false).expect("checkout");
    database.drop_table("users").expect("drop");
    database.stage("users").expect("add stages the deletion");
    database.commit("drop users on feature").expect("commit");

    database.checkout("main", false).expect("checkout");
    database
        .put("users", b"alice", &"changed".into())
        .expect("put");
    database.stage("users").expect("stage");
    database.commit("modify alice on main").expect("commit");

    let conflicts = match database.merge("feature") {
        Err(MergeError::Conflicts { conflicts }) => conflicts,
        other => panic!("expected a drop/modify conflict, got {other:?}"),
    };
    assert!(
        conflicts
            .iter()
            .any(|entry| entry.table.as_str() == "users"),
        "a dropped table that the other side modified conflicts"
    );
}

#[test]
fn dropped_table_untouched_by_their_side_stays_dropped() {
    let (_dir, repo) = seeded();
    let database = Database::open(&repo);
    database.checkout("feature", false).expect("checkout");
    database.drop_table("users").expect("drop");
    database.stage("users").expect("add stages the deletion");
    database.commit("drop users on feature").expect("commit");

    database.checkout("main", false).expect("checkout");
    database
        .merge("feature")
        .expect("an untouched drop carries over");
    assert!(matches!(
        database.table_root("users"),
        Err(Error::TableNotFound(_))
    ));
}

#[test]
fn missing_branches_are_refused() {
    let (_dir, repo) = fresh_repo();
    let database = Database::open(&repo);
    database.init().expect("init");
    assert!(matches!(
        database.merge("no-such-branch"),
        Err(MergeError::State(_))
    ));
}

#[test]
fn unrelated_histories_are_refused() {
    let (_dir, repo) = fresh_repo();
    let database = Database::open(&repo);
    database.init().expect("init");
    database.create_table("t").expect("create");
    database.put("t", b"k", &"v".into()).expect("put");
    database.stage("t").expect("stage");
    database.commit("first").expect("commit");

    // Forge an orphan branch: a commit with no parents whose tree is a valid
    // empty snapshot, reachable through refs/db/heads/orphan.
    let orphan_tree = Snapshot::empty(*database.store().config())
        .write(&repo)
        .expect("empty snapshot tree");
    let signature = repo.committer().expect("committer").expect("signature");
    let orphan = repo
        .write_object(&gix::objs::Commit {
            tree: orphan_tree,
            parents: Vec::<ObjectId>::new().into(),
            author: signature.into(),
            committer: signature.into(),
            encoding: None,
            message: "orphan".into(),
            extra_headers: vec![],
        })
        .expect("commit")
        .detach();
    gix_refstore_shim::create_ref(&repo, "refs/db/heads/orphan", orphan);

    match database.merge("orphan") {
        Err(MergeError::NoMergeBase { .. }) => {}
        other => panic!("expected a no-merge-base refusal, got {other:?}"),
    }
}

/// Minimal ref creation shim for the orphan-commit test above.
mod gix_refstore_shim {
    use super::*;

    pub fn create_ref(repo: &gix::Repository, name: &str, oid: ObjectId) {
        use gix_refstore::RefStore as _;
        use gix_refstore::{GixRefStore, RefEdit, RefName};
        let name = RefName::new(name).expect("valid ref");
        GixRefStore::new(repo)
            .apply(RefEdit::Create { name, new: oid })
            .expect("create ref");
    }
}

#[test]
fn the_merge_base_is_found_across_divergence() {
    let (_dir, repo) = seeded();
    let database = Database::open(&repo);
    database.checkout("feature", false).expect("checkout");
    database
        .put("users", b"carol", &"three".into())
        .expect("put");
    database.stage("users").expect("stage");
    database.commit("feature side").expect("commit");

    database.checkout("main", false).expect("checkout");
    database
        .put("users", b"alice", &"one-changed".into())
        .expect("put");
    database.stage("users").expect("stage");
    database.commit("main side").expect("commit");

    database.merge("feature").expect("a shared base merges");
    assert_eq!(
        database.get("users", b"alice").expect("get"),
        Some("one-changed".into())
    );
    assert_eq!(
        database.get("users", b"carol").expect("get"),
        Some("three".into())
    );
    assert_eq!(
        database.get("users", b"bob").expect("get"),
        Some("two".into())
    );
}
