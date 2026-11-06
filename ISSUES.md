# Issues

## Meta-codec: multiple codecs with codec-relative schemas

`git-store` currently layers its document envelope, refs, commits, and
tombstones over exactly one encoding: `facet-git-tree`'s schema-directed value
codec.

### The existing seam is not this seam

`encoding::Encoding` looks like a codec seam, but it is parameterized by
`facet_git_tree::Schema` (`doc: &Schema`). `Typed` and `Dynamic` are two Rust
representations of **one** wire format. Its own documentation says it is an
extension point for the store's tree format, not a general serialization
interface.

### Problem

A second codec exists: an operation-log snapshot is a Git-shaped tree — `refs/`
as a nested tree, `config` and `description` as plainly named blobs, absent
files omitted. It is **schemaless**; Git's modes and paths are the shape.
`git-op` hand-writes it today in `snapshot.rs`. It cannot be stored by
`git-store` without being forced through the `facet-git-tree` value format.

Relatedly, a schema is only meaningful relative to the codec that interprets
it. Two codecs can write byte-identical schema trees that mean different
things, so a bare schema OID is ambiguous.

### Proposal

1. Introduce a `Codec` concept owned by `git-store` (the meta layer). A codec
   owns its schema vocabulary and how a schema is encoded (if it has one), its
   value encode/decode, and its validation. A codec declares whether it is
   **schematized** or **schemaless**.
2. Make schemas codec-relative. A schema's identity is `(codec, schema-tree)`;
   schema refs become `refs/schema/<codec>/<kind>`.
3. Name the codec in the document so documents stay self-contained:

   ```text
   { <value>..., .codec -> <codec name>, .schema -> <unbound schema> }
   ```

   `.codec` is reserved (escaped as `%.codec` for user fields). A document
   with no `.codec` defaults to `facet-git-tree`, keeping existing documents
   readable.
4. The meta layer owns codec identity, the name-to-codec registry, the
   codec-tagged envelope, and the existing identity/CAS/publication/tombstone
   machinery. It stays oid-in, oid-out.
5. A schemaless codec writes no `.schema` entry. The "`.schema` root must
   resolve to a struct" rule is a `facet-git-tree` codec rule, not a
   meta-layer rule.

### Open decisions

- Static `Store<C: Codec>` with associated `Schema`/`Value` types, versus a
  dynamic `dyn Codec` behind OIDs. Leaning dynamic: the store is already
  oid-centric, and static genericity ripples through every consumer.
- Whether an absent `.schema` is legal for a schemaless codec, or schemaless
  codecs still embed a fixed singleton schema.
- Whether `.codec` carries a version (e.g. `facet-git-tree/1`) so a codec's
  rules can evolve without ambiguity.

### Cost

Everything schema-facing is welded to `facet_git_tree::Schema` and
`Rc<Schema>`: `document.rs`, `envelope.rs`, `identity.rs` (`SchemaTree`),
`kind.rs` (`KindSchema`), the `Store` schema cache, and
`Subtree::{Schema, Value}`. This is a `git-store`-level spec revision, not a
`facet-git-tree` format change; `format.adoc` stays frozen.

### Downstream

This is what would let `git-op` store its snapshots as first-class store
entities (aliases, history, indexing, CAS) under a filesystem codec, instead of
hand-writing `snapshot.rs` against `gix` directly.
