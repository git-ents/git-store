#!/usr/bin/env bash
# Migration anchor: 2026-08-14, design-alignment non-issue entity backfill.
# This is the exact plumbing transcript for the live repositories. It never
# deletes refs, publication commits, or schema history.
#
# Each legacy value is explicitly decoded, encoded against the current schema,
# bound into a self-contained document, and published with the old publication
# commit as its first parent. The CLI intentionally has no migrate workflow;
# this Bash composition is the workflow.

set -euo pipefail

GIT_STORE="${GIT_STORE:-git store}"

store() {
  # GIT_STORE may be a command plus arguments, e.g. `git store` or
  # `/path/to/git-store`.
  # shellcheck disable=SC2086
  $GIT_STORE "$@"
}

normalize_schema() {
  local data_prefix=$1
  local kind=$2

  # A strict inspection is the feature probe. Legacy schema documents are
  # explicitly opted into, then immediately republished in the current format.
  if store --data-prefix "$data_prefix" --schema-prefix refs/schema \
    schema inspect "$kind" --at "refs/schema/$kind" >/dev/null 2>&1; then
    return
  fi

  store --data-prefix "$data_prefix" --schema-prefix refs/schema \
    schema get "$kind" --legacy-leaves \
    | store --data-prefix "$data_prefix" --schema-prefix refs/schema \
      schema put "$kind"
}

migrate_document() {
  local data_prefix=$1
  local kind=$2
  local alias_path=$3
  local parent=$4
  local legacy_document=$5

  local value_json value_tree document_tree
  value_json=$(store --data-prefix "$data_prefix" --schema-prefix refs/schema \
    get "$legacy_document" --legacy-leaves)
  value_tree=$(printf '%s\n' "$value_json" \
    | store --data-prefix "$data_prefix" --schema-prefix refs/schema \
      value encode --schema "refs/schema/$kind")
  document_tree=$(store --data-prefix "$data_prefix" --schema-prefix refs/schema \
    document bind "$value_tree" --schema "refs/schema/$kind")

  store --data-prefix "$data_prefix" --schema-prefix refs/schema \
    document inspect "$document_tree"

  # The legacy ref keeps its name: migration rebinds that name to the current
  # schema in place rather than introducing a second, content-named ref. The
  # derived root makes the transcript safe to resume, since republishing the
  # same document over the same name is a no-op. alias_path may nest deeper
  # than the parent oid (e.g. comments live under refs/forge/comment/review/<id>/<oid>).
  local reference="refs/${data_prefix#refs/}/$kind/$alias_path"
  if [ "$(git rev-parse --verify --quiet "$reference^{tree}")" = "$document_tree" ]; then
    printf 'already migrated %s -> %s\n' "$reference" "$document_tree" >&2
    return
  fi

  store --data-prefix "$data_prefix" --schema-prefix refs/schema \
    document publish "$kind" "$document_tree" \
    --alias "$alias_path" --parent "$parent" --expected "$parent"
}

repository_root=$(git rev-parse --show-toplevel)
repository_name=$(basename "$repository_root")

