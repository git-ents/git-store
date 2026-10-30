//! The database CLI: the Dolt-shaped `git store db` command surface, and the
//! output-format and exit-code plumbing it shares with the `git-store`
//! binary's own CLI.

pub mod db;

use std::fmt;

use anyhow::Result;
use clap::ValueEnum;
use facet_value::{VArray, VObject, Value};
use gix::ObjectId;

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
    Ndjson,
}

impl OutputFormat {
    pub fn from_cli(format: Option<Self>, json: bool) -> Self {
        if json {
            Self::Json
        } else {
            format.unwrap_or(Self::Text)
        }
    }

    pub fn machine(self) -> bool {
        !matches!(self, Self::Text)
    }
}

/// The stable machine-facing class of a CLI failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitClass {
    Cas,
    NotFound,
    Schema,
    Invalid,
    Other,
}

impl ExitClass {
    fn rank(self) -> u8 {
        match self {
            Self::Cas => 4,
            Self::NotFound => 3,
            Self::Schema => 2,
            Self::Invalid => 1,
            Self::Other => 0,
        }
    }

    pub fn code(self) -> i32 {
        match self {
            Self::Cas => 4,
            Self::NotFound => 3,
            Self::Schema => 5,
            Self::Invalid => 2,
            Self::Other => 1,
        }
    }

    pub fn prefer(self, other: Self) -> Self {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }
}

/// A CLI-owned typed context: its display text is for people, while `class` is
/// the independent machine-facing classification used for the process exit.
#[derive(Debug)]
pub struct ClassifiedContext {
    pub class: ExitClass,
    message: String,
}

impl ClassifiedContext {
    fn new(class: ExitClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
        }
    }
}

impl fmt::Display for ClassifiedContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ClassifiedContext {}

pub fn cli_error(class: ExitClass, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(ClassifiedContext::new(class, message))
}

pub fn cli_context(class: ExitClass, message: impl Into<String>) -> ClassifiedContext {
    ClassifiedContext::new(class, message)
}

pub fn json_record() -> VObject {
    let mut record = VObject::new();
    record.insert("status", "ok");
    record.insert("code", "ok");
    record
}

pub fn oid_value(oid: impl Into<ObjectId>) -> Value {
    oid.into().to_string().into()
}

pub fn string_array<I>(items: I) -> Value
where
    I: IntoIterator,
    I::Item: Into<String>,
{
    let mut array = VArray::new();
    for item in items {
        array.push(item.into());
    }
    array.into()
}

pub fn emit_record(format: OutputFormat, record: VObject) -> Result<()> {
    debug_assert!(format.machine(), "emit_record is for json/ndjson only");
    let value: Value = record.into();
    println!(
        "{}",
        facet_json::to_string(&value).map_err(|e| anyhow::anyhow!("encoding JSON: {e}"))?
    );
    Ok(())
}

/// Print one command's result: `text()` under [`OutputFormat::Text`], or
/// `fields` layered over the standard `status`/`code` envelope under `json`
/// or `ndjson` — the two formats agree for a single-result command, and
/// differ only for a list ([`emit_list`]). Every command routes its output
/// through this or [`emit_list`], so no command can silently ignore the
/// selected format.
pub fn emit_single(
    format: OutputFormat,
    fields: VObject,
    text: impl FnOnce() -> String,
) -> Result<()> {
    if format.machine() {
        emit_fields(format, fields)
    } else {
        println!("{}", text());
        Ok(())
    }
}

/// [`emit_single`]'s machine-format half: the standard envelope with `fields`
/// layered over it, for a command whose text rendering is empty (`check`) or
/// handled separately by the caller.
pub fn emit_fields(format: OutputFormat, fields: VObject) -> Result<()> {
    let mut record = json_record();
    record.extend(fields);
    emit_record(format, record)
}

/// One row of a [`emit_list`] result: `fields` are the bare, unwrapped
/// machine data for that row and `text` its one-line human rendering.
pub struct ListItem {
    pub fields: VObject,
    pub text: String,
}

/// Print a list-shaped command's result. Text prints one line per item.
/// `ndjson` prints one enveloped record per item. `json` prints one envelope
/// whose `list_field` holds the array of bare item objects — the same shape
/// the `git-store` CLI's ref and object listings already used, generalized to
/// every list-producing command.
pub fn emit_list(format: OutputFormat, list_field: &str, items: Vec<ListItem>) -> Result<()> {
    match format {
        OutputFormat::Text => {
            for item in items {
                println!("{}", item.text);
            }
            Ok(())
        }
        OutputFormat::Ndjson => {
            for item in items {
                emit_fields(format, item.fields)?;
            }
            Ok(())
        }
        OutputFormat::Json => {
            let mut values = VArray::new();
            for item in items {
                values.push(Value::from(item.fields));
            }
            let mut record = json_record();
            record.insert(list_field, values);
            emit_record(format, record)
        }
    }
}
