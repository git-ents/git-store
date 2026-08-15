# Development plan: align `git-store` with the settled design

## Objective

Bring `git-store` from its current mixed state to the settled storage model without
losing the useful work already present in embedded `{schema, value}` frames.
The end state must be self-describing at the document boundary, independent of
schema refs for ordinary reads, free of schema/provenance trailers, and explicit
about identity, bootstrap, and deletion semantics.

This plan is intentionally staged. Each stage should leave the workspace
buildable and should add regression coverage before removing the old behavior.

> **Final implementation status.** The current implementation has reached the
> storage contract described below; the phase sections that follow are retained
> as historical design and rollout context, not as a list of unimplemented
> changes.
>
> - `EntityId` is the OID of the complete bound document tree, whose root is
>   exactly `{schema/, value/}`. It is neither a publication commit OID nor a
>   caller-selected name. Canonical refs are direct
>   `refs/store/<kind>/<entity-id>` children. Named refs are compatibility
>   aliases, excluded from the canonical index, and may be maintained for old
>   callers.
> - `EntityState::{Present, Absent, Deleted}` distinguishes a live value, no
>   ref (including an old hard-deleted ref), and an explicit typed tombstone.
>   Tombstones are ordinary bound documents carrying `TombstoneState::Deleted`,
>   the kind, and the original entity ID. Repeating delete is idempotent; exact
>   content restores the tombstoned ID, while different content creates a new
>   ID.
> - `git store get <tree-ish>` decodes a bound tree directly. The two-argument
>   `get <kind> <name>` and `rm <kind> <name>` forms are compatibility paths:
>   `get` distinguishes deleted from absent, and `rm` publishes a tombstone
>   rather than removing refs.
> - New commits reject and do not emit lines beginning with the reserved
>   `Schema:`, `Schema-Version:`, or `Ents-Ref:` trailer names. Readers ignore
>   those trailers on historical objects and select schemas from the embedded
>   tree only.
> - Old schema documents without an embedded kind name are not repaired by
>   guessing from a ref and are not automatically upgraded. They need an
>   explicit conversion or republishing under the current format. Base reads do
>   not use migration history; the `get_migrated` family is an explicit opt-in
>   compatibility convenience whose current target is the selected `Kind`'s
>   current published schema/history. Missing source history or migration edges
>   fail rather than being guessed, and stored objects are not rewritten.

## Target invariants

The implementation is complete when all of these are true:

1. A data document's root contains exactly `schema/` and `value/`.
2. `schema/` contains the exact schema tree used to validate `value/`; ordinary
   reads decode only those two subtrees from the document commit.
3. The schema document contains its kind name. A ref namespace is an index or
   publication mechanism, not the type definition.
4. Newly written commits contain no `Schema:`, `Schema-Version:`, or `Ents-Ref:`
   trailers. Existing trailers may be ignored for backward-compatible reads,
   but must never be required to decode or resolve a schema.
5. The meta-schema is the reification of `facet::Shape`, has an empty-tree
   `schema` entry (`4b825dc...` for SHA-1 Git), and passes a fixed-point
   decode/re-encode check.
6. A `doctor` check can verify the fixed point against a compile-time digest.
7. Entity identity is derived from the stored content according to one
   documented rule and is used consistently; caller-selected names are not the
   authoritative identity.
8. Deletion has an explicit, documented representation that remains
   distinguishable from an absent/pruned ref.
9. Schema history and migration are explicit authoring/history operations, not
   hidden dependencies of base reads.

## Current baseline

The existing implementation already provides the most important reachability
property: `Store::bind_schema()` creates the two root entries, and ordinary
entity decoding reads the embedded schema. Preserve that representation and
its fetch-without-`refs/schema/*` regression while changing the surrounding
APIs.

The main remaining seams are:

- `crates/gix-store/src/kind.rs`: kind lookup, put/remove behavior, commit
  construction, schema publication, and migrated reads.
- `crates/gix-store/src/store.rs`: kind/dynamic handles and provenance access.
- `crates/gix-store/src/provenance.rs`: trailer parsing and public provenance.
- `crates/gix-store/src/migrate.rs`: schema-history-dependent upcasting.
- `crates/facet-git-tree/src/schema/mod.rs`: the schema document shape.
- `crates/facet-git-tree/src/schema/pin.rs` and `schema/codec.rs`: current
  generation/bootstrap machinery.
