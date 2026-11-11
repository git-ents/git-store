//! Codecs for bare values stored as trees. Two implementations ship: [`Typed`]
//! and [`Dynamic`].

use std::marker::PhantomData;

use facet::Facet;
use facet_git_tree::{
    HashFormat, ObjectId, Oid, Schema, Tagged, deserialize, deserialize_value_with_schema,
    serialize_into, serialize_value_with_schema, validate_with_schema,
};
use facet_value::Value;
use gix::objs::{Find, Write};

use crate::error::Error;

/// A codec for values stored as `facet-git-tree` objects.
///
/// Implementations use the kind's published [`Schema`] and a `gix` object
/// database. This is an extension point for the store's tree format, not a
/// general serialization interface.
///
/// The hash format is carried by the object database the codec is handed —
/// `S: Tagged<F>` — not by the codec, so one codec serves a store of any
/// format. The written root is returned as a bare [`ObjectId`]: the store
/// addresses objects by untyped id and re-derives the format at each boundary.
pub trait Encoding {
    /// The Rust type entities of this kind are read and written as.
    type Value;

    /// Encode `value` under `doc`, returning the written root object.
    fn write<F: HashFormat, S: Find + Write + Tagged<F> + ?Sized>(
        value: &Self::Value,
        doc: &Schema,
        objects: &S,
    ) -> Result<ObjectId, Error>;

    /// Decode the value rooted at `root`, guided by `doc`.
    fn read<F: HashFormat, S: Find + Tagged<F> + ?Sized>(
        root: &ObjectId,
        doc: &Schema,
        objects: &S,
    ) -> Result<Self::Value, Error>;
}

/// Values of a `Facet`-derived Rust type, encoded natively.
pub struct Typed<T>(PhantomData<fn() -> T>);

/// Values of unknown shape, encoded under the kind's published schema.
pub struct Dynamic;

impl<T: for<'a> Facet<'a>> Encoding for Typed<T> {
    type Value = T;

    fn write<F: HashFormat, S: Find + Write + Tagged<F> + ?Sized>(
        value: &T,
        doc: &Schema,
        objects: &S,
    ) -> Result<ObjectId, Error> {
        let root: Oid<F> = serialize_into(value, objects)?;
        // The native encoding is byte-identical to the schema-directed one,
        // so validating here keeps the invariant that a document's value
        // conforms to the `.schema` it binds.
        let checked = doc.checked()?;
        validate_with_schema(&root, &checked, objects)?;
        Ok(root.into_raw())
    }

    fn read<F: HashFormat, S: Find + Tagged<F> + ?Sized>(
        root: &ObjectId,
        doc: &Schema,
        objects: &S,
    ) -> Result<T, Error> {
        let root = Oid::<F>::new(*root).map_err(Error::backend)?;
        let checked = doc.checked()?;
        validate_with_schema(&root, &checked, objects)?;
        Ok(deserialize(&root, objects)?)
    }
}

impl Encoding for Dynamic {
    type Value = Value;

    fn write<F: HashFormat, S: Find + Write + Tagged<F> + ?Sized>(
        value: &Value,
        doc: &Schema,
        objects: &S,
    ) -> Result<ObjectId, Error> {
        let checked = doc.checked()?;
        Ok(serialize_value_with_schema(value, &checked, objects)?.into_raw())
    }

    fn read<F: HashFormat, S: Find + Tagged<F> + ?Sized>(
        root: &ObjectId,
        doc: &Schema,
        objects: &S,
    ) -> Result<Value, Error> {
        let root = Oid::<F>::new(*root).map_err(Error::backend)?;
        let checked = doc.checked()?;
        Ok(deserialize_value_with_schema(&root, &checked, objects)?)
    }
}
