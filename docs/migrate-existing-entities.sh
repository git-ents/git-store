#!/usr/bin/env bash
# Purpose: republish the current live non-issue documents into canonical refs.
# Date: 2026-08-14
# Existing aliases and history are never deleted.

set -euo pipefail

GIT_STORE="${GIT_STORE:-git store}"

store() {
  # Allow GIT_STORE to name a shell function or contain a command with arguments.
  # shellcheck disable=SC2086
  $GIT_STORE "$@"
}

repository_root=$(git rev-parse --show-toplevel)
repository_name=$(basename "$repository_root")

case "$repository_name" in
git-store)
  # Source alias: refs/forge/review/1044d4ed268836b5d39d0b3ad11e4d3bfbd32d08
  # Canonical root: refs/forge/review/d8a60c419fae0d7ee474d5b507a49bce2951ca73
  store --data-prefix refs/forge --schema-prefix refs/schema \
    document inspect d8a60c419fae0d7ee474d5b507a49bce2951ca73
  store --data-prefix refs/forge --schema-prefix refs/schema \
    document publish review d8a60c419fae0d7ee474d5b507a49bce2951ca73 \
    --parent 1044d4ed268836b5d39d0b3ad11e4d3bfbd32d08 \
    --expected absent

  # Source alias: refs/forge/comment/review/1044d4ed268836b5d39d0b3ad11e4d3bfbd32d08/0c968682dc05c1eafda06b5c71e0677b2574fbe8
  # Canonical root: refs/forge/comment/0dc9213fcf347778ca531d1897e3348cfd920cf4
  store --data-prefix refs/forge --schema-prefix refs/schema \
    document inspect 0dc9213fcf347778ca531d1897e3348cfd920cf4
  store --data-prefix refs/forge --schema-prefix refs/schema \
    document publish comment 0dc9213fcf347778ca531d1897e3348cfd920cf4 \
    --parent 0c968682dc05c1eafda06b5c71e0677b2574fbe8 \
    --expected absent

  # Source alias: refs/forge/comment/review/1044d4ed268836b5d39d0b3ad11e4d3bfbd32d08/1777e0242a9ec45e1aa5cb40c5d6279a76d73df4
  # Canonical root: refs/forge/comment/a0fc118dcf1cd139428ba6ef829f43c63d6994ca
  store --data-prefix refs/forge --schema-prefix refs/schema \
    document inspect a0fc118dcf1cd139428ba6ef829f43c63d6994ca
  store --data-prefix refs/forge --schema-prefix refs/schema \
    document publish comment a0fc118dcf1cd139428ba6ef829f43c63d6994ca \
    --parent 1777e0242a9ec45e1aa5cb40c5d6279a76d73df4 \
    --expected absent

  # Source alias: refs/forge/comment/review/1044d4ed268836b5d39d0b3ad11e4d3bfbd32d08/35c1bc853aa91880193ea1c47fe5e0cbb694fd4a
  # Canonical root: refs/forge/comment/79421d97a565d0e0d0e5728b35697eada4f6bafc
  store --data-prefix refs/forge --schema-prefix refs/schema \
    document inspect 79421d97a565d0e0d0e5728b35697eada4f6bafc
  store --data-prefix refs/forge --schema-prefix refs/schema \
    document publish comment 79421d97a565d0e0d0e5728b35697eada4f6bafc \
    --parent 35c1bc853aa91880193ea1c47fe5e0cbb694fd4a \
    --expected absent

  # Source alias: refs/forge/comment/review/1044d4ed268836b5d39d0b3ad11e4d3bfbd32d08/6c23a057d2f9d00ab4c7d78da70665effebc09
  # Canonical root: refs/forge/comment/3f549a0c82e17a408740ac69232ab5e73f17eb80
  store --data-prefix refs/forge --schema-prefix refs/schema \
    document inspect 3f549a0c82e17a408740ac69232ab5e73f17eb80
  store --data-prefix refs/forge --schema-prefix refs/schema \
    document publish comment 3f549a0c82e17a408740ac69232ab5e73f17eb80 \
    --parent 6c23a057d2f9d00ab4c7d78da70665effebc09 \
    --expected absent

  # Source alias: refs/forge/comment/review/1044d4ed268836b5d39d0b3ad11e4d3bfbd32d08/72300fbc8e16d3feb88ea6dc2fa7bbc5a03b1780
  # Canonical root: refs/forge/comment/63bfb2a8c39a514b2320b4ba016a5b8e71721d68
  store --data-prefix refs/forge --schema-prefix refs/schema \
    document inspect 63bfb2a8c39a514b2320b4ba016a5b8e71721d68
  store --data-prefix refs/forge --schema-prefix refs/schema \
    document publish comment 63bfb2a8c39a514b2320b4ba016a5b8e71721d68 \
    --parent 72300fbc8e16d3feb88ea6dc2fa7bbc5a03b1780 \
    --expected absent

  # Source alias: refs/forge/comment/review/1044d4ed268836b5d39d0b3ad11e4d3bfbd32d08/9c16a6a2933a7e72dd9ebe51d7f3ee9d9d6a8b7d
  # Canonical root: refs/forge/comment/53896aa32383ee44782ff3eadc57304d9ebc1f6f
  store --data-prefix refs/forge --schema-prefix refs/schema \
    document inspect 53896aa32383ee44782ff3eadc57304d9ebc1f6f
  store --data-prefix refs/forge --schema-prefix refs/schema \
    document publish comment 53896aa32383ee44782ff3eadc57304d9ebc1f6f \
    --parent 9c16a6a2933a7e72dd9ebe51d7f3ee9d9d6a8b7d \
    --expected absent

  # Source alias: refs/forge/comment/review/1044d4ed268836b5d39d0b3ad11e4d3bfbd32d08/b495599ab78aaf7772f0014b6a8bca4ef40f2740
  # Canonical root: refs/forge/comment/846e71adba9fd478f9c4ccf1f43324e5674c670c
  store --data-prefix refs/forge --schema-prefix refs/schema \
    document inspect 846e71adba9fd478f9c4ccf1f43324e5674c670c
  store --data-prefix refs/forge --schema-prefix refs/schema \
    document publish comment 846e71adba9fd478f9c4ccf1f43324e5674c670c \
    --parent b495599ab78aaf7772f0014b6a8bca4ef40f2740 \
    --expected absent

  # Source alias: refs/forge/comment/review/1044d4ed268836b5d39d0b3ad11e4d3bfbd32d08/bd020a37cd60fc44ee12fb5ee8af5584f03a61ac
  # Canonical root: refs/forge/comment/e40679a67a921a37bad757a845e52b87405776ac
  store --data-prefix refs/forge --schema-prefix refs/schema \
    document inspect e40679a67a921a37bad757a845e52b87405776ac
  store --data-prefix refs/forge --schema-prefix refs/schema \
    document publish comment e40679a67a921a37bad757a845e52b87405776ac \
    --parent bd020a37cd60fc44ee12fb5ee8af5584f03a61ac \
    --expected absent

  # Source alias: refs/forge/comment/review/1044d4ed268836b5d39d0b3ad11e4d3bfbd32d08/e8615241699c068f5bfc3bee376d990a926606e0
  # Canonical root: refs/forge/comment/f508c09d097ea8b7b6f71b0284591a0cc517ffd1
  store --data-prefix refs/forge --schema-prefix refs/schema \
    document inspect f508c09d097ea8b7b6f71b0284591a0cc517ffd1
  store --data-prefix refs/forge --schema-prefix refs/schema \
    document publish comment f508c09d097ea8b7b6f71b0284591a0cc517ffd1 \
    --parent e8615241699c068f5bfc3bee376d990a926606e0 \
    --expected absent

  # Source alias: refs/forge/comment/review/1044d4ed268836b5d39d0b3ad11e4d3bfbd32d08/f869717358423a0a42fd4d3dc5046bfa99ce0df4
  # Canonical root: refs/forge/comment/265c223328ac474270c5869bb877db21a036ed53
  store --data-prefix refs/forge --schema-prefix refs/schema \
    document inspect 265c223328ac474270c5869bb877db21a036ed53
  store --data-prefix refs/forge --schema-prefix refs/schema \
    document publish comment 265c223328ac474270c5869bb877db21a036ed53 \
    --parent f869717358423a0a42fd4d3dc5046bfa99ce0df4 \
    --expected absent

  # Source alias: refs/meta/rules/review
  # Canonical root: refs/meta/rules/33053f7f8c4fca3040dcb85d1dc780315a5398dc
  store --data-prefix refs/meta \
    document inspect 33053f7f8c4fca3040dcb85d1dc780315a5398dc
  store --data-prefix refs/meta \
    document publish rules 33053f7f8c4fca3040dcb85d1dc780315a5398dc \
    --parent aa17bc7c79ed4bd992ee664a56dbce362bf6a65e \
    --expected absent
  ;;
git-forge)
  # Source alias: refs/forge/member/503bd6f4150c1edd020219847dcb3197bff91aea
  # Canonical root: refs/forge/member/6d5d32330aa0721159f71ed7fc429376a5d3477c
  store --data-prefix refs/forge --schema-prefix refs/schema \
    document inspect 6d5d32330aa0721159f71ed7fc429376a5d3477c
  store --data-prefix refs/forge --schema-prefix refs/schema \
    document publish member 6d5d32330aa0721159f71ed7fc429376a5d3477c \
    --parent 503bd6f4150c1edd020219847dcb3197bff91aea \
    --expected absent

  # Source alias: refs/meta/rules/review
  # Canonical root: refs/meta/rules/33053f7f8c4fca3040dcb85d1dc780315a5398dc
  store --data-prefix refs/meta \
    document inspect 33053f7f8c4fca3040dcb85d1dc780315a5398dc
  store --data-prefix refs/meta \
    document publish rules 33053f7f8c4fca3040dcb85d1dc780315a5398dc \
    --parent adb116625b808d65a2f55139168d9461beb57526 \
    --expected absent
  ;;
*)
  printf 'run this script from the git-store or git-forge repository\n' >&2
  exit 2
  ;;
esac
