//! A pointer to the `.schema` document envelope.
//!
//! The envelope codec — the reserved `.schema` entry, the [`split`] validating
//! parser, and the [`UnboundValue`] view that hides `.schema` — now lives in
//! [`facet_git_tree::document`]. This module re-exports the pieces `git-store`
//! uses, so its call sites keep reading as `crate::envelope::…`, and defers the
//! format's design to the format crate.

pub(crate) use facet_git_tree::document::{
    SCHEMA_ENTRY, UnboundValue, ensure_struct_root, read_value, split,
};
