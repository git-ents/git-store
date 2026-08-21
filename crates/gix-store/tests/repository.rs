//! [`Store`] behavior that genuinely needs a real, on-disk `gix` repository:
//! the on-disk ref layout, a real `git fetch` reachability repro,
//! cross-thread concurrency against the on-disk backend, and the
//! `git ls-tree`-shaped plumbing assertions against a committed tree.
//! Everything that can run against [`gix_store::MemoryRefStore`] instead
//! lives in `tests/store.rs`.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Barrier};

use facet::Facet;
use facet_git_tree::schema_of;
use facet_value::value;
use gix_store::{
    DeleteResult, EntityState, Layout, RefPath, RefPrefix, RefSegment, RepoStore, SignatureBytes,
    Signer,
};
use test_support::init_repo;

fn seg(s: &str) -> RefSegment {
    RefSegment::new(s).unwrap()
}

fn entity(s: &str) -> RefPath {
    RefPath::new(s).unwrap()
}

#[derive(Facet)]
struct Recipe {
    title: String,
    serves: u32,
    steps: Vec<String>,
}

#[derive(Facet)]
struct Counter {
    n: u32,
}

#[test]
fn default_open_still_uses_refs_store_and_refs_schema() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let repo = gix::open(dir.path()).unwrap();
    let store = RepoStore::open(&repo);

    store
        .dynamic(seg("counter"))
        .schema()
        .put(&schema_of::<Counter>().unwrap())
        .unwrap();
    store
        .dynamic(seg("counter"))
        .put(&entity("c"), &value!({ "n": 1 }))
        .unwrap();

    assert!(
        repo.find_reference("refs/schema/counter").is_ok(),
        "schema ref should land under the default refs/schema prefix"
    );
    assert!(
        repo.find_reference("refs/store/counter/c").is_ok(),
        "data ref should land under the default refs/store prefix"
    );
}

#[test]
fn custom_layout_lands_refs_under_the_custom_namespace() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let repo = gix::open(dir.path()).unwrap();
    let store = RepoStore::open_with_layout(
        &repo,
        Layout {
            data: RefPrefix::new("refs/meta/rules").unwrap(),
            schema: RefPrefix::new("refs/meta/rules-schema").unwrap(),
        },
    );

    store
        .dynamic(seg("module"))
        .schema()
        .put(&schema_of::<Counter>().unwrap())
        .unwrap();
    store
        .dynamic(seg("module"))
        .put(&entity("a"), &value!({ "n": 1 }))
        .unwrap();

    assert!(repo.find_reference("refs/meta/rules-schema/module").is_ok());
    assert!(repo.find_reference("refs/meta/rules/module/a").is_ok());
    assert!(
        repo.try_find_reference("refs/store/module/a")
            .unwrap()
            .is_none()
    );
    assert!(
        repo.try_find_reference("refs/schema/module")
            .unwrap()
            .is_none()
    );
}

/// The repro from issue `0b4a9b27`: a data ref fetched into a fresh
/// repository that has *no* `refs/schema/*` at all — not evolved past it,
/// never had it — must still read back. Before subtree binding, the schema
/// commit was only named in a `Schema:` trailer, unreachable from the data
/// commit, so a real `git fetch` of just the data ref left it looking present
/// (`git ls-tree` works) but unreadable (`Kind::get_at` fails looking up an
/// object nothing brought along). The fix makes the schema part of the data
/// commit's own tree, so ordinary tree reachability — which `git fetch`
/// already respects — carries it along for free.
#[test]
fn fetched_data_ref_reads_back_without_any_schema_ref() {
    #[derive(Facet)]
    struct Recipe {
        title: String,
        serves: u32,
    }

    let origin_dir = tempfile::tempdir().unwrap();
    init_repo(origin_dir.path());
    let origin = gix::open(origin_dir.path()).unwrap();
    let origin_store = RepoStore::open(&origin);

    origin_store
        .dynamic(seg("recipe"))
        .schema()
        .put(&schema_of::<Recipe>().unwrap())
        .unwrap();
    let carbonara = value!({ "title": "Carbonara", "serves": 4 });
    let commit = origin_store
        .dynamic(seg("recipe"))
        .put(&entity("carbonara"), &carbonara)
        .unwrap();

    // A fresh repository, never told about `recipe`'s schema at all.
    let consumer_dir = tempfile::tempdir().unwrap();
    init_repo(consumer_dir.path());

    // A real `git fetch` of exactly the data ref — nothing under
    // `refs/schema/*` — exactly as a refspec-scoped sync (a mirror, a partial
    // clone, `git push refs/store/*`) would do.
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(consumer_dir.path())
        .arg("fetch")
        .arg(origin_dir.path())
        .arg("refs/store/recipe/carbonara:refs/store/recipe/carbonara")
        .status()
        .expect("run git fetch");
    assert!(status.success(), "git fetch failed");

    let consumer = gix::open(consumer_dir.path()).unwrap();
    assert!(
        consumer
            .try_find_reference("refs/schema/recipe")
            .unwrap()
            .is_none(),
        "the repro requires no refs/schema/* to have been fetched"
    );

    let consumer_store = RepoStore::open(&consumer);
    assert_eq!(
        consumer_store
            .dynamic(seg("recipe"))
            .get_at(commit)
            .unwrap(),
        carbonara
    );
    // The data ref itself was fetched, so the ordinary ref-based read works too.
    assert_eq!(
        consumer_store
            .dynamic(seg("recipe"))
            .get(&entity("carbonara"))
            .unwrap(),
        Some(carbonara)
    );
}

