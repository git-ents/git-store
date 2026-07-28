# `facet-git-tree`

*[facet.rs](https://facet.rs) format crates for Git tree objects.*

## Cargo Features

| Feature | Default | Description |
| ------- | ------- | ----------- |
| `value` | no      | Enables the [`facet-value`](https://crates.io/crates/facet-value) integration: exact 128-bit number text for dynamic writes, and the schema-driven reader (`deserialize_value_with_schema`, `validate_with_schema`). Dynamic serialization and the heuristic dynamic read work without it. |

## Dynamic Values

Any tree written by this crate can be loaded without its compile-time type by deserializing into `facet_value::Value`:

```rust
use facet::Facet;
use facet_git_tree::{deserialize, serialize};
use facet_value::Value;

#[derive(Facet)]
struct Person {
    name: String,
    age: u32,
}

fn main() -> anyhow::Result<()> {
    let ada = Person { name: "Ada".into(), age: 36 };
    let (oid, store) = serialize(&ada)?;

    // Load the same tree with no compile-time type.
    let value = deserialize::<Value>(&oid, &store)?;
    println!("{value:?}");
    Ok(())
}
```

> **Warning:** the encoding is schemaless, so a bare dynamic read is a documented *lossy* heuristic (spec: `deserialization.dynamic.heuristic`).
> Blobs come back as `String` (or `Bytes` when not UTF-8), so numbers, bools, chars, and datetimes read back as strings; `null` and empty arrays read back as empty objects; objects whose keys are all decimal ordinals read back as arrays.
> Use a typed read or a schema-driven read to recover full fidelity.

## Schemas

Schemas are self-hosted: `SchemaDoc` is itself a `Facet` type, stored through this crate's own tree encoding.
Generate one with `schema_of`, store it like any other value, and use it to read data back with full type fidelity — numbers as numbers, enums as tagged objects.
The schema-driven reader requires the `value` feature.

```rust
use facet::Facet;
use facet_git_tree::{deserialize, deserialize_value_with_schema, schema_of, serialize, serialize_into};
use facet_git_tree::SchemaDoc;

#[derive(Facet)]
struct Person {
    name: String,
    age: u32,
}

fn main() -> anyhow::Result<()> {
    // Serialize a value.
    let ada = Person { name: "Ada".into(), age: 36 };
    let (person_oid, store) = serialize(&ada)?;

    // Generate the schema and store it in the same object store.
    let doc = schema_of::<Person>()?;
    let schema_oid = serialize_into(&doc, &store)?;

    // Later, or elsewhere: load the schema back from its oid, then use it to
    // read the value faithfully — `age` is a number, not a string.
    let doc = deserialize::<SchemaDoc>(&schema_oid, &store)?;
    let value = deserialize_value_with_schema(&person_oid, &doc, &store)?;
    println!("{value:?}");
    Ok(())
}
```

### Publishing Schemas Under Refs

`facet-git-tree` is oid-in/oid-out and performs no ref operations.
The `refs/schema/<name>` convention — e.g. `refs/schema/issue` — is owned by higher layers such as `git-store`: a schema is published by serializing its `SchemaDoc` and pointing the ref at the resulting tree oid, or at a commit wrapping that tree for history and signing.
That choice belongs to the higher layer.

## Code of Conduct

Please refer to the in-source [code of conduct](../../CONDUCT.md) for all behavioral expectations.

## Contribution Guide

Contributions are welcome.
Please refer to the in-source [contribution guide](../../CONTRIBUTING.md).
