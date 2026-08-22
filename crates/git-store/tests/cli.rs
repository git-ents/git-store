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
    let (stdout, stderr, code) = run_with_status(dir, stdin, args);
    (stdout, stderr, code == 0)
}

/// Run the binary and retain its numeric exit status for classification tests.
fn run_with_status(dir: &Path, stdin: Option<&str>, args: &[&str]) -> (String, String, i32) {
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
        out.status.code().unwrap_or(-1),
    )
}

fn run_with_layout(
    dir: &Path,
    stdin: Option<&str>,
    data_prefix: &str,
    schema_prefix: &str,
    args: &[&str],
) -> (String, String, bool) {
    let mut full = vec![
        "--data-prefix",
        data_prefix,
        "--schema-prefix",
        schema_prefix,
    ];
    full.extend_from_slice(args);
    run(dir, stdin, &full)
}

fn git_bytes(path: &Path, args: &[&str], input: Option<&[u8]>) -> Vec<u8> {
    let mut command = Command::new("git");
    command.current_dir(path).args(args);
    command.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(input) = input {
        child.stdin.take().unwrap().write_all(input).unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

/// Remove the current schema's `kind` and pin entries and rewrite all leaf
/// blobs without their current framing newline, producing a pre-kind fixture.
fn legacy_schema_tree(path: &Path, tree: &str, root: bool) -> String {
    let listing = git_bytes(path, &["ls-tree", tree], None);
    let mut entries = Vec::new();
    for line in String::from_utf8(listing).unwrap().lines() {
        let (metadata, name) = line.split_once('\t').unwrap();
        let mut fields = metadata.split_whitespace();
        let mode = fields.next().unwrap();
        let kind = fields.next().unwrap();
        let oid = fields.next().unwrap();
        if root && !matches!(name, "root" | "defs") {
            continue;
        }
        let child = if kind == "blob" {
            let bytes = git_bytes(path, &["cat-file", "blob", oid], None);
            let bytes = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
            String::from_utf8(git_bytes(
                path,
                &["hash-object", "-w", "--stdin"],
                Some(bytes),
            ))
            .unwrap()
            .trim()
            .to_owned()
        } else if kind == "tree" {
            legacy_schema_tree(path, oid, false)
        } else {
            panic!("unexpected schema entry kind {kind:?}");
        };
        entries.push(format!("{mode} {kind} {child}\t{name}\n"));
    }
    entries.sort();
    String::from_utf8(git_bytes(
        path,
        &["mktree"],
        Some(entries.concat().as_bytes()),
    ))
    .unwrap()
    .trim()
    .to_owned()
}

#[test]
fn store_get_list_and_remove() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let path = dir.path();

    let schema = facet_json::to_string(&schema_of::<Recipe>().unwrap()).unwrap();
    let (_, err, ok) = run(path, Some(&schema), &["schema", "put", "recipe"]);
    assert!(ok, "schema put failed: {err}");

    // `git store put recipe carbonara --legacy-name` — explicit compatibility
    // path; the name is positional and content comes from stdin.
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

    // Bare `git store` prints help, like any clap app, rather than listing
    // kinds; `list` (alias `ls`) is the explicit way to do that.
    let (out, err, _) = run(path, None, &[]);
    assert!(out.contains("Usage: git-store") || err.contains("Usage: git-store"));

    let (out, _, ok) = run(path, None, &["list"]);
    assert!(ok);
    assert_eq!(out.trim(), "recipe");

    let (out, _, ok) = run(path, None, &["ls", "recipe"]);
    assert!(ok);
    let listed: Vec<_> = out.lines().collect();
    assert_eq!(listed, ["carbonara"]);

    let (out, _, ok) = run(path, None, &["schema", "show", "recipe"]);
    assert!(ok);
    assert!(out.contains("serves: uint"), "schema show output: {out}");

    let refs_before = Command::new("git")
        .current_dir(path)
        .args(["for-each-ref", "--format=%(refname)", "refs/store/recipe"])
        .output()
        .unwrap();
    assert!(
        refs_before.status.success(),
        "could not inspect refs before rm"
    );
    let refs_before = String::from_utf8_lossy(&refs_before.stdout).into_owned();

    let (out, err, ok) = run(path, None, &["rm", "recipe", "carbonara"]);
    assert!(ok, "rm failed: {err}");
    assert_eq!(out.trim(), "deleted recipe/carbonara");

    // Deletion publishes a tombstone rather than removing either the legacy
    // named ref or the canonical ref that backs it.
    let refs_after = Command::new("git")
        .current_dir(path)
        .args(["for-each-ref", "--format=%(refname)", "refs/store/recipe"])
        .output()
        .unwrap();
    assert!(
        refs_after.status.success(),
        "could not inspect refs after rm"
    );
    assert_eq!(
        String::from_utf8_lossy(&refs_after.stdout),
        refs_before,
        "rm must publish over existing refs, not hard-remove them"
    );
    let (out, err, ok) = run(path, None, &["ls", "recipe"]);
    assert!(ok, "list after rm failed: {err}");
    let listed_after_delete: Vec<_> = out.lines().collect();
    assert!(listed_after_delete.is_empty(), "list after rm: {out}");
    assert!(
        !listed_after_delete.contains(&"carbonara"),
        "deleted alias must not list as live: {out}"
    );

    let (out, err, ok) = run(path, None, &["get", "recipe", "carbonara"]);
    assert!(!ok, "get after rm should fail");
    assert!(out.is_empty(), "deleted get stdout: {out}");
    assert!(
        err.contains("entity recipe/carbonara is deleted"),
        "stderr: {err}"
    );
    assert!(
        !err.contains("no entity"),
        "deleted must not look absent: {err}"
    );

    // Repeating deletion is idempotent and reports Deleted separately from a
    // missing entity.
    let (out, err, ok) = run(path, None, &["rm", "recipe", "carbonara"]);
    assert!(ok, "repeated rm failed: {err}");
    assert_eq!(out.trim(), "already deleted recipe/carbonara");

    let (out, err, ok) = run(path, None, &["get", "recipe", "missing"]);
    assert!(!ok, "get of an absent entity should fail");
    assert!(out.is_empty(), "absent get stdout: {out}");
    assert!(err.contains("no entity recipe/missing"), "stderr: {err}");
    assert!(
        !err.contains("is deleted"),
        "absent must not look deleted: {err}"
    );

    let (out, err, ok) = run(path, None, &["rm", "recipe", "missing"]);
    assert!(!ok, "rm of an absent entity should fail");
    assert!(out.is_empty(), "absent rm stdout: {out}");
    assert!(err.contains("no entity recipe/missing"), "stderr: {err}");
    assert!(
        !err.contains("is deleted"),
        "absent rm must not look deleted: {err}"
    );
}

#[test]
fn interactive_put_builds_value_from_prompts() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let path = dir.path();

    let schema = facet_json::to_string(&schema_of::<Recipe>().unwrap()).unwrap();
    let (_, err, ok) = run(path, Some(&schema), &["schema", "put", "recipe"]);
    assert!(ok, "schema put failed: {err}");

    // A schema's fields are prompted in name order (`Node::Struct` is
    // name-keyed, not declaration-ordered): serves, then the steps list
    // (add? / value until a `n` closes it), then title.
    let answers = "4\ny\nboil\ny\nfry\nn\nCarbonara\n";
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
    // Each field is gated by an "add a field?" confirm, followed by its type
    // and then a "does this field have a default?" confirm (`n` for all
    // three here); a final `n` ends the field loop.
    let answers = "struct\ny\ntitle\nstring\nn\ny\nserves\nuint\nu64\nn\ny\ndone\nbool\nn\nn\n";
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

#[test]
fn failures_have_typed_exit_codes() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let path = dir.path();

    // Malformed command input is an invalid-input failure, before any schema
    // lookup or object write is attempted.
    let (_, err, code) = run_with_status(path, None, &["compile", "recipe", "{"]);
    assert_eq!(code, 2, "invalid JSON stderr: {err}");

    let schema = facet_json::to_string(&schema_of::<Recipe>().unwrap()).unwrap();
    let (_, err, code) = run_with_status(path, Some(&schema), &["schema", "put", "recipe"]);
    assert_eq!(code, 0, "schema put stderr: {err}");

    // A schema-directed mismatch is distinct from malformed JSON.
    let invalid_value = r#"{"title":"Carbonara","serves":"four","steps":[]}"#;
    let (_, err, code) = run_with_status(path, Some(invalid_value), &["compile", "recipe"]);
    assert_eq!(code, 5, "schema validation stderr: {err}");

    let (_, err, code) = run_with_status(path, None, &["get", "recipe", "missing"]);
    assert_eq!(code, 3, "not-found stderr: {err}");

    // Publishing the same prepared document twice with `absent` exercises the
    // public typed ApplyError chain rather than its human-readable message.
    let value = r#"{"title":"Carbonara","serves":4,"steps":[]}"#;
    let (document, err, code) = run_with_status(path, Some(value), &["compile", "recipe"]);
    assert_eq!(code, 0, "compile stderr: {err}");
    let document = document.trim().to_owned();
    let publish_args = [
        "document",
        "publish",
        "recipe",
        document.as_str(),
        "--expected",
        "absent",
    ];
    let (_, err, code) = run_with_status(path, None, &publish_args);
    assert_eq!(code, 0, "first publish stderr: {err}");
    let (_, err, code) = run_with_status(path, None, &publish_args);
    assert_eq!(code, 4, "CAS stderr: {err}");
}

