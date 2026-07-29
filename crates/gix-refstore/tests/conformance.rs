//! One behaviour, one generic fn, run against every backend: `mod memory`
//! and `mod repo` each instantiate the whole list, so a failure names which
//! backend broke.
//!
//! `GixRefStore::read`/`prefixed` peel a ref to its object, which fails if
//! that object is absent from the odb — unlike `MemoryRefStore`, which is
//! just a map. So every test takes an `oid` factory alongside the store:
//! `memory_oid` fabricates distinct hashes, `with_repo_store`'s factory
//! writes a real blob.

use gix_refstore::{
    ApplyError, Committer, Expectation, GixRefStore, MemoryRefStore, ObjectId, RefEdit, RefName,
    RefPrefix, RefStore,
};

fn memory_oid(n: u32) -> ObjectId {
    format!("{n:040x}").parse().expect("valid hex oid")
}

fn with_repo_store<T>(f: impl FnOnce(&GixRefStore, &dyn Fn(u32) -> ObjectId) -> T) -> T {
    let dir = tempfile::tempdir().expect("create tempdir");
    test_support::init_repo(dir.path());
    let repo = gix::open(dir.path()).expect("open repo");
    let store = GixRefStore::new(&repo);
    let oid = |n: u32| {
        repo.write_blob(format!("gix-refstore conformance blob {n}").into_bytes())
            .expect("write blob")
            .detach()
    };
    f(&store, &oid)
}

fn read_absent<S: RefStore>(store: &S, _oid: impl Fn(u32) -> ObjectId) {
    let name = RefName::new("refs/store/absent").expect("valid name");
    assert_eq!(store.read(&name).expect("read"), None);
}

fn create_then_read<S: RefStore>(store: &S, oid: impl Fn(u32) -> ObjectId) {
    let name = RefName::new("refs/store/recipe/carbonara").expect("valid name");
    let new = oid(1);
    store
        .apply(RefEdit::Create {
            name: name.clone(),
            new,
        })
        .expect("create");
    assert_eq!(store.read(&name).expect("read"), Some(new));
}

fn create_existing_fails_and_leaves_value<S: RefStore>(store: &S, oid: impl Fn(u32) -> ObjectId) {
    let name = RefName::new("refs/store/recipe/carbonara").expect("valid name");
    let original = oid(1);
    store
        .apply(RefEdit::Create {
            name: name.clone(),
            new: original,
        })
        .expect("create");

    let err = store
        .apply(RefEdit::Create {
            name: name.clone(),
            new: oid(2),
        })
        .expect_err("create over an existing ref must lose the race");
    match err {
        ApplyError::LostRace {
            name: got,
            expected,
        } => {
            assert_eq!(got, name);
            assert_eq!(expected, Expectation::Absent);
        }
        ApplyError::Backend(err) => panic!("expected LostRace, got backend error: {err}"),
    }
    assert_eq!(store.read(&name).expect("read"), Some(original));
}

/// A create must lose the race even when the ref already holds the very oid
/// it would have written: two racing creates of the same value are still two
/// creates, and only one of them can have happened.
fn create_existing_with_same_oid_fails<S: RefStore>(store: &S, oid: impl Fn(u32) -> ObjectId) {
    let name = RefName::new("refs/store/recipe/carbonara").expect("valid name");
    let new = oid(1);
    store
        .apply(RefEdit::Create {
            name: name.clone(),
            new,
        })
        .expect("create");

    let err = store
        .apply(RefEdit::Create {
            name: name.clone(),
            new,
        })
        .expect_err("create over an existing ref must lose the race");
    assert!(matches!(err, ApplyError::LostRace { .. }));
    assert_eq!(store.read(&name).expect("read"), Some(new));
}

fn update_with_correct_expected_moves_ref<S: RefStore>(store: &S, oid: impl Fn(u32) -> ObjectId) {
    let name = RefName::new("refs/store/recipe/carbonara").expect("valid name");
    let original = oid(1);
    let new = oid(2);
    store
        .apply(RefEdit::Create {
            name: name.clone(),
            new: original,
        })
        .expect("create");
    store
        .apply(RefEdit::Update {
            name: name.clone(),
            expected: original,
            new,
        })
        .expect("update");
    assert_eq!(store.read(&name).expect("read"), Some(new));
}