- `crates/gix-store/tests/{document,identity,repository,store}.rs`: frame,
  identity, repository, and API regression coverage.
- `crates/git-store/src/main.rs`: CLI commands, including the future
  `doctor` command.
- `README.md` and `crates/gix-store/README.md`: currently document named refs
  and the `Schema:` trailer and must be updated with the final contract.

## Phase 0: freeze the contract and establish characterization tests

Before changing public APIs, write down the exact settled wire/ref contract in
`docs/` or the existing specification, then add tests for behavior that must
not regress:

- assert the data root is exactly `{schema, value}`;
- fetch/copy only the entity ref and verify ordinary decode still succeeds;
- verify the embedded schema tree OID is the schema identity used by the
  decoder;
- verify old schema versions remain readable without consulting the current
  schema ref;
- add a test that records the current behavior of old documents with
  `Schema:` trailers so compatibility policy is deliberate rather than
  accidental.

Run the focused baseline before implementation:

```console
cargo test -p gix-store
cargo test -p facet-git-tree
```

## Phase 1: make the schema document self-contained

### 1.1 Add the kind name to `Schema`

Extend `facet_git_tree::Schema` with the settled field for the kind name and
update all construction, serialization, deserialization, equality, and schema
validation paths. Preserve deterministic field ordering and ensure the name is
validated using the same identity/name normal-form rules already enforced in
`gix-store`.

Update:

- `crates/facet-git-tree/src/schema/mod.rs`;
- schema read/write and codec fixtures;
- `crates/gix-store/src/kind.rs` schema registration and publication;
- schema tests and any `Schema` literals throughout the workspace.

Add tests proving that two schema documents with different embedded kind names
are different content and that decoding does not infer the name from a ref.

### 1.2 Replace the bootstrap with the `facet::Shape` fixed point

Refactor the custom `SchemaSchema` generation/pinning path so the canonical
meta-schema is the reification of `facet::Shape`. Its own schema entry must be
the empty tree. Keep a compile-time expected hash, but make it a consequence of
the canonical representation rather than a generation pin that silently
changes with feature selection.

Implement a single checked bootstrap path that:

1. constructs the canonical meta-schema;
2. encodes it using the empty-tree schema identity;
3. decodes it through the normal schema reader;
4. re-encodes it;
5. compares the resulting tree/object identity (and, where applicable, bytes)
   with the compile-time expected digest.

Update `schema/pin.rs`, `schema/codec.rs`, and self-hosting tests. Keep the
error actionable: report the expected and observed digest and whether the
failure occurred during decode or re-encode.

### 1.3 Add `doctor`

Add a library-level doctor/check function so validation is not coupled to CLI
argument parsing. Add `git store doctor` in `crates/git-store/src/main.rs` that
runs the check against the repository/object store and exits nonzero with a
useful diagnostic on failure.

The command should validate at least the meta-schema fixed point and the
currently supported Git object format. Add CLI integration coverage or a
command-level test using the existing test support crate.

## Phase 2: remove trailer and provenance coupling

Stop writing `Schema:` in `kind.rs` commit creation. Remove schema trailer
parsing from the ordinary store API and delete or narrow
`Store::provenance()`/`provenance.rs` so it cannot be mistaken for schema
resolution.

Required tests:

- every newly created data and schema commit has an empty/non-schema trailer
  set;
- ordinary reads succeed when all trailers are stripped;
- malformed or conflicting legacy trailers do not change the schema selected
  for decoding;
- the repository regression that currently asserts `Schema:` is replaced with
  the zero-trailer invariant.

For compatibility, retain only the minimum read-side handling needed for old
objects, and document that legacy trailers are ignored. Do not preserve a
public API whose purpose is to expose schema metadata if the settled design
requires zero provenance trailers.

## Phase 3: separate type identity from ref namespace

Introduce an explicit schema/document lookup model rather than making
`Kind::new("recipe")` the source of truth for type selection.

Suggested sequence:

1. Define a schema identity type based on the schema tree/content digest.
2. Make schema publication refs point to that identity/history, while storing
   the kind name inside the schema document.
3. Add APIs that can decode a document from its embedded schema without a kind
   handle or schema ref.
4. Retain named convenience APIs only as an optional index/compatibility layer,
   and make their delegation to content-based lookup explicit.
5. Update migration APIs so they require an explicitly selected target schema
   rather than silently treating the current `refs/schema/<kind>` value as
   part of a base read.

