# git-store

Store _anything_ in Git — not as a blob of JSON smuggled into a file, but as a
real tree the stock plumbing can read. Define a kind by publishing its schema,
then use the CLI's composable plumbing commands to encode, inspect, bind, and
optionally publish a document. JSON is only a CLI boundary; the stored objects
are ordinary Git trees.

The binary is named `git-store`, so git's external-subcommand dispatch makes
`git store …` work with nothing more than `PATH`.

## Demo

```console
$ git store schema put recipe -F crates/git-store/schemas/recipe.json
$ printf '%s\n' '{"title":"Carbonara","serves":4,"ingredients":["egg","pancetta"],"steps":["boil pasta","fry pancetta","combine"]}' > value.json
$ git store value encode --schema refs/schema/recipe -F value.json
<value-tree>
$ git store document bind <value-tree> --schema refs/schema/recipe
<document-tree>
$ git store document inspect <document-tree>
bound document <document-tree>
  value: <value-tree>
  schema: <schema-tree>
$ git store document publish recipe <document-tree> --expected absent --alias carbonara
<entity-id>
$ git store cat <document-tree> | jq .serves
4
```

`compile <kind> [<value>]` is a convenient pure compile: it prints the bound
document-tree OID and advances no ref. `put <kind> <name> [<value>]` does the
same, then publishes it under `<name>`, advancing that name's ref — arity
alone distinguishes the two verbs, so there is no flag to select between a
compile and a named write. The explicit `value` and `document` commands make
each step addressable for a manually scripted migration.

What landed is a real Git tree, not an opaque payload — `git ls-tree` and
`git cat-file` are still useful query tools. A bound document's root is exactly
two entries, `value/` and `schema/`; the schema used to validate the value
travels inside the document, so it stays readable wherever its tree goes, with
no dependence on also having `refs/schema/*`:

```console
$ git ls-tree refs/store/recipe/carbonara^{tree}
040000 tree …    schema
040000 tree …    value

$ git ls-tree refs/store/recipe/carbonara^{tree}:value
040000 tree …    ingredients
100644 blob …    serves
040000 tree …    steps
100644 blob …    title

$ git cat-file blob $(git rev-parse refs/store/recipe/carbonara^{tree}:value/serves)
4

$ git log --oneline refs/store/recipe/carbonara     # this name's history
9f50ef7 publish recipe
15e425b publish recipe

$ git store get recipe 'carbonara~1' | jq .serves   # name-addressed, with revision
4
```

The schema is data too — the current publication lives at
`refs/schema/<kind>` in the same repository, stored through the same tree
encoding, with the same history. `schema show <kind> --at <commit>` addresses
an historical publication commit directly:

```console
$ git ls-tree refs/schema/recipe^{tree}
040000 tree …    defs
040000 tree …    root

$ git store schema show
recipe
  title: string
  serves: uint
  ingredients: [string]
  steps: [string]

$ git store schema show recipe --at <schema-commit> --json
{ ... schema record: kind, commit, schema_tree, schema ... }
$ git store schema show recipe --at <schema-commit>
kind: recipe
commit: <schema-commit>
schema tree: <schema-tree>
...
```

## Why trees, not a JSON blob

A blob of JSON in a file is opaque to Git: `git log` shows "the file changed,"
diffs are line noise, and nothing but your application can read a field. Here,
every field is its own object, addressed by content:

- **`git` is the query language.** `git ls-tree`, `git cat-file`, `git log`,
  `git diff` all work — no application required to inspect stored data.
- **History and blame are per field.** Changing `serves` writes a new `serves`
  blob; the unchanged `title` blob keeps its object id across versions.
- **No format the reader must know.** The structure _is_ the Git tree. JSON
  exists only at this CLI's boundary (`facet-json` in, `facet-json` out); what
  is stored is the structural tree [`facet-git-tree`](../facet-git-tree)
  produces.
- **The schema isn't a config file on someone's laptop.** It is versioned data
  at `refs/schema/<kind>`, and every bound document carries the exact schema it
  was validated against inside its own `schema/` subtree — reachable, gc-safe,
  and fetch-complete by ordinary Git tree reachability — so old values stay
  readable after a kind evolves and after a fetch, push, or mirror that moves
  only the data ref. New commits contain no `Schema:`, `Schema-Version:`, or
  `Ents-Ref:` trailers. Readers ignore those legacy trailers on older Git
  objects, including malformed or conflicting values, and never use them to
  select a schema.

