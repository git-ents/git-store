//! Schema registration enforces the identity normal form: a kind whose
//! identity- or key-bearing subtree leaves the universe cannot be published.

use facet::Facet;
use facet_git_tree::{ObjectStore, UniverseError};
use gix_store::{Error, MemoryRefStore, RefSegment, Store, schema_of};

/// An anchor identity: three non-derivable coordinates, all in the universe.
#[derive(Facet)]
#[facet(facet_git_tree::identity_key)]
struct Identity {
    genesis_rev: [u8; 20],
    path: String,
    span: (u64, u64),
}

#[derive(Facet)]
struct Binding {
    identity: Identity,
    body: String,
}

/// An action whose `params` are an enum, which the normal form cannot express.
#[derive(Facet)]
struct Action {
    #[facet(facet_git_tree::identity_key)]
    key: ActionKey,
    output: String,
}

#[derive(Facet)]
struct ActionKey {
    executor: String,
    params: Params,
}

#[derive(Facet)]
#[repr(u8)]
enum Params {
    None,
    Diff,
}

fn store() -> Store<MemoryRefStore, ObjectStore> {
    Store::new(MemoryRefStore::new(), ObjectStore::default())
}

fn seg(name: &str) -> RefSegment {
    RefSegment::new(name).unwrap()
}

#[test]
fn a_marked_subtree_in_the_universe_registers() {
    let store = store();
    let doc = schema_of::<Binding>().unwrap();
    store.dynamic(seg("binding")).schema().put(&doc).unwrap();
    assert_eq!(
        store.dynamic(seg("binding")).schema().get().unwrap(),
        Some(doc)
    );
}

#[test]
fn a_marked_subtree_outside_the_universe_is_refused() {
    let store = store();
    let err = store
        .dynamic(seg("action"))
        .schema()
        .put(&schema_of::<Action>().unwrap())
        .unwrap_err();

    let Error::IdentityUniverse { kind, source, .. } = err else {
        panic!("expected a universe refusal, got {err}");
    };
    assert_eq!(kind, seg("action"));
    let UniverseError::Excluded { path, found } = source else {
        panic!("expected an exclusion, got {source}");
    };
    assert_eq!(found, "Enum");
    assert!(path.ends_with(".params"), "{path}");

    assert_eq!(store.dynamic(seg("action")).schema().get().unwrap(), None);
    assert!(store.kinds().unwrap().is_empty());
}

/// An unmarked schema is untouched by the check, however far outside the
/// universe it reaches: the normal form governs identity subtrees only.
#[test]
fn an_unmarked_schema_is_unconstrained() {
    let store = store();
    let doc = schema_of::<ActionKey>().unwrap();
    store.dynamic(seg("params")).schema().put(&doc).unwrap();
    assert_eq!(
        store.dynamic(seg("params")).schema().get().unwrap(),
        Some(doc)
    );
}
