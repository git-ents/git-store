//! The signing seam: a store with a [`Signer`] carries its bytes on every
//! commit it writes, and hands them back verbatim.
//!
//! There is nothing to test on the "store never inspects the bytes" half of the
//! contract — a capability the code does not have cannot be exercised — so that
//! invariant lives in [`Store::signature`]'s and [`Signer`]'s documentation
//! instead. What *is* testable is that the bytes come back unchanged, whatever
//! they are: the signer below returns an armored-looking block that is not a
//! signature in any format, and the store carries it anyway — through the
//! `gpgsig` header's continuation-line folding, which is what makes the
//! round-trip worth asserting rather than obvious.
//!
//! Verifying the transport against real `git` needs a real key and a real
//! repository, so that lives in `tests/repository.rs`.

use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;

use facet::Facet;
use facet_git_tree::ObjectStore;
use gix_store::{MemoryRefStore, RefPath, RefSegment, SignatureBytes, Signer, Store, schema_of};

#[derive(Facet, Debug, PartialEq)]
struct Counter {
    n: u32,
}

/// The bytes each `sign` call was asked to cover, shared with the test that
/// handed the signer to a store.
type Covered = Rc<RefCell<Vec<Vec<u8>>>>;

/// Signs by recording what it was asked to cover and returning bytes that are
/// not a signature in any format.
struct Recorder(Covered);

impl Signer for Recorder {
    type Error = Infallible;

    fn sign(&self, bytes: &[u8]) -> Result<SignatureBytes, Self::Error> {
        self.0.borrow_mut().push(bytes.to_vec());
        Ok(SignatureBytes::from(OPAQUE.to_vec()))
    }
}

/// The signature bytes the signer above produces, whatever it is asked to sign:
/// shaped like the armored block a real signer emits — several lines, a
/// trailing newline — and gibberish inside, since nothing here may care.
const OPAQUE: &[u8] =
    b"-----BEGIN NOT A SIGNATURE-----\n\xffnot base64 either\n\n-----END NOT A SIGNATURE-----\n";

fn store() -> Store<MemoryRefStore, ObjectStore> {
    Store::new(MemoryRefStore::new(), ObjectStore::default())
}

fn kind() -> RefSegment {
    RefSegment::new("counter").unwrap()
}

fn name() -> RefPath {
    RefPath::new("one").unwrap()
}

#[test]
fn an_unsigned_write_carries_no_signature() {
    let store = store();
    store
        .kind::<Counter>(kind())
        .schema()
        .put(&schema_of::<Counter>().unwrap())
        .unwrap();
    let commit = store
        .kind::<Counter>(kind())
        .put(&name(), &Counter { n: 1 })
        .unwrap();

    assert_eq!(store.signature(commit).unwrap(), None);
    assert_eq!(
        store.kind::<Counter>(kind()).read(name()).unwrap().value(),
        Some(Counter { n: 1 })
    );
}

#[test]
fn a_signed_write_returns_its_bytes_verbatim() {
    let store = store().with_signer(Recorder(Covered::default()));
    store
        .kind::<Counter>(kind())
        .schema()
        .put(&schema_of::<Counter>().unwrap())
        .unwrap();
    let commit = store
        .kind::<Counter>(kind())
        .put(&name(), &Counter { n: 1 })
        .unwrap();

    assert_eq!(
        store
            .signature(commit)
            .unwrap()
            .as_ref()
            .map(|s| s.as_bytes()),
        Some(OPAQUE)
    );
    assert_eq!(
        store.kind::<Counter>(kind()).read(name()).unwrap().value(),
        Some(Counter { n: 1 })
    );
}

/// The bytes handed to the signer are the commit's own canonical bytes as they
/// stand with no signature present — the header cannot be inside what it
/// covers — and they name the tree that was written.
#[test]
fn the_signer_covers_the_unsigned_commit_bytes() {
    let covered = Covered::default();
    let store = store().with_signer(Recorder(Rc::clone(&covered)));
    store
        .kind::<Counter>(kind())
        .schema()
        .put(&schema_of::<Counter>().unwrap())
        .unwrap();
    let commit = store
        .kind::<Counter>(kind())
        .put(&name(), &Counter { n: 1 })
        .unwrap();

    let covered = covered.borrow().last().cloned().expect("the value write");
    let unsigned = gix::objs::CommitRef::from_bytes(&covered, gix::hash::Kind::Sha1).unwrap();
    assert!(unsigned.extra_headers.is_empty());

    let written = store.objects().get(&commit).expect("the commit");
    let gix::objs::Object::Commit(written) = written else {
        panic!("not a commit");
    };
    assert_eq!(unsigned.tree(), written.tree);
    assert_eq!(unsigned.message, written.message);
}
