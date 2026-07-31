# gix-refstore

Compare-and-swap storage for Git refs, factored out from any one backend.

```rust
use gix_refstore::{ApplyError, MemoryRefStore, RefEdit, RefName, RefStore};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let store = MemoryRefStore::new();
let name = RefName::new("refs/store/recipe/carbonara")?;
let new: gix_hash::ObjectId = "0".repeat(40).parse()?;
loop {
    let current = store.read(&name)?;
    let edit = match current {
        Some(expected) => RefEdit::Update { name: name.clone(), expected, new },
        None => RefEdit::Create { name: name.clone(), new },
    };
    match store.apply(edit) {
        Ok(()) => break,
        Err(ApplyError::LostRace { .. }) => continue,
        Err(ApplyError::Backend(err)) => return Err(err.into()),
    }
}
# Ok(())
# }
```

## Scope

`RefStore` is the write primitive: every mutation is a compare-and-swap, and
`apply_batch` publishes several conditional ref edits as one transaction. A
lost batch names the expectation that failed and publishes none of its edits.
`GixRefStore` delegates batches to gitoxide's multi-reference transaction API;
`MemoryRefStore` checks the complete batch under one lock. `Committer` carries
the identity to stamp on writes, kept separate because a store's refs and a
repository's configured identity are independent concerns.

Objects are written through `gix_object::Write`; this crate is refs only.
A batch may therefore publish object IDs whose objects were written before the
transaction, leaving unreachable objects if publication loses its race.
