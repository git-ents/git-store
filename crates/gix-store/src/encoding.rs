//! How a kind's values are encoded into, and read back from, the `value/`
//! subtree. Two implementations ship: [`Typed`] and [`Dynamic`]. Everything
//! above this trait — refs, commits, schema binding — is shared.

use std::marker::PhantomData;

use facet::Facet;
use facet_git_tree::{
    ObjectId, Schema, deserialize, deserialize_value_with_schema, serialize_into,
    serialize_value_with_schema, validate_with_schema,
};
use facet_value::Value;
use gix::objs::{Find, Write};

use crate::error::Error;

/// How a kind's values are read from, and written to, its `value/` subtree.
pub trait Encoding {
    /// The Rust type entities of this kind are read and written as.
    type Value;

    /// Encode `value` under `doc`, returning the written root object.
    fn write<S: Find + Write + ?Sized>(
        value: &Self::Value,
        doc: &Schema,
        objects: &S,
    ) -> Result<ObjectId, Error>;

    /// Decode the value rooted at `root`, guided by `doc`.
    fn read<S: Find + Write + ?Sized>(
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

    fn write<S: Find + Write + ?Sized>(
        value: &T,
        doc: &Schema,
        objects: &S,
    ) -> Result<ObjectId, Error> {
        let root = serialize_into(value, objects)?;
        // The native encoding is byte-identical to the schema-directed one,
        // so validating here keeps the invariant that `value/` conforms to
        // the `schema/` the same commit binds.
        validate_with_schema(&root, doc, objects)?;
        Ok(root)
    }

    fn read<S: Find + Write + ?Sized>(
        root: &ObjectId,
        doc: &Schema,
        objects: &S,
    ) -> Result<T, Error> {
        validate_with_schema(root, doc, objects)?;
        Ok(deserialize(root, objects)?)
    }
}

impl Encoding for Dynamic {
    type Value = Value;

    fn write<S: Find + Write + ?Sized>(
        value: &Value,
        doc: &Schema,
        objects: &S,
    ) -> Result<ObjectId, Error> {
        Ok(serialize_value_with_schema(value, doc, objects)?)
    }

    fn read<S: Find + Write + ?Sized>(
        root: &ObjectId,
        doc: &Schema,
        objects: &S,
    ) -> Result<Value, Error> {
        Ok(deserialize_value_with_schema(root, doc, objects)?)
    }
}