/// A hand-authored schema JSON document — exactly the shape every file under
/// `crates/git-store/schemas/*.json` has — carries an explicit `kind` but no
/// `version` key because there is no such field any more: the schema-schema pin
/// is a storage-layer splice `schema put` adds on write, not something a caller
/// declares. The publication kind is selected by the CLI/ref, and `schema get`
/// prints the normalized document.
#[test]
fn hand_authored_schema_json_publishes_with_no_version_key() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let path = dir.path();

    let book_schema = r#"{
        "kind": "input-name-is-replaced",
        "root": { "Ref": "Book" },
        "defs": {
            "Book": {
                "Struct": {
                    "title": { "node": "String", "has_default": false },
                    "year": { "node": "U16", "has_default": false }
                }
            }
        }
    }"#;
    let (_, err, ok) = run(path, Some(book_schema), &["schema", "put", "book"]);
    assert!(ok, "schema put failed: {err}");

    let (out, err, ok) = run(path, None, &["schema", "show", "book", "--json"]);
    assert!(ok, "schema show --json failed: {err}");
    assert!(
        !out.contains("\"version\""),
        "schema show output must carry no version key: {out}"
    );
    assert_eq!(json_string_field(&out, "kind"), "book");

    // And it accepts a conforming value, exactly as a schema published from
    // a `#[derive(Facet)]` type would.
    let value = r#"{"title":"Dune","year":1965}"#;
    let (_, err, ok) = run(path, Some(value), &["put", "book", "dune"]);
    assert!(ok, "put against hand-authored schema failed: {err}");
}

