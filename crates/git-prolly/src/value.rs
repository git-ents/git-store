use facet::Facet;
use facet_value::{VArray, VNumber, VObject, Value};

#[derive(Facet)]
#[repr(C)]
pub(crate) struct WireField {
    key: String,
    value: WireValue,
}

#[derive(Facet)]
#[repr(u8)]
pub(crate) enum WireValue {
    Null,
    Bool(bool),
    I64(i64),
    U64(u64),
    I128(i128),
    U128(u128),
    F64(f64),
    String(String),
    Array(Vec<WireValue>),
    Object(Vec<WireField>),
}

impl TryFrom<&Value> for WireValue {
    type Error = &'static str;

    fn try_from(value: &Value) -> Result<Self, Self::Error> {
        match value.destructure_ref() {
            facet_value::DestructuredRef::Null => Ok(Self::Null),
            facet_value::DestructuredRef::Bool(value) => Ok(Self::Bool(value)),
            facet_value::DestructuredRef::Number(number) => number_wire(number),
            facet_value::DestructuredRef::String(value) => {
                Ok(Self::String(value.as_str().to_owned()))
            }
            facet_value::DestructuredRef::Array(values) => values
                .as_slice()
                .iter()
                .map(Self::try_from)
                .collect::<Result<Vec<_>, _>>()
                .map(Self::Array),
            facet_value::DestructuredRef::Object(values) => {
                let mut fields = values
                    .iter()
                    .map(|(key, value)| {
                        Ok(WireField {
                            key: key.as_str().to_owned(),
                            value: Self::try_from(value)?,
                        })
                    })
                    .collect::<Result<Vec<_>, Self::Error>>()?;
                fields.sort_by(|left, right| left.key.cmp(&right.key));
                Ok(Self::Object(fields))
            }
            _ => Err("database values must be JSON values"),
        }
    }
}

fn number_wire(number: &VNumber) -> Result<WireValue, &'static str> {
    if number.is_float() {
        return number
            .to_f64()
            .map(WireValue::F64)
            .ok_or("database values must contain finite JSON numbers");
    }
    if let Some(value) = number.to_i64() {
        return Ok(WireValue::I64(value));
    }
    if let Some(value) = number.to_u64() {
        return Ok(WireValue::U64(value));
    }
    if let Some(value) = number.to_i128() {
        return Ok(WireValue::I128(value));
    }
    number
        .to_u128()
        .map(WireValue::U128)
        .ok_or("unsupported database number")
}

impl From<WireValue> for Value {
    fn from(value: WireValue) -> Self {
        match value {
            WireValue::Null => Value::NULL,
            WireValue::Bool(value) => value.into(),
            WireValue::I64(value) => value.into(),
            WireValue::U64(value) => value.into(),
            WireValue::I128(value) => value.into(),
            WireValue::U128(value) => value.into(),
            WireValue::F64(value) => value.into(),
            WireValue::String(value) => value.into(),
            WireValue::Array(values) => {
                let mut array = VArray::with_capacity(values.len());
                for value in values {
                    array.push(Value::from(value));
                }
                array.into()
            }
            WireValue::Object(values) => {
                let mut object = VObject::with_capacity(values.len());
                for field in values {
                    object.insert(field.key, Value::from(field.value));
                }
                object.into()
            }
        }
    }
}

pub(crate) fn encode(value: &Value) -> Result<WireValue, &'static str> {
    WireValue::try_from(value)
}

pub(crate) fn decode(value: WireValue) -> Value {
    value.into()
}
