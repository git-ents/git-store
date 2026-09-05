//! The Dolt-shaped command surface end to end: `ls`, `table mv/import/export`,
//! `schema show`, `show`, `branch` list/delete/rename, `checkout -b` and
//! table restore, `tag`, `reset`, and commit-to-commit `diff`.

use std::path::Path;
use std::process::Command;

use test_support::init_repo;

const BIN: &str = env!("CARGO_BIN_EXE_git-store");

fn run(dir: &Path, args: &[&str]) -> (String, String, i32) {
    let out = Command::new(BIN)
        .current_dir(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("spawn binary");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let (stdout, stderr, code) = run(dir, args);
    assert_eq!(code, 0, "command {args:?} failed: {stderr}");
    stdout
}

fn seed(dir: &Path) {
    ok(dir, &["db", "init"]);
    ok(dir, &["db", "table", "create", "users"]);
    ok(dir, &["db", "put", "users", "alice", "\"one\""]);
    ok(dir, &["db", "put", "users", "bob", "\"one\""]);
    ok(dir, &["db", "add", "users"]);
    ok(dir, &["db", "commit", "-m", "seed users"]);
    ok(dir, &["db", "put", "users", "alice", "\"two\""]);
    ok(dir, &["db", "add", "users"]);
    ok(dir, &["db", "commit", "-m", "alice is two"]);
}

#[test]
fn ls_schema_and_table_moves() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    init_repo(dir.path());
    seed(dir.path());
    ok(dir.path(), &["db", "table", "create", "logs"]);

    let out = ok(dir.path(), &["db", "ls"]);
    assert!(out.contains("users"), "ls: {out}");
    assert!(out.contains("logs"), "ls: {out}");

    let out = ok(dir.path(), &["db", "ls", "-v"]);
    assert!(out.contains("users (2 rows)"), "ls -v: {out}");
    assert!(out.contains("logs (0 rows)"), "ls -v: {out}");

    // schema show reports the honest key/value state.
    let out = ok(dir.path(), &["db", "schema", "show"]);
    assert!(out.contains("users"), "schema show: {out}");
    assert!(out.contains("key/value"), "schema show: {out}");
    let out = ok(dir.path(), &["db", "schema", "show", "users"]);
    assert!(out.contains("users"), "schema show users: {out}");
    let (_, _, code) = run(dir.path(), &["db", "schema", "show", "ghost"]);
    assert_eq!(code, 3, "schema show of a missing table");

    // mv renames rows intact.
    ok(dir.path(), &["db", "table", "mv", "logs", "events"]);
    let out = ok(dir.path(), &["db", "ls"]);
    assert!(!out.contains("logs"), "logs is gone: {out}");
    assert!(out.contains("events"), "events is there: {out}");
    let (_, stderr, code) = run(dir.path(), &["db", "table", "mv", "users", "events"]);
    assert_eq!(code, 2, "mv onto an existing table: {stderr}");
    let (_, stderr, code) = run(dir.path(), &["db", "table", "mv", "ghost", "anything"]);
    assert_eq!(code, 3, "mv of a missing table: {stderr}");
}

