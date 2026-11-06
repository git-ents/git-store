//! Building one struct-shaped document from named field values under a
//! published [`Schema`].

use std::collections::BTreeMap;

use facet_git_tree::{Node, ObjectId, Schema};
use facet_value::{VObject, Value};

use crate::identity::{DocumentTree, SchemaTree};

/// A self-contained document tree and the schema it embeds.
///
/// The `document_tree` holds the value's own entries beside the reserved
/// `.schema` entry, which names `schema_tree` — the exact schema the value was
/// validated against. Instances are produced by
/// [`Store::bind_document`](crate::Store::bind_document), which writes no commit
/// and advances no ref.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreparedDocument {
    /// The bound root: the value's own entries plus `.schema`.
    pub document_tree: DocumentTree,
    /// The pinned schema tree embedded as `.schema`.
    pub schema_tree: SchemaTree,
}

impl PreparedDocument {
    /// Return the complete bound document tree.
    pub const fn document_tree(&self) -> DocumentTree {
        self.document_tree
    }

    /// Return the schema tree used for the value.
    pub const fn schema_tree(&self) -> SchemaTree {
        self.schema_tree
    }
}

/// The kind of tree found by [`Store::inspect_document`](crate::Store::inspect_document).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentKind {
    /// An exact, readable document envelope: a `.schema` entry and the value's
    /// own entries beside it.
    Bound,
    /// A tree with no `.schema` entry, so it can be treated as a historical
    /// value-root candidate, but has no schema binding.
    LegacyValueRoot,
    /// A tree that looks like a document envelope but has an invalid shape.
    Malformed,
}

/// Why a tree was classified as a malformed document envelope.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DocumentShapeError {
    /// The `.schema` entry exists but is not a tree object.
    #[error("document schema entry is not a tree")]
    SchemaNotTree,
    /// The `.schema` entry is the root's only entry, so no value entries sit
    /// beside it.
    ///
    /// A struct root always contributes at least one entry — the `none` marker
    /// when it is empty — so a `.schema`-only root is not an envelope this
    /// format can have written.
    #[error("document envelope has no value entries beside `.schema`")]
    NoValueEntries,
    /// The `.schema` entry names an object that is not present in the database.
    #[error("document schema object {oid} is missing")]
    SchemaMissing {
        /// The missing schema object.
        oid: ObjectId,
    },
}

/// Structured metadata for a tree at a document boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocumentInspection {
    /// A complete, structurally readable bound document.
    Bound(PreparedDocument),
    /// A historical value-root tree with no `.schema` envelope.
    LegacyValueRoot {
        /// The tree that contains the unbound value.
        value_tree: ObjectId,
    },
    /// A tree with an envelope-like shape that cannot be used as a document.
    Malformed {
        /// The inspected root tree.
        document_tree: ObjectId,
        /// The names found at the root.
        found: Vec<String>,
        /// The structural problem detected.
        reason: DocumentShapeError,
    },
}

impl DocumentInspection {
    /// Return the coarse classification without matching the enum variants.
    pub const fn kind(&self) -> DocumentKind {
        match self {
            Self::Bound(_) => DocumentKind::Bound,
            Self::LegacyValueRoot { .. } => DocumentKind::LegacyValueRoot,
            Self::Malformed { .. } => DocumentKind::Malformed,
        }
    }
}

/// A captured schema publication and its immutable schema contents.
///
/// The decoded [`Schema`] is owned, so the snapshot remains usable after the
/// publication ref advances or disappears. `commit` and `schema_tree` retain
/// the content-addressed identities that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaSnapshot {
    /// The schema publication commit.
    pub commit: ObjectId,
    /// The schema document tree stored by `commit`.
    pub schema_tree: SchemaTree,
    /// The decoded schema at `schema_tree`.
    pub schema: Schema,
}

impl SchemaSnapshot {
    /// Return the publication commit captured by this snapshot.
    pub const fn commit(&self) -> ObjectId {
        self.commit
    }

    /// Return the schema document tree captured by this snapshot.
    pub const fn schema_tree(&self) -> SchemaTree {
        self.schema_tree
    }

    /// Return the owned schema contents.
    pub fn schema(&self) -> &Schema {
        &self.schema
    }
}

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
    /// [`DocumentBuilder::build`] found fields — the format emits every field —
    /// still unset.
    #[error("missing required field(s): {names:?}")]
    MissingFields {
        /// The unset fields' names.
        names: Vec<String>,
    },
}

/// A struct document under construction: [`set`](Self::set) accepts defined
/// field names, and [`build`](Self::build) refuses unless every field is set.
///
/// The format emits every struct field and has no defaulted fields, so a
/// document omits nothing.
pub struct DocumentBuilder<'s> {
    fields: &'s BTreeMap<String, Node>,
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

    /// The fields this document accepts: name and shape, in name order.
    pub fn fields(&self) -> impl Iterator<Item = (&str, &Node)> {
        self.fields
            .iter()
            .map(|(name, field)| (name.as_str(), field))
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

    /// Finish the document. Refuses, naming every offender, if any field was
    /// never [`set`](Self::set).
    pub fn build(self) -> Result<Value, DocumentError> {
        let names: Vec<String> = self
            .fields
            .keys()
            .filter(|name| !self.values.contains_key(name.as_str()))
            .cloned()
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
