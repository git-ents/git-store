# git-store

Store *anything* in Git — not as a blob of JSON smuggled into a file, but as a
real tree the stock plumbing can read. Define a kind by publishing its schema,
pipe JSON at it, and every field becomes a tree entry, every write a commit.

The binary is named `git-store`, so git's external-subcommand dispatch makes
`git store …` work with nothing more than `PATH`.

## Demo

```console
$ git store schema put recipe -F crates/git-store/schemas/recipe.json
$ echo '{"title":"Carbonara","serves":4,"ingredients":["egg","pancetta"],
        "steps":["boil pasta","fry pancetta","combine"]}' \
    | git store put recipe carbonara
$ git store get recipe carbonara | jq .serves
4
```

What landed is a real Git tree, not an opaque payload — `git ls-tree` and
`git cat-file` are the query language:

```console
$ git ls-tree refs/store/recipe/carbonara^{tree}
040000 tree …    ingredients
100644 blob …    serves
040000 tree …    steps
100644 blob …    title

$ git cat-file blob $(git rev-parse refs/store/recipe/carbonara^{tree}:serves)
4

$ git log --oneline refs/store/recipe/carbonara     # history for free
9f50ef7 store recipe/carbonara
15e425b store recipe/carbonara

$ git store get recipe carbonara --at '~1' | jq .serves   # time-travel by revision
4
```

The schema is data too — it lives at `refs/schema/<kind>` in the same
repository, stored through the same tree encoding, with the same history:

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
```

## Why trees, not a JSON blob

A blob of JSON in a file is opaque to Git: `git log` shows "the file changed,"
diffs are line noise, and nothing but your application can read a field. Here,
every field is its own object, addressed by content:

- **`git` is the query language.** `git ls-tree`, `git cat-file`, `git log`,
  `git diff` all work — no application required to inspect stored data.
- **History and blame are per field.** Changing `serves` writes a new `serves`
  blob; the unchanged `title` blob keeps its object id across versions.
- **No format the reader must know.** The structure *is* the Git tree. JSON
  exists only at this CLI's boundary (`facet-json` in, `facet-json` out); what
  is stored is the structural tree [`facet-git-tree`](../facet-git-tree)
  produces.
- **The schema isn't a config file on someone's laptop.** It is versioned data
  at `refs/schema/<kind>`, and every entity commit names the exact schema commit
  it was validated against, so old versions stay readable after a kind evolves.

## Commands

```
git store                                   # list kinds
git store put <kind> [<name>]               # store an entity; name defaults to the kind
    -F, --file <FILE>                       #   content from a file (else stdin, else $EDITOR)
    -m, --message <MSG>                     #   commit message for this version
    -e, --edit                              #   edit the content in $VISUAL/$EDITOR first
git store get  <kind> <name> [--at <rev>]   # read back as JSON; --at reads a past version (oid, ~N, @{date})
git store list [<kind>]   (alias: ls)       # kinds, or entity names within a kind
git store log  <kind> <name>                # oid + date per version
git store rm   <kind> <name>                # delete an entity
git store schema put  <kind> [-F <file>]    # define or evolve a kind (else stdin)
git store schema get  <kind>                # the schema as JSON
git store schema show [<kind>]              # field layout, human-readable (all kinds when omitted)
git store schema list     (alias: ls)       # kinds that have a published schema
git store schema log  <kind>                # schema evolution history
```

Writing is always an explicit `put`, and reads default to listing — the same
shape as `git remote`/`git branch`. At a terminal with no `-F` and nothing
piped, `put` opens `$EDITOR` seeded from the schema, like `git notes add`.

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