#[test]
fn schema_legacy_leaves_is_explicit_for_current_and_historical_reads() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let path = dir.path();

    let schema = facet_json::to_string(&schema_of::<Recipe>().unwrap()).unwrap();
    let (current_commit, err, ok) = run(path, Some(&schema), &["schema", "put", "recipe"]);
    assert!(ok, "schema put failed: {err}");
    let current_commit = current_commit.trim();
    let current_tree = String::from_utf8(git_bytes(
        path,
        &["rev-parse", "refs/schema/recipe^{tree}"],
        None,
    ))
    .unwrap()
    .trim()
    .to_owned();
    let legacy_tree = legacy_schema_tree(path, &current_tree, true);
    let legacy_commit = String::from_utf8(git_bytes(
        path,
        &["commit-tree", &legacy_tree, "-m", "legacy schema"],
        None,
    ))
    .unwrap()
    .trim()
    .to_owned();
    let update = git_bytes(
        path,
        &[
            "update-ref",
            "refs/schema/recipe",
            &legacy_commit,
            current_commit,
        ],
        None,
    );
    assert!(
        update.is_empty(),
        "schema ref update unexpectedly wrote output"
    );

    let (_, err, ok) = run(path, None, &["schema", "get", "recipe"]);
    assert!(
        !ok,
        "strict schema get unexpectedly accepted legacy schema: {err}"
    );
    assert!(
        err.contains("no schema-schema pin") || err.contains("unpinned"),
        "strict diagnostic: {err}"
    );

    let (out, err, ok) = run(
        path,
        None,
        &["schema", "get", "recipe", "--legacy-leaves", "--json"],
    );
    assert!(ok, "legacy schema get failed: {err}");
    assert_eq!(json_string_field(&out, "kind"), "legacy-unknown");
    assert!(out.contains("\"root\""), "schema: {out}");
    assert!(out.contains("\"defs\""), "schema: {out}");

    let (out, err, ok) = run(
        path,
        None,
        &[
            "schema",
            "get",
            "recipe",
            "--at",
            &legacy_commit,
            "--legacy-leaves",
            "--json",
        ],
    );
    assert!(ok, "historical legacy schema get failed: {err}");
    assert!(out.contains("\"commit\""), "machine schema record: {out}");
    assert!(out.contains(&legacy_commit), "machine schema record: {out}");
    assert!(
        out.contains("\"schema_tree\""),
        "machine schema record: {out}"
    );

    let (_, err, ok) = run(
        path,
        None,
        &["schema", "inspect", "recipe", "--at", &legacy_commit],
    );
    assert!(
        !ok,
        "strict schema inspect unexpectedly accepted legacy schema: {err}"
    );

    let (out, err, ok) = run(
        path,
        None,
        &[
            "schema",
            "inspect",
            "recipe",
            "--at",
            &legacy_commit,
            "--legacy-leaves",
        ],
    );
    assert!(ok, "legacy schema inspect failed: {err}");
    assert!(out.contains("kind: legacy-unknown"), "inspect: {out}");
    assert!(out.contains("schema tree:"), "inspect: {out}");
    assert!(out.contains("serves: uint"), "inspect: {out}");
}

/// The actual hand-authored files under `crates/git-store/schemas/*.json`
/// load through `-F`.
#[test]
fn preexisting_schema_json_files_load_via_file_flag() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let path = dir.path();

    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for kind in ["book", "recipe", "task"] {
        let file = manifest_dir.join("schemas").join(format!("{kind}.json"));
        let (_, err, ok) = run(
            path,
            None,
            &["schema", "put", kind, "-F", file.to_str().unwrap()],
        );
        assert!(ok, "schema put {kind} from file failed: {err}");
    }
}

/// A nested entity name is one argument, not a special syntax: it stores,
/// lists as the path it was given, and lands at the ref that spells it out.
#[test]
fn entity_names_may_nest() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let path = dir.path();

    let schema = facet_json::to_string(&schema_of::<Recipe>().unwrap()).unwrap();
    let (_, err, ok) = run(path, Some(&schema), &["schema", "put", "recipe"]);
    assert!(ok, "schema put failed: {err}");

    let recipe = r#"{"title":"Carbonara","serves":4,"steps":["boil"]}"#;
    let (_, err, ok) = run(path, Some(recipe), &["put", "recipe", "italian/carbonara"]);
    assert!(ok, "nested put failed: {err}");

    let (out, err, ok) = run(path, None, &["get", "recipe", "italian/carbonara"]);
    assert!(ok, "nested get failed: {err}");
    assert!(out.contains("\"serves\": 4"), "get output: {out}");

    let (out, _, ok) = run(path, None, &["ls", "recipe"]);
    assert!(ok);
    assert_eq!(out.trim(), "italian/carbonara");

    let refs = Command::new("git")
        .current_dir(path)
        .args(["for-each-ref", "--format=%(refname)", "refs/store"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&refs.stdout).trim(),
        "refs/store/recipe/italian/carbonara"
    );
}

