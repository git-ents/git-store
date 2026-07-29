//! The authoring surface, compiled into the type: rename correspondence is
//! the one fact a schema diff cannot contain, so it is declared on the type
//! itself and compiled into the migration document.
//!
//! Defaults are NOT expressible here — `facet`'s attribute grammar only
//! accepts strings and bools as attribute values — and are supplied through
//! [`Hints::defaulted`](crate::migration::Hints::defaulted) instead.

facet::define_attr_grammar! {
    ns "migrate";
    crate_path ::facet_git_tree::migration::attr;

    pub enum Attr {
        /// The source-side field name a struct field was renamed from.
        ///
        /// Usage: `#[facet(facet_git_tree::renamed_from = "old_name")]`.
        RenamedFrom(&'static str),
    }
}

/// The rename hint `field` declares via `#[facet(migrate::renamed_from = …)]`,
/// if any.
pub(crate) fn renamed_from(field: &'static facet::Field) -> Option<&'static str> {
    field
        .attributes
        .iter()
        .find(|attr| attr.ns == Some("migrate") && attr.key == "renamed_from")
        .and_then(|attr| attr.get_as::<&'static str>())
        .copied()
}
