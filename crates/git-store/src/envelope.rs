//! The `.schema` document envelope.
//!
//! A bound document is a Git tree whose entries are the entries of the value's
//! own bare encoding, plus one reserved entry, `.schema`, naming the unbound
//! schema the value conforms to:
//!
//! ```text
//! { <value's own entries>..., .schema -> <unbound schema> }
//! ```
//!
//! There is no `value/` subtree: a document's schema root resolves to a struct,
//! so the value's fields sit directly at the root beside `.schema` and
//! `git ls-tree` shows them there. `.value` is reserved and never written. The
//! bare value codec knows nothing of this entry, so a reader of a document
//! works through [`UnboundValue`], a view that hides `.schema`.

use facet_git_tree::ObjectId;
use facet_git_tree::{Node, Schema};
use gix::objs::{Data, Find, Kind, Tree, TreeRef, WriteTo, tree::Entry};

use crate::error::{Error, Subtree};

/// The reserved root entry naming a document's unbound schema.
pub(crate) const SCHEMA_ENTRY: &str = Subtree::Schema.as_str();

/// Refuse a schema that cannot serve as a document root.
///
/// Only a struct root has named entries that can sit beside `.schema`; any
/// other resolved root describes a bare value, not a document. A root that
/// does not resolve at all — a `ref` naming a definition absent from `defs`,
/// or a ref cycle — can likewise never form a document, and is reported as
/// [`Error::SchemaRootUnresolved`] rather than published and failing every
/// later write.
pub(crate) fn ensure_struct_root(doc: &Schema) -> Result<(), Error> {
    match doc.resolved_root() {
        Ok(Node::Struct(_)) => Ok(()),
        Ok(node) => Err(Error::DocumentRootNotStruct {
            kind: doc.kind.clone(),
            node: node.keyword(),
        }),
        Err(source) => Err(Error::SchemaRootUnresolved {
            kind: doc.kind.clone(),
            source,
        }),
    }
}

/// Find the object kind of `oid`, or `None` when it is absent.
pub(crate) fn object_kind<S: Find + ?Sized>(
    objects: &S,
    oid: ObjectId,
) -> Result<Option<Kind>, Error> {
    let mut buf = Vec::new();
    Ok(objects
        .try_find(&oid, &mut buf)
        .map_err(Error::backend)?
        .map(|data| data.kind))
}

/// Split a document root into the root itself and its embedded schema tree.
///
/// This is the *validating parser* the read path is built on: it accepts only a
/// well-formed envelope and rejects everything else.
/// [`crate::Store::inspect_document`] is the *classifier* over the same shapes:
/// it reports a `.schema`-less tree as
/// [`DocumentInspection::LegacyValueRoot`](crate::DocumentInspection) (the
/// absence of an envelope, not a value) and an empty or defective root as
/// malformed, where this function returns an error. The two are deliberately
/// different postures, not two verdicts on the same question: a tree is a
/// document to the classifier exactly when this function succeeds.
///
/// The root must carry a `.schema` entry holding a tree, and at least one other
/// entry for the value; every other entry is the value's own. `commit` names
/// the commit the root came from, for diagnostics.
///
/// The returned root is the *document* root, not a bare value tree: reading its
/// value goes through [`UnboundValue`].
pub(crate) fn split<S: Find + ?Sized>(
    root: ObjectId,
    commit: ObjectId,
    objects: &S,
) -> Result<(ObjectId, ObjectId), Error> {
    let mut buf = Vec::new();
    let data = objects
        .try_find(&root, &mut buf)
        .map_err(Error::backend)?
        .ok_or(Error::MissingObject { oid: root })?;
    if data.kind != Kind::Tree {
        return Err(Error::NotATree { oid: root });
    }
    let tree = TreeRef::from_bytes(data.data, data.object_hash).map_err(Error::backend)?;
    let not_bound = || Error::NotSubtreeBound {
        commit,
        found: tree
            .entries
            .iter()
            .map(|entry| String::from_utf8_lossy(entry.filename).into_owned())
            .collect::<Vec<_>>()
            .join(", "),
    };
    // A struct emits at least one entry (the `none` marker for an empty one), so
    // `.schema` is never a root's only entry.
    if tree.entries.len() < 2 {
        return Err(not_bound());
    }
    let schema = tree
        .entries
        .iter()
        .find(|entry| entry.filename == SCHEMA_ENTRY.as_bytes())
        .ok_or_else(not_bound)?;
    if !schema.mode.is_tree() {
        return Err(not_bound());
    }
    let schema = schema.oid.to_owned();
    match object_kind(objects, schema)? {
        Some(Kind::Tree) => {}
        Some(_) => return Err(Error::NotATree { oid: schema }),
        None => {
            return Err(Error::SubtreeMissing {
                subtree: Subtree::Schema,
                oid: schema,
                commit,
            });
        }
    }
    Ok((root, schema))
}

