//! Building one struct-shaped document from named field values under a
//! published [`Schema`].

use std::collections::BTreeMap;

use facet_git_tree::{Node, Schema, StructField};
use facet_value::{VObject, Value};

/// [`DocumentBuilder`]'s failure modes.
#[derive(Debug, thiserror::Error)]
pub enum DocumentError {
    /// The node a [`DocumentBuilder`] was asked to build over does not
    /// resolve to [`Node::Struct`], so it has no named fields.
    #[error("schema does not describe a struct document")]
    NotAStruct,
    /// [`DocumentBuilder::set`] named a field the schema does not define.
    #[error("unknown field {name:?}")]
    UnknownField {
        /// The offending name.
        name: String,
    },
    /// [`DocumentBuilder::build`] found required fields — no supplied value,
    /// no schema default — still unset.
    #[error("missing required field(s): {names:?}")]
    MissingFields {
        /// The unset fields' names.
        names: Vec<String>,
    },
}

/// A struct document under construction: [`set`](Self::set) accepts defined
/// field names, and [`build`](Self::build) refuses if any required field is
/// unset. Unset defaultable fields are omitted from the resulting [`Value`].
pub struct DocumentBuilder<'s> {
    fields: &'s BTreeMap<String, StructField>,
    values: VObject,
}

impl<'s> DocumentBuilder<'s> {
    /// A builder over `doc`'s root struct fields, resolving [`Node::Ref`]
    /// through `doc.defs`.
    pub fn for_schema(doc: &'s Schema) -> Result<Self, DocumentError> {
        Self::for_node(&doc.root, doc)
    }

    /// A builder over `node`'s struct fields, resolved through `doc.defs`.
    /// `node` need not be `doc.root` — any struct reachable in the schema
    /// resolves the same way.
    pub fn for_node(node: &'s Node, doc: &'s Schema) -> Result<Self, DocumentError> {
        match resolve(node, doc) {
            Node::Struct(fields) => Ok(DocumentBuilder {
                fields,
                values: VObject::new(),
            }),
            _ => Err(DocumentError::NotAStruct),
        }
    }

    /// The fields this document accepts: name, shape, and whether
    /// [`build`](Self::build) may leave it unset.
    pub fn fields(&self) -> impl Iterator<Item = (&str, &Node, bool)> {
        self.fields
            .iter()
            .map(|(name, field)| (name.as_str(), &field.node, field.has_default))
    }

    /// Supply `name`'s value, overwriting any previous one. The field name is
    /// checked against the schema; the value is stored without validation.
    pub fn set(&mut self, name: &str, value: impl Into<Value>) -> Result<(), DocumentError> {
        if !self.fields.contains_key(name) {
            return Err(DocumentError::UnknownField {
                name: name.to_owned(),
            });
        }
        self.values.insert(name, value);
        Ok(())
    }

    /// Finish the document. Refuses, naming every offender, if a field with
    /// no schema default was never [`set`](Self::set). Unset defaultable
    /// fields are omitted.
    pub fn build(self) -> Result<Value, DocumentError> {
        let names: Vec<String> = self
            .fields
            .iter()
            .filter(|(name, field)| !field.has_default && !self.values.contains_key(name.as_str()))
            .map(|(name, _)| name.clone())
            .collect();
        if !names.is_empty() {
            return Err(DocumentError::MissingFields { names });
        }
        Ok(self.values.into())
    }
}

/// Follow a [`Node::Ref`] to the definition it names, through `doc.defs`.
fn resolve<'s>(node: &'s Node, doc: &'s Schema) -> &'s Node {
    match node {
        Node::Ref(name) => doc.defs.get(name).map_or(node, |n| resolve(n, doc)),
        other => other,
    }
}
