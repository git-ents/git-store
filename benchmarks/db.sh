#!/usr/bin/env bash
set -euo pipefail

# Compare the command-line latency of the equivalent operations in the
# repository's Git-backed database and Dolt. Setup is done by hyperfine's
# --prepare hook, so fixture creation and reset time is not measured.

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

if [[ -n "${GIT_STORE_BIN:-}" ]]; then
  git_store_bin=$GIT_STORE_BIN
elif command -v git-store >/dev/null 2>&1; then
  git_store_bin=$(command -v git-store)
elif [[ -x "$repo_root/target/release/git-store" ]]; then
  git_store_bin=$repo_root/target/release/git-store
elif [[ -x "$repo_root/target/debug/git-store" ]]; then
  git_store_bin=$repo_root/target/debug/git-store
else
  printf 'git-store was not found; install it or set GIT_STORE_BIN\n' >&2
  exit 1
fi

dolt_bin=${DOLT_BIN:-$(command -v dolt || true)}
if [[ -z "$dolt_bin" ]]; then
  printf 'dolt was not found; install it or set DOLT_BIN\n' >&2
  exit 1
fi

command -v hyperfine >/dev/null 2>&1 || {
  printf 'hyperfine was not found; install it before running this benchmark\n' >&2
  exit 1
}

rows=${ROWS:-2}
if ! [[ "$rows" =~ ^[1-9][0-9]*$ ]]; then
  printf 'ROWS must be a positive integer (got %q)\n' "$rows" >&2
  exit 2
fi

benchmarks=${BENCHMARKS:-all}
benchmark_enabled() {
  case ",$benchmarks," in
    *,all,*|*,"$1",*) return 0 ;;
    *) return 1 ;;
  esac
}

warmup=${WARMUP:-1}
if ! [[ "$warmup" =~ ^[0-9]+$ ]]; then
  printf 'WARMUP must be a non-negative integer (got %q)\n' "$warmup" >&2
  exit 2
fi
hyperfine_base=(--shell=bash --warmup "$warmup")
if [[ -n "${RUNS:-}" ]]; then
  if ! [[ "$RUNS" =~ ^[1-9][0-9]*$ ]]; then
    printf 'RUNS must be a positive integer (got %q)\n' "$RUNS" >&2
    exit 2
  fi
  hyperfine_base+=(--runs "$RUNS")
fi

root=$(mktemp -d "${TMPDIR:-/tmp}/git-store-dolt-bench.XXXXXX")
trap 'rm -rf "$root"' EXIT HUP INT TERM

# printf %q gives hyperfine's shell the same path/value boundaries that this
# script has, including when a temporary directory contains spaces.
quote() {
  printf '%q' "$1"
}

git_q=$(quote "$git_store_bin")
dolt_q=$(quote "$dolt_bin")

key_for_row() {
  case "$1" in
    1) printf 'alice' ;;
    2) printf 'bob' ;;
    *) printf 'user-%s' "$1" ;;
  esac
}

seed_git() {
  "$git_store_bin" db table create users >/dev/null
  local i key value role
  for ((i = 1; i <= rows; i++)); do
    key=$(key_for_row "$i")
    value=$(printf '{"name":"%s","role":"%s"}' "${key^}" "$([[ "$i" == 1 ]] && printf admin || printf viewer)")
    "$git_store_bin" db put users "$key" "$value" >/dev/null
  done
}

create_dolt_table() {
  PAGER=cat DOLT_PAGER=cat "$dolt_bin" sql -q \
    'CREATE TABLE users (id VARCHAR(64) PRIMARY KEY, name VARCHAR(255), role VARCHAR(64));' >/dev/null
}

seed_dolt() {
  create_dolt_table

  local values=() i key name role
  for ((i = 1; i <= rows; i++)); do
    key=$(key_for_row "$i")
    name=${key^}
    role=$([[ "$i" == 1 ]] && printf admin || printf viewer)
    values+=("('$key', '$name', '$role')")
  done
  PAGER=cat DOLT_PAGER=cat "$dolt_bin" sql -q \
    "INSERT INTO users VALUES $(IFS=', '; printf '%s' "${values[*]}");" >/dev/null
}

init_git_repo() {
  local dir=$1
  (cd "$dir" && \
    git init -q -b main && \
    git config user.name 'Benchmark User' && \
    git config user.email benchmark@example.com && \
    "$git_store_bin" db init >/dev/null)
}

