# git-store

Store self-contained structured documents in Git with versioned schemas and
ordinary Git history. Every document carries the exact `{schema, value}` pair
used to encode it, so older values remain readable after a schema evolves and
ordinary reads do not depend on a current schema ref. The CLI is composable
plumbing for manually scripted migrations; it does not contain a hard-coded
migration workflow.

## Install

```console
cargo install --path crates/git-store
```

## Usage

```console
git store schema put recipe -F <schema.json>
git store put recipe carbonara -F <value.json>
git store get recipe carbonara
git store compile recipe <value.json>
git store value encode --schema refs/schema/recipe -F <value.json>
git store value decode <value-tree> --schema <schema-tree-or-commit>
git store document bind <value-tree> --schema <schema-tree-or-commit>
git store document inspect <document-tree>
git store document publish recipe <document-tree> --expected absent --alias <name>
git store cat <document-tree>
```

`git store put <kind> <name> [<value>]` is the everyday write: it encodes,
binds, and publishes in one step, advancing that name's ref.
`git store compile <kind> [<value>]` is the pure form of the same composition:
it creates a bound document whose root contains exactly `schema/` and `value/`,
then prints the document-tree OID without advancing a ref. The explicit `value`
and `document` commands expose the individual steps for scripts. `value encode` and
`value decode` always take an explicit schema tree or schema publication commit;
`document bind` does too. They do not infer a schema from a kind ref, commit
trailer, or caller-selected name.

The `schema/` tree is the exact schema used to validate `value/`, so
`git store cat` can decode the document from its own tree even when no
`refs/schema/*` ref is available. A published schema ref is a discovery,
history, or authoring index; it does not define the type of an existing
document.

The printed hash is the OID of that complete bound `{schema, value}` root tree.
It is the document's `EntityId`: it is not the publication commit OID and is
not a caller-provided name. Schema and value both contribute to the ID.
Canonical entity refs use that ID as the final segment:
`refs/store/<kind>/<entity-id>`. A caller-selected name is only a compatibility
alias, and aliases are not the source of truth for identity or the canonical
entity index.

`schema show <kind>` prints a kind's field layout as text and its full schema
record as JSON, reporting the publication commit OID and schema-tree OID
alongside it; omit `<kind>` to show every kind. `--at <commit>` addresses a
historical publication instead of the current one, and never substitutes the
current schema for it. `schema log <kind>` traces a kind's schema evolution,
newest first.

The plumbing inspection surface is deliberately Git-shaped:
`ref list [--prefix <full-prefix>] [--kind <kind>]`, `ref resolve <full-ref>`,
`object inspect <object-ish>`, and `object tree <tree-ish>`. These expose full
refs and stable object IDs rather than abbreviated, display-only names.

For additive plumbing commands, `--format text|json|ndjson` selects output;
`--json` is compact JSON. JSON records contain stable OIDs and full refs, and
NDJSON emits one record per item for list-like results. Success is on stdout;
diagnostics are on stderr. Current exit categories are `1` for other
operational errors, `2` for invalid arguments/object shape, `3` for missing
refs/objects/schemas/entities, `4` for CAS conflicts, and `5` for
schema/value/document failures.

`document publish` requires `--expected absent` or `--expected <full-oid>`. The
expectation applies to the canonical ref unless `--alias` is supplied, in which
case it applies to that alias. The canonical ref, alias, and materialized index
advance in one CAS batch. A stale expectation fails without retry; objects
written before a lost CAS may remain unreachable. Scripts own retry, resume,
and conflict policy.

`cat <tree-ish>` resolves a tree-ish and decodes the bound document directly:
content-addressed, like `git cat-file`, needing neither a name nor a schema
ref. `get <kind> <name>` resolves a name first, and accepts Git revision
suffixes on it (`carbonara~1`, `carbonara@{yesterday}`, `carbonara@<oid>`); it
also reports the publication commit, so a caller holds the compare-and-swap
token for the value it just read. The `rm <kind> <name>` command publishes a
typed tombstone over the canonical ref and any alias that points at that
publication; it does not hard-delete the refs. `get` reports a tombstoned entity as deleted and a missing ref as absent,
while repeated `rm` reports `already deleted` and `rm` of an absent ref fails.