/// `compile <kind> <value>` compiles a document and prints its tree hash —
/// no ref is touched — and `cat <tree-ish>` decodes it back, addressed
/// purely by that hash.
#[test]
fn compile_prints_a_tree_hash_and_cat_decodes_it_back() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let path = dir.path();

    let schema = facet_json::to_string(&schema_of::<Recipe>().unwrap()).unwrap();
    let (_, err, ok) = run(path, Some(&schema), &["schema", "put", "recipe"]);
    assert!(ok, "schema put failed: {err}");

    let recipe = r#"{"title":"Carbonara","serves":4,"steps":["boil","fry"]}"#;
    let (out, err, ok) = run(path, None, &["compile", "recipe", recipe]);
    assert!(ok, "compile failed: {err}");
    let hash = out.trim();
    assert_eq!(hash.len(), 40, "expected a hex object id, got {hash:?}");

    // Nothing was written to any ref: `list` still reports no entities.
    let (out, _, ok) = run(path, None, &["ls", "recipe"]);
    assert!(ok);
    assert_eq!(out.trim(), "");

    let (out, err, ok) = run(path, None, &["cat", hash]);
    assert!(ok, "get failed: {err}");
    assert!(out.contains("\"serves\": 4"), "get output: {out}");
    assert!(
        out.contains("\"title\": \"Carbonara\""),
        "get output: {out}"
    );

    // Compiling byte-identical content twice is idempotent: same hash.
    let (out2, err, ok) = run(path, None, &["compile", "recipe", recipe]);
    assert!(ok, "second compile failed: {err}");
    assert_eq!(out2.trim(), hash, "compiling the same value twice diverged");
}

/// `compile <kind> [<value>]` never takes a name and `put <kind> <name>
/// [<value>]` always does: arity distinguishes the two verbs, so a malformed
/// inline value can no longer be mistaken for a named write — the ambiguity
/// the old single `put --legacy-name` verb had to guard against with a flag.
#[test]
fn compile_and_put_are_distinguished_by_arity_not_a_flag() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let path = dir.path();

    let schema = facet_json::to_string(&schema_of::<Recipe>().unwrap()).unwrap();
    let (_, err, ok) = run(path, Some(&schema), &["schema", "put", "recipe"]);
    assert!(ok, "schema put failed: {err}");

    // A bare token is malformed JSON: `compile` rejects it outright, and
    // touches no ref either way.
    let (out, err, ok) = run(path, None, &["compile", "recipe", "carbonara"]);
    assert!(!ok, "malformed JSON unexpectedly succeeded: {out}{err}");
    assert!(out.is_empty(), "unexpected stdout: {out}");
    assert!(err.contains("invalid JSON value"), "stderr: {err}");

    let (out, err, ok) = run(path, None, &["list", "recipe"]);
    assert!(ok, "list after rejected compile failed: {err}");
    assert!(out.is_empty(), "malformed input created an entity: {out}");

    // The same token as a `put` name, with content on stdin, is a legitimate
    // named write — no flag selects it, `put` always takes a name.
    let recipe = r#"{"title":"Carbonara","serves":4,"steps":["boil"]}"#;
    let (_, err, ok) = run(path, Some(recipe), &["put", "recipe", "carbonara"]);
    assert!(ok, "named put failed: {err}");
}

#[test]
fn compile_without_an_inline_value_still_reads_stdin() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let path = dir.path();

    let schema = facet_json::to_string(&schema_of::<Recipe>().unwrap()).unwrap();
    let (_, err, ok) = run(path, Some(&schema), &["schema", "put", "recipe"]);
    assert!(ok, "schema put failed: {err}");

    let recipe = r#"{"title":"Carbonara","serves":4,"steps":["boil","fry"]}"#;
    let (out, err, ok) = run(path, Some(recipe), &["compile", "recipe"]);
    assert!(ok, "compile failed: {err}");
    assert_eq!(out.trim().len(), 40);
    let hash = out.trim();

    let (out, err, ok) = run(path, None, &["cat", hash]);
    assert!(ok, "get failed: {err}");
    assert!(out.contains("\"serves\": 4"), "get output: {out}");
}

#[test]
fn doctor_accepts_a_valid_repository_and_current_meta_schema() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());

    let (out, err, ok) = run(dir.path(), None, &["doctor"]);
    assert!(ok, "doctor failed: {err}");
    assert_eq!(
        out.trim(),
        "git-store doctor: ok (object format: sha1; schema fixed point: valid)"
    );
    assert!(err.is_empty(), "doctor stderr: {err}");
}

