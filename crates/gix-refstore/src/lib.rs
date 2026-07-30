//! Compare-and-swap storage for Git refs, factored out from any one backend.
//!
//! [`RefStore`] is the write primitive — one ref, one CAS, atomic against
//! concurrent writers — so a caller depends on the trait rather than being
//! welded to `gix::Repository`. [`Committer`] carries the identity to stamp
//! on writes, kept separate since a store's refs and a repository's
//! configured identity are independent concerns, and [`Signer`] carries the
//! opaque signature bytes a write may be covered by — bytes this crate moves
//! and never interprets. [`MemoryRefStore`] is a `BTreeMap`-backed
//! implementation of both store traits, for tests.
//!
//! # Example
//!
//! The compare-and-swap retry loop every caller ends up writing, so a lost
//! race just means "read again and retry" rather than a hand-rolled lock:
//!
//! ```
//! use gix_refstore::{ApplyError, MemoryRefStore, RefEdit, RefName, RefStore};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let store = MemoryRefStore::new();
//! let name = RefName::new("refs/store/recipe/carbonara")?;
//! let new: gix_hash::ObjectId = "0".repeat(40).parse()?;
//! loop {
//!     let current = store.read(&name)?;
//!     let edit = match current {
//!         Some(expected) => RefEdit::Update { name: name.clone(), expected, new },
//!         None => RefEdit::Create { name: name.clone(), new },
//!     };
//!     match store.apply(edit) {
//!         Ok(()) => break,
//!         Err(ApplyError::LostRace { .. }) => continue,
//!         Err(ApplyError::Backend(err)) => return Err(err.into()),
//!     }
//! }
//! # Ok(())
//! # }
//! ```
#![forbid(unsafe_code)]

mod edit;
mod memory;
mod name;
mod repo;
mod signer;
mod store;

pub use edit::{Expectation, RefEdit};
pub use gix::actor::Signature;
pub use gix_hash::ObjectId;
pub use memory::MemoryRefStore;
pub use name::{InvalidRefName, RefName, RefPath, RefPrefix, RefSegment, Violation};
pub use repo::{GixError, GixRefStore};
pub use signer::{ErasedSigner, SignatureBytes, Signer};
pub use store::{ApplyError, Committer, RefStore};
