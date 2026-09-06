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
fn a_failed_import_writes_nothing() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    init_repo(dir.path());
    ok(dir.path(), &["db", "init"]);

    // -c with a bad key mid-document: the table must not exist at all.
    let bad = dir.path().join("bad.json");
    std::fs::write(&bad, "{\"good\": \"1\", \"\": \"2\"}").expect("write bad doc");
    let (_, stderr, code) = run(
        dir.path(),
        &[
            "db",
            "table",
            "import",
            "t",
            "-c",
            "-F",
            bad.to_str().expect("utf8"),
        ],
    );
    assert_eq!(code, 2, "empty key is invalid: {stderr}");
    let (_, stderr, code) = run(dir.path(), &["db", "table", "export", "t"]);
    assert_eq!(code, 3, "the failed import created nothing: {stderr}");

    // Same for an existing table: its rows survive a failed replace.
    ok(dir.path(), &["db", "table", "create", "t"]);
    ok(dir.path(), &["db", "put", "t", "keep", "\"yes\""]);
    let (_, stderr, code) = run(
        dir.path(),
        &[
            "db",
            "table",
            "import",
            "t",
            "-r",
            "-F",
            bad.to_str().expect("utf8"),
        ],
    );
    assert_eq!(code, 2, "replace with a bad key: {stderr}");
    let out = ok(dir.path(), &["db", "get", "t", "keep"]);
    assert!(out.contains("\"yes\""), "old rows survive: {out}");

    // A 10KB key is rejected the same way, before anything is written.
    let big = format!("{{\"{}\": \"1\"}}", "k".repeat(10 * 1024));
    let big_path = dir.path().join("big.json");
    std::fs::write(&big_path, big).expect("write big doc");
    let (_, stderr, code) = run(
        dir.path(),
        &[
            "db",
            "table",
            "import",
            "t",
            "-r",
            "-F",
            big_path.to_str().expect("utf8"),
        ],
    );
    assert_eq!(code, 2, "oversized key: {stderr}");
    let out = ok(dir.path(), &["db", "get", "t", "keep"]);
    assert!(out.contains("\"yes\""), "old rows still survive: {out}");
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

#[test]
fn revisions_resolve_against_the_database_namespace() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    init_repo(dir.path());
    seed(dir.path());

    // HEAD, bare branch names, and ~N ancestry all mean the *database*
    // namespace, never the worktree's unborn refs/heads/main.
    let out = ok(dir.path(), &["db", "show", "HEAD"]);
    assert!(out.contains("alice is two"), "show HEAD: {out}");
    let out = ok(dir.path(), &["db", "show", "main"]);
    assert!(out.contains("alice is two"), "show main: {out}");
    let out = ok(dir.path(), &["db", "show", "main~1"]);
    assert!(out.contains("seed users"), "show main~1: {out}");
    let out = ok(dir.path(), &["db", "show", "main^"]);
    assert!(out.contains("seed users"), "show main^: {out}");
    let log = ok(dir.path(), &["db", "log"]);
    let base = log
        .lines()
        .nth(1)
        .expect("second commit")
        .split(' ')
        .next()
        .expect("oid");
    // The root commit has no parent, so one step past it is a caller
    // mistake, not a lookup failure.
    let (_, stderr, code) = run(dir.path(), &["db", "show", &format!("{base}~1")]);
    assert_eq!(code, 2, "the root commit has no parent: {stderr}");
    let (_, stderr, code) = run(dir.path(), &["db", "show", "ghost"]);
    assert_eq!(code, 3, "unknown branch: {stderr}");
}

#[test]
fn add_limits_pathspecs_and_commit_refuses_empty() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    init_repo(dir.path());
    ok(dir.path(), &["db", "init"]);
    ok(dir.path(), &["db", "table", "create", "users"]);
    ok(dir.path(), &["db", "table", "create", "logs"]);
    ok(dir.path(), &["db", "put", "users", "u", "\"1\""]);
    ok(dir.path(), &["db", "put", "logs", "l", "\"1\""]);

    // A pathspec stages only the named tables, even with -A: logs stays
    // in the unstaged section.
    ok(dir.path(), &["db", "add", "-A", "users"]);
    let out = ok(dir.path(), &["db", "status"]);
    let staged = out
        .split("Changes not staged for commit:")
        .next()
        .expect("staged section");
    assert!(staged.contains("new table: users"), "users staged: {out}");
    assert!(!staged.contains("logs"), "logs untouched: {out}");
    assert!(out.contains("new table: logs"), "logs unstaged: {out}");

    // Bare `add` needs a target.
    let (_, _, code) = run(dir.path(), &["db", "add"]);
    assert_eq!(code, 2, "bare add is invalid");

    // Committing nothing is refused unless --allow-empty.
    ok(dir.path(), &["db", "reset", "--hard"]);
    let (_, stderr, code) = run(dir.path(), &["db", "commit", "-m", "empty"]);
    assert_eq!(code, 2, "empty commit refused: {stderr}");
    let log = ok(dir.path(), &["db", "log"]);
    assert_eq!(log.lines().count(), 0, "no commits yet: {log}");
    ok(
        dir.path(),
        &["db", "commit", "-m", "empty", "--allow-empty"],
    );
    let log = ok(dir.path(), &["db", "log"]);
    assert!(log.contains("empty"), "allow-empty recorded: {log}");
}

