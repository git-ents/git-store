#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

rows_list=${ROWS_LIST:-"2 100 1000 5000"}
benchmarks=${BENCHMARKS:-export}

for rows in $rows_list; do
  printf '\n######## ROWS=%s ########\n' "$rows"
  ROWS="$rows" BENCHMARKS="$benchmarks" WARMUP="${WARMUP:-1}" \
    "$repo_root/benchmarks/db.sh"
done