#[test]
fn fetched_tombstone_is_deleted_without_schema_ref_or_index() {
    let origin_dir = tempfile::tempdir().unwrap();
    init_repo(origin_dir.path());
    let origin = gix::open(origin_dir.path()).unwrap();
    let origin_store = RepoStore::open(&origin);
    let kind = origin_store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    let id = kind.put_entity(&value!({ "n": 1 })).unwrap();
    let tombstone_commit = match kind.delete_entity(id).unwrap() {
        DeleteResult::Deleted(entry) => entry.commit,
        other => panic!("expected a new tombstone, got {other:?}"),
    };

    // The tombstone is a normal bound frame, not an empty tree.
    let root = origin
        .find_commit(tombstone_commit)
        .unwrap()
        .tree()
        .unwrap();
    let names: Vec<_> = root
        .iter()
        .map(|entry| entry.unwrap().inner.filename.to_string())
        .collect();
    assert_eq!(names, vec!["schema", "value"]);

    let consumer_dir = tempfile::tempdir().unwrap();
    init_repo(consumer_dir.path());
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(consumer_dir.path())
        .arg("fetch")
        .arg(origin_dir.path())
        .arg(format!("refs/store/counter/{id}:refs/store/counter/{id}"))
        .status()
        .expect("run git fetch");
    assert!(status.success(), "git fetch failed");

    let consumer = gix::open(consumer_dir.path()).unwrap();
    assert!(
        consumer
            .try_find_reference("refs/schema/counter")
            .unwrap()
            .is_none()
    );

    match RepoStore::open(&consumer)
        .dynamic(seg("counter"))
        .read_entity(id)
        .unwrap()
    {
        EntityState::Deleted(entry) => {
            assert_eq!(entry.commit, tombstone_commit);
            assert_eq!(entry.tombstone.entity_id(), Some(id));
        }
        other => panic!("expected fetched tombstone, got {other:?}"),
    }
}

#[test]
fn concurrent_rewrite_and_delete_leave_a_name_in_an_explicit_state() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let repo = gix::open(dir.path()).unwrap();
    let store = RepoStore::open(&repo);
    let kind = store.dynamic(seg("counter"));
    kind.schema().put(&schema_of::<Counter>().unwrap()).unwrap();
    let name = entity("legacy/counter");
    kind.put(&name, &value!({ "n": 1 })).unwrap();

    let barrier = Arc::new(Barrier::new(2));
    let path = dir.path();
    std::thread::scope(|scope| {
        let write_barrier = Arc::clone(&barrier);
        let write_name = name.clone();
        scope.spawn(move || {
            let repo = gix::open(path).unwrap();
            let store = RepoStore::open(&repo);
            write_barrier.wait();
            store
                .dynamic(seg("counter"))
                .put(&write_name, &value!({ "n": 2 }))
                .unwrap();
        });

        let delete_barrier = Arc::clone(&barrier);
        let delete_name = name.clone();
        scope.spawn(move || {
            let repo = gix::open(path).unwrap();
            let store = RepoStore::open(&repo);
            delete_barrier.wait();
            let _ = store
                .dynamic(seg("counter"))
                .delete_name(&delete_name)
                .unwrap();
        });
    });

    // Either order is a valid outcome, but the name must be either live or
    // explicitly deleted -- never absent, and never disagreeing with the index.
    match kind.read(name.clone()).unwrap() {
        EntityState::Present(entry) => {
            assert_eq!(kind.list().unwrap(), vec![name.clone()]);
            assert_eq!(
                kind.list_entries().unwrap(),
                vec![(name.clone(), entry.commit)]
            );
        }
        EntityState::Deleted(entry) => {
            assert!(kind.list().unwrap().is_empty());
            assert_eq!(
                kind.list_entries().unwrap(),
                vec![(name.clone(), entry.commit)]
            );
        }
        other => panic!("write/delete race must leave an explicit state, got {other:?}"),
    }
}

