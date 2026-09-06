//! Drive `git store db …` against a temp repo: the first end-to-end
//! milestone (init → put → status → diff → add → commit → log) plus exit-code
//! classification and JSON output.

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

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

#[test]
fn the_first_milestone_end_to_end() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    init_repo(dir.path());

    // init is idempotent.
    ok(dir.path(), &["db", "init"]);
    ok(dir.path(), &["db", "init"]);

    // Nothing exists yet.
    let out = ok(dir.path(), &["db", "status"]);
    assert!(out.contains("clean"), "an empty database is clean: {out}");

    // put before create is refused with a not-found exit code.
    let (_, stderr, code) = run(dir.path(), &["db", "put", "users", "alice", "1"]);
    assert_eq!(code, 3, "missing table is not found: {stderr}");

    ok(dir.path(), &["db", "table", "create", "users"]);
    ok(dir.path(), &["db", "put", "users", "alice", "\"one\""]);
    ok(dir.path(), &["db", "put", "users", "bob", "\"two\""]);

    // get reads back.
    let out = ok(dir.path(), &["db", "get", "users", "alice"]);
    assert!(out.contains("\"one\""), "get returns JSON: {out}");

    // status shows unstaged work before add, in Dolt's wording.
    let out = ok(dir.path(), &["db", "status"]);
    assert!(out.contains("new table: users"), "status: {out}");

    let out = ok(dir.path(), &["db", "diff"]);
    assert!(out.contains("+ alice"), "diff: {out}");
    assert!(out.contains("+ bob"), "diff: {out}");

    // Invalid JSON is an invalid-args failure (exit 2), like `table import`.
    let (_, stderr, code) = run(dir.path(), &["db", "put", "users", "x", "{nope"]);
    assert_eq!(code, 2, "invalid value JSON: {stderr}");

    // add stages; diff --staged sees the staged rows; working diff empties.
    ok(dir.path(), &["db", "add", "users"]);
    let out = ok(dir.path(), &["db", "diff", "--staged"]);
    assert!(out.contains("+ alice"), "staged diff: {out}");
    let out = ok(dir.path(), &["db", "diff"]);
    assert_eq!(out.trim(), "", "nothing unstaged after add");

    let commit_out = ok(dir.path(), &["db", "commit", "-m", "seed users"]);
    let commit = commit_out.trim().to_owned();

    let out = ok(dir.path(), &["db", "status"]);
    assert!(out.contains("clean"), "post-commit status: {out}");

    let log = ok(dir.path(), &["db", "log"]);
    assert!(log.contains(&commit), "log shows the commit: {log}");
    assert!(log.contains("seed users"), "log shows the message: {log}");

    // The published snapshot is inspectable with git plumbing: the branch
    // tip is an ordinary commit holding an ordinary tree.
    git(dir.path(), &["ls-tree", "refs/db/heads/main^{tree}"]);
    git(dir.path(), &["cat-file", "-e", &commit]);
    let fsck = Command::new("git")
        .current_dir(dir.path())
        .args(["fsck", "--no-dangling"])
        .output()
        .expect("git fsck");
    assert!(
        fsck.status.success(),
        "git fsck validates the database: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}

#[test]
fn json_output_is_stable() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    init_repo(dir.path());
    ok(dir.path(), &["db", "init"]);
    ok(dir.path(), &["db", "table", "create", "users"]);
    ok(dir.path(), &["db", "put", "users", "alice", "\"one\""]);

    let out = ok(
        dir.path(),
        &["--format", "json", "db", "get", "users", "alice"],
    );
    assert!(out.contains("\"value\":\"one\""), "{out}");
    assert!(out.contains("\"found\":true"), "json: {out}");

    let out = ok(dir.path(), &["--format", "ndjson", "db", "table", "list"]);
    assert!(out.contains("\"table\":\"users\""), "ndjson: {out}");
    assert!(out.contains("\"root\":\""), "ndjson: {out}");

    let (_, _, code) = run(
        dir.path(),
        &["--format", "json", "db", "table", "create", "users"],
    );
    assert_eq!(code, 2, "duplicate create exits invalid");
}

#[test]
fn rm_and_table_drop_report_not_found() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    init_repo(dir.path());
    ok(dir.path(), &["db", "init"]);
    ok(dir.path(), &["db", "table", "create", "t"]);
    ok(dir.path(), &["db", "put", "t", "k", "\"v\""]);

    let (_, stderr, code) = run(dir.path(), &["db", "get", "missing", "k"]);
    assert_eq!(code, 3, "{stderr}");
    let (_, stderr, code) = run(dir.path(), &["db", "rm", "t", "absent"]);
    assert_eq!(code, 3, "missing key: {stderr}");

    ok(dir.path(), &["db", "rm", "t", "k"]);
    let out = ok(dir.path(), &["db", "get", "t", "k"]);
    assert!(
        out.contains("not set") || out.contains("\"found\":false"),
        "{out}"
    );

    ok(dir.path(), &["db", "table", "drop", "t"]);
    let (_, stderr, code) = run(dir.path(), &["db", "table", "drop", "t"]);
    assert_eq!(code, 3, "dropping twice: {stderr}");
}

#[test]
fn invalid_table_names_are_rejected_with_exit_two() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    init_repo(dir.path());
    ok(dir.path(), &["db", "init"]);
    for name in ["!meta", "a/b", ".", ".."] {
        let (_, stderr, code) = run(dir.path(), &["db", "table", "create", name]);
        assert_eq!(code, 2, "invalid table {name:?}: {stderr}");
    }
}
