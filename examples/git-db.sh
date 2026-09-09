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
  workdir=$(mktemp -d "${TMPDIR:-/tmp}/git-db-example.XXXXXX")
  cleanup=1
fi

finish() {
  if [ "$cleanup" -eq 1 ]; then
    rm -rf "$workdir"
  fi
}
trap finish EXIT HUP INT TERM

if [ -e "$workdir/.git" ]; then
  printf 'refusing to use an existing Git repository: %s\n' "$workdir" >&2
  exit 1
fi

cd "$workdir"

# Keep every Git invocation non-interactive and pager-free.
git_store() {
  GIT_PAGER=cat PAGER=cat git --no-pager store "$@"
}

git init -q -b main
git config user.name 'Example User'
git config user.email example@example.com

git_store db init
printf '\n== Create a table and write rows ==\n'
git_store db table create users
git_store db put users alice '{"name":"Alice","role":"admin"}'
git_store db put users bob '{"name":"Bob","role":"viewer"}'
git_store db get users alice

git_store db status
printf '\n== Review, stage, and commit ==\n'
git_store db diff
git_store db add users
git_store db diff --staged
git_store db commit -m 'seed users'
git_store db log

printf '\n== Update and delete rows ==\n'
git_store db put users alice '{"name":"Alice","role":"owner"}'
git_store db rm users bob
git_store db diff
git_store db add -A
git_store db commit -m 'update users'
git_store db get users alice
git_store db get users bob || true
git_store db log

printf '\n== Export and import a table ==\n'
git_store db table export users -F users.json
cat users.json
printf '\n'
git_store db table import users_archive -c -F users.json
git_store db add users_archive
git_store db commit -m 'import users archive'
git_store db ls -v

if [ "$cleanup" -eq 0 ]; then
  printf '\nGit database: %s\n' "$workdir"
fi
