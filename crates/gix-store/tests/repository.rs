//! [`Store`] behavior that genuinely needs a real, on-disk `gix` repository:
//! the on-disk ref layout, a real `git fetch` reachability repro,
//! cross-thread concurrency against the on-disk backend, and the
//! `git ls-tree`-shaped plumbing assertions against a committed tree.
//! Everything that can run against [`gix_store::MemoryRefStore`] instead
//! lives in `tests/store.rs`.

use std::collections::HashSet;

use facet::Facet;
use facet_git_tree::schema_of;
use facet_value::value;
use gix_store::{Layout, RefPrefix, RefSegment, RepoStore};
use test_support::init_repo;

fn seg(s: &str) -> RefSegment {
    RefSegment::new(s).unwrap()
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
        .put(&seg("c"), &value!({ "n": 1 }))
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
        .put(&seg("a"), &value!({ "n": 1 }))
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
        .put(&seg("carbonara"), &carbonara)
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
            .get(&seg("carbonara"))
            .unwrap(),
        Some(carbonara)
    );
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
                        .put(&seg("c"), &value!({ "n": (n) }))
                        .unwrap();
                }
            });
        }
    });

    let repo = gix::open(path).unwrap();
    let store = RepoStore::open(&repo);
    let history = store.dynamic(seg("counter")).history(&seg("c")).unwrap();

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
/// are the stored struct's field names; and the commit message still ends
/// with a human-readable `Schema: <oid>` trailer naming the schema commit,
/// even though no reader needs it.
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
        .put(&seg("carbonara"), &carbonara)
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

    let message = commit_obj.message_raw_sloppy().to_string();
    assert!(
        message.ends_with(&format!("Schema: {schema_commit}\n")),
        "commit message should end with the Schema: trailer, got {message:?}"
    );
}