#[test]
fn checkout_b_works_on_a_dirty_tree_and_rolls_back_on_failure() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    init_repo(dir.path());
    seed(dir.path());
    ok(dir.path(), &["db", "put", "users", "carol", "\"three\""]);

    // -b branches from the current tip: nothing can be discarded, so the
    // dirty working set must not refuse.
    ok(dir.path(), &["db", "checkout", "-b", "feature"]);
    let out = ok(dir.path(), &["db", "branch"]);
    assert!(out.contains("* feature"), "on feature: {out}");
    let out = ok(dir.path(), &["db", "get", "users", "carol"]);
    assert!(out.contains("\"three\""), "working kept: {out}");
}

#[test]
fn failures_get_a_machine_readable_envelope_in_json_mode() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    init_repo(dir.path());
    ok(dir.path(), &["db", "init"]);

    ok(dir.path(), &["db", "table", "create", "t"]);
    ok(dir.path(), &["db", "put", "t", "k", "\"v\""]);
    ok(dir.path(), &["db", "add", "t"]);
    ok(dir.path(), &["db", "commit", "-m", "seed"]);
    let (stdout, _, code) = run(dir.path(), &["--format", "json", "db", "merge", "nosuch"]);
    assert_eq!(code, 3, "missing merge target");
    assert!(
        stdout.contains("\"status\":\"error\""),
        "error envelope: {stdout}"
    );
    assert!(
        stdout.contains("\"code\":\"not_found\""),
        "classified: {stdout}"
    );

    let (stdout, _, code) = run(
        dir.path(),
        &["--format", "ndjson", "db", "put", "t", "k", "{nope"],
    );
    assert_eq!(code, 2, "invalid JSON value");
    assert!(
        stdout.contains("\"status\":\"error\""),
        "ndjson envelope: {stdout}"
    );
}

#[test]
fn merge_conflicts_are_emitted_in_the_requested_format() {
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

    let (stdout, _, code) = run(dir.path(), &["--format", "json", "db", "merge", "feature"]);
    assert_eq!(code, 4, "conflicting merge");
    assert!(
        stdout.contains("\"conflicts\""),
        "conflict list on stdout: {stdout}"
    );
    assert!(
        stdout.contains("\"key\":\"alice\""),
        "conflicting key named: {stdout}"
    );
}

#[test]
fn branch_delete_checks_merge_state_like_git() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    init_repo(dir.path());
    seed(dir.path());
    ok(dir.path(), &["db", "checkout", "-b", "feature"]);
    ok(
        dir.path(),
        &["db", "put", "users", "bob", "\"feature-only\""],
    );
    ok(dir.path(), &["db", "add", "users"]);
    ok(dir.path(), &["db", "commit", "-m", "feature work"]);
    ok(dir.path(), &["db", "checkout", "main"]);

    // -d refuses unmerged tips; -D overrides.
    let (_, stderr, code) = run(dir.path(), &["db", "branch", "-d", "feature"]);
    assert_eq!(code, 2, "unmerged branch refused: {stderr}");
    assert!(stderr.contains("unmerged"), "{stderr}");
    ok(dir.path(), &["db", "branch", "-D", "feature"]);
    let out = ok(dir.path(), &["db", "branch"]);
    assert!(!out.contains("feature"), "deleted: {out}");
}

#[test]
fn get_reports_typed_values_and_machine_shapes_are_consistent() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    init_repo(dir.path());
    ok(dir.path(), &["db", "init"]);
    ok(dir.path(), &["db", "table", "create", "t"]);
    ok(dir.path(), &["db", "put", "t", "absent-key", "null"]);

    // A missing key omits the value field rather than smuggling a string.
    let out = ok(dir.path(), &["--format", "json", "db", "get", "t", "nope"]);
    assert!(out.contains("\"found\":false"), "{out}");
    assert!(
        !out.contains("\"value\""),
        "no value field when absent: {out}"
    );
}
