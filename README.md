# git-store

Store structured values in Git with versioned schemas and ordinary Git history.
Older values remain readable after a schema evolves, and migrations do not
rewrite stored objects.

## Install

```console
cargo install --path crates/git-store
```

## Usage

```console
git store schema put <kind> -F <schema.json>
printf '<value>' | git store put <kind> <name>
git store get <kind> <name>
```

Every write creates a Git commit. Values can be read at any revision, and
schema migrations are applied when they are read.

## Crates

- [`facet-git-tree`](crates/facet-git-tree) — Git tree encoding for Facet types.
- [`gix-refstore`](crates/gix-refstore) — compare-and-swap storage for Git refs.
- [`gix-store`](crates/gix-store) — schema-aware storage for Rust programs.
- [`git-store`](crates/git-store) — the command-line interface.

## Documentation

- [`docs/specification.adoc`](docs/specification.adoc) — tree serialization and deserialization.
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — contribution guide.
- [`CONDUCT.md`](CONDUCT.md) — code of conduct.