#[test]
fn concurrent_writers_land_a_linear_history() {
    const THREADS: usize = 3;
    const WRITES: usize = 10;

    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    {
        let repo = gix::open(dir.path()).unwrap();
        RepoStore::open(&repo)
            .dynamic(seg("counter"))
            .schema()
            .put(&schema_of::<Counter>().unwrap())
            .unwrap();
    }

    let path = dir.path();
    std::thread::scope(|scope| {
        for t in 0..THREADS {
            scope.spawn(move || {
                // Each thread is its own writer: a separate repository handle
                // racing on the one ref, exactly as separate processes would.
                let repo = gix::open(path).unwrap();
                let store = RepoStore::open(&repo);
                for i in 0..WRITES {
                    let n = (t * WRITES + i) as u32;
                    store
                        .dynamic(seg("counter"))
                        .put(&entity("c"), &value!({ "n": (n) }))
                        .unwrap();
                }
            });
        }
    });

    let repo = gix::open(path).unwrap();
    let store = RepoStore::open(&repo);
    let history = store.dynamic(seg("counter")).history(&entity("c")).unwrap();

    // Every write committed forward: none was lost to a race, and the chain is
    // linear (distinct commits, one per write).
    assert_eq!(history.len(), THREADS * WRITES);
    let distinct: HashSet<_> = history.iter().collect();
    assert_eq!(distinct.len(), history.len());

    // Every distinct value survives somewhere in the history.
    let stored: HashSet<u32> = history
        .iter()
        .map(|&id| {
            let v = store.dynamic(seg("counter")).get_at(id).unwrap();
            v.as_object()
                .unwrap()
                .get("n")
                .unwrap()
                .as_number()
                .unwrap()
                .to_u128()
                .unwrap() as u32
        })
        .collect();
    let expected: HashSet<u32> = (0..(THREADS * WRITES) as u32).collect();
    assert_eq!(stored, expected);
}

/// The claim subtree binding exists to make true: stock `git` plumbing —
/// `git ls-tree`, not just [`gix_store`] — can read a data commit. The root
/// tree has exactly `schema` and `value`, both trees; `value`'s own entries
/// are the stored struct's field names; and newly written commits contain no
/// schema or provenance trailers.
#[test]
fn committed_tree_has_the_plumbing_shape_stock_git_expects() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let repo = gix::open(dir.path()).unwrap();
    let store = RepoStore::open(&repo);

    let schema_commit = store
        .dynamic(seg("recipe"))
        .schema()
        .put(&schema_of::<Recipe>().unwrap())
        .unwrap();
    let carbonara = value!({ "title": "Carbonara", "serves": 4, "steps": ["boil", "fry"] });
    let commit = store
        .dynamic(seg("recipe"))
        .put(&entity("carbonara"), &carbonara)
        .unwrap();

    let commit_obj = repo.find_commit(commit).unwrap();
    let root = commit_obj.tree().unwrap();

    let mut top_level: Vec<String> = root
        .iter()
        .map(|e| e.unwrap().inner.filename.to_string())
        .collect();
    top_level.sort();
    assert_eq!(top_level, vec!["schema".to_owned(), "value".to_owned()]);

    let schema_entry = root.find_entry("schema").unwrap();
    assert!(
        schema_entry.inner.mode.is_tree(),
        "schema entry must be a tree"
    );
    let schema_commit_obj = repo.find_commit(schema_commit).unwrap();
    assert_eq!(
        schema_entry.object_id(),
        schema_commit_obj.tree().unwrap().id().detach(),
        "the document must embed the exact schema tree used to publish the schema"
    );
    let value_entry = root.find_entry("value").unwrap();
    assert!(
        value_entry.inner.mode.is_tree(),
        "value entry must be a tree: Recipe is struct-shaped"
    );

    let value_tree = repo.find_tree(value_entry.object_id()).unwrap();
    let mut fields: Vec<String> = value_tree
        .iter()
        .map(|e| e.unwrap().inner.filename.to_string())
        .collect();
    fields.sort();
    assert_eq!(
        fields,
        vec!["serves".to_owned(), "steps".to_owned(), "title".to_owned()]
    );

    let schema_message = repo
        .find_commit(schema_commit)
        .unwrap()
        .message_raw_sloppy()
        .to_string();
    let message = commit_obj.message_raw_sloppy().to_string();
    for (kind, message) in [("schema", schema_message), ("data", message)] {
        assert!(
            !message.lines().any(|line| {
                line.starts_with("Schema:")
                    || line.starts_with("Schema-Version:")
                    || line.starts_with("Ents-Ref:")
            }),
            "{kind} commit should contain no schema/provenance trailers, got {message:?}"
        );
    }
}