#[test]
fn table_import_and_export_round_trip() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    init_repo(dir.path());
    seed(dir.path());

    // Export to a file, then import into a new table.
    let export_path = dir.path().join("users.json");
    ok(
        dir.path(),
        &[
            "db",
            "table",
            "export",
            "users",
            "-F",
            export_path.to_str().expect("utf8 path"),
        ],
    );
    ok(
        dir.path(),
        &[
            "db",
            "table",
            "import",
            "people",
            "-c",
            "-F",
            export_path.to_str().expect("utf8 path"),
        ],
    );
    let out = ok(dir.path(), &["db", "get", "people", "alice"]);
    assert!(out.contains("\"two\""), "imported rows read back: {out}");

    // -c refuses an existing table; no flags refuse a missing one.
    let (_, stderr, code) = run(
        dir.path(),
        &[
            "db",
            "table",
            "import",
            "people",
            "-c",
            "-F",
            export_path.to_str().expect("utf8 path"),
        ],
    );
    assert_eq!(code, 2, "import -c over an existing table: {stderr}");
    let (_, stderr, code) = run(
        dir.path(),
        &[
            "db",
            "table",
            "import",
            "ghost",
            "-F",
            export_path.to_str().expect("utf8 path"),
        ],
    );
    assert_eq!(code, 3, "import into a missing table without -c: {stderr}");

    // -r replaces contents.
    let replace_path = dir.path().join("fresh.json");
    std::fs::write(&replace_path, "{\"z\": \"last\"}").expect("write import source");
    ok(
        dir.path(),
        &[
            "db",
            "table",
            "import",
            "people",
            "-r",
            "-F",
            replace_path.to_str().expect("utf8 path"),
        ],
    );
    let out = ok(dir.path(), &["db", "get", "people", "alice"]);
    assert!(out.contains("not set"), "replace dropped old rows: {out}");
    let out = ok(dir.path(), &["db", "get", "people", "z"]);
    assert!(out.contains("\"last\""), "replace kept new rows: {out}");

    // Export to stdout as JSON.
    let out = ok(dir.path(), &["db", "table", "export", "people"]);
    assert_eq!(out.trim(), "{\"z\":\"last\"}", "stdout export: {out}");
}

#[test]
fn show_prints_a_commit_message_and_rows() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    init_repo(dir.path());
    seed(dir.path());
    let log = ok(dir.path(), &["db", "log"]);
    let head = log
        .lines()
        .next()
        .expect("a commit")
        .split(' ')
        .next()
        .expect("oid");

    let out = ok(dir.path(), &["db", "show", head]);
    assert!(out.contains("alice is two"), "show: {out}");
    assert!(out.contains("users (2 rows)"), "show: {out}");
    assert!(out.contains("alice = \"two\""), "show: {out}");

    let out = ok(dir.path(), &["--format", "json", "db", "show", head]);
    assert!(
        out.contains("\"message\":\"alice is two\""),
        "show json: {out}"
    );

    let (_, stderr, code) = run(dir.path(), &["db", "show", "deadbeef"]);
    assert_eq!(code, 3, "show of an unknown commit: {stderr}");
}

#[test]
fn branch_checkout_tag_and_reset() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    init_repo(dir.path());
    seed(dir.path());

    // Bare branch lists with the current marker.
    let out = ok(dir.path(), &["db", "branch"]);
    assert_eq!(out.trim(), "* main", "branch list: {out}");

    // checkout -b creates and switches.
    ok(dir.path(), &["db", "checkout", "-b", "feature"]);
    let out = ok(dir.path(), &["db", "branch"]);
    assert!(out.contains("* feature"), "branch list: {out}");
    assert!(out.contains("main"), "branch list: {out}");

    // Tag the tip, list it, delete it.
    let log = ok(dir.path(), &["db", "log"]);
    let head = log
        .lines()
        .next()
        .expect("a commit")
        .split(' ')
        .next()
        .expect("oid");
    ok(dir.path(), &["db", "tag", "v1", head]);
    let out = ok(dir.path(), &["db", "tag"]);
    assert!(out.contains("v1"), "tag list: {out}");
    let (_, stderr, code) = run(dir.path(), &["db", "tag", "v1"]);
    assert_eq!(code, 2, "duplicate tag: {stderr}");
    ok(dir.path(), &["db", "tag", "-d", "v1"]);
    let (_, stderr, code) = run(dir.path(), &["db", "tag", "-d", "v1"]);
    assert_eq!(code, 3, "deleting a missing tag: {stderr}");

    // checkout of a table discards unstaged changes.
    ok(dir.path(), &["db", "put", "users", "carol", "\"three\""]);
    ok(dir.path(), &["db", "checkout", "users"]);
    let out = ok(dir.path(), &["db", "get", "users", "carol"]);
    assert!(
        out.contains("not set"),
        "restored table dropped the row: {out}"
    );
    let (_, stderr, code) = run(dir.path(), &["db", "checkout", "neither-branch-nor-table"]);
    assert_eq!(code, 3, "checkout of an unknown target: {stderr}");

    // reset unstages; reset --hard discards everything.
    ok(dir.path(), &["db", "put", "users", "dave", "\"four\""]);
    ok(dir.path(), &["db", "add", "users"]);
    let out = ok(dir.path(), &["db", "status"]);
    assert!(
        out.contains("modified: users"),
        "staged before reset: {out}"
    );
    ok(dir.path(), &["db", "reset", "users"]);
    let out = ok(dir.path(), &["db", "status"]);
    assert!(
        out.contains("modified: users"),
        "reset keeps the change unstaged: {out}"
    );
    ok(dir.path(), &["db", "reset", "--hard"]);
    let out = ok(dir.path(), &["db", "status"]);
    assert!(out.contains("clean"), "hard reset cleans: {out}");

    // branch -m renames; -d refuses the current branch.
    ok(dir.path(), &["db", "checkout", "main"]);
    ok(dir.path(), &["db", "branch", "-m", "trunk"]);
    let out = ok(dir.path(), &["db", "branch"]);
    assert!(out.contains("* trunk"), "renamed current: {out}");
    let (_, stderr, code) = run(dir.path(), &["db", "branch", "-d", "trunk"]);
    assert_eq!(code, 2, "deleting the current branch: {stderr}");
    ok(dir.path(), &["db", "branch", "side"]);
    ok(dir.path(), &["db", "branch", "-d", "side"]);
    let (_, stderr, code) = run(dir.path(), &["db", "branch", "-d", "side"]);
    assert_eq!(code, 3, "deleting a missing branch: {stderr}");
}