#[test]
fn custom_layout_drives_schema_data_compatibility_and_ref_filtering() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let path = dir.path();
    let data_prefix = "refs/legacy-data";
    let schema_prefix = "refs/legacy-schema";
    let schema = facet_json::to_string(&schema_of::<Recipe>().unwrap()).unwrap();

    let (_, err, ok) = run_with_layout(
        path,
        Some(&schema),
        data_prefix,
        schema_prefix,
        &["schema", "put", "recipe"],
    );
    assert!(ok, "custom schema put failed: {err}");

    let value = r#"{"title":"Legacy","serves":3,"steps":["read"]}"#;
    let (_, err, ok) = run_with_layout(
        path,
        Some(value),
        data_prefix,
        schema_prefix,
        &["put", "recipe", "legacy"],
    );
    assert!(ok, "custom compatibility put failed: {err}");

    let (out, err, ok) =
        run_with_layout(path, None, data_prefix, schema_prefix, &["list", "recipe"]);
    assert!(ok, "custom list failed: {err}");
    assert_eq!(out.trim(), "legacy");

    let (out, err, ok) = run_with_layout(path, None, data_prefix, schema_prefix, &["ls"]);
    assert!(ok, "custom list failed: {err}");
    assert_eq!(out.trim(), "recipe");

    let (document_tree, err, ok) = run_with_layout(
        path,
        None,
        data_prefix,
        schema_prefix,
        &[
            "compile",
            "recipe",
            r#"{"title":"Prepared","serves":5,"steps":[]}"#,
        ],
    );
    assert!(ok, "custom pure compile failed: {err}");
    let document_tree = document_tree.trim().to_owned();
    let (_, err, ok) = run_with_layout(
        path,
        None,
        data_prefix,
        schema_prefix,
        &[
            "document",
            "publish",
            "recipe",
            &document_tree,
            "--alias",
            "prepared",
            "--expected",
            "absent",
        ],
    );
    assert!(ok, "custom prepared publication failed: {err}");

    let (out, err, ok) = run_with_layout(
        path,
        None,
        data_prefix,
        schema_prefix,
        &["get", "recipe", "legacy"],
    );
    assert!(ok, "custom compatibility get failed: {err}");
    assert!(out.contains("\"title\": \"Legacy\""), "get output: {out}");

    let (out, err, ok) = run_with_layout(
        path,
        None,
        data_prefix,
        schema_prefix,
        &["ref", "list", "--kind", "recipe"],
    );
    assert!(ok, "custom ref list failed: {err}");
    let refs: Vec<_> = out.lines().collect();
    assert!(
        refs.iter()
            .any(|reference| reference.starts_with("refs/legacy-data/recipe/")),
        "custom data ref missing from kind filter: {out}"
    );
    assert!(
        refs.iter()
            .any(|reference| reference.starts_with("refs/legacy-schema/recipe ")),
        "custom schema ref missing from kind filter: {out}"
    );

    let (out, err, ok) = run_with_layout(
        path,
        None,
        data_prefix,
        schema_prefix,
        &["ref", "list", "--prefix", "refs/store", "--kind", "recipe"],
    );
    assert!(ok, "default namespace ref list failed: {err}");
    assert!(
        out.is_empty(),
        "custom refs leaked into default namespace: {out}"
    );

    let (_, err, ok) = run_with_layout(
        path,
        None,
        data_prefix,
        schema_prefix,
        &["rm", "recipe", "legacy"],
    );
    assert!(ok, "custom compatibility rm failed: {err}");

    let (out, err, ok) = run(path, None, &["ref", "list", "--kind", "recipe"]);
    assert!(ok, "default-layout ref list failed: {err}");
    assert!(
        out.is_empty(),
        "custom refs visible without layout selection: {out}"
    );
}

#[test]
fn ref_list_kind_uses_selected_forge_data_prefix() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let path = dir.path();
    let schema = facet_json::to_string(&schema_of::<Recipe>().unwrap()).unwrap();

    let (_, err, ok) = run_with_layout(
        path,
        Some(&schema),
        "refs/forge",
        "refs/schema",
        &["schema", "put", "recipe"],
    );
    assert!(ok, "forge schema put failed: {err}");

    let value = r#"{"title":"Forge","serves":2,"steps":["test"]}"#;
    let (_, err, ok) = run_with_layout(
        path,
        Some(value),
        "refs/forge",
        "refs/schema",
        &["put", "recipe", "issue-1"],
    );
    assert!(ok, "forge compatibility put failed: {err}");

    let (out, err, ok) = run_with_layout(
        path,
        None,
        "refs/forge",
        "refs/schema",
        &["ref", "list", "--prefix", "refs/forge", "--kind", "recipe"],
    );
    assert!(ok, "forge ref list failed: {err}");
    assert!(
        out.lines()
            .any(|reference| reference.starts_with("refs/forge/recipe/issue-1 ")),
        "forge data ref missing from kind filter: {out}"
    );
}

#[test]
fn doctor_rejects_a_sha256_repository() {
    let dir = tempfile::tempdir().unwrap();
    let status = Command::new("git")
        .args(["init", "--quiet", "--object-format=sha256"])
        .arg(dir.path())
        .status()
        .unwrap();
    assert!(status.success(), "git cannot create a SHA-256 repository");

    let (out, err, ok) = run(dir.path(), None, &["doctor"]);
    assert!(!ok, "doctor unexpectedly accepted SHA-256: {out}{err}");
    assert!(out.is_empty(), "unexpected stdout: {out}");
    assert!(
        err.contains("unsupported Git object format") && err.contains("sha256"),
        "unexpected diagnostic: {err}"
    );
}

