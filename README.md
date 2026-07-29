# git-store

Store *anything* in Git — not as a blob of JSON smuggled into a file, but as a real tree the stock plumbing can read.
Define a kind by publishing its schema, pipe JSON at it, and every field becomes a tree entry, every write a commit.

The binary is named `git-store`, so git's external-subcommand dispatch makes `git store …` work with nothing more than `PATH`.

```console
cargo install --path crates/git-store
```

## Demo

```console
$ git store schema put recipe -F crates/git-store/schemas/recipe.json
984ccde

$ echo '{"title":"Carbonara","serves":4,"ingredients":["egg","pancetta"],
        "steps":["boil pasta","fry pancetta","combine"]}' \
    | git store put recipe carbonara
47b3037

$ git store get recipe carbonara | jq .serves
4
```

What landed is a real Git tree, not an opaque payload — `git ls-tree` and `git cat-file` are the query language:

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
```

The commit carries the schema it was written against beside the value, so it is readable on its own terms.
Fetch just this one ref into an empty repository and the read still works — nothing has to know where the schema lives, because it came along.

Every write is a commit, so history and time travel come for free:

```console
$ git store get recipe carbonara | jq '.serves = 6' | git store put recipe carbonara
39da00d

$ git log --oneline refs/store/recipe/carbonara
39da00d store recipe/carbonara
47b3037 store recipe/carbonara

$ git store get recipe 'carbonara~1' | jq .serves   # time-travel by revision
4
```

## Why trees, not a JSON blob

A blob of JSON in a file is opaque to Git: `git log` shows "the file changed," diffs are line noise, and nothing but your application can read a field.
Here, every field is its own object, addressed by content:

- **`git` is the query language.**
  `git ls-tree`, `git cat-file`, `git log`, `git diff` all work — no application required to inspect stored data.
- **History and blame are per field.**
  Changing `serves` writes a new `serves` blob; the unchanged `title` blob keeps its object id across versions.
- **No format the reader must know.**
  The structure *is* the Git tree.
  JSON exists only at this CLI's boundary.
- **The schema isn't a config file on someone's laptop.**
  It is versioned data at `refs/schema/<kind>`, stored through the same tree encoding, and every entity names the exact schema it was validated against — so old versions stay readable after a kind evolves.

See [`crates/git-store`](crates/git-store) for the full demo, including the schema's own history.

## The four crates

This repository builds `git-store` in layers, each usable on its own — read the one that matches what you're trying to do:

- [`facet-git-tree`](crates/facet-git-tree) — the serialization core.
  Encodes any [`facet`](https://facet.rs) type, or a dynamic `facet_value::Value`, into a Git tree and back.
  Oid-in/oid-out, with no concept of a repository or a ref.
  Start here if you want the tree encoding on its own, with no opinions about how it's stored.
- [`gix-refstore`](crates/gix-refstore) — compare-and-swap storage for Git refs.
  Validated ref-name types and one atomic edit primitive, with a `gix` backend and an in-memory one.
  Start here if you want the ref half without the schema half.
- [`gix-store`](crates/gix-store) — the library.
  Layers self-hosted schemas and ref conventions (`refs/store/<kind>/<name>`, `refs/schema/<kind>`) on top of `facet-git-tree`, over any `gix-refstore` backend and any `gix` object database.
  Start here if you're embedding storage in a Rust program.
- [`git-store`](crates/git-store) — the CLI shown above, built on `gix-store`.
  Start here if you just want `git store put`/`get` at a terminal.

## Documentation

- [`docs/specification.adoc`](docs/specification.adoc) — the tree serialization and deserialization specification.
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — how to contribute.
- [`CONDUCT.md`](CONDUCT.md) — code of conduct.
