# gix-store

The library behind [`git-store`](../git-store): store anything in a `gix`
repository as a real Git tree, keyed by a self-hosted schema. Oid-in,
oid-out — JSON belongs at a CLI boundary, never here.

```rust
use gix_store::{Store, schema_of};
use facet_value::value;

let repo = gix::discover(".")?;
let store = Store::open(&repo);

// A kind is defined by publishing its schema to refs/schema/<kind>.
# #[derive(facet::Facet)]
# struct Recipe { title: String, serves: u32 }
store.put_schema("recipe", &schema_of::<Recipe>()?)?;

// Entities live at refs/store/<kind>/<name>; every write is a commit.
store.store("recipe", "carbonara", &value!({ "title": "Carbonara", "serves": 4 }), None)?;
let got = store.retrieve("recipe", "carbonara")?;   // Some(Value)
# Ok::<(), Box<dyn std::error::Error>>(())
```

## How it works

- **Schemas are self-hosted.** A [`SchemaDoc`] is an ordinary `Facet` value
  stored through the same tree encoding as everything else, committed to
  `refs/schema/<kind>`. Its history is the kind's evolution audit.
- **Entities are schema-directed.** `store` encodes a
  [`facet_value::Value`] with `serialize_value_with_schema` — encoding *is*
  validation, so a nonconforming value fails with the offending path instead of
  writing a lossy tree. The result is byte-identical to the typed encoding.
- **Reads never guess, and never depend on a second ref.** Every data commit's
  tree is a two-entry root, `{value/, schema/}`: the value under `value/`, and
  the tree of the exact schema it was validated against under `schema/`.
  `retrieve`/`retrieve_at` read both straight out of the one commit, so they
  recover full fidelity — numbers as numbers, enums as tagged objects — and
  old versions stay readable after a kind evolves *and* after a `git fetch`,
  `git push`, or mirror that moves only the data ref. (A `Schema:`
  trailer is still written, naming the same schema commit for `git log`, but
  it is provenance only — nothing reads it back to resolve the schema.)
- **Writes are serialized, not lossy.** Each ref update takes a per-ref lock
  (under `<git-dir>/gix-store-locks/`, kept separate from git's own
  `<ref>.lock`) so concurrent writers — threads *or* processes — produce a
  linear history with no lost updates, then commits forward over the current
  tip.

## Scope

`gix-store` is the untrusted-single-writer-friendly demo primitive: one entity,
one ref, serialized forward. Signing, gate policy, event sinks, and multi-ref
atomic transactions are out of scope by design.

[`SchemaDoc`]: facet_git_tree::SchemaDoc