init_dolt_repo() {
  local dir=$1
  (cd "$dir" && PAGER=cat DOLT_PAGER=cat "$dolt_bin" init \
    --name 'Benchmark User' --email benchmark@example.com \
    --initial-branch main >/dev/null)
}

make_git_fixture() {
  local state=$1 dir=$2
  mkdir -p "$dir"
  init_git_repo "$dir"
  case "$state" in
    empty) ;;
    table) (cd "$dir" && "$git_store_bin" db table create users >/dev/null) ;;
    rows|dirty|staged|committed)
      (cd "$dir" && seed_git)
      ;;
    *) printf 'unknown Git fixture state: %s\n' "$state" >&2; exit 2 ;;
  esac
  case "$state" in
    dirty|staged|committed)
      (cd "$dir" && "$git_store_bin" db put users alice \
        '{"name":"Alice","role":"owner"}' >/dev/null)
      ;;
  esac
  case "$state" in
    staged|committed)
      (cd "$dir" && "$git_store_bin" db add users >/dev/null)
      ;;
  esac
  if [[ "$state" == committed ]]; then
    (cd "$dir" && "$git_store_bin" db commit -m 'seed users' >/dev/null)
  fi
}

make_dolt_fixture() {
  local state=$1 dir=$2
  mkdir -p "$dir"
  init_dolt_repo "$dir"
  case "$state" in
    empty) ;;
    table) (cd "$dir" && create_dolt_table) ;;
    rows|dirty|staged|committed)
      (cd "$dir" && seed_dolt)
      ;;
    *) printf 'unknown Dolt fixture state: %s\n' "$state" >&2; exit 2 ;;
  esac
  case "$state" in
    dirty|staged|committed)
      (cd "$dir" && PAGER=cat DOLT_PAGER=cat "$dolt_bin" sql -q \
        "UPDATE users SET role = 'owner' WHERE id = 'alice';" >/dev/null)
      ;;
  esac
  case "$state" in
    staged|committed)
      (cd "$dir" && PAGER=cat DOLT_PAGER=cat "$dolt_bin" add users >/dev/null)
      ;;
  esac
  if [[ "$state" == committed ]]; then
    (cd "$dir" && PAGER=cat DOLT_PAGER=cat "$dolt_bin" commit -m 'seed users' >/dev/null)
  fi
}

# Create each fixture once. A fixture is copied by --prepare before every
# sample, which makes mutating commands safe to benchmark repeatedly.
for state in empty table rows dirty staged committed; do
  make_git_fixture "$state" "$root/git-$state"
  make_dolt_fixture "$state" "$root/dolt-$state"
done

run_dir=$root/run
git_run=$run_dir/git
dolt_run=$run_dir/dolt

prepare_fixture() {
  local state=$1
  printf 'rm -rf %s; mkdir -p %s; cp -a %s %s; cp -a %s %s' \
    "$(quote "$run_dir")" "$(quote "$run_dir")" \
    "$(quote "$root/git-$state")" "$(quote "$git_run")" \
    "$(quote "$root/dolt-$state")" "$(quote "$dolt_run")"
}

bench_fixture() {
  local name=$1 state=$2 git_command=$3 dolt_command=$4
  printf '\n== %s (ROWS=%s) ==\n' "$name" "$rows"
  hyperfine "${hyperfine_base[@]}" \
    --prepare "$(prepare_fixture "$state")" \
    --command-name 'git-store' "$git_command" \
    --command-name 'dolt' "$dolt_command"
}

bench_fixture_if_enabled() {
  local benchmark=$1
  shift
  if benchmark_enabled "$benchmark"; then
    bench_fixture "$@"
  fi
}

# Initialization has no fixture: --prepare only creates an empty directory,
# and repository/database initialization is part of the measured operation.
init_prepare="rm -rf $(quote "$run_dir"); mkdir -p $(quote "$git_run") $(quote "$dolt_run")"
init_git="cd $(quote "$git_run") && git init -q -b main && git config user.name 'Benchmark User' && git config user.email benchmark@example.com && $git_q db init >/dev/null"
init_dolt="cd $(quote "$dolt_run") && PAGER=cat DOLT_PAGER=cat $dolt_q init --name 'Benchmark User' --email benchmark@example.com --initial-branch main >/dev/null"
if benchmark_enabled initialize; then
  printf '\n== initialize (ROWS=%s) ==\n' "$rows"
  hyperfine "${hyperfine_base[@]}" --prepare "$init_prepare" \
    --command-name 'git-store' "$init_git" \
    --command-name 'dolt' "$init_dolt"
