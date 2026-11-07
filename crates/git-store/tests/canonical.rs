//! The canonical commit writer: bytes exactly as declared, signer untouched,
//! message and parent rules enforced.
//!
//! The golden object id in [`stored_bytes_match_git_exactly`] was produced by
//! `git hash-object -t commit --stdin` over the hand-written bytes the test
//! builds, so a pass proves byte-exact agreement with git's own framing.

use git_store::{CanonicalCommit, Error, MemoryRefStore, ObjectId, SignatureBytes, Signer, Store};
use gix::actor::Signature;
use gix::bstr::ByteSlice;
use gix::date::Time;
use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;

use facet_git_tree::ObjectStore;

const GOLDEN_TWO_PARENTS: &str = "76d7af494e03e031b318b7d87b8df89284d7a57f";

fn oid(hex: &str) -> ObjectId {
    ObjectId::from_hex(hex.as_bytes()).unwrap()
}

fn sig(name: &str, email: &str, seconds: i64) -> Signature {
    Signature {
        name: name.into(),
        email: email.into(),
        time: Time { seconds, offset: 0 },
    }
}

fn sample() -> CanonicalCommit {
    CanonicalCommit::new(
        oid("0123456789abcdef0123456789abcdef01234567"),
        sig("Kiln Author", "author@kiln.test", 1_700_000_000),
        sig("Kiln Committer", "committer@kiln.test", 1_700_000_001),
        "kiln: canonical commit\n\nbody line\n",
    )
    .with_parent(oid("1111111111111111111111111111111111111111"))
    .with_parent(oid("2222222222222222222222222222222222222222"))
}

/// Signs by recording what it was asked to cover; a canonical write must
/// never ask.
type Covered = Rc<RefCell<Vec<Vec<u8>>>>;

struct Recorder(Covered);

impl Signer for Recorder {
    type Error = Infallible;

    fn sign(&self, bytes: &[u8]) -> Result<SignatureBytes, Self::Error> {
        self.0.borrow_mut().push(bytes.to_vec());
        Ok(SignatureBytes::from(b"not a signature".to_vec()))
    }
}

fn store_with_signer() -> (Store<MemoryRefStore, ObjectStore>, Covered) {
    let covered: Covered = Rc::default();
    let store = Store::new(MemoryRefStore::new(), ObjectStore::default())
        .with_signer(Recorder(covered.clone()));
    (store, covered)
}

#[test]
fn stored_bytes_match_git_exactly() {
    let store = Store::new(MemoryRefStore::new(), ObjectStore::default());
    let commit = sample();
    let id = store.write_canonical(commit.clone()).unwrap();

    assert_eq!(id, oid(GOLDEN_TWO_PARENTS));

    let gix::objs::Object::Commit(read_back) = store.objects().get(&id).unwrap().unwrap() else {
        panic!("canonical write did not produce a commit");
    };
    assert_eq!(read_back.tree, commit.tree());
    assert_eq!(read_back.parents.to_vec(), commit.parents());
    assert_eq!(
        read_back.author,
        sig("Kiln Author", "author@kiln.test", 1_700_000_000)
    );
    assert_eq!(
        read_back.committer,
        sig("Kiln Committer", "committer@kiln.test", 1_700_000_001)
    );
    assert_eq!(
        read_back.message.as_bytes(),
        b"kiln: canonical commit\n\nbody line\n"
    );
    assert!(read_back.extra_headers.is_empty());
}

#[test]
fn the_same_declared_fields_write_the_same_object() {
    let store = Store::new(MemoryRefStore::new(), ObjectStore::default());
    let first = store.write_canonical(sample()).unwrap();
    let second = store.write_canonical(sample()).unwrap();
    assert_eq!(first, second);
}

#[test]
fn a_configured_signer_is_never_consulted_and_no_signature_is_stored() {
    let (store, covered) = store_with_signer();
    let id = store.write_canonical(sample()).unwrap();
    assert!(covered.borrow().is_empty());
    assert_eq!(store.signature(id).unwrap(), None);
}

#[test]
fn a_reserved_trailer_in_the_message_is_refused() {
    let store = Store::new(MemoryRefStore::new(), ObjectStore::default());
    let commit = CanonicalCommit::new(
        oid("0123456789abcdef0123456789abcdef01234567"),
        sig("A", "a@kiln.test", 0),
        sig("A", "a@kiln.test", 0),
        "fine line\nSchema: legacy\n",
    );
    assert!(matches!(
        store.write_canonical(commit),
        Err(Error::ReservedTrailer { trailer: "Schema:" })
    ));
}

#[test]
fn duplicate_parents_are_refused() {
    let store = Store::new(MemoryRefStore::new(), ObjectStore::default());
    let duplicate = oid("1111111111111111111111111111111111111111");
    let commit =
        sample().with_parents([duplicate, oid("2222222222222222222222222222222222222222")]);
    assert!(matches!(
        store.write_canonical(commit),
        Err(Error::DuplicateParent { parent }) if parent == duplicate
    ));
}

