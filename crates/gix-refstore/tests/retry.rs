//! Proves the compare-and-swap retry loop loses no update under real
//! contention: `THREADS` threads race to bump the same ref `ITERATIONS`
//! times each, and the final oid must decode to the full total.
//!
//! `GixRefStore::read` peels to the target object, so each backend needs its
//! own `encode`/`decode` pair: `GixRefStore`'s oid is a real blob, not an
//! arbitrary hash. `gix::Repository` is not `Sync`, so each thread opens its
//! own handle onto the same on-disk repository.

use gix_refstore::{ApplyError, GixRefStore, MemoryRefStore, ObjectId, RefEdit, RefName, RefStore};

const THREADS: usize = 8;
const ITERATIONS: usize = 25;

fn race<S, Encode, Decode>(
    store: &S,
    name: &RefName,
    encode: Encode,
    decode: Decode,
) -> Result<(), S::Error>
where
    S: RefStore,
    Encode: Fn(usize) -> ObjectId,
    Decode: Fn(ObjectId) -> usize,
{
    loop {
        let current = store.read(name)?;
        let n = current.map(&decode).unwrap_or(0);
        let new = encode(n + 1);
        let edit = match current {
            Some(expected) => RefEdit::Update {
                name: name.clone(),
                expected,
                new,
            },
            None => RefEdit::Create {
                name: name.clone(),
                new,
            },
        };
        match store.apply(edit) {
            Ok(()) => return Ok(()),
            Err(ApplyError::LostRace { .. }) => continue,
            Err(ApplyError::Backend(err)) => return Err(err),
        }
    }
}

#[test]
fn no_update_lost_under_contention_memory() {
    let name = RefName::new("refs/store/counter").expect("valid name");
    let encode = |n: usize| -> ObjectId { format!("{n:040x}").parse().expect("valid hex oid") };
    let decode =
        |id: ObjectId| usize::from_str_radix(&id.to_hex().to_string(), 16).expect("hex counter");

    let store = MemoryRefStore::new();
    std::thread::scope(|scope| {
        for _ in 0..THREADS {
            scope.spawn(|| {
                for _ in 0..ITERATIONS {
                    race(&store, &name, encode, decode).expect("apply");
                }
            });
        }
    });

    let final_value = store.read(&name).expect("read").expect("ref was created");
    assert_eq!(decode(final_value), THREADS * ITERATIONS);
}

#[test]
fn no_update_lost_under_contention_repo() {
    let dir = tempfile::tempdir().expect("create tempdir");
    test_support::init_repo(dir.path());
    let path = dir.path();
    let name = RefName::new("refs/store/counter").expect("valid name");

    // Write every counter's blob up front so the racing threads only ever
    // read the odb through `GixRefStore` itself: a blob written by one
    // thread's handle need not be visible to another's without a refresh,
    // and that is not what this test is measuring.
    let repo = gix::open(path).expect("open repo");
    let counters: Vec<ObjectId> = (0..=THREADS * ITERATIONS)
        .map(|n| {
            repo.write_blob(n.to_string().into_bytes())
                .expect("write blob")
                .detach()
        })
        .collect();
    let encode = |n: usize| counters[n];
    let decode = |id: ObjectId| {
        counters
            .iter()
            .position(|counter| *counter == id)
            .expect("oid is one of the counter blobs")
    };

    std::thread::scope(|scope| {
        for _ in 0..THREADS {
            scope.spawn(|| {
                let repo = gix::open(path).expect("open repo");
                let store = GixRefStore::new(&repo);
                for _ in 0..ITERATIONS {
                    race(&store, &name, encode, decode).expect("apply");
                }
            });
        }
    });

    let store = GixRefStore::new(&repo);
    let final_value = store.read(&name).expect("read").expect("ref was created");
    assert_eq!(decode(final_value), THREADS * ITERATIONS);
}
