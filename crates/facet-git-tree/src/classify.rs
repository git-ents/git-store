use facet::{Def, Shape, Type, UserType};

use crate::RawTree;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShapeClass {
    TransparentPointer,
    TransparentNewtype,
    RawTree,
    Dynamic,
    Scalar,
    Bytes,
    Struct,
    Sequence,
    Map,
    Option,
    Enum,
    Unsupported,
}

pub(crate) fn collapse_shape(mut shape: &'static Shape) -> &'static Shape {
    loop {
        if let Def::Pointer(pd) = shape.def {
            match pd.pointee {
                Some(pointee) => {
                    shape = pointee;
                    continue;
                }
                None => return shape,
            }
        }
        if shape.inner.is_some() && shape.vtable.has_try_borrow_inner() {
            shape = shape.inner.expect("checked is_some above");
            continue;
        }
        return shape;
    }
}

pub(crate) fn classify(shape: &'static Shape) -> ShapeClass {
    if let Def::Pointer(pd) = shape.def
        && pd.pointee.is_some()
    {
        return ShapeClass::TransparentPointer;
    }
    if shape.inner.is_some() && shape.vtable.has_try_borrow_inner() {
        return ShapeClass::TransparentNewtype;
    }

    let shape = collapse_shape(shape);
    if shape.is_type::<RawTree>() {
        return ShapeClass::RawTree;
    }
    if matches!(shape.def, Def::DynamicValue(_)) {
        return ShapeClass::Dynamic;
    }
    if matches!(shape.def, Def::Scalar) {
        return ShapeClass::Scalar;
    }
    if is_byte_seq(shape) {
        return ShapeClass::Bytes;
    }
    if matches!(shape.ty, Type::User(UserType::Struct(_))) {
        return ShapeClass::Struct;
    }
    if matches!(shape.def, Def::List(_) | Def::Array(_) | Def::Slice(_)) {
        return ShapeClass::Sequence;
    }
    if matches!(shape.def, Def::Map(_)) {
        return ShapeClass::Map;
    }
    if matches!(shape.def, Def::Option(_)) {
        return ShapeClass::Option;
    }
    if matches!(shape.ty, Type::User(UserType::Enum(_))) {
        return ShapeClass::Enum;
    }
    ShapeClass::Unsupported
}

pub(crate) fn seq_elem(shape: &Shape) -> Option<&'static Shape> {
    match shape.def {
        Def::List(d) => Some(d.t),
        Def::Array(d) => Some(d.t),
        Def::Slice(d) => Some(d.t),
        _ => None,
    }
}

pub(crate) fn is_byte_seq(shape: &Shape) -> bool {
    seq_elem(shape).is_some_and(|t| t.is_type::<u8>())
}
