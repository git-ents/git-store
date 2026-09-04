//! Benchmarks: git-tree-backed Prolly vs. the raw costs of its operations.
//!
//! Run with `cargo bench -p git-prolly`. Object counts, byte totals, and
//! rewrite counts are measured by `examples/object-stats.rs`; these benches
//! measure wall-clock build, lookup, iteration, mutation, and diff costs.

#![expect(clippy::indexing_slicing, reason = "benchmarks index deliberately")]

use std::hint::black_box;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use facet_value::{VObject, Value};
use git_prolly::ProllyStore;

fn user(i: usize) -> Value {
    let mut object = VObject::new();
    object.insert("name", Value::from(format!("user-{i}")));
    object.insert("email", Value::from(format!("user-{i}@example.com")));
    object.insert("active", Value::TRUE);
    Value::from(object)
}

fn entries(n: u32) -> Vec<(Vec<u8>, Value)> {
    (0..n)
        .map(|i| (format!("key-{i:08}").into_bytes(), user(i as usize)))
        .collect()
}

fn bench_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("build");
    group.sample_size(10);
    for size in [1_000_u32, 10_000, 100_000] {
        group.throughput(criterion::Throughput::Elements(size as u64));
        group.bench_with_input(format!("{size}"), &size, |b, &size| {
            b.iter_batched(
                || tempfile::TempDir::new().expect("temp dir"),
                |dir| {
                    let repo = gix::init(dir.path()).expect("init");
                    let store = ProllyStore::open(&repo);
                    store.build(entries(size)).expect("build")
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

fn bench_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("lookup");
    let dir = tempfile::TempDir::new().expect("temp dir");
    let repo = gix::init(dir.path()).expect("init");
    let store = ProllyStore::open(&repo);
    let data = entries(1_000);
    let root = store.build(data.iter().cloned()).expect("build");
    let keys: Vec<&[u8]> = data.iter().map(|(key, _)| key.as_slice()).collect();

    group.bench_function("1000_random_hits", |b| {
        b.iter(|| {
            for i in 0..1_000_usize {
                let key = keys[(i * 7919) % keys.len()];
                black_box(store.get(root, black_box(key)).expect("get"));
            }
        })
    });
    group.bench_function("1000_random_misses", |b| {
        b.iter(|| {
            for i in 0..1_000_usize {
                let key = format!("missing-{i:08}");
                black_box(store.get(root, black_box(key.as_bytes())).expect("get"));
            }
        })
    });
    group.finish();
}

fn bench_iterate(c: &mut Criterion) {
    let mut group = c.benchmark_group("iterate");
    let dir = tempfile::TempDir::new().expect("temp dir");
    let repo = gix::init(dir.path()).expect("init");
    let store = ProllyStore::open(&repo);
    let root = store.build(entries(1_000)).expect("build");
    group.bench_function("1000_entries", |b| {
        b.iter(|| {
            let mut count = 0;
            for item in store.iter(root).expect("iterate") {
                black_box(item.expect("item"));
                count += 1;
            }
            assert_eq!(count, 1_000);
        })
    });
    group.finish();
}

fn bench_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert");
    group.sample_size(10);
    group.bench_function("into_1000_entry_tree", |b| {
        b.iter_batched(
            || {
                let dir = tempfile::TempDir::new().expect("temp dir");
                let repo = gix::init(dir.path()).expect("init");
                let root = ProllyStore::open(&repo)
                    .build(entries(1_000))
                    .expect("build");
                (dir, root)
            },
            |(dir, root)| {
                // Re-opening the repository is part of the routine so the
                // store can borrow it; it is microseconds against the
                // mutation's entry-level walk.
                let repo = gix::open(dir.path()).expect("reopen");
                let store = ProllyStore::open(&repo);
                store
                    .insert(Some(root), b"zzz-new-key", &user(9_999))
                    .expect("insert")
            },
            BatchSize::PerIteration,
        );
    });
    group.finish();
}

fn bench_diff(c: &mut Criterion) {
    let mut group = c.benchmark_group("diff");
    let dir = tempfile::TempDir::new().expect("temp dir");
    let repo = gix::init(dir.path()).expect("init");
    let store = ProllyStore::open(&repo);
    let base = entries(1_000);
    let root_a = store.build(base.iter().cloned()).expect("build a");
    let mut changed = base.clone();
    for i in (0..1_000_usize).step_by(100) {
        changed[i].1 = user(10_000 + i);
    }
    let root_b = store.build(changed).expect("build b");

    group.bench_function("1000_entries_1_percent_changed", |bencher| {
        bencher.iter(|| black_box(store.diff(root_a, root_b).expect("diff").len()))
    });
    group.bench_function("identical_1000_entry_roots", |bencher| {
        bencher.iter(|| black_box(store.diff(root_a, root_a).expect("diff").len()))
    });
    group.finish();
}

fn bench_repeated_values(c: &mut Criterion) {
    let mut group = c.benchmark_group("build_shared_values");
    group.sample_size(10);
    group.bench_function("1000_entries_one_value_object", |b| {
        b.iter_batched(
            || tempfile::TempDir::new().expect("temp dir"),
            |dir| {
                let repo = gix::init(dir.path()).expect("init");
                let store = ProllyStore::open(&repo);
                let shared = user(0);
                store
                    .build(
                        (0..1_000_u32)
                            .map(|i| (format!("key-{i:08}").into_bytes(), shared.clone())),
                    )
                    .expect("build")
            },
            BatchSize::PerIteration,
        );
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_build,
    bench_lookup,
    bench_iterate,
    bench_insert,
    bench_diff,
    bench_repeated_values,
);
criterion_main!(benches);