#[test]
fn embedded_document_reads_without_schema_ref_and_ignores_legacy_trailers() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let path = dir.path();

    let schema = facet_json::to_string(&schema_of::<Recipe>().unwrap()).unwrap();
    let (_, err, ok) = run(path, Some(&schema), &["schema", "put", "recipe"]);
    assert!(ok, "schema put failed: {err}");

    let recipe = r#"{"title":"Carbonara","serves":4,"steps":["boil","fry"]}"#;
    let message = "legacy write\n\nSchema: refs/schema/recipe\nSchema-Version: 1\nEnts-Ref: refs/store/recipe/carbonara";
    let (_, err, ok) = run(
        path,
        Some(recipe),
        &["put", "recipe", "carbonara", "-m", message],
    );
    assert!(!ok, "reserved trailers must be rejected: {err}");
    assert!(
        err.contains("reserved trailer line") && err.contains("Schema:"),
        "unexpected diagnostic: {err}"
    );

    // First create an ordinary store-written commit. The legacy object below
    // is then constructed with Git plumbing, rather than making `git-store`
    // emit a reserved trailer while testing that old objects remain readable.
    let (_, err, ok) = run(
        path,
        Some(recipe),
        &["put", "recipe", "carbonara", "-m", "legacy write"],
    );
    assert!(ok, "put failed: {err}");
    let tree = Command::new("git")
        .current_dir(path)
        .args(["rev-parse", "refs/store/recipe/carbonara^{tree}"])
        .output()
        .unwrap();
    assert!(tree.status.success(), "could not resolve the stored tree");
    let tree = String::from_utf8_lossy(&tree.stdout).trim().to_owned();
    let parent = Command::new("git")
        .current_dir(path)
        .args(["rev-parse", "refs/store/recipe/carbonara"])
        .output()
        .unwrap();
    assert!(
        parent.status.success(),
        "could not resolve the stored commit"
    );
    let parent = String::from_utf8_lossy(&parent.stdout).trim().to_owned();
    let legacy = Command::new("git")
        .current_dir(path)
        .arg("commit-tree")
        .arg(&tree)
        .arg("-p")
        .arg(&parent)
        .arg("-m")
        .arg(message)
        .output()
        .unwrap();
    assert!(
        legacy.status.success(),
        "could not construct legacy commit: {}",
        String::from_utf8_lossy(&legacy.stderr)
    );
    let legacy = String::from_utf8_lossy(&legacy.stdout).trim().to_owned();
    let update = Command::new("git")
        .current_dir(path)
        .arg("update-ref")
        .arg("refs/store/recipe/carbonara")
        .arg(&legacy)
        .arg(&parent)
        .status()
        .unwrap();
    assert!(update.success(), "could not install the legacy ref");

    let commit_message = Command::new("git")
        .current_dir(path)
        .args(["log", "-1", "--format=%B", "refs/store/recipe/carbonara"])
        .output()
        .unwrap();
    let commit_message = String::from_utf8_lossy(&commit_message.stdout);
    assert!(commit_message.contains("Schema: refs/schema/recipe"));
    assert!(commit_message.contains("Schema-Version: 1"));
    assert!(commit_message.contains("Ents-Ref: refs/store/recipe/carbonara"));

    let status = Command::new("git")
        .current_dir(path)
        .args(["update-ref", "-d", "refs/schema/recipe"])
        .status()
        .unwrap();
    assert!(status.success(), "removing the schema ref failed");
    let schema_ref = Command::new("git")
        .current_dir(path)
        .args(["show-ref", "--verify", "--quiet", "refs/schema/recipe"])
        .status()
        .unwrap();
    assert!(!schema_ref.success(), "schema ref unexpectedly remains");

    let (out, err, ok) = run(path, None, &["get", "recipe", "carbonara"]);
    assert!(ok, "get without schema ref failed: {err}");
    assert!(
        out.contains("\"title\": \"Carbonara\""),
        "get output: {out}"
    );
    assert!(out.contains("\"serves\": 4"), "get output: {out}");

    let (out, err, ok) = run(path, None, &["doctor"]);
    assert!(ok, "doctor without a schema ref failed: {err}");
    assert_eq!(
        out.trim(),
        "git-store doctor: ok (object format: sha1; schema fixed point: valid)"
    );
}

/// `cat` decodes through any commit/ref whose tree has the compiled
/// `{value/, schema/}` shape — the same shape a named, ref-addressed entity
/// (written through `put <kind> <name>`) has — since it is content-addressed
/// rather than resolving a name through a kind.
#[test]
fn cat_decodes_through_a_ref_naming_a_bound_commit() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let path = dir.path();

    let schema = facet_json::to_string(&schema_of::<Recipe>().unwrap()).unwrap();
    let (_, err, ok) = run(path, Some(&schema), &["schema", "put", "recipe"]);
    assert!(ok, "schema put failed: {err}");

    let recipe = r#"{"title":"Carbonara","serves":4,"steps":["boil"]}"#;
    let (_, err, ok) = run(path, Some(recipe), &["put", "recipe", "carbonara"]);
    assert!(ok, "named put failed: {err}");

    let (out, err, ok) = run(path, None, &["cat", "refs/store/recipe/carbonara"]);
    assert!(ok, "cat via ref failed: {err}");
    assert!(out.contains("\"serves\": 4"), "cat output: {out}");
}

