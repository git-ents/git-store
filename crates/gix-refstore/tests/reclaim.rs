//! Lock reclamation: a writer killed between creating its per-ref lock and
//! dropping it leaves the lock file behind, and the ref would otherwise be
//! unwritable forever. These tests pin the repair and its two boundaries —
//! a live owner is never reclaimed, and concurrent multi-ref writers still
//! serialise without deadlocking.
//!
//! The lock lives at `<git-dir>/gix-refstore-locks/<encoded-ref>.lock`, where
//! `<encoded-ref>` is the ref with `%` and `/` percent-escaped. It is a
//! `GixRefStore`-only concern, so this is a `repo`-specific suite rather than
//! a conformance case.

use gix_refstore::{ApplyError, GixRefStore, RefEdit, RefName, RefStore};

/// Two writers that touch the same two refs in opposite order must still
/// serialise: names are acquired in a fixed order, so neither can hold one
/// half of the pair while waiting on the other. A regression here deadlocks,
/// which the scoped join would turn into a hang, not a passing test.
#[test]
fn concurrent_reverse_order_batches_do_not_deadlock() {
    let dir = tempfile::tempdir().expect("create tempdir");
    test_support::init_repo(dir.path());
    let path = dir.path();
    let repo = gix::open(path).expect("open repo");
    let store = GixRefStore::new(&repo);

    let a = RefName::new("refs/store/a").expect("valid name");
    let b = RefName::new("refs/store/b").expect("valid name");
    let oa = repo.write_blob(b"a").expect("write blob").detach();
    let ob = repo.write_blob(b"b").expect("write blob").detach();
    store
        .apply(RefEdit::Create {
            name: a.clone(),
            new: oa,
        })
        .expect("seed a");
    store
        .apply(RefEdit::Create {
            name: b.clone(),
            new: ob,
        })
        .expect("seed b");

    std::thread::scope(|scope| {
        for reverse in [false, true] {
            let (a, b) = (a.clone(), b.clone());
            scope.spawn(move || {
                let repo = gix::open(path).expect("open repo");
                let store = GixRefStore::new(&repo);
                for _ in 0..25 {
                    loop {
                        let edit_a = RefEdit::Update {
                            name: a.clone(),
                            expected: oa,
                            new: oa,
                        };
                        let edit_b = RefEdit::Update {
                            name: b.clone(),
                            expected: ob,
                            new: ob,
                        };
                        let batch = if reverse {
                            vec![edit_b, edit_a]
                        } else {
                            vec![edit_a, edit_b]
                        };
                        match store.apply_batch(batch) {
                            Ok(()) => break,
                            Err(ApplyError::LostRace { .. }) => continue,
                            Err(ApplyError::Backend(err)) => panic!("backend failure: {err}"),
                        }
                    }
                }
            });
        }
    });

    assert_eq!(store.read(&a).expect("read a"), Some(oa));
    assert_eq!(store.read(&b).expect("read b"), Some(ob));
}

/// Reclamation needs a way to prove an owner is gone, which is only available
/// on Linux (a missing `/proc/<pid>`); elsewhere the crate never reclaims, so
/// these cases are Linux-only.
#[cfg(target_os = "linux")]
mod linux {
    use std::io::Write as _;
    use std::path::PathBuf;
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::{Duration, Instant, SystemTime};

    use gix_refstore::{GixRefStore, RefEdit, RefName, RefStore};

    /// The lock directory and the lock file a ref's write would create.
    fn lock_paths(repo: &gix::Repository, name: &str) -> (PathBuf, PathBuf) {
        let dir = repo.git_dir().join("gix-refstore-locks");
        let encoded = name.replace('%', "%25").replace('/', "%2F");
        let lock = dir.join(format!("{encoded}.lock"));
        (dir, lock)
    }

    /// A pid that provably belongs to no live process — the site the crate's
    /// own liveness probe reads.
    fn unused_pid() -> u32 {
        (4_000_000u32..5_000_000)
            .find(|pid| !std::path::Path::new(&format!("/proc/{pid}")).exists())
            .expect("some pid is unused")
    }

    /// A dead owner's leftover marker must be reclaimed, so the ref becomes
    /// writable again promptly instead of stalling for the full lock timeout
    /// and failing for good.
    #[test]
    fn dead_owner_lock_is_reclaimed() {
        let dir = tempfile::tempdir().expect("create tempdir");
        test_support::init_repo(dir.path());
        let repo = gix::open(dir.path()).expect("open repo");
        let store = GixRefStore::new(&repo);

        let name = RefName::new("refs/store/z").expect("valid name");
        let (lock_dir, lock_path) = lock_paths(&repo, "refs/store/z");
        std::fs::create_dir_all(&lock_dir).expect("create lock dir");

        // What a `SIGKILL`ed writer leaves: a lock file owned by a process that
        // is gone, last touched well before the grace period.
        let dead = unused_pid();
        let mut marker = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&lock_path)
            .expect("plant marker");
        marker
            .write_all(format!("{dead}\n").as_bytes())
            .expect("record dead owner");
        marker
            .set_modified(SystemTime::now() - Duration::from_secs(3600))
            .expect("age the marker");
        drop(marker);

        let oid = repo.write_blob(b"z").expect("write blob").detach();
        let started = Instant::now();
        store
            .apply(RefEdit::Create {
                name: name.clone(),
                new: oid,
            })
            .expect("reclaim the dead lock, then create");
        let elapsed = started.elapsed();

        assert_eq!(store.read(&name).expect("read"), Some(oid));
        assert!(!lock_path.exists(), "the reclaimed marker must be gone");
        assert!(
            elapsed < Duration::from_secs(5),
            "reclaiming must not wait out the 30s lock timeout: took {elapsed:?}",
        );
    }

    /// A live owner is not a crash: its lock must be respected, never
    /// reclaimed, and the batch must not silently succeed over the holder.
    #[test]
    fn live_owner_lock_is_respected() {
        let dir = tempfile::tempdir().expect("create tempdir");
        test_support::init_repo(dir.path());
        let repo = gix::open(dir.path()).expect("open repo");
        let store = GixRefStore::new(&repo);

        let name = RefName::new("refs/store/held").expect("valid name");
        let (lock_dir, lock_path) = lock_paths(&repo, "refs/store/held");
        std::fs::create_dir_all(&lock_dir).expect("create lock dir");

        // Owned by THIS process — live — with a fresh mtime.
        let mut marker = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&lock_path)
            .expect("plant marker");
        marker
            .write_all(format!("{}\n", std::process::id()).as_bytes())
            .expect("record live owner");
        drop(marker);

        let oid = repo.write_blob(b"held").expect("write blob").detach();
        let path = dir.path().to_owned();
        let (tx, rx) = mpsc::channel();
        let held = name.clone();
        std::thread::spawn(move || {
            let repo = gix::open(&path).expect("open repo");
            let store = GixRefStore::new(&repo);
            let _ = tx.send(
                store
                    .apply(RefEdit::Create {
                        name: held,
                        new: oid,
                    })
                    .is_ok(),
            );
        });

        match rx.recv_timeout(Duration::from_millis(500)) {
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => panic!("the apply thread died"),
            Ok(succeeded) => {
                panic!(
                    "apply over a live holder returned instead of blocking (success={succeeded})"
                )
            }
        }
        assert!(
            lock_path.exists(),
            "a live holder's marker must not be reclaimed",
        );
        assert_eq!(
            store.read(&name).expect("read"),
            None,
            "the batch must not clobber the live holder",
        );
    }
}