Use `kind.rs`, `store.rs`, `migrate.rs`, and `index.rs` as the primary change
sites. Add tests for:

- decoding from an entity commit with no kind/ref namespace supplied;
- moving or renaming a publication ref without changing decode behavior;
- two kinds with distinct embedded names/content not colliding in lookup;
- base reads never invoking schema-history lookup;
- migrated reads explicitly requiring the target schema/history and failing
  clearly when it is unavailable.

Do not remove the old named API until callers in the CLI and workspace tests
have migrated; mark it deprecated if a compatibility release is required.

## Phase 4: make entity identity content-derived

First settle and document the exact derivation rule. A literal Git commit OID
is not available until after commit metadata and the parent are known, so a
rule based on the commit OID can be unstable across equivalent writes. The
recommended rule is to derive the entity ID from the canonical document tree
(or complete frame) OID, then use that ID in the entity ref/index. If the
settled design truly requires the commit OID, specify the two-step publication
and its implications for retries, timestamps, and parent-dependent identity
before implementation.

Then:

- expose an API that returns the derived entity ID for any document, published
  or not, so identity never depends on where a document was written;
- keep ref naming as application policy: `put(name, value)` publishes at the
  caller's name, and naming an entity by its derived ID is one available
  choice rather than a layout the store imposes;
- define collision behavior and idempotent repeated writes;
- migrate CLI syntax and documentation away from mandatory caller-controlled
  names;
- preserve a compatibility read path for existing `refs/store/<kind>/<name>`
  refs during the transition, if required by the release policy.

Expand `identity.rs`, repository tests, index tests, and concurrent/CAS tests.
Verify that identical canonical documents deduplicate and that changing any
identity-bearing content changes the derived ID.

## Phase 5: define and implement deletion/tombstones

Choose the tombstone wire representation before changing `remove()`. It must
be a normal content-addressed document, distinguishable from absence, and must
remain readable when an entity ref is fetched without the latest index or
history. The representation should reuse the `{schema, value}` frame where
possible, with an explicit deletion state in the value/schema contract rather
than an empty tree that could be confused with pruning.

Implement:

- a typed tombstone state and validation at the document boundary;
- atomic publication of tombstone/entity refs and index updates;
- reads that return a distinct deleted result rather than `None`;
- explicit restore/recreate semantics;
- compatibility behavior for old hard-deleted refs.

Add tests for delete, fetch, mirror, prune/partial-fetch simulation, repeated
delete, restore, and concurrent delete-vs-update races. Update the CLI and
README to distinguish “not found” from “deleted”.

## Phase 6: migration, compatibility, and documentation cleanup

After the new primitives are in place:

- make schema history an opt-in migration source, not a dependency of base
  decoding;
- define how old documents lacking the new kind-name field are handled;
- define whether old named entity refs are rewritten, indexed as aliases, or
  supported indefinitely;
- define whether old `Schema:` trailers are retained only as ignored bytes;
- update `README.md`, `crates/gix-store/README.md`, and relevant API docs to
  remove claims that the kind ref defines the type or that trailers are useful
  provenance;
- update examples and CLI help;
- add a compatibility section to the specification with versioning and
  rollout expectations.

Avoid rewriting historical Git objects. Prefer readers that understand both
formats and writers that emit only the settled format. If an incompatible
wire/schema change is unavoidable, gate it behind an explicit format version
and provide a diagnostic rather than silently guessing.

## Validation matrix

Run the narrowest relevant checks after each phase, then the full workspace
suite before declaring completion:

```console
cargo fmt --all -- --check
cargo test -p facet-git-tree
cargo test -p gix-store
cargo test -p git-store
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Also run the doctor check in a repository containing:

- the canonical meta-schema;
- a current document with no schema ref available;
- an old document with a legacy `Schema:` trailer;
- a tombstoned entity;
- a schema history with at least two versions.

## Suggested delivery order

1. Phase 0 characterization tests and written contract.
2. Phase 1 schema self-hosting/bootstrap and `doctor`.
3. Phase 2 trailer removal.
4. Phase 3 content-based schema/type lookup.
5. Phase 4 entity identity migration.
6. Phase 5 tombstones.
7. Phase 6 compatibility and documentation cleanup.

The ordering matters: bootstrap and schema identity must be stable before
entity refs are changed, and identity must be settled before tombstone refs and
indexes are finalized. Each phase should be reviewed as a separately testable
change, with no force-push or historical object rewrite required.