## Commands

```
git store                                   # print help
git store compile <kind> [<value>]          # pure compile; prints document-tree OID
git store put <kind> <name> [<value>]       # compile, then publish under <name>
git store cat <tree-ish>                    # decode any document tree, ref, or commit
git store get <kind> <name>                 # resolve a name, then decode it
git store check <tree-ish> --schema <kind>  # validate a value tree against a published schema
git store list [<kind>]   (alias: ls)       # kinds, or live entity names
git store log  <kind> <name>                # commit OID + date per publication
git store rm   <kind> <name>                # publish a tombstone over a name
git store schema put <kind> [-F <file>]    # define/evolve a kind (else stdin, or -i)
git store schema show [<kind>] [--at <commit>] # field layout, or full schema record as json
git store schema log <kind>                 # schema evolution history
git store ref list [--prefix <ref>] [--kind <kind>]
git store ref resolve <full-ref>            # resolve a full ref to an OID
git store object inspect <object-ish>       # object kind, OID, and size
git store object tree <tree-ish>            # direct tree entries
git store value encode --schema <tree-ish> [-F <file>]
git store value decode <value-tree> --schema <tree-ish>
git store document inspect <document-tree>
git store document bind <value-tree> --schema <schema-tree>
git store document publish <kind> <document-tree> --expected <absent|OID> [--alias <name>]
git store entity delete <kind> <entity-id>  # typed tombstone over a canonical entity id

# Global layout options (defaults shown):
git store --data-prefix refs/store --schema-prefix refs/schema <command>
git store --compat strict|legacy-leaves <command>  # how every decode reads stored leaves

# Authoring flags on compile and put:
    -F, --file <FILE>                       # content from a file (else stdin, else $EDITOR)
    -m, --message <MSG>                     # publication message (put only)
    -e, --edit                              # edit content in $VISUAL/$EDITOR first
    -i, --interactive                       # build a value by prompting for each field
```

Writing is always an explicit `compile` or `put`, and a bare invocation
prints help, like any clap app; `list` (alias `ls`) is the explicit way to
list kinds. At a terminal with no `-F` and nothing piped, `compile`/`put`
open `$EDITOR` seeded from the selected schema, like `git notes add`. The
explicit plumbing commands never infer a schema from a kind ref, commit
trailer, or caller-selected name: pass `--schema` to `check`, `value encode`,
`value decode`, and `document bind`.

All commands accept the global `--data-prefix` and `--schema-prefix` options.
They select the data/compatibility and schema ref namespaces for the entire
invocation, respectively; both default to `refs/store` and `refs/schema`. This
also controls kind/schema operations, compatibility reads and deletes, prepared
publication, and `ref list --kind` filtering. The options are useful when a
repository keeps multiple independent stores in namespaces such as
`refs/legacy-data` and `refs/legacy-schema`.

`-i` skips the editor and walks you through the value one field at a time —
arrow-key menus for enum variants and schema types, yes/no for options and list
items, text and number inputs for leaves (a rich terminal UI at a tty, a plain
one-answer-per-line reader when stdin is piped, so it stays scriptable). For
`schema put`, the same `-i` walks the type grammar to build a schema without
hand-written JSON.

## Authoring a schema

A schema is itself a `Facet` value, so there is no bespoke schema language: hand-write
the JSON, or derive it from a Rust struct. The
[`schemas`](examples/schemas.rs) example does the latter — the struct authors
the schema:

```console
$ cargo run -p git-store --example schemas -- recipe | git store schema put recipe
```

(`schema put` reads stdin when given no `-F`.)

The checked-in [`schemas/`](schemas) files were generated exactly this way.

## Plumbing contracts

### Explicit schemas and document composition

`value encode --schema <tree-ish>` validates JSON from stdin or `-F <file>`
against exactly the schema tree selected by the argument and prints the encoded
value-tree OID. `value decode <value-tree> --schema <tree-ish>` performs the
inverse using that same explicit selection. The schema argument may be a schema
tree, a schema publication commit, or any Git revision that peels to the schema
tree; no kind lookup, schema-history guess, or trailer is consulted.