/// `check <tree-ish> <schema>` validates a bare value tree against a schema
/// without decoding it: it succeeds for the `value` subtree a matching
/// document compiled to, and refuses the bound `{value/, schema/}` root
/// itself, which is not a value tree.
fn refs_snapshot(path: &Path) -> String {
    let output = Command::new("git")
        .current_dir(path)
        .args(["for-each-ref", "--format=%(refname)=%(objectname)"])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn json_string_field(json: &str, field: &str) -> String {
    let marker = format!("\"{field}\":\"");
    let start = json
        .find(&marker)
        .unwrap_or_else(|| panic!("missing {field} in {json}"))
        + marker.len();
    let end = json[start..]
        .find('"')
        .map(|offset| start + offset)
        .expect("unterminated JSON string field");
    json[start..end].to_owned()
}

#[test]
fn composable_plumbing_supports_json_ndjson_and_explicit_cas() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let path = dir.path();

    let schema = facet_json::to_string(&schema_of::<Recipe>().unwrap()).unwrap();
    let (schema_out, err, ok) = run(path, Some(&schema), &["schema", "put", "recipe"]);
    assert!(ok, "schema put failed: {err}");
    let schema_commit = schema_out.trim().to_owned();
    assert_eq!(schema_commit.len(), 40);

    let recipe = r#"{"title":"Carbonara","serves":4,"steps":["boil","fry"]}"#;
    let refs_before_reads = refs_snapshot(path);

    // This is the composable shell/jq-like sequence: each command consumes an
    // object id emitted by the previous command, and no migration command is
    // involved.
    let (encoded, err, ok) = run(
        path,
        Some(recipe),
        &["value", "encode", "--schema", &schema_commit, "--json"],
    );
    assert!(ok, "value encode failed: {err}");
    assert_eq!(encoded.lines().count(), 1);
    assert_eq!(json_string_field(&encoded, "status"), "ok");
    let value_tree = json_string_field(&encoded, "value_tree");
    assert_eq!(value_tree.len(), 40);

    let (decoded, err, ok) = run(
        path,
        None,
        &[
            "value",
            "decode",
            &value_tree,
            "--schema",
            &schema_commit,
            "--format",
            "ndjson",
        ],
    );
    assert!(ok, "value decode failed: {err}");
    assert_eq!(decoded.lines().count(), 1);
    assert!(decoded.contains("Carbonara"), "decoded record: {decoded}");
    assert!(
        decoded.contains("\"serves\":4"),
        "decoded record: {decoded}"
    );

    let (bound, err, ok) = run(
        path,
        None,
        &[
            "document",
            "bind",
            &value_tree,
            "--schema",
            &schema_commit,
            "--json",
        ],
    );
    assert!(ok, "document bind failed: {err}");
    let document_tree = json_string_field(&bound, "document_tree");
    assert_eq!(document_tree.len(), 40);

    let (inspection, err, ok) = run(
        path,
        None,
        &["document", "inspect", &document_tree, "--format", "ndjson"],
    );
    assert!(ok, "document inspect failed: {err}");
    assert_eq!(json_string_field(&inspection, "kind"), "bound");
    assert_eq!(json_string_field(&inspection, "schema_tree").len(), 40);

    let (object, err, ok) = run(path, None, &["object", "inspect", &document_tree, "--json"]);
    assert!(ok, "object inspect failed: {err}");
    assert_eq!(json_string_field(&object, "oid"), document_tree);
    assert_eq!(json_string_field(&object, "kind"), "tree");

    let (tree, err, ok) = run(
        path,
        None,
        &["object", "tree", &document_tree, "--format", "ndjson"],
    );
    assert!(ok, "object tree failed: {err}");
    assert!(tree.contains("schema"), "tree record: {tree}");
    assert!(tree.contains("value"), "tree record: {tree}");

    let (historical, err, ok) = run(
        path,
        None,
        &["schema", "get", "recipe", "--at", &schema_commit, "--json"],
    );
    assert!(ok, "historical schema get failed: {err}");
    assert_eq!(json_string_field(&historical, "commit"), schema_commit);
    assert_eq!(json_string_field(&historical, "kind"), "recipe");

    let (schema_info, err, ok) = run(
        path,
        None,
        &[
            "schema",
            "inspect",
            "recipe",
            "--at",
            &schema_commit,
            "--json",
        ],
    );
    assert!(ok, "historical schema inspect failed: {err}");
    assert_eq!(json_string_field(&schema_info, "schema_tree").len(), 40);

    let (refs, err, ok) = run(path, None, &["ref", "list", "--format", "ndjson"]);
    assert!(ok, "ref list failed: {err}");
    assert!(refs.contains("refs/schema/recipe"), "ref list: {refs}");
    let (resolved, err, ok) = run(
        path,
        None,
        &["ref", "resolve", "refs/schema/recipe", "--json"],
    );
    assert!(ok, "ref resolve failed: {err}");
    assert_eq!(json_string_field(&resolved, "oid"), schema_commit);
    assert_eq!(
        refs_before_reads,
        refs_snapshot(path),
        "read-only plumbing changed refs"
    );

    let (published, err, ok) = run(
        path,
        None,
        &[
            "document",
            "publish",
            "recipe",
            &document_tree,
            "--alias",
            "carbonara",
            "--expected",
            "absent",
            "--message",
            "publish carbonara",
            "--json",
        ],
    );
    assert!(ok, "document publish failed: {err}");
    let entity_id = json_string_field(&published, "id");
    let publication_commit = json_string_field(&published, "commit");
    assert_eq!(
        entity_id, document_tree,
        "entity identity must be the document tree"
    );
    assert_ne!(
        entity_id, publication_commit,
        "publication commit is not entity identity"
    );
    assert!(refs_snapshot(path).contains("refs/store/recipe/carbonara"));

    let (_, stale_err, stale_ok) = run(
        path,
        None,
        &[
            "document",
            "publish",
            "recipe",
            &document_tree,
            "--alias",
            "carbonara",
            "--expected",
            "absent",
        ],
    );
    assert!(!stale_ok, "stale CAS unexpectedly succeeded");
    assert!(
        stale_err.contains("compare-and-swap"),
        "stale CAS: {stale_err}"
    );

    // `entity delete` is id-addressed against the canonical (no-alias) ref;
    // publishing under an alias only advances that alias, not the id ref
    // (see `PublishOptions::with_alias`), so publish this document tree
    // canonically too before addressing it by id.
    let (_, err, ok) = run(
        path,
        None,
        &[
            "document",
            "publish",
            "recipe",
            &document_tree,
            "--expected",
            "absent",
            "--message",
            "publish canonically",
        ],
    );
    assert!(ok, "canonical document publish failed: {err}");

    let (deleted, err, ok) = run(
        path,
        None,
        &[
            "entity",
            "delete",
            "recipe",
            &entity_id,
            "--format",
            "ndjson",
        ],
    );
    assert!(ok, "entity delete failed: {err}");
    assert_eq!(json_string_field(&deleted, "status"), "deleted");
    let (already, err, ok) = run(
        path,
        None,
        &["entity", "delete", "recipe", &entity_id, "--json"],
    );
    assert!(ok, "repeated entity delete failed: {err}");
    assert_eq!(json_string_field(&already, "status"), "already_deleted");
}

