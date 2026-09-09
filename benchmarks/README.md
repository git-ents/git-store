# Git database vs. Dolt benchmarks

`db.sh` uses [hyperfine](https://github.com/sharkdp/hyperfine) to compare the
Dolt-shaped `git store db` CLI with equivalent Dolt SQL/porcelain commands.
The benchmark measures process-level CLI latency, including repository/database
open, command parsing, storage work, and output generation. It does **not**
measure the one-time fixture setup: each sample starts from a freshly copied
fixture via hyperfine's `--prepare` hook.

## Run

Build the optimized binary and run the benchmark:

```sh
cargo build --release -p git-store
./benchmarks/db.sh
```

The script finds `git-store`, `dolt`, and `hyperfine` on `PATH`. Override paths
when needed:

```sh
GIT_STORE_BIN="$PWD/target/release/git-store" DOLT_BIN="$HOME/bin/dolt" \
  ./benchmarks/db.sh
```

Use `ROWS` to make the row-oriented operations and inspection operations more
representative of a larger table:

```sh
ROWS=100 ./benchmarks/db.sh
```

`ROWS` controls the initial table size. The benchmark still performs the
single-row read/write comparisons against `alice`; `status`, `diff`, history,
and export expose the cost of the full fixture. Hyperfine options can be
adjusted in `benchmarks/db.sh` if a longer run or a saved JSON/CSV result is
wanted.

## Operations

The benchmark covers equivalent operations from the examples:

| Operation                     | Git database                           | Dolt                                    |
| ----------------------------- | -------------------------------------- | --------------------------------------- |
| initialize                    | `git store db init` (after `git init`) | `dolt init`                             |
| create table                  | `db table create users`                | `CREATE TABLE users (...)`              |
| insert/read/update/delete row | `db put/get/rm`                        | SQL `INSERT`/`SELECT`/`UPDATE`/`DELETE` |
| inspect unstaged work         | `db status`, `db diff`                 | `dolt status`, `dolt diff`              |
| stage and commit              | `db add`, `db commit`                  | `dolt add`, `dolt commit`               |
| history                       | `db log -n 1`                          | `dolt log -n 1`                         |
| export                        | `db table export`                      | `dolt table export`                     |

The Git and Dolt fixtures use the same logical `users` data (`id`, `name`, and
`role`). The Git database is currently a JSON key/value store, so the Git
operation is compared with the nearest relational SQL operation rather than an
identical query planner or schema implementation. Results are therefore useful
for understanding CLI and storage characteristics, not as a claim that the two
systems implement identical database engines.

## Results

A reference run on September 6, 2026 used the optimized binary and the
following command:

```sh
cargo build --release -p git-store
ROWS=2 WARMUP=1 RUNS=5 \
  GIT_STORE_BIN="$PWD/target/release/git-store" ./benchmarks/db.sh
```

Environment: macOS on Apple Silicon, Git 2.55.0, `git-store` 0.1.0,
Dolt 2.3.2, and hyperfine 1.20.0. The fixture contained two rows. Times below
are hyperfine means across five runs; they include process startup and command
output, but not fixture setup.

| Operation                       | Git store |     Dolt | Faster             |
| ------------------------------- | --------: | -------: | ------------------ |
| initialize                      |  186.6 ms |  99.1 ms | Dolt (1.88×)       |
| create table                    |    7.0 ms | 127.4 ms | Git store (18.31×) |
| insert one row                  |    7.6 ms | 101.6 ms | Git store (13.34×) |
| read one row                    |    5.0 ms |  99.1 ms | Git store (19.88×) |
| update one row                  |    7.7 ms |  99.9 ms | Git store (12.93×) |
| delete one row                  |    7.4 ms | 101.5 ms | Git store (13.64×) |
| status with one unstaged update |    4.9 ms |  96.1 ms | Git store (19.47×) |
| diff with one unstaged update   |    4.9 ms |  94.5 ms | Git store (19.11×) |
| stage one table                 |    7.0 ms |  82.5 ms | Git store (11.75×) |
| commit staged changes           |    7.7 ms |  85.8 ms | Git store (11.10×) |
| read history                    |    4.8 ms |  98.3 ms | Git store (20.62×) |
| export table                    |    5.2 ms |  96.0 ms | Git store (18.50×) |

The small Git store timings (under 5 ms) triggered hyperfine's shell-startup
accuracy warning, so those numbers should be treated as approximate. The
comparison is also dominated by process startup for these single-command
benchmarks; repeat with larger `ROWS` values and more runs before drawing
storage-engine conclusions.

## Interpreting results

Run each comparison on the same machine and with no concurrent repository
writers. Repeat with several `ROWS` values. Look at both absolute latency and
how latency changes as `ROWS` increases. In particular:

- initialization and table creation include process startup and metadata setup;
- row reads/writes isolate a small operation on an already-open fixture;
- status/diff, export, and history include scanning or walking persisted data;
- commit includes the database's snapshot and Git/Dolt commit work;
- filesystem cache state, SSD performance, Dolt version, Rust build profile,
  and `ROWS` all materially affect the result.
