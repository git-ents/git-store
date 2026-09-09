#!/usr/bin/env bash
set -eu

if [ "$#" -gt 1 ]; then
  printf 'usage: %s [directory]\n' "$0" >&2
  exit 2
fi

cleanup=0
if [ "$#" -eq 1 ]; then
  workdir=$1
  mkdir -p "$workdir"
else
  workdir=$(mktemp -d "${TMPDIR:-/tmp}/dolt-db-example.XXXXXX")
  cleanup=1
fi

finish() {
  if [ "$cleanup" -eq 1 ]; then
    rm -rf "$workdir"
  fi
}
trap finish EXIT HUP INT TERM

if [ -e "$workdir/.dolt" ]; then
  printf 'refusing to use an existing Dolt repository: %s\n' "$workdir" >&2
  exit 1
fi

cd "$workdir"

dolt_cmd() {
  PAGER=cat dolt "$@"
}

# Dolt tables are relational, so schema and row changes use SQL.
dolt_cmd init --name 'Example User' --email example@example.com --initial-branch main >/dev/null
cat >users-schema.sql <<'SQL'
CREATE TABLE users (
  id VARCHAR(64) PRIMARY KEY,
  name VARCHAR(255),
  role VARCHAR(64)
);
SQL

printf '\n== Create a table and write rows ==\n'
dolt_cmd sql -q "CREATE TABLE users (id VARCHAR(64) PRIMARY KEY, name VARCHAR(255), role VARCHAR(64)); INSERT INTO users VALUES ('alice', 'Alice', 'admin'), ('bob', 'Bob', 'viewer');"
dolt_cmd sql -q "SELECT * FROM users WHERE id = 'alice';"
dolt_cmd status

printf '\n== Review, stage, and commit ==\n'
dolt_cmd diff
dolt_cmd add users
dolt_cmd diff --staged
dolt_cmd commit -m 'seed users' >/dev/null
dolt_cmd log -n 1

printf '\n== Update and delete rows ==\n'
dolt_cmd sql -q "UPDATE users SET role = 'owner' WHERE id = 'alice'; DELETE FROM users WHERE id = 'bob';"
dolt_cmd diff
dolt_cmd add users
dolt_cmd commit -m 'update users' >/dev/null
dolt_cmd sql -q "SELECT * FROM users ORDER BY id;"
dolt_cmd log -n 2

printf '\n== Export and import a table ==\n'
dolt_cmd table export users users.json >/dev/null
cat users.json
printf '\n'
cat >users-archive-schema.sql <<'SQL'
CREATE TABLE users_archive (
  id VARCHAR(64) PRIMARY KEY,
  name VARCHAR(255),
  role VARCHAR(64)
);
SQL
# JSON import requires the schema table name to match the destination table.
dolt_cmd table import -c --schema users-archive-schema.sql users_archive users.json
dolt_cmd add users_archive
dolt_cmd commit -m 'import users archive' >/dev/null
dolt_cmd sql -q 'SHOW TABLES;'

if [ "$cleanup" -eq 0 ]; then
  printf '\nDolt database: %s\n' "$workdir"
fi