#[test]
fn check_validates_a_value_tree_against_a_schema() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let path = dir.path();

    let schema = facet_json::to_string(&schema_of::<Recipe>().unwrap()).unwrap();
    let (_, err, ok) = run(path, Some(&schema), &["schema", "put", "recipe"]);
    assert!(ok, "schema put failed: {err}");

    let recipe = r#"{"title":"Carbonara","serves":4,"steps":["boil"]}"#;
    let (out, err, ok) = run(path, None, &["compile", "recipe", recipe]);
    assert!(ok, "compile failed: {err}");
    let hash = out.trim();

    let (_, err, ok) = run(path, None, &["check", &format!("{hash}:value"), "recipe"]);
    assert!(ok, "check of a conforming value tree failed: {err}");

    let (_, err, ok) = run(path, None, &["check", hash, "recipe"]);
    assert!(!ok, "check should refuse a non-value tree: {err}");
}

/// Parse `json` as a generic value, panicking with the offending text on
/// failure — the assertion `--format json` must satisfy for every command.
fn assert_parses_as_json(json: &str) {
    facet_json::from_str::<facet_value::Value>(json)
        .unwrap_or_else(|e| panic!("not valid JSON ({e}): {json:?}"));
}

/// Every porcelain command honors `--format`/`--json`, not just the
/// plumbing group: `ls`, `log`, and `schema show` all emit parseable JSON
/// under `--format json`, and `--json` is a working alias for it.
#[test]
fn porcelain_commands_honor_the_output_format() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let path = dir.path();

    let schema = facet_json::to_string(&schema_of::<Recipe>().unwrap()).unwrap();
    let (_, err, ok) = run(path, Some(&schema), &["schema", "put", "recipe", "--json"]);
    assert!(ok, "schema put failed: {err}");

    let recipe = r#"{"title":"Carbonara","serves":4,"steps":["boil"]}"#;
    let (_, err, ok) = run(path, Some(recipe), &["put", "recipe", "carbonara"]);
    assert!(ok, "put failed: {err}");

    let (out, err, ok) = run(path, None, &["ls", "--json"]);
    assert!(ok, "ls --json failed: {err}");
    assert_parses_as_json(out.trim());
    assert!(out.contains("recipe"), "ls --json output: {out}");

    let (out, err, ok) = run(path, None, &["ls", "recipe", "--format", "json"]);
    assert!(ok, "ls recipe --format json failed: {err}");
    assert_parses_as_json(out.trim());
    assert!(out.contains("carbonara"), "ls recipe --format json: {out}");

    let (out, err, ok) = run(path, None, &["log", "recipe", "carbonara", "--json"]);
    assert!(ok, "log --json failed: {err}");
    assert_parses_as_json(out.trim());

    let (out, err, ok) = run(path, None, &["schema", "show", "recipe", "--json"]);
    assert!(ok, "schema show --json failed: {err}");
    assert_parses_as_json(out.trim());
    assert_eq!(json_string_field(&out, "kind"), "recipe");

    let (out, err, ok) = run(path, None, &["get", "recipe", "carbonara", "--json"]);
    assert!(ok, "get --json failed: {err}");
    assert_parses_as_json(out.trim());

    let (out, err, ok) = run(path, None, &["doctor", "--json"]);
    assert!(ok, "doctor --json failed: {err}");
    assert_parses_as_json(out.trim());
    assert_eq!(json_string_field(&out, "status"), "ok");

    let (out, err, ok) = run(path, None, &["rm", "recipe", "carbonara", "--json"]);
    assert!(ok, "rm --json failed: {err}");
    assert_parses_as_json(out.trim());
    assert_eq!(json_string_field(&out, "status"), "deleted");
}