#[test]
fn merge_refuses_same_key_changes_with_the_conflicts_listed() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    init_repo(dir.path());
    ok(dir.path(), &["db", "init"]);
    ok(dir.path(), &["db", "table", "create", "users"]);
    ok(dir.path(), &["db", "put", "users", "alice", "\"base\""]);
    ok(dir.path(), &["db", "add", "users"]);
    ok(dir.path(), &["db", "commit", "-m", "base"]);
    ok(dir.path(), &["db", "checkout", "-b", "feature"]);
    ok(dir.path(), &["db", "put", "users", "alice", "\"feature\""]);
    ok(dir.path(), &["db", "add", "users"]);
    ok(dir.path(), &["db", "commit", "-m", "feature"]);
    ok(dir.path(), &["db", "checkout", "main"]);
    ok(dir.path(), &["db", "put", "users", "alice", "\"main\""]);
    ok(dir.path(), &["db", "add", "users"]);
    ok(dir.path(), &["db", "commit", "-m", "main side"]);

    let (stdout, stderr, code) = run(dir.path(), &["db", "merge", "feature"]);
    assert_eq!(
        code, 4,
        "a same-key merge is a CAS refusal: {stdout} {stderr}"
    );
    let out = format!("{stdout}{stderr}");
    assert!(out.contains("merge refused"), "{out}");
    assert!(out.contains("alice"), "the conflicting row is named: {out}");
}

#[test]
fn diff_takes_commit_ranges() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    init_repo(dir.path());
    seed(dir.path());
    let log = ok(dir.path(), &["db", "log"]);
    let lines: Vec<&str> = log.lines().collect();
    let head = lines[0].split(' ').next().expect("oid");
    let base = lines[1].split(' ').next().expect("oid");

    // diff <tip> compares the tip against the working set (clean here).
    let out = ok(dir.path(), &["db", "diff", head]);
    assert_eq!(out.trim(), "", "a clean working set diffs empty: {out}");

    // diff <base> <head> shows what the second commit changed.
    let out = ok(dir.path(), &["db", "diff", base, head]);
    assert!(out.contains("modified: users"), "commit diff: {out}");
    assert!(out.contains("~ alice"), "commit diff rows: {out}");

    let (_, stderr, code) = run(dir.path(), &["db", "diff", "deadbeef"]);
    assert_eq!(code, 3, "diff from an unknown commit: {stderr}");
}
