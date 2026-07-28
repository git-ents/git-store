//! Every rejection rule in `RefName`/`RefPrefix`/`RefSegment::new`, plus the
//! accepted shapes and the path-composition helpers built on top of them.

use gix_refstore::{RefName, RefPrefix, RefSegment, Violation};

fn assert_path_violation(value: &str, expected: Violation) {
    let name_err = RefName::new(value).expect_err("RefName::new should reject");
    assert_eq!(name_err.violation(), expected, "RefName::new({value:?})");
    let prefix_err = RefPrefix::new(value).expect_err("RefPrefix::new should reject");
    assert_eq!(
        prefix_err.violation(),
        expected,
        "RefPrefix::new({value:?})"
    );
}

#[test]
fn path_violations() {
    assert_path_violation("", Violation::Empty);
    assert_path_violation("/refs/store", Violation::BoundarySlash);
    assert_path_violation("refs/store/", Violation::BoundarySlash);
    assert_path_violation("refs//store", Violation::Empty);
    assert_path_violation(".refs/store", Violation::BoundaryDot);
    assert_path_violation("refs/store.", Violation::BoundaryDot);
    assert_path_violation("refs/store.lock", Violation::LockSuffix);
    assert_path_violation("refs/sto..re", Violation::AmbiguousSequence);
    assert_path_violation("refs/sto@{re", Violation::AmbiguousSequence);
    assert_path_violation("@", Violation::LoneAt);
    assert_path_violation("refs/sto\u{1}re", Violation::ControlOrSpace);
    assert_path_violation("refs/sto re", Violation::ControlOrSpace);
    for c in ['~', '^', ':', '?', '*', '[', '\\'] {
        assert_path_violation(&format!("refs/sto{c}re"), Violation::ForbiddenCharacter);
    }
}

fn assert_segment_violation(value: &str, expected: Violation) {
    let err = RefSegment::new(value).expect_err("RefSegment::new should reject");
    assert_eq!(err.violation(), expected, "RefSegment::new({value:?})");
}

#[test]
fn segment_violations() {
    assert_segment_violation("", Violation::Empty);
    assert_segment_violation(".carbonara", Violation::BoundaryDot);
    assert_segment_violation("carbonara.", Violation::BoundaryDot);
    assert_segment_violation("carbonara.lock", Violation::LockSuffix);
    assert_segment_violation("carbo..nara", Violation::AmbiguousSequence);
    assert_segment_violation("carbo@{nara", Violation::AmbiguousSequence);
    assert_segment_violation("@", Violation::LoneAt);
    assert_segment_violation("carbo\u{1}nara", Violation::ControlOrSpace);
    assert_segment_violation("carbo nara", Violation::ControlOrSpace);
    for c in ['~', '^', ':', '?', '*', '[', '\\'] {
        assert_segment_violation(&format!("carbo{c}nara"), Violation::ForbiddenCharacter);
    }
    assert_segment_violation("carbo/nara", Violation::Separator);
}

#[test]
fn accepted_paths() {
    RefName::new("refs/store").expect("two segments");
    RefName::new("refs/store/recipe/carbonara").expect("four segments");
    RefPrefix::new("refs/store").expect("two segments");
    RefPrefix::new("refs/store/recipe").expect("three segments");
}

#[test]
fn accepted_segments() {
    RefSegment::new("carbo-nara_v2.final").expect("dashes, underscores, and a mid-word dot");
    RefSegment::new("carbo@nara").expect("an embedded '@' that is neither lone nor '@{'");
}

#[test]
fn prefix_join_and_child() {
    let prefix = RefPrefix::new("refs/store/recipe").expect("valid prefix");
    let segment = RefSegment::new("carbonara").expect("valid segment");
    assert_eq!(
        prefix.join(&segment).as_str(),
        "refs/store/recipe/carbonara"
    );
    assert_eq!(
        prefix.child(&segment).as_str(),
        "refs/store/recipe/carbonara"
    );
}

#[test]
fn name_join() {
    let name = RefName::new("refs/store/recipe").expect("valid name");
    let segment = RefSegment::new("carbonara").expect("valid segment");
    assert_eq!(name.join(&segment).as_str(), "refs/store/recipe/carbonara");
}

#[test]
fn name_strip_prefix() {
    let prefix = RefPrefix::new("refs/store/foo").expect("valid prefix");

    let nested = RefName::new("refs/store/foo/a/b").expect("valid name");
    assert_eq!(nested.strip_prefix(&prefix), Some("a/b"));

    let same_leading_text = RefName::new("refs/store/foobar").expect("valid name");
    assert_eq!(
        same_leading_text.strip_prefix(&prefix),
        None,
        "a shared text prefix across a segment boundary is not containment"
    );

    let unrelated = RefName::new("refs/heads/main").expect("valid name");
    assert_eq!(unrelated.strip_prefix(&prefix), None);
}
