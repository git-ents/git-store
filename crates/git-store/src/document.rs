//! Building one struct-shaped document from named field values under a
//! published [`Schema`].

use std::collections::BTreeMap;

use facet_git_tree::{Node, ObjectId, Schema, StructField};
use facet_value::{VObject, Value};

use crate::identity::{DocumentTree, SchemaTree, ValueTree};

/// A self-contained document tree with its two constituent subtrees.
///
/// The `document_tree` is the root containing exactly `schema/` and `value/`;
/// `schema_tree` is the schema used to validate `value_tree`. Instances are
/// produced by [`Store::bind_document`](crate::Store::bind_document), which
/// writes no commit and advances no ref.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreparedDocument {
    /// The root tree containing the `schema/` and `value/` entries.
    pub document_tree: DocumentTree,
    /// The encoded value subtree.
    pub value_tree: ValueTree,
    /// The pinned schema subtree used for the value.
    pub schema_tree: SchemaTree,
}

impl PreparedDocument {
    /// Return the complete bound document tree.
    pub const fn document_tree(&self) -> DocumentTree {
        self.document_tree
    }

    /// Return the encoded value subtree.
    pub const fn value_tree(&self) -> ValueTree {
        self.value_tree
    }

    /// Return the schema subtree used for the value.
    pub const fn schema_tree(&self) -> SchemaTree {
        self.schema_tree
    }
}

/// The kind of tree found by [`Store::inspect_document`](crate::Store::inspect_document).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentKind {
    /// An exact, readable `{schema/, value/}` document envelope.
    Bound,
    /// A tree that does not use the envelope entries and can be treated as a
    /// historical value-root candidate, but has no schema binding.
    LegacyValueRoot,
    /// A tree that looks like a document envelope but has an invalid shape.
    Malformed,
}

/// Why a tree was classified as a malformed document envelope.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DocumentShapeError {
    /// The root has an envelope-like entry set other than exactly `schema` and
    /// `value`.
    #[error("document envelope has entries {found:?}, expected exactly [\"schema\", \"value\"]")]
    UnexpectedEntries {
        /// The entry names found in the root tree.
        found: Vec<String>,
    },
    /// The `schema` entry exists but is not a tree object.
    #[error("document schema entry is not a tree")]
    SchemaNotTree,
    /// The `value` entry names an object that is not present in the database.
    #[error("document value object {oid} is missing")]
    ValueMissing {
        /// The missing value object.
        oid: ObjectId,
    },
    /// The `schema` entry names an object that is not present in the database.
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
    /// A historical value-root tree with no `{schema/, value/}` envelope.
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
