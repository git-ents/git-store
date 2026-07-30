//! `AsOfRefStore` answers as though a fixed set of ref values held, without
//! ever writing to the store it wraps -- the seam for evaluating something
//! against the repository as it was before a ref moved, instead of
//! rewinding the real ref and restoring it around the read.

use std::collections::BTreeMap;

use gix_refstore::{
    ApplyError, AsOfError, AsOfRefStore, MemoryRefStore, ObjectId, RefEdit, RefName, RefPrefix,
    RefStore,
};

fn oid(n: u32) -> ObjectId {
    format!("{n:040x}").parse().expect("valid hex oid")
}

fn name(value: &str) -> RefName {
    RefName::new(value).expect("valid name")
}

fn prefix(value: &str) -> RefPrefix {
    RefPrefix::new(value).expect("valid prefix")
}

/// An override shadows a real ref while the view exists, and the underlying
/// store is provably unchanged once the view is consumed: `AsOfRefStore`
/// computes an answer, it never writes one.
#[test]
fn override_shadows_a_real_ref_and_leaves_the_store_unchanged() {
    let inner = MemoryRefStore::new();
    let target = name("refs/store/recipe/carbonara");
    inner
        .apply(RefEdit::Create {
            name: target.clone(),
            new: oid(1),
        })
        .expect("seed real value");

    let overrides = BTreeMap::from([(target.clone(), Some(oid(2)))]);
    let view = AsOfRefStore::new(inner, overrides);

    assert_eq!(view.read(&target).expect("read through view"), Some(oid(2)));
    // The wrapped store answers for itself, unaffected by the view built
    // over it.
    assert_eq!(
        view.inner().read(&target).expect("read inner"),
        Some(oid(1))
    );

    let inner = view.into_inner();
    assert_eq!(
        inner.read(&target).expect("read after view dropped"),
        Some(oid(1))
    );
}

/// An override to "absent" hides a ref that really exists, for both `read`
/// and `prefixed`.
#[test]
fn override_to_absent_hides_a_real_ref() {
    let inner = MemoryRefStore::new();
    let target = name("refs/store/recipe/carbonara");
    inner
        .apply(RefEdit::Create {
            name: target.clone(),
            new: oid(1),
        })
        .expect("seed real value");

    let overrides = BTreeMap::from([(target.clone(), None)]);
    let view = AsOfRefStore::new(inner, overrides);

    assert_eq!(view.read(&target).expect("read through view"), None);
    let listing = view
        .prefixed(&prefix("refs/store/recipe"))
        .expect("prefixed");
    assert!(listing.is_empty());

    // The real ref is untouched underneath.
    assert_eq!(
        view.inner().read(&target).expect("read inner"),
        Some(oid(1))
    );
}

/// An override that inserts a name not present underneath appears in
/// `prefixed`, sorted into its correct position rather than appended --
/// here, alphabetically between two real refs.
#[test]
fn inserted_override_lands_in_sorted_position() {
    let inner = MemoryRefStore::new();
    let alpha = name("refs/store/recipe/alpha");
    let omega = name("refs/store/recipe/omega");
    let middle = name("refs/store/recipe/middle");
    inner
        .apply(RefEdit::Create {
            name: alpha.clone(),
            new: oid(1),
        })
        .expect("seed alpha");
    inner
        .apply(RefEdit::Create {
            name: omega.clone(),
            new: oid(2),
        })
        .expect("seed omega");

    let overrides = BTreeMap::from([(middle.clone(), Some(oid(3)))]);
    let view = AsOfRefStore::new(inner, overrides);

    let listing = view
        .prefixed(&prefix("refs/store/recipe"))
        .expect("prefixed");
    assert_eq!(
        listing,
        vec![(alpha, oid(1)), (middle, oid(3)), (omega, oid(2))]
    );
}

/// Overrides outside the queried prefix do not leak into `prefixed`, even
/// when they would insert or hide a name that sorts inside the requested
/// range.
#[test]
fn overrides_outside_the_prefix_do_not_leak_in() {
    let inner = MemoryRefStore::new();
    let in_scope = name("refs/store/recipe/carbonara");
    inner
        .apply(RefEdit::Create {
            name: in_scope.clone(),
            new: oid(1),
        })
        .expect("seed real value");

    let outside_insert = name("refs/store/other/pancake");
    let outside_absent = name("refs/store/other/waffle");
    let overrides = BTreeMap::from([
        (outside_insert, Some(oid(9))),
        (outside_absent, None),
        (in_scope.clone(), Some(oid(2))),
    ]);
    let view = AsOfRefStore::new(inner, overrides);

    let listing = view
        .prefixed(&prefix("refs/store/recipe"))
        .expect("prefixed");
    assert_eq!(listing, vec![(in_scope, oid(2))]);
}

/// `apply` through an as-of view always fails, having changed nothing --
/// writing through a historical view is a category error, not a race to
/// retry.
#[test]
fn apply_through_the_view_fails_and_changes_nothing() {
    let inner = MemoryRefStore::new();
    let target = name("refs/store/recipe/carbonara");
    inner
        .apply(RefEdit::Create {
            name: target.clone(),
            new: oid(1),
        })
        .expect("seed real value");

    let overrides: BTreeMap<RefName, Option<ObjectId>> = BTreeMap::new();
    let view = AsOfRefStore::new(inner, overrides);

    let err = view
        .apply(RefEdit::Update {
            name: target.clone(),
            expected: oid(1),
            new: oid(2),
        })
        .expect_err("apply through a read-only view must fail");
    match err {
        ApplyError::Backend(AsOfError::ReadOnly) => {}
        ApplyError::Backend(AsOfError::Inner(err)) => {
            panic!("expected ReadOnly, got inner backend error: {err}")
        }
        ApplyError::LostRace { .. } => panic!("apply must not be reported as a retryable race"),
    }

    assert_eq!(
        view.inner().read(&target).expect("read inner"),
        Some(oid(1)),
        "a rejected apply must not have changed the underlying store"
    );
}