fi

bench_fixture_if_enabled create-table 'create table' empty \
  "cd $(quote "$git_run") && $git_q db table create users >/dev/null" \
  "cd $(quote "$dolt_run") && PAGER=cat DOLT_PAGER=cat $dolt_q sql -q $(quote 'CREATE TABLE users (id VARCHAR(64) PRIMARY KEY, name VARCHAR(255), role VARCHAR(64));') >/dev/null"

insert_json=$(quote '{"name":"Alice","role":"admin"}')
insert_sql=$(quote "INSERT INTO users VALUES ('alice', 'Alice', 'admin');")
select_sql=$(quote "SELECT * FROM users WHERE id = 'alice';")
update_json=$(quote '{"name":"Alice","role":"owner"}')
update_sql=$(quote "UPDATE users SET role = 'owner' WHERE id = 'alice';")
delete_sql=$(quote "DELETE FROM users WHERE id = 'alice';")

bench_fixture_if_enabled insert-row 'insert one row' table \
  "cd $(quote "$git_run") && $git_q db put users alice $insert_json >/dev/null" \
  "cd $(quote "$dolt_run") && PAGER=cat DOLT_PAGER=cat $dolt_q sql -q $insert_sql >/dev/null"

bench_fixture_if_enabled read-row 'read one row' rows \
  "cd $(quote "$git_run") && $git_q db get users alice >/dev/null" \
  "cd $(quote "$dolt_run") && PAGER=cat DOLT_PAGER=cat $dolt_q sql -q $select_sql >/dev/null"

bench_fixture_if_enabled update-row 'update one row' rows \
  "cd $(quote "$git_run") && $git_q db put users alice $update_json >/dev/null" \
  "cd $(quote "$dolt_run") && PAGER=cat DOLT_PAGER=cat $dolt_q sql -q $update_sql >/dev/null"

bench_fixture_if_enabled delete-row 'delete one row' rows \
  "cd $(quote "$git_run") && $git_q db rm users alice >/dev/null" \
  "cd $(quote "$dolt_run") && PAGER=cat DOLT_PAGER=cat $dolt_q sql -q $delete_sql >/dev/null"

bench_fixture_if_enabled status 'status with one unstaged update' dirty \
  "cd $(quote "$git_run") && $git_q db status >/dev/null" \
  "cd $(quote "$dolt_run") && PAGER=cat DOLT_PAGER=cat $dolt_q status >/dev/null"

bench_fixture_if_enabled diff 'diff with one unstaged update' dirty \
  "cd $(quote "$git_run") && $git_q db diff >/dev/null" \
  "cd $(quote "$dolt_run") && PAGER=cat DOLT_PAGER=cat $dolt_q diff >/dev/null"

bench_fixture_if_enabled stage 'stage one table' dirty \
  "cd $(quote "$git_run") && $git_q db add users >/dev/null" \
  "cd $(quote "$dolt_run") && PAGER=cat DOLT_PAGER=cat $dolt_q add users >/dev/null"

bench_fixture_if_enabled commit 'commit staged changes' staged \
  "cd $(quote "$git_run") && $git_q db commit -m 'update users' >/dev/null" \
  "cd $(quote "$dolt_run") && PAGER=cat DOLT_PAGER=cat $dolt_q commit -m 'update users' >/dev/null"

bench_fixture_if_enabled history 'read history' committed \
  "cd $(quote "$git_run") && $git_q db log -n 1 >/dev/null" \
  "cd $(quote "$dolt_run") && PAGER=cat DOLT_PAGER=cat $dolt_q log -n 1 >/dev/null"

export_git_file=$(quote "$git_run/users.json")
export_dolt_file=$(quote "$dolt_run/users.json")
bench_fixture_if_enabled export 'export table' committed \
  "cd $(quote "$git_run") && $git_q db table export users -F $export_git_file >/dev/null" \
  "cd $(quote "$dolt_run") && PAGER=cat DOLT_PAGER=cat $dolt_q table export users $export_dolt_file >/dev/null"

printf '\nFixtures used ROWS=%s rows. Set ROWS to compare larger tables.\n' "$rows"