/// Signs by shelling out to `ssh-keygen -Y sign`, so the bytes on the commit are
/// byte-for-byte what git's own ssh signing backend would have produced.
struct SshKeygen {
    key: PathBuf,
}

impl Signer for SshKeygen {
    type Error = std::io::Error;

    fn sign(&self, bytes: &[u8]) -> Result<SignatureBytes, Self::Error> {
        // `-n git` is git's own SSHSIG namespace, and reading the payload from
        // stdin keeps the signed bytes off disk.
        let mut child = Command::new("ssh-keygen")
            .arg("-Y")
            .arg("sign")
            .arg("-q")
            .arg("-n")
            .arg("git")
            .arg("-f")
            .arg(&self.key)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        child.stdin.take().expect("piped stdin").write_all(bytes)?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            return Err(std::io::Error::other("ssh-keygen -Y sign failed"));
        }
        Ok(SignatureBytes::from(out.stdout))
    }
}

/// An ed25519 key at `<dir>/key`, plus an allowed-signers file trusting it for
/// `test@example.com` — the identity [`init_repo`] configures, and the principal
/// git hands `ssh-keygen -Y verify`.
fn ssh_key(dir: &Path) -> PathBuf {
    let key = dir.join("key");
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "test", "-f"])
        .arg(&key)
        .status()
        .expect("run ssh-keygen");
    assert!(status.success(), "ssh-keygen keygen failed");

    let public = std::fs::read_to_string(dir.join("key.pub")).expect("read public key");
    std::fs::write(
        dir.join("allowed_signers"),
        format!("test@example.com namespaces=\"git\" {public}"),
    )
    .expect("write allowed signers");
    key
}

/// The `gpgsig` transport exists to make true: real `git` verifies a commit this
/// store wrote. Nothing here knows what a signature is — the [`Signer`] emits an
/// SSHSIG block, the store carries it in git's header, and `git verify-commit`
/// is the oracle that says the framing and the signed payload are both git's.
#[test]
fn a_signed_commit_verifies_under_real_git() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let key = ssh_key(dir.path());

    let mut config = std::fs::OpenOptions::new()
        .append(true)
        .open(dir.path().join(".git/config"))
        .expect("open config");
    writeln!(
        config,
        "[gpg]\n\tformat = ssh\n[gpg \"ssh\"]\n\tallowedSignersFile = {}",
        dir.path().join("allowed_signers").display()
    )
    .expect("write config");
    drop(config);

    let repo = gix::open(dir.path()).unwrap();
    let store = RepoStore::open(&repo).with_signer(SshKeygen { key });
    store
        .dynamic(seg("counter"))
        .schema()
        .put(&schema_of::<Counter>().unwrap())
        .unwrap();
    let commit = store
        .dynamic(seg("counter"))
        .put(&entity("c"), &value!({ "n": 1 }))
        .unwrap();

    let out = Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .arg("verify-commit")
        .arg("-v")
        .arg(commit.to_string())
        .output()
        .expect("run git verify-commit");
    assert!(
        out.status.success(),
        "git verify-commit rejected a store-written commit: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let report = String::from_utf8_lossy(&out.stderr);
    assert!(
        report.contains("Good \"git\" signature"),
        "expected a good-signature report, got {report:?}"
    );

    // And the same bytes come back out of the header, un-folded.
    let signature = store.signature(commit).unwrap().expect("a signature");
    assert!(
        signature
            .as_bytes()
            .starts_with(b"-----BEGIN SSH SIGNATURE-----\n"),
        "the header should hold the armored block verbatim"
    );
}
