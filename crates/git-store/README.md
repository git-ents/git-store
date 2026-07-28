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
`git cat-file` are the query language. An entity commit's tree is a two-entry
root, `value/` and `schema/` — the schema an entity was validated against
travels inside the commit itself, so the entity stays readable wherever the
commit goes, with no dependence on also having `refs/schema/*`:

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

$ git log --oneline refs/store/recipe/carbonara     # history for free
9f50ef7 store recipe/carbonara
15e425b store recipe/carbonara

$ git store get recipe 'carbonara~1' | jq .serves   # time-travel by revision
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
  at `refs/schema/<kind>`, and every entity commit carries the tree of the
  exact schema it was validated against inside its own `schema/` subtree —
  reachable, gc-safe, and fetch-complete by ordinary Git tree reachability —
  so old versions stay readable after a kind evolves, and after a `git fetch`,
  `git push`, or mirror that moves only the entity ref. (A `Schema:`
  commit-message trailer names the same schema commit too, for `git log` —
  human-readable provenance, not a second read path.)

## Commands

```
git store                                   # list kinds
git store put <kind> [<name>]               # store an entity; name defaults to the kind
    -F, --file <FILE>                       #   content from a file (else stdin, else $EDITOR)
    -m, --message <MSG>                     #   commit message for this version
    -e, --edit                              #   edit the content in $VISUAL/$EDITOR first
    -i, --interactive                       #   build the value by prompting for each field
git store get  <kind> <name>                # read back as JSON; <name> may carry a
                                            #   revision to read a past version:
                                            #   <name>~N, <name>@{date}, <name>@<oid>
git store list [<kind>]   (alias: ls)       # kinds, or entity names within a kind
git store log  <kind> <name>                # oid + date per version
git store rm   <kind> <name>                # delete an entity
git store schema put  <kind> [-F <file>]    # define or evolve a kind (else stdin, or -i to prompt)
git store schema get  <kind>                # the schema as JSON
git store schema show [<kind>]              # field layout, human-readable (all kinds when omitted)
git store schema list     (alias: ls)       # kinds that have a published schema
git store schema log  <kind>                # schema evolution history
```

Writing is always an explicit `put`, and reads default to listing — the same
shape as `git remote`/`git branch`. At a terminal with no `-F` and nothing
piped, `put` opens `$EDITOR` seeded from the schema, like `git notes add`.

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