case "$repository_name" in
git-store)
  # Source ref: refs/forge/review/1044d4ed268836b5d39d0b3ad11e4d3bfbd32d08
  # Legacy document root: d8a60c419fae0d7ee474d5b507a49bce2951ca73
  # The bound root is derived after re-encoding under current review schema.
  normalize_schema refs/forge review
  migrate_document refs/forge review \
    1044d4ed268836b5d39d0b3ad11e4d3bfbd32d08 \
    1044d4ed268836b5d39d0b3ad11e4d3bfbd32d08 \
    d8a60c419fae0d7ee474d5b507a49bce2951ca73

  # Source refs are nested under the review subject. They keep their names and
  # advance to a bound document commit.
  normalize_schema refs/forge comment
  migrate_document refs/forge comment \
    review/1044d4ed268836b5d39d0b3ad11e4d3bfbd32d08/0c968682dc05c1eafda06b5c71e0677b2574fbe8 \
    0c968682dc05c1eafda06b5c71e0677b2574fbe8 \
    0dc9213fcf347778ca531d1897e3348cfd920cf4
  migrate_document refs/forge comment \
    review/1044d4ed268836b5d39d0b3ad11e4d3bfbd32d08/1777e0242a9ec45e1aa5cb40c5d6279a76d73df4 \
    1777e0242a9ec45e1aa5cb40c5d6279a76d73df4 \
    a0fc118dcf1cd139428ba6ef829f43c63d6994ca
  migrate_document refs/forge comment \
    review/1044d4ed268836b5d39d0b3ad11e4d3bfbd32d08/35c1bc853aa91880193ea1c47fe5e0cbb694fd4a \
    35c1bc853aa91880193ea1c47fe5e0cbb694fd4a \
    79421d97a565d0e0d0e5728b35697eada4f6bafc
  migrate_document refs/forge comment \
    review/1044d4ed268836b5d39d0b3ad11e4d3bfbd32d08/6c23a057d2f9d00ab4c7d78da70665effdfebc09 \
    6c23a057d2f9d00ab4c7d78da70665effdfebc09 \
    3f549a0c82e17a408740ac69232ab5e73f17eb80
  migrate_document refs/forge comment \
    review/1044d4ed268836b5d39d0b3ad11e4d3bfbd32d08/72300fbc8e16d3feb88ea6dc2fa7bbc5a03b1780 \
    72300fbc8e16d3feb88ea6dc2fa7bbc5a03b1780 \
    63bfb2a8c39a514b2320b4ba016a5b8e71721d68
  migrate_document refs/forge comment \
    review/1044d4ed268836b5d39d0b3ad11e4d3bfbd32d08/9c16a6a2933a7e72dd9ebe51d7f3ee9d9d6a8b7d \
    9c16a6a2933a7e72dd9ebe51d7f3ee9d9d6a8b7d \
    53896aa32383ee44782ff3eadc57304d9ebc1f6f
  migrate_document refs/forge comment \
    review/1044d4ed268836b5d39d0b3ad11e4d3bfbd32d08/b495599ab78aaf7772f0014b6a8bca4ef40f2740 \
    b495599ab78aaf7772f0014b6a8bca4ef40f2740 \
    846e71adba9fd478f9c4ccf1f43324e5674c670c
  migrate_document refs/forge comment \
    review/1044d4ed268836b5d39d0b3ad11e4d3bfbd32d08/bd020a37cd60fc44ee12fb5ee8af5584f03a61ac \
    bd020a37cd60fc44ee12fb5ee8af5584f03a61ac \
    e40679a67a921a37bad757a845e52b87405776ac
  migrate_document refs/forge comment \
    review/1044d4ed268836b5d39d0b3ad11e4d3bfbd32d08/e8615241699c068f5bfc3bee376d990a926606e0 \
    e8615241699c068f5bfc3bee376d990a926606e0 \
    f508c09d097ea8b7b6f71b0284591a0cc517ffd1
  migrate_document refs/forge comment \
    review/1044d4ed268836b5d39d0b3ad11e4d3bfbd32d08/f869717358423a0a42fd4d3dc5046bfa99ce0df4 \
    f869717358423a0a42fd4d3dc5046bfa99ce0df4 \
    265c223328ac474270c5869bb877db21a036ed53

  # Source alias: refs/meta/rules/review
  normalize_schema refs/meta rules
  migrate_document refs/meta rules \
    review \
    aa17bc7c79ed4bd992ee664a56dbce362bf6a65e \
    33053f7f8c4fca3040dcb85d1dc780315a5398dc
  ;;
git-forge)
  # Source alias: refs/forge/member/503bd6f4150c1edd020219847dcb3197bff91aea
  normalize_schema refs/forge member
  migrate_document refs/forge member \
    503bd6f4150c1edd020219847dcb3197bff91aea \
    503bd6f4150c1edd020219847dcb3197bff91aea \
    6d5d32330aa0721159f71ed7fc429376a5d3477c

  # Source alias: refs/meta/rules/review
  normalize_schema refs/meta rules
  migrate_document refs/meta rules \
    review \
    adb116625b808d65a2f55139168d9461beb57526 \
    33053f7f8c4fca3040dcb85d1dc780315a5398dc
  ;;
*)
  printf 'run this script from the git-store or git-forge repository\n' >&2
  exit 2
  ;;
esac
