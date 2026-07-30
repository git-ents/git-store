//! The crate's authoring surface: the attributes a schema author declares on a
//! Rust type, and which schema generation compiles into the schema document.
//!
//! `facet` permits one attribute grammar per crate, so every attribute this
//! crate recognizes is declared here and written `#[facet(facet_git_tree::…)]`:
//!
//! - `renamed_from = "old"` — the source-side field name a struct field was
//!   renamed from, the one fact a schema diff cannot contain, so it is declared
//!   on the type itself and compiled into the migration document.
//! - `identity_key` — the annotated field, or the annotated type, is identity-
//!   or key-bearing, so its subtree must lie inside the identity normal form's
//!   universe (see [`crate::normal_form`]).
//!
//! Defaults are NOT expressible here — `facet`'s attribute grammar only accepts
//! strings and bools as attribute values — and are supplied through
//! [`Hints::defaulted`](crate::migration::Hints::defaulted) instead.

facet::define_attr_grammar! {
    ns "git_tree";
    crate_path ::facet_git_tree::attr;

    pub enum Attr {
        /// The source-side field name a struct field was renamed from.
        ///
        /// Usage: `#[facet(facet_git_tree::renamed_from = "old_name")]`.
        RenamedFrom(&'static str),

        /// Marks the annotated field, or the annotated type, as identity- or
        /// key-bearing.
        ///
        /// Usage: `#[facet(facet_git_tree::identity_key)]`.
        IdentityKey,
    }
}

/// The namespace every attribute of this grammar is stored under.
const NS: Option<&str> = Some("git_tree");

/// The rename hint `field` declares via
/// `#[facet(facet_git_tree::renamed_from = …)]`, if any.
pub(crate) fn renamed_from(field: &'static facet::Field) -> Option<&'static str> {
    field
        .attributes
        .iter()
        .find(|attr| attr.ns == NS && attr.key == "renamed_from")
        .and_then(|attr| attr.get_as::<&'static str>())
        .copied()
}

/// Whether `attributes` carries `#[facet(facet_git_tree::identity_key)]`.
///
/// One function covers both altitudes: a `facet_core::FieldAttribute` *is* a
/// `facet::Attr`, so a field's attributes and a container's are the same type.
pub(crate) fn is_identity_key(attributes: &'static [facet::Attr]) -> bool {
    attributes
        .iter()
        .any(|attr| attr.ns == NS && attr.key == "identity_key")
}