`document bind <value-tree> --schema <schema-tree>` creates only the complete
`{schema/, value/}` root tree. `document inspect <document-tree>` reports
`bound`, `legacy_value_root`, or `malformed` without guessing. `document publish`
then commits a prepared bound tree and updates the named ref and materialized
index in one compare-and-swap batch.

`schema show <kind> --at <commit>` returns the schema snapshot at an explicit
publication commit; without `--at` it returns the current snapshot. Text
renders the field layout (plus, with `--at`, the commit and schema-tree OIDs);
`--json`/`--format json` render the full schema record (kind, commit,
schema-tree OID, and schema). With no `<kind>` at all, it lists every
published kind's field layout, and `--at` is rejected since there is no
single kind to select a revision of. This is the historical schema selection
surface; it does not mutate history. (`schema get`, `schema inspect`, and
`schema list` remain as hidden, deprecated aliases.)

This and every other decoding command are strict by default. Pass the global
`--compat legacy-leaves` only when reading pre-`kind` schema documents or
schema trees whose leaf blobs predate the newline framing. It applies the
explicit compatibility decoder for the whole invocation and exports the
normalized JSON without changing ordinary reads. (`--legacy-leaves` remains
as a hidden, deprecated per-invocation alias.)

### Refs, objects, and machine output

`ref list` emits full ref names and their OIDs; `ref resolve` accepts a full ref
name and emits its OID. `object inspect` resolves an object or revision and
reports its kind, full OID, and byte size. `object tree` lists direct entries
with mode, OID, kind, and name. Text output is intended for people.

**Every command honors every format.** `--format text|json|ndjson` (default
`text`) and `--json` (a hidden alias for `--format json`) are global options,
resolved once, and every command — porcelain and plumbing alike — renders its
result through them; none falls back to text silently. JSON output uses
records with stable OIDs and full refs; `ndjson` emits one JSON record per
item for list-like commands (`ls`, `log`, `ref list`, `object tree`, `schema
show`) and one record for single-result commands. Text and JSON are
information-equivalent: both are rendered from the same result value, just
through a different renderer, so neither can drift arbitrarily far from the
other. Successful machine output is on stdout. Diagnostics are on stderr and
a failed command does not emit a success record.

The current exit categories are: `1` other operational errors; `2` invalid
arguments or object shape; `3` missing refs, objects, schemas, or entities;
`4` compare-and-swap conflicts; and `5` schema, value, or document failures.
These categories are intended for scripts; callers should still preserve stderr
for the actionable diagnostic.

### Publication, identity, and deletion

`document publish` requires `--expected absent` or `--expected <full-oid>`. With
`--alias <name>` it publishes at `<data-prefix>/<kind>/<name>`; with no alias it
publishes at the entity's content-derived name. The expectation applies to that
ref. A stale expectation fails with exit category
`4`, publishes none of the batch, and is **not retried**. Objects written before
a lost CAS may remain unreachable; a script decides whether to retry, resume, or
report the conflict. The CLI does not hide that policy.

The bound document-tree OID is the content-derived `EntityId`, so both schema
and value contribute to identity. It is a derived value, independent of where
the document is published. Ref layout is application policy: an entity lives at
`<data-prefix>/<kind>/<name>` for whatever name the caller picks, and naming it
`<entity-id>` is one such choice rather than a rule the store imposes. The
per-kind index under `refs/cache/` is a materialized cache of the entity refs:
those refs remain authoritative, and readers can fall back to them when the
index is absent, malformed, or stale. `entity delete <kind> <entity-id>`
addresses an entity by its canonical, content-derived id and publishes a typed
tombstone over that ref, updating the index atomically without pruning the
ref; `rm <kind> <name>` is the equivalent name-addressed porcelain.

### Script boundary

The CLI intentionally has no hard-coded `migrate` workflow. Bash, Git, and the
calling program own traversal, source/target schema selection, transforms,
batching, retry/resume, and policy. `git-store` supplies stable plumbing for
reading and composing objects, plus explicit CAS publication. The library's
read-time migration APIs are separate opt-in conveniences; neither the CLI's
base reads nor its plumbing commands silently rewrite stored objects.