fn update_with_stale_expected_fails_and_leaves_value<S: RefStore>(
    store: &S,
    oid: impl Fn(u32) -> ObjectId,
) {
    let name = RefName::new("refs/store/recipe/carbonara").expect("valid name");
    let original = oid(1);
    let stale = oid(2);
    store
        .apply(RefEdit::Create {
            name: name.clone(),
            new: original,
        })
        .expect("create");

    let err = store
        .apply(RefEdit::Update {
            name: name.clone(),
            expected: stale,
            new: oid(3),
        })
        .expect_err("update against a stale expectation must lose the race");
    match err {
        ApplyError::LostRace {
            name: got,
            expected,
        } => {
            assert_eq!(got, name);
            assert_eq!(expected, Expectation::Exactly(stale));
        }
        ApplyError::Backend(err) => panic!("expected LostRace, got backend error: {err}"),
    }
    assert_eq!(store.read(&name).expect("read"), Some(original));
}

fn delete_with_correct_expected_removes_ref<S: RefStore>(store: &S, oid: impl Fn(u32) -> ObjectId) {
    let name = RefName::new("refs/store/recipe/carbonara").expect("valid name");
    let original = oid(1);
    store
        .apply(RefEdit::Create {
            name: name.clone(),
            new: original,
        })
        .expect("create");
    store
        .apply(RefEdit::Delete {
            name: name.clone(),
            expected: original,
        })
        .expect("delete");
    assert_eq!(store.read(&name).expect("read"), None);
}

fn delete_with_stale_expected_fails_and_leaves_value<S: RefStore>(
    store: &S,
    oid: impl Fn(u32) -> ObjectId,
) {
    let name = RefName::new("refs/store/recipe/carbonara").expect("valid name");
    let original = oid(1);
    let stale = oid(2);
    store
        .apply(RefEdit::Create {
            name: name.clone(),
            new: original,
        })
        .expect("create");

    let err = store
        .apply(RefEdit::Delete {
            name: name.clone(),
            expected: stale,
        })
        .expect_err("delete against a stale expectation must lose the race");
    match err {
        ApplyError::LostRace {
            name: got,
            expected,
        } => {
            assert_eq!(got, name);
            assert_eq!(expected, Expectation::Exactly(stale));
        }
        ApplyError::Backend(err) => panic!("expected LostRace, got backend error: {err}"),
    }
    assert_eq!(store.read(&name).expect("read"), Some(original));
}

fn update_nonexistent_fails<S: RefStore>(store: &S, oid: impl Fn(u32) -> ObjectId) {
    let name = RefName::new("refs/store/recipe/ghost").expect("valid name");
    let expected = oid(1);
    let err = store
        .apply(RefEdit::Update {
            name: name.clone(),
            expected,
            new: oid(2),
        })
        .expect_err("update against a nonexistent ref must lose the race");
    match err {
        ApplyError::LostRace {
            name: got,
            expected: got_expected,
        } => {
            assert_eq!(got, name);
            assert_eq!(got_expected, Expectation::Exactly(expected));
        }
        ApplyError::Backend(err) => panic!("expected LostRace, got backend error: {err}"),
    }
}

fn delete_nonexistent_fails<S: RefStore>(store: &S, oid: impl Fn(u32) -> ObjectId) {
    let name = RefName::new("refs/store/recipe/ghost").expect("valid name");
    let expected = oid(1);
    let err = store
        .apply(RefEdit::Delete {
            name: name.clone(),
            expected,
        })
        .expect_err("delete against a nonexistent ref must lose the race");
    match err {
        ApplyError::LostRace {
            name: got,
            expected: got_expected,
        } => {
            assert_eq!(got, name);
            assert_eq!(got_expected, Expectation::Exactly(expected));
        }
        ApplyError::Backend(err) => panic!("expected LostRace, got backend error: {err}"),
    }
}

