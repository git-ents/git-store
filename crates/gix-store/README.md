# gix-store

The library behind [`git-store`](../git-store): store anything in a Git
repository as a real Git tree, keyed by a self-hosted schema. Oid-in,
oid-out — JSON belongs at a CLI boundary, never here.

```rust
use gix_store::{RefSegment, Store, schema_of};
use facet_value::value;

#[derive(facet::Facet)]
struct Recipe { title: String, serves: u32 }

let repo = gix::discover(".")?;
let store = Store::open(&repo);
let recipes = store.kind::<Recipe>(RefSegment::new("recipe")?);

// A kind is defined by publishing its schema to refs/schema/<kind>.
recipes.publish()?;

// Entities live at refs/store/<kind>/<name>; every write is a commit.
recipes.put(&RefSegment::new("carbonara")?, &Recipe { title: "Carbonara".into(), serves: 4 })?;
let got: Option<Recipe> = recipes.get(&RefSegment::new("carbonara")?)?;

// The same refs, read and written as dynamic values instead.
let dynamic = store.dynamic(RefSegment::new("recipe")?);
dynamic.put(&RefSegment::new("cacio")?, &value!({ "title": "Cacio e Pepe", "serves": 2 }))?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

## How it works

- **Typed and dynamic are one code path.** A `Kind` handle fixes the kind and
  its [`Encoding`] at construction, so the kind's validity is a fact of the
  type rather than a string checked on every call. `Typed<T>` encodes a
  `Facet`-derived Rust type natively; `Dynamic` encodes a
  [`facet_value::Value`] against the kind's published schema. Everything above
  the encoding — refs, commits, schema binding — is shared, and the two
  encodings are byte-identical, so the two handles interoperate over the same
  refs.
- **Schemas are self-hosted.** A [`Schema`] is an ordinary `Facet` value
  stored through the same tree encoding as everything else, committed to
  `refs/schema/<kind>`. Its history is the kind's evolution audit. Each stored
  schema pins the generation of the schema-schema it was written against, so a
  document this binary does not speak is refused rather than misread.
- **Encoding is validation.** A value that does not conform to its kind's
  schema fails with the offending path instead of writing a lossy tree.
- **Reads never guess, and never depend on a second ref.** Every data commit's
  tree is a two-entry root, `{value/, schema/}`: the value under `value/`, and
  the tree of the exact schema it was validated against under `schema/`.
  `get`/`get_at` read both straight out of the one commit, so old versions stay
  readable after a kind evolves *and* after a `git fetch`, `git push`, or
  mirror that moves only the data ref. (A `Schema:` trailer is still written,
  naming the same schema commit for `git log`, but it is provenance only —
  `Store::provenance` returns it as a label, and nothing reads it back to
  resolve a schema.)
- **Writes compare and swap.** Each entity write reads the entity and index
  refs, writes its commit and the next index tree, then applies both edits with
  one [`RefStore::apply_batch`](gix_refstore::RefStore::apply_batch)
  transaction. A lost race retries the whole read/build/publication sequence;
  object writes may remain unreachable, but the old pair of refs remains valid.
- **Kinds have a materialized index.** The private cache ref
  `refs/gix-store/index/v1/<kind-encoding>` points directly to a canonical Git
  tree. Kind names are encoded as `k` followed by lowercase hexadecimal UTF-8
  bytes, an injective encoding. Tree entries use `160000` commit mode and
  encode each length-framed, nested `RefPath` as one flat filename, avoiding
  the Git tree file/directory prefix collision while preserving every segment.
  The existing fingerprint domain and kind-name component are unchanged, so
  schema refs remain excluded and different kinds cannot share a cache key.
- **Indexes are advisory.** Entity refs remain authoritative. Reads validate a
  present index against the complete entity-ref mapping and fall back safely to
  that mapping when the index is absent, malformed, or stale. This preserves
  correctness after low-level or external ref writes, at the cost of validation
  work; `Kind::rebuild_index` is the explicit repair/materialization API and
  getters never publish refs implicitly. Writes through `Kind` maintain the
  pair atomically. Schema interpretation is still separate from this
  entity-only fingerprint.

## Backends

`Store` is generic over a [`gix_refstore::RefStore`] for refs and a
`gix_object::Find`/`Write` object database for objects.
`Store::open(&repo)` is the specialization over a real repository. For workloads
that repeatedly read objects, configure gix's object cache before opening the
store:

```rust
let mut repo = gix::discover(".")?;
repo.object_cache_size_if_unset(4 * 1024 * 1024);
let store = Store::open(&repo);
```

`Store::new(MemoryRefStore::new(), ObjectStore::default())` is the same store
with no filesystem behind it, which is what most of this crate's own tests use.

## Scope

`gix-store` keeps entity refs as the source of truth and maintains the
per-kind index only as a persisted read cache. Low-level ref access remains an
escape hatch; its changes are detected by read validation rather than trusted
as index maintenance. Signing, gate policy, and event sinks are out of scope.

[`Schema`]: facet_git_tree::Schema
[`RefEdit`]: gix_refstore::RefEdit
