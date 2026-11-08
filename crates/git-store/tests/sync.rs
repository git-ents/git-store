//! Compile-time and runtime thread-safety of [`Store`].

use std::sync::Mutex;
use std::thread;

use facet::Facet;
use facet_git_tree::{ObjectStore, Schema, schema_of};
use git_store::{MemoryRefStore, RefPath, RefSegment, Store};

fn assert_sync<T: Sync>() {}
fn assert_send_sync<T: Send + Sync>() {}

/// A `Store` is `Sync` when its ref store and object store are. The generic
/// function is type-checked for every `R: RefStore + Sync`, `O: Find + Write +
/// Sync`, so it proves the conditional rather than one instantiation.
fn store_is_sync_when_backends_are<R, O>()
where
    R: git_store::RefStore + Sync,
    O: gix::objs::Find + gix::objs::Write + Sync,
{
    assert_sync::<Store<R, O>>();
}

#[test]
fn schema_is_send_and_sync() {
    assert_send_sync::<Schema>();
}

#[test]
fn store_is_sync_when_backends_are_sync() {
    // No production object store in this workspace can serve a `Sync` store,
    // for two different reasons. `facet_git_tree::ObjectStore` is
    // `RefCell`-backed and so `!Sync`, but it is still `Send` — which is why
    // the `Mutex` wrapper below works: `Mutex<T>` is `Sync` whenever `T: Send`.
    // `gix::OdbHandle` is not fixable the same way. Built without `gix-odb`'s
    // `parallel` feature it holds its pack store behind an `Rc`, so it is
    // `!Send`, and no `Mutex` around a `!Send` type can be `Sync`. A
    // repo-backed `Store` therefore stays `!Sync` without a dependency change
    // or a backend change. `SyncObjectStore` is a test-local `Mutex` wrapper
    // around the in-memory object store; it is the concrete `Sync` backend
    // used to exercise the concurrency path below.
    assert_sync::<Store<MemoryRefStore, SyncObjectStore>>();
    store_is_sync_when_backends_are::<MemoryRefStore, SyncObjectStore>();
}

/// A test-local `Sync` object store: a `Mutex` around the in-memory
/// `facet_git_tree::ObjectStore`, which is `RefCell`-backed and therefore
/// `!Sync`. The lock serializes object access, so the resulting store is
/// `Send + Sync` and usable through `&Store` across threads.
struct SyncObjectStore(Mutex<ObjectStore>);

impl SyncObjectStore {
    fn new() -> Self {
        Self(Mutex::new(ObjectStore::default()))
    }
}

impl gix::objs::Find for SyncObjectStore {
    fn try_find<'a>(
        &self,
        id: &gix::hash::oid,
        buffer: &'a mut Vec<u8>,
    ) -> Result<Option<gix::objs::Data<'a>>, gix::objs::find::Error> {
        let store = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        store.try_find(id, buffer)
    }
}

impl gix::objs::Write for SyncObjectStore {
    fn write_buf_with_known_id(
        &self,
        kind: gix::objs::Kind,
        from: &[u8],
        id: gix::hash::ObjectId,
    ) -> Result<gix::hash::ObjectId, gix::objs::write::Error> {
        let store = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        store.write_buf_with_known_id(kind, from, id)
    }

    fn write_stream(
        &self,
        kind: gix::objs::Kind,
        size: u64,
        from: &mut dyn std::io::Read,
    ) -> Result<gix::hash::ObjectId, gix::objs::write::Error> {
        let store = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        store.write_stream(kind, size, from)
    }

    fn write_stream_with_known_id(
        &self,
        kind: gix::objs::Kind,
        size: u64,
        from: &mut dyn std::io::Read,
        id: gix::hash::ObjectId,
    ) -> Result<gix::hash::ObjectId, gix::objs::write::Error> {
        let store = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        store.write_stream_with_known_id(kind, size, from, id)
    }
}

#[derive(Facet, Debug, PartialEq)]
struct Counter {
    n: u32,
}

#[test]
fn a_store_can_be_shared_across_threads() {
    let store = Store::new(MemoryRefStore::new(), SyncObjectStore::new());
    assert_sync::<Store<MemoryRefStore, SyncObjectStore>>();

    let kind = RefSegment::new("counter").unwrap();
    let doc = schema_of::<Counter>().unwrap();
    store
        .kind::<Counter>(kind.clone())
        .schema()
        .put(&doc)
        .unwrap();

    thread::scope(|scope| {
        for i in 0..4u32 {
            let store = &store;
            let kind = kind.clone();
            let doc = doc.clone();
            scope.spawn(move || {
                let name = RefPath::new(format!("counter-{i}")).unwrap();
                let kind = store.kind::<Counter>(kind);
                kind.put(&name, &Counter { n: i }).unwrap();
                assert_eq!(kind.read(&name).unwrap().value(), Some(Counter { n: i }));

                // Every thread reads the published schema through the shared
                // cache; the cache is content-addressed and never invalidated.
                let cached = kind.schema().get().unwrap().expect("schema is published");
                assert_eq!(cached, doc);
            });
        }
    });
}