fn prefixed_returns_ascending_order<S: RefStore>(store: &S, oid: impl Fn(u32) -> ObjectId) {
    let prefix = RefPrefix::new("refs/store/recipe").expect("valid prefix");
    let names = [
        "refs/store/recipe/tiramisu",
        "refs/store/recipe/carbonara",
        "refs/store/recipe/nested/amatriciana",
        "refs/store/recipe/bolognese",
    ];
    for (i, name) in names.iter().enumerate() {
        let name = RefName::new(*name).expect("valid name");
        store
            .apply(RefEdit::Create {
                name,
                new: oid(i as u32),
            })
            .expect("create");
    }

    let listed = store.prefixed(&prefix).expect("prefixed");
    let listed_names: Vec<&str> = listed.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        listed_names,
        vec![
            "refs/store/recipe/bolognese",
            "refs/store/recipe/carbonara",
            "refs/store/recipe/nested/amatriciana",
            "refs/store/recipe/tiramisu",
        ]
    );
}

fn prefixed_respects_segment_boundary<S: RefStore>(store: &S, oid: impl Fn(u32) -> ObjectId) {
    let under_prefix = RefName::new("refs/store/foo/x").expect("valid name");
    let foobar = RefName::new("refs/store/foobar/y").expect("valid name");
    store
        .apply(RefEdit::Create {
            name: under_prefix.clone(),
            new: oid(1),
        })
        .expect("create");
    store
        .apply(RefEdit::Create {
            name: foobar,
            new: oid(2),
        })
        .expect("create");

    let prefix = RefPrefix::new("refs/store/foo").expect("valid prefix");
    let listed = store.prefixed(&prefix).expect("prefixed");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].0, under_prefix);
}

fn prefixed_empty_when_nothing_under_prefix<S: RefStore>(store: &S, oid: impl Fn(u32) -> ObjectId) {
    let elsewhere = RefName::new("refs/store/other/thing").expect("valid name");
    store
        .apply(RefEdit::Create {
            name: elsewhere,
            new: oid(1),
        })
        .expect("create");

    let prefix = RefPrefix::new("refs/store/recipe").expect("valid prefix");
    assert_eq!(store.prefixed(&prefix).expect("prefixed"), vec![]);
}

fn committer_signature_is_usable<S: RefStore + Committer>(
    store: &S,
    _oid: impl Fn(u32) -> ObjectId,
) {
    let signature = store.signature().expect("signature");
    assert!(!signature.name.is_empty());
    assert!(!signature.email.is_empty());
}

fn author_signature_is_usable<S: RefStore + Committer>(store: &S, _oid: impl Fn(u32) -> ObjectId) {
    let author = store.author().expect("author");
    assert!(!author.name.is_empty());
    assert!(!author.email.is_empty());
}

macro_rules! conformance_tests {
    ($($name:ident),+ $(,)?) => {
        mod memory {
            use super::*;

            $(
                #[test]
                fn $name() {
                    super::$name(&MemoryRefStore::new(), super::memory_oid);
                }
            )+
        }

        mod repo {
            $(
                #[test]
                fn $name() {
                    super::with_repo_store(|store, oid| super::$name(store, oid));
                }
            )+
        }
    };
}

conformance_tests! {
    read_absent,
    create_then_read,
    create_existing_fails_and_leaves_value,
    create_existing_with_same_oid_fails,
    update_with_correct_expected_moves_ref,
    update_with_stale_expected_fails_and_leaves_value,
    delete_with_correct_expected_removes_ref,
    delete_with_stale_expected_fails_and_leaves_value,
    update_nonexistent_fails,
    delete_nonexistent_fails,
    prefixed_returns_ascending_order,
    prefixed_respects_segment_boundary,
    prefixed_empty_when_nothing_under_prefix,
    committer_signature_is_usable,
    author_signature_is_usable,
}

/// `author.*` and `committer.*` are separate git configuration, so a backend
/// that reads a repository must not collapse them onto one identity.
#[test]
fn repo_author_is_read_separately_from_committer() {
    use std::io::Write as _;

    let dir = tempfile::tempdir().expect("create tempdir");
    test_support::init_repo(dir.path());
    let repo = gix::open(dir.path()).expect("open repo");
    let mut config = std::fs::OpenOptions::new()
        .append(true)
        .open(repo.git_dir().join("config"))
        .expect("open config");
    writeln!(config, "[author]\n\tname = Ada\n\temail = ada@example.com").expect("write config");
    drop(config);

    let repo = gix::open(dir.path()).expect("reopen repo");
    let store = GixRefStore::new(&repo);
    assert_eq!(store.author().expect("author").name, "Ada");
    assert_eq!(store.signature().expect("signature").name, "Test");
}
