//! Drive the built `git-store` binary against a temp repo, exactly as
//! `git store …` would.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use facet::Facet;
use facet_git_tree::schema_of;
use test_support::init_repo;

const BIN: &str = env!("CARGO_BIN_EXE_git-store");

#[derive(Facet)]
struct Recipe {
    title: String,
    serves: u32,
    steps: Vec<String>,
}

/// Run the binary in `dir`, feeding `stdin`, returning `(stdout, stderr, ok)`.
fn run(dir: &Path, stdin: Option<&str>, args: &[&str]) -> (String, String, bool) {
    let mut child = Command::new(BIN)
        .current_dir(dir)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let out = child.wait_with_output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

#[test]
fn store_get_list_and_remove() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let path = dir.path();

    let schema = facet_json::to_string(&schema_of::<Recipe>().unwrap()).unwrap();
    let (_, err, ok) = run(path, Some(&schema), &["schema", "put", "recipe"]);
    assert!(ok, "schema put failed: {err}");

    // `git store put recipe carbonara` — name positional, content from stdin.
    let recipe = r#"{"title":"Carbonara","serves":4,"steps":["boil","fry"]}"#;
    let (_, err, ok) = run(path, Some(recipe), &["put", "recipe", "carbonara"]);
    assert!(ok, "put failed: {err}");

    let (out, err, ok) = run(path, None, &["get", "recipe", "carbonara"]);
    assert!(ok, "get failed: {err}");
    assert!(out.contains("\"serves\": 4"), "get output: {out}");
    assert!(
        out.contains("\"title\": \"Carbonara\""),
        "get output: {out}"
    );

    // A second version, then read the prior one via a revision folded into the
    // name (`carbonara~1`), and the same via the explicit `@` separator.
    let v2 = r#"{"title":"Carbonara","serves":6,"steps":["boil","fry"]}"#;
    let (_, err, ok) = run(path, Some(v2), &["put", "recipe", "carbonara"]);
    assert!(ok, "second put failed: {err}");

    let (out, err, ok) = run(path, None, &["get", "recipe", "carbonara"]);
    assert!(ok, "get failed: {err}");
    assert!(out.contains("\"serves\": 6"), "current version: {out}");

    let (out, err, ok) = run(path, None, &["get", "recipe", "carbonara~1"]);
    assert!(ok, "get carbonara~1 failed: {err}");
    assert!(out.contains("\"serves\": 4"), "prior version: {out}");

    let (out, err, ok) = run(path, None, &["get", "recipe", "carbonara@~1"]);
    assert!(ok, "get carbonara@~1 failed: {err}");
    assert!(out.contains("\"serves\": 4"), "prior version via @: {out}");

    // Bare `git store` lists kinds; `ls` is an alias for `list`.
    let (out, _, ok) = run(path, None, &[]);
    assert!(ok);
    assert_eq!(out.trim(), "recipe");

    let (out, _, ok) = run(path, None, &["ls", "recipe"]);
    assert!(ok);
    assert_eq!(out.trim(), "carbonara");

    let (out, _, ok) = run(path, None, &["schema", "show", "recipe"]);
    assert!(ok);
    assert!(out.contains("serves: uint"), "schema show output: {out}");

    let (_, _, ok) = run(path, None, &["rm", "recipe", "carbonara"]);
    assert!(ok, "rm failed");
    let (_, _, ok) = run(path, None, &["get", "recipe", "carbonara"]);
    assert!(!ok, "get after rm should fail");
}

#[test]
fn interactive_put_builds_value_from_prompts() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let path = dir.path();

    let schema = facet_json::to_string(&schema_of::<Recipe>().unwrap()).unwrap();
    let (_, err, ok) = run(path, Some(&schema), &["schema", "put", "recipe"]);
    assert!(ok, "schema put failed: {err}");

    // One answer per prompt: title, serves, then the steps list (add? / value)
    // until a `n` closes it.
    let answers = "Carbonara\n4\ny\nboil\ny\nfry\nn\n";
    let (_, err, ok) = run(path, Some(answers), &["put", "recipe", "carbonara", "-i"]);
    assert!(ok, "interactive put failed: {err}");

    let (out, err, ok) = run(path, None, &["get", "recipe", "carbonara"]);
    assert!(ok, "get failed: {err}");
    assert!(
        out.contains("\"title\": \"Carbonara\""),
        "get output: {out}"
    );
    assert!(out.contains("\"serves\": 4"), "get output: {out}");
    assert!(out.contains("\"boil\""), "get output: {out}");
    assert!(out.contains("\"fry\""), "get output: {out}");
}

#[test]
fn interactive_schema_builds_kind_from_prompts() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let path = dir.path();

    // Root struct with a string, a u64 (default width), and a bool field.
    // Each field is gated by an "add a field?" confirm; a final `n` ends them.
    let answers = "struct\ny\ntitle\nstring\ny\nserves\nuint\nu64\ny\ndone\nbool\nn\n";
    let (_, err, ok) = run(path, Some(answers), &["schema", "put", "task", "-i"]);
    assert!(ok, "interactive schema put failed: {err}");

    let (out, _, ok) = run(path, None, &["schema", "show", "task"]);
    assert!(ok);
    assert!(out.contains("title: string"), "schema show: {out}");
    assert!(out.contains("serves: uint"), "schema show: {out}");
    assert!(out.contains("done: bool"), "schema show: {out}");

    // The built schema accepts a conforming value round-trip.
    let value = r#"{"title":"ship","serves":3,"done":true}"#;
    let (_, err, ok) = run(path, Some(value), &["put", "task", "release"]);
    assert!(ok, "put against built schema failed: {err}");
    let (out, err, ok) = run(path, None, &["get", "task", "release"]);
    assert!(ok, "get failed: {err}");
    assert!(out.contains("\"serves\": 3"), "get output: {out}");
    assert!(out.contains("\"done\": true"), "get output: {out}");
}

#[test]
fn unknown_kind_reports_no_schema() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());

    let (_, err, ok) = run(dir.path(), Some("{}"), &["put", "ghost", "x"]);
    assert!(!ok);
    assert!(err.contains("no schema published"), "stderr: {err}");
}