New commits do not write `Schema:`, `Schema-Version:`, or `Ents-Ref:` trailers,
and new messages containing lines beginning with those reserved legacy trailer
names are rejected. Readers ignore those trailers when they occur on older Git
objects, including malformed or conflicting values, and never use them to
select a schema. A named write returns a publication commit OID while
maintaining the canonical ID/ref underneath.

## Entity state and compatibility

The library's `EntityState` distinguishes `Present`, `Absent`, and `Deleted`.
`Absent` means no canonical/alias ref exists, including an old hard-deleted
ref. `Deleted` means a ref still exists and points to a typed tombstone whose
value carries an explicit `Deleted` state, kind, and original `EntityId`.
Tombstones use the same bound `{schema, value}` frame as ordinary documents, so
they remain readable after the entity ref is fetched without the schema ref or
materialized index. `read` returns that state; project it with `value()` when
`Absent` and `Deleted` may both collapse to `None`, and match on
`EntityState` when the distinction matters.

Writing the same bound content after deletion restores the original canonical
entity: a normal value commit is appended after the tombstone, and aliases that
were pointing at that tombstone are restored atomically. Writing different
content creates a new `EntityId`; the old ID remains deleted. There is no CLI
restore command—recreation/restoration is performed through the library write
APIs.

`Kind::read` takes an `At` address — a name, an `EntityId`, a commit, or a
bound tree — so the content-derived identity and the caller-chosen name reach
the same reads. Prefer `put_entity`, `put_with_alias`, `compile_entity`, and
`entity_reference` when the content-derived identity is part of the
contract. `refs/schema/<publication>` remains a
schema publication/history ref, not the type definition of an existing
document. `entity delete <kind> <entity-id>` publishes a typed tombstone and
keeps the canonical ref; it does not hard-delete or prune the entity.

Bash, Git, or another caller owns migration traversal, source/target selection,
transforms, batching, retry/resume, and policy. The CLI provides no `migrate`
subcommand and does not silently rewrite stored objects. It does supply the
plumbing such a caller needs: `ls <kind> --stale` is the worklist of entities
not bound to the kind's current schema tree, `get` and `entity resolve` return
the compare-and-swap token alongside the value so no caller constructs a ref
path, and `document publish --batch` lands a rewritten set as one
all-or-nothing compare-and-swap. See the migration recipe in
[`crates/git-store/README.md`](crates/git-store/README.md#script-boundary).

## Compatibility limitations

A schema document must carry an embedded kind name and a schema-schema pin that
this build recognizes. Historical schema objects from before the embedded kind
field cannot be made self-describing by inferring a name from a ref; they are
not automatically upgraded. They require an explicit compatibility conversion
or republishing under the current schema format before new self-contained data
can be written against them. Likewise, base reads do not guess a migration
source or target: `read_as` is an explicit opt-in operation taking a
`TargetSchema` the caller selects, which `KindSchema::current_target` builds
from the selected `Kind`'s current published schema and history. If the source schema is not in that history, or a migration
edge is missing, the operation fails rather than silently guessing or rewriting
the stored object.

## Crates

- [`facet-git-tree`](crates/facet-git-tree) — Git tree encoding for Facet types.
- [`gix-refstore`](crates/gix-refstore) — compare-and-swap storage for Git refs.
- [`gix-store`](crates/gix-store) — schema-aware storage for Rust programs.
- [`git-store`](crates/git-store) — the command-line interface.

## Documentation

- [`docs/specification.adoc`](docs/specification.adoc) — tree serialization and deserialization.
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — contribution guide.
- [`CONDUCT.md`](CONDUCT.md) — code of conduct.