/// Decode the value a bound document root encloses, under `doc`.
///
/// The schema root must resolve to a struct; the root's own entries, with the
/// reserved `.schema` entry hidden, are then read by `E`.
pub(crate) fn read_value<E, S>(root: ObjectId, doc: &Schema, objects: &S) -> Result<E::Value, Error>
where
    E: crate::encoding::Encoding,
    S: Find + ?Sized,
{
    ensure_struct_root(doc)?;
    let value = UnboundValue::new(root, objects)?;
    E::read(&root, doc, &value)
}

/// A document root presented to a bare-value codec as the value it encloses.
///
/// The value's own entries sit in the document root beside `.schema`, so a
/// reader cannot be pointed at that root directly. This view answers for the
/// root's own id with a tree holding exactly the value's entries — `.schema`
/// removed — while delegating every other object to the underlying store. It
/// writes nothing and leaves every other object untouched.
///
/// A subtree can never share the root's id (Git's object graph is acyclic and
/// content-addressed), so intercepting that one id cannot shadow a value's own
/// subtree.
pub(crate) struct UnboundValue<'a, S: ?Sized> {
    inner: &'a S,
    root: ObjectId,
    bytes: Vec<u8>,
}

impl<'a, S: Find + ?Sized> UnboundValue<'a, S> {
    /// View `root`'s entries with `.schema` removed, as one synthesized tree.
    pub(crate) fn new(root: ObjectId, objects: &'a S) -> Result<Self, Error> {
        let mut buf = Vec::new();
        let data = objects
            .try_find(&root, &mut buf)
            .map_err(Error::backend)?
            .ok_or(Error::MissingObject { oid: root })?;
        if data.kind != Kind::Tree {
            return Err(Error::NotATree { oid: root });
        }
        let tree = TreeRef::from_bytes(data.data, data.object_hash).map_err(Error::backend)?;
        let entries: Vec<Entry> = tree
            .entries
            .iter()
            .filter(|entry| entry.filename != SCHEMA_ENTRY.as_bytes())
            .map(|entry| Entry {
                mode: entry.mode,
                filename: entry.filename.into(),
                oid: entry.oid.to_owned(),
            })
            .collect();
        // Removing an entry preserves Git's entry order, so the synthesized
        // tree is canonical exactly when the document root was.
        let mut bytes = Vec::new();
        Tree { entries }
            .write_to(&mut bytes)
            .map_err(Error::backend)?;
        Ok(Self {
            inner: objects,
            root,
            bytes,
        })
    }
}

impl<S: Find + ?Sized> Find for UnboundValue<'_, S> {
    fn try_find<'b>(
        &self,
        id: &gix::hash::oid,
        buffer: &'b mut Vec<u8>,
    ) -> Result<Option<Data<'b>>, gix::objs::find::Error> {
        if id == self.root.as_ref() {
            buffer.clear();
            buffer.extend_from_slice(&self.bytes);
            return Ok(Some(Data {
                data: buffer.as_slice(),
                kind: Kind::Tree,
                object_hash: self.root.kind(),
            }));
        }
        self.inner.try_find(id, buffer)
    }
}