#[test]
fn zero_parents_write_a_root_commit() {
    let store = Store::new(MemoryRefStore::new(), ObjectStore::default());
    let commit = CanonicalCommit::new(
        oid("0123456789abcdef0123456789abcdef01234567"),
        sig("Kiln Author", "author@kiln.test", 1_700_000_000),
        sig("Kiln Committer", "committer@kiln.test", 1_700_000_001),
        "seal\n",
    );
    let id = store.write_canonical(commit).unwrap();
    let gix::objs::Object::Commit(read_back) = store.objects().get(&id).unwrap().unwrap() else {
        panic!("canonical write did not produce a commit");
    };
    assert!(read_back.parents.is_empty());
    assert_eq!(store.signature(id).unwrap(), None);
}

/// A newline in an identity field re-frames the commit header: gix reads
/// headers positionally, so a `committer` line smuggled in through the author
/// email displaces the declared committer. The write must be refused before any
/// object is stored.
#[test]
fn a_newline_in_an_identity_cannot_forge_a_commit_header() {
    let store = Store::new(MemoryRefStore::new(), ObjectStore::default());

    // The newline turns the remainder of the email into a whole `committer`
    // header, which gix reads as *the* committer.
    let mut author = sig("Kiln Author", "author@kiln.test", 1_700_000_000);
    author.email = "author@kiln.test>\ncommitter Forged <forged@evil.test> 1720000000 +0000".into();
    let commit = CanonicalCommit::new(
        oid("0123456789abcdef0123456789abcdef01234567"),
        author,
        sig("Kiln Committer", "committer@kiln.test", 1_700_000_001),
        "kiln: canonical commit\n",
    );

    match store.write_canonical(commit) {
        Ok(id) => {
            let gix::objs::Object::Commit(read_back) = store.objects().get(&id).unwrap().unwrap()
            else {
                panic!("canonical write did not produce a commit");
            };
            panic!(
                "injection accepted: stored committer is {:?}, not the declared \
                 \"Kiln Committer\"; extra headers {:?}",
                read_back.committer, read_back.extra_headers
            );
        }
        Err(err) => assert!(
            matches!(
                err,
                Error::InvalidIdentity {
                    field: "author email",
                    ..
                }
            ),
            "unexpected error: {err}"
        ),
    }
}

/// Every identity byte set that would re-frame the `name <email>` header is
/// refused, whichever field carries it: a newline (which starts a new header),
/// an angle bracket (which delimits the identity), an empty field, or
/// whitespace inside an email.
#[test]
fn identity_bytes_that_re_frame_the_header_are_refused() {
    let cases: &[(&str, &str)] = &[
        ("author name", "A\ncommitter F <f@e> 1 +0000"),
        ("author email", "a@kiln.test>\ncommitter F <f@e> 1 +0000"),
        ("author name", "A <b@c"),
        ("author email", "a@kiln.test>"),
        ("committer name", "A\ncommitter F <f@e> 1 +0000"),
        ("committer email", "a b@kiln.test"),
        ("author name", ""),
        ("committer email", ""),
    ];

    for &(field, bytes) in cases {
        let mut author = sig("Kiln Author", "author@kiln.test", 1_700_000_000);
        let mut committer = sig("Kiln Committer", "committer@kiln.test", 1_700_000_001);
        match field {
            "author name" => author.name = bytes.into(),
            "author email" => author.email = bytes.into(),
            "committer name" => committer.name = bytes.into(),
            "committer email" => committer.email = bytes.into(),
            _ => unreachable!(),
        }
        let store = Store::new(MemoryRefStore::new(), ObjectStore::default());
        let commit = CanonicalCommit::new(
            oid("0123456789abcdef0123456789abcdef01234567"),
            author,
            committer,
            "seal\n",
        );
        assert!(
            matches!(
                store.write_canonical(commit),
                Err(Error::InvalidIdentity { field: f, .. }) if f == field
            ),
            "expected {field:?} to be refused for bad bytes {bytes:?}"
        );
    }
}

/// Valid identity bytes — spaces, dots, `+`, and non-ASCII — are still
/// accepted and round-trip unchanged.
#[test]
fn ordinary_identity_bytes_are_still_accepted() {
    let store = Store::new(MemoryRefStore::new(), ObjectStore::default());
    let name = "Zoë Ättström +dev.co";
    let email = "zoe+dev.tag@例え.co.uk";
    let commit = CanonicalCommit::new(
        oid("0123456789abcdef0123456789abcdef01234567"),
        sig(name, email, 1_700_000_000),
        sig(name, email, 1_700_000_001),
        "seal\n",
    );
    let id = store.write_canonical(commit).unwrap();
    let gix::objs::Object::Commit(read_back) = store.objects().get(&id).unwrap().unwrap() else {
        panic!("canonical write did not produce a commit");
    };
    assert_eq!(read_back.author.name, name.as_bytes());
    assert_eq!(read_back.author.email, email.as_bytes());
}

#[test]
fn negative_time_offsets_frame_like_git() {
    let store = Store::new(MemoryRefStore::new(), ObjectStore::default());
    let mut author = sig("Kiln Author", "author@kiln.test", 1_700_000_000);
    author.time.offset = -18000;
    let commit = CanonicalCommit::new(
        oid("0123456789abcdef0123456789abcdef01234567"),
        author.clone(),
        author,
        "offset\n",
    );
    let id = store.write_canonical(commit).unwrap();
    let gix::objs::Object::Commit(read_back) = store.objects().get(&id).unwrap().unwrap() else {
        panic!("canonical write did not produce a commit");
    };
    assert_eq!(read_back.author.time.offset, -18000);
}
