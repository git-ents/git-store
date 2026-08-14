# gix-store

The library behind [`git-store`](../git-store): store anything in a Git
repository as a real Git tree, keyed by a self-hosted schema. Oid-in,
oid-out — JSON belongs at a CLI boundary, never here.

```rust
use gix_store::{EntityState, RefPath, RefSegment, Store, schema_of};
use facet_value::value;

#[derive(facet::Facet)]
struct Recipe { title: String, serves: u32 }

let repo = gix::discover(".")?;
let store = Store::open(&repo);
let recipes = store.kind::<Recipe>(RefSegment::new("recipe")?);

// Publish the schema for discovery and history. The publication ref is not
// the type definition: stored documents carry their own schema tree.
recipes.publish()?;

// The canonical ref is named by the complete bound document-tree OID.
// Keep a named ref only as an explicit compatibility alias.
let id = recipes.put_with_alias(
    &RefPath::new("carbonara")?,
    &Recipe { title: "Carbonara".into(), serves: 4 },
)?;
match recipes.read_entity(id)? {
    EntityState::Present(entry) => assert_eq!(entry.value.serves, 4),
    EntityState::Absent | EntityState::Deleted(_) => unreachable!(),
}

// The same refs, read and written as dynamic values instead.
let dynamic = store.dynamic(RefSegment::new("recipe")?);
dynamic.put(&RefSegment::new("cacio")?, &value!({ "title": "Cacio e Pepe", "serves": 2 }))?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

## How it works

- **Typed and dynamic are one code path.** A `Kind` handle selects the
  encoding and schema used by the API; it is not the authority for decoding a
  stored document. `Typed<T>` encodes a `Facet`-derived Rust type natively;
  `Dynamic` encodes a [`facet_value::Value`] against a selected published
  schema. Everything above the encoding — refs, commits, schema binding — is
  shared, and the two encodings are byte-identical, so the two handles
  interoperate over the same addressing/index layer.
- **Schemas are self-hosted and refs are separate.** A [`Schema`] is an
  ordinary `Facet` value stored through the same tree encoding as everything
  else. A schema publication such as `refs/schema/<publication>` is an index
  and history/authoring mechanism; it is not the type definition. The schema
  document carries its own kind name and pins the generation of the
  schema-schema it was written against, so a document this binary does not
  speak is refused rather than misread.
- **Encoding is validation.** A value that does not conform to its selected
  schema fails with the offending path instead of writing a lossy tree.
- **Documents are self-contained.** Every data commit's tree is exactly a
  two-entry root, `{schema/, value/}`: `value/` contains the data and `schema/`
  contains the exact schema tree used to validate it. `get`/`get_at` read both
  directly from the one commit, so old versions stay readable after a schema
  evolves and after a fetch, push, or mirror that moves only the data ref.
  Newly written commits contain none of the `Schema:`, `Schema-Version:`, or
  `Ents-Ref:` trailers. Those trailers are historical compatibility metadata:
  readers ignore them, including malformed or conflicting values, and never
  use them to resolve a schema.
- **Entity identity is the complete bound tree OID.** `EntityId` is the OID of
  the root tree containing exactly `schema/` and `value/`. It is not the
  publication commit OID and not a caller alias; changing either the bound
  schema or encoded value changes the ID. Canonical refs are direct children
  named by that ID (`refs/store/<kind>/<entity-id>`). Named refs, including
  nested legacy paths, are compatibility aliases and are not the canonical
  entity index. `put_entity`, `put_with_alias`, `compile_entity`,
  `read_entity`, and `entity_reference` expose the canonical layer.
- **Named handles are a compatibility convenience.** `Kind::put`, `get`,
  `reference`, `entity_name`, `anonymous`, and related named APIs remain useful
  for ref-addressed callers and old repositories. They may maintain or read an
  alias, but a name or schema publication never replaces the document's
  embedded schema or content identity. Option-returning `get` methods also map
  both absent and deleted entities to `None`; use `read`/`read_entity` and
  `EntityState` when that distinction matters.
- **Prepared documents expose the plumbing boundary.** `Store::encode_value`
  and `Store::decode_value` take an explicit schema tree and operate on an
  unbound value tree. `Store::bind_document` creates the complete
  `{schema/, value/}` root without a commit or ref; `Store::inspect_document`
  classifies that root as bound, legacy value-root, or malformed. A
  `Kind::publish_prepared` call then publishes the prepared tree, returning
  both its content-derived `EntityId` and publication commit.
- **Writes compare and swap.** Each entity write reads the entity and index
  refs, writes its commit and the next index tree, then applies both edits with
  one [`RefStore::apply_batch`](gix_refstore::RefStore::apply_batch)
  transaction. A lost race retries the whole read/build/publication sequence for
  compatibility writes; object writes may remain unreachable, but the old pair
  of refs remains valid. `PublishOptions::with_expectation` changes this to a
  one-shot CAS: a stale expectation returns an error and is never retried.
- **Kinds have a materialized index.** The private cache ref
  `refs/cache/<kind-encoding>/index-v1` points directly to a canonical Git
  tree. Kind names are encoded as `k` followed by lowercase hexadecimal UTF-8
  bytes, an injective encoding. Ephemeral caches use the `refs/cache/<kind>/<name>`
  namespace; `index-v1` identifies this cache's format and domain. Tree entries
  use `160000` commit mode and encode each length-framed, nested `RefPath` as
  one flat filename, avoiding the Git tree file/directory prefix collision while
  preserving every segment.
  The existing fingerprint domain and kind-name component are unchanged, so
  schema refs remain excluded and different kinds cannot share a cache key.
- **Indexes are advisory.** Entity refs remain authoritative. Reads validate a
  present index against the complete canonical entity-ref mapping and fall back
  safely to that mapping when the index is absent, malformed, or stale. This
  preserves correctness after low-level or external ref writes, at the cost of
  validation work; `Kind::rebuild_index` is the explicit repair/materialization
  API and getters never publish refs implicitly. Writes through `Kind` maintain
  the pair atomically. Schema interpretation is still separate from this
  entity-only fingerprint.
- **Deletion is typed state.** `Tombstone` uses the same bound
  `{schema/, value/}` frame as a live document. Its value carries an explicit
  `TombstoneState::Deleted`, kind, and original `EntityId`, so a fetched
  tombstone cannot be confused with a pruned ref. `EntityState::Absent` means
  no ref exists (including an old hard-deleted ref); `EntityState::Deleted`
  means an explicit tombstone is still published. `delete` is idempotent:
  deleting a live ID publishes a tombstone, repeating it returns
  `DeleteResult::AlreadyDeleted`, and a missing ID returns
  `DeleteResult::Absent`. Writing the exact same bound content restores the ID
  after the tombstone; different content creates a new ID. There is no
  separate CLI restore operation.
- **New writes reject legacy trailers.** Store-written commits contain none of
  `Schema:`, `Schema-Version:`, or `Ents-Ref:`. A message line beginning with
  one of those reserved names fails with `Error::ReservedTrailer`. Existing
  Git objects carrying those trailers remain readable because readers ignore
  them, including malformed or conflicting values, and use the embedded schema
  tree instead.

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

## CLI and migration boundaries

The CLI is a composable Git-plumbing boundary, not a migration engine. Its pure
forms are `git store put <schema> <value>` (compile and print a bound
document-tree OID) and `git store get <tree-ish>` (decode that tree directly).
The explicit library operations behind the plumbing are
`encode_value`/`decode_value`, `bind_document`, `inspect_document`, and
`publish_prepared`. The hidden two-argument forms are compatibility paths:
`get <kind> <name>` reads an alias or legacy named ref, while
`rm <kind> <name>` publishes a tombstone over the canonical ref and aliases
rather than removing them.

Schema selection for an unbound value is always explicit. The CLI equivalents
are `git store value encode --schema <tree-ish>`,
`git store value decode <value-tree> --schema <tree-ish>`, and
`git store document bind <value-tree> --schema <tree-ish>`. The schema argument
may name a schema tree, publication commit, or revision peeling to that tree;
no kind lookup, schema-history guess, or commit trailer is consulted.
`git store schema get <kind> --at <commit>` and
`git store schema inspect <kind> --at <commit>` address historical schema
publications directly. A base document read uses the schema embedded in that
document and does not consult schema history.

The CLI also exposes `ref list`, `ref resolve`, `object inspect`, and
`object tree`. Their machine-readable forms are `--format text|json|ndjson` or
`--json`; JSON contains stable full OIDs and full ref names, while NDJSON emits
one record per list item. Success is stdout-only; diagnostics are written to
stderr. The current exit categories are `1` for other operational errors, `2`
for invalid arguments/object shape, `3` for missing refs/objects/schemas/entities,
`4` for CAS conflicts, and `5` for schema/value/document failures.

`git store document publish <kind> <document-tree> --expected <absent|OID>`
uses a one-shot compare-and-swap. The expectation applies to the canonical ref
unless an alias is supplied, in which case it applies to that alias. The
canonical ref, optional alias, and materialized index are published in one
batch. A stale expectation fails without retry; objects written before a lost
CAS may remain unreachable. Bash, Git, or another caller owns traversal,
transforms, batching, retry/resume, and policy. There is no hard-coded CLI
`migrate` workflow, and these commands never silently rewrite a stored tree.

The `get_migrated` family remains an explicit library convenience for in-memory
upcasting toward the selected `Kind`'s current published schema and history.
It fails when the source schema or migration edge is unavailable and does not
rewrite the stored tree.

A schema document must carry the embedded kind name and a schema-schema pin
recognized by this build. Old schema objects lacking the kind name cannot be
made self-describing by guessing from `refs/schema/<publication>` and are not
automatically upgraded; republish or explicitly convert them before relying
on the current self-contained contract. SHA-256 repositories are also outside
this build's supported object format.

## Scope

`gix-store` keeps entity refs as the source of truth and maintains the
per-kind index only as a persisted read cache. Low-level ref access remains an
escape hatch; its changes are detected by read validation rather than trusted
as index maintenance. Signing, gate policy, and event sinks are out of scope.

[`Schema`]: facet_git_tree::Schema
[`RefEdit`]: gix_refstore::RefEdit
