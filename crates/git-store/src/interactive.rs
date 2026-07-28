//! Interactive builders for `put -i` and `schema put -i`: prompt for one value
//! at a time so neither an entity nor a schema needs hand-written JSON.
//!
//! At a terminal the prompts are the rich [`dialoguer`] widgets (arrow-key
//! selects, inline editing) `gh` and friends use; with stdin piped — scripts,
//! tests, automation — they degrade to a plain line reader that takes one
//! answer per line. Both satisfy [`Ask`], so the walk is written once. Either
//! way prompts render to stderr, leaving stdout for the commit id `put` prints.

use std::collections::BTreeMap;
use std::io::{BufRead, IsTerminal, StdinLock, Write as _};
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use dialoguer::{Confirm, Input, Select};
use facet_git_tree::{Schema, SchemaDoc, VariantKind};
use facet_value::{VArray, VObject, Value};
use gix_store::Store;

use crate::{is_scalar_schema, resolve};

/// Build the entity value for `kind`, prompting for each leaf its schema names.
pub fn value_for_kind(store: &Store, kind: &str) -> Result<Value> {
    let doc = store
        .schema(kind)?
        .with_context(|| format!("no schema published for kind {kind:?}"))?;
    build_value(&doc.root, &doc, kind, prompter().as_mut())
}

/// Build a schema document by prompting for the root type. `defs` stays empty:
/// nested user types are inlined, a form the encoder accepts directly.
pub fn build_schema() -> Result<SchemaDoc> {
    let root = build_schema_node(prompter().as_mut(), "root type")?;
    Ok(SchemaDoc {
        root,
        defs: Default::default(),
    })
}

/// The three questions every prompt in this module reduces to.
trait Ask {
    /// A free-text answer (possibly empty).
    fn text(&mut self, prompt: &str) -> Result<String>;
    /// A yes/no answer, `default` taken on an empty accept.
    fn confirm(&mut self, prompt: &str, default: bool) -> Result<bool>;
    /// One option's index, cursor starting at `default`.
    fn select(&mut self, prompt: &str, options: &[&str], default: usize) -> Result<usize>;
}

/// The rich terminal prompter when stdin is a tty, else the scripted one.
fn prompter() -> Box<dyn Ask> {
    if std::io::stdin().is_terminal() {
        Box::new(Rich)
    } else {
        Box::new(Scripted {
            input: std::io::stdin().lock(),
            buf: String::new(),
        })
    }
}

/// `dialoguer`-backed prompts: full-screen selects, line editing, all on stderr.
struct Rich;

impl Ask for Rich {
    fn text(&mut self, prompt: &str) -> Result<String> {
        Ok(Input::<String>::new()
            .with_prompt(prompt)
            .allow_empty(true)
            .interact_text()?)
    }

    fn confirm(&mut self, prompt: &str, default: bool) -> Result<bool> {
        Ok(Confirm::new()
            .with_prompt(prompt)
            .default(default)
            .interact()?)
    }

    fn select(&mut self, prompt: &str, options: &[&str], default: usize) -> Result<usize> {
        Ok(Select::new()
            .with_prompt(prompt)
            .items(options)
            .default(default)
            .interact()?)
    }
}

/// One-answer-per-line fallback for non-terminal stdin, so the builders stay
/// scriptable and testable without a pseudo-terminal.
struct Scripted {
    input: StdinLock<'static>,
    buf: String,
}

impl Scripted {
    /// Read one line, trailing newline removed; `None` at end of input.
    fn line(&mut self, prompt: &str) -> Result<Option<String>> {
        eprint!("{prompt}: ");
        let _ = std::io::stderr().flush();
        self.buf.clear();
        if self
            .input
            .read_line(&mut self.buf)
            .context("reading stdin")?
            == 0
        {
            return Ok(None);
        }
        Ok(Some(self.buf.trim_end_matches(['\n', '\r']).to_owned()))
    }
}

impl Ask for Scripted {
    fn text(&mut self, prompt: &str) -> Result<String> {
        self.line(prompt)?.context("unexpected end of input")
    }

    fn confirm(&mut self, prompt: &str, default: bool) -> Result<bool> {
        let hint = if default { "[Y/n]" } else { "[y/N]" };
        loop {
            match self
                .text(&format!("{prompt} {hint}"))?
                .trim()
                .to_ascii_lowercase()
                .as_str()
            {
                "" => return Ok(default),
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => eprintln!("please answer y or n"),
            }
        }
    }

    fn select(&mut self, prompt: &str, options: &[&str], default: usize) -> Result<usize> {
        for (i, opt) in options.iter().enumerate() {
            eprintln!("  {}) {opt}", i + 1);
        }
        loop {
            let ans = self.text(&format!("{prompt} [{}]", options[default]))?;
            let ans = ans.trim();
            if ans.is_empty() {
                return Ok(default);
            } else if let Some(n) = ans
                .parse::<usize>()
                .ok()
                .filter(|n| (1..=options.len()).contains(n))
            {
                return Ok(n - 1);
            } else if let Some(i) = options.iter().position(|o| o.eq_ignore_ascii_case(ans)) {
                return Ok(i);
            }
            eprintln!("not a valid choice");
        }
    }
}

/// The value for one schema node, prompting under `label` for its position.
fn build_value(schema: &Schema, doc: &SchemaDoc, label: &str, ask: &mut dyn Ask) -> Result<Value> {
    Ok(match resolve(schema, doc) {
        Schema::Unit => Value::NULL,
        Schema::Bool => Value::from(ask.confirm(label, false)?),
        Schema::Char => return ask_scalar::<char>(ask, label, "char"),
        Schema::String => Value::from(ask.text(&format!("{label} (string)"))?.as_str()),
        Schema::Bytes => Value::from(ask.text(&format!("{label} (bytes, as text)"))?.as_str()),
        Schema::I8 => return ask_scalar::<i8>(ask, label, "i8"),
        Schema::I16 => return ask_scalar::<i16>(ask, label, "i16"),
        Schema::I32 => return ask_scalar::<i32>(ask, label, "i32"),
        Schema::I64 => return ask_scalar::<i64>(ask, label, "i64"),
        Schema::I128 => return ask_scalar::<i128>(ask, label, "i128"),
        Schema::ISize => return ask_scalar::<isize>(ask, label, "isize"),
        Schema::U8 => return ask_scalar::<u8>(ask, label, "u8"),
        Schema::U16 => return ask_scalar::<u16>(ask, label, "u16"),
        Schema::U32 => return ask_scalar::<u32>(ask, label, "u32"),
        Schema::U64 => return ask_scalar::<u64>(ask, label, "u64"),
        Schema::U128 => return ask_scalar::<u128>(ask, label, "u128"),
        Schema::USize => return ask_scalar::<usize>(ask, label, "usize"),
        Schema::F32 => return ask_scalar::<f32>(ask, label, "f32"),
        Schema::F64 => return ask_scalar::<f64>(ask, label, "f64"),
        Schema::Struct(fields) => build_struct(fields, doc, label, ask)?,
        Schema::Tuple(elems) => {
            let mut arr = VArray::new();
            for (i, elem) in elems.iter().enumerate() {
                arr.push(build_value(elem, doc, &index(label, i), ask)?);
            }
            arr.into()
        }
        Schema::List(elem) => {
            let mut arr = VArray::new();
            while ask.confirm(&format!("add an item to {label}?"), false)? {
                let i = arr.len();
                arr.push(build_value(elem, doc, &index(label, i), ask)?);
            }
            arr.into()
        }
        Schema::Array { elem, len } => {
            let mut arr = VArray::new();
            for i in 0..*len {
                arr.push(build_value(elem, doc, &index(label, i), ask)?);
            }
            arr.into()
        }
        Schema::Map { key, value } => build_map(key, value, doc, label, ask)?,
        Schema::Optional(inner) => {
            if ask.confirm(&format!("set {label}?"), false)? {
                build_value(inner, doc, label, ask)?
            } else {
                Value::NULL
            }
        }
        Schema::Enum(variants) => build_enum(variants, doc, label, ask)?,
        Schema::RawTree => Value::from(ask.text(&format!("{label} (tree object-id)"))?.as_str()),
        Schema::Dynamic => loop {
            let raw = ask.text(&format!("{label} (JSON value)"))?;
            match facet_json::from_str::<Value>(&raw) {
                Ok(v) => break v,
                Err(e) => eprintln!("invalid JSON: {e}"),
            }
        },
        Schema::Ref(_) => unreachable!("resolve strips Ref before the match"),
    })
}

/// Prompt for a `FromStr` scalar, re-asking until it parses.
fn ask_scalar<T>(ask: &mut dyn Ask, label: &str, ty: &str) -> Result<Value>
where
    T: FromStr,
    Value: From<T>,
{
    loop {
        let raw = ask.text(&format!("{label} ({ty})"))?;
        match raw.trim().parse::<T>() {
            Ok(v) => return Ok(Value::from(v)),
            Err(_) => eprintln!("expected {ty}"),
        }
    }
}

fn build_struct(
    fields: &BTreeMap<String, Schema>,
    doc: &SchemaDoc,
    label: &str,
    ask: &mut dyn Ask,
) -> Result<Value> {
    let mut obj = VObject::new();
    for (name, schema) in fields {
        let value = build_value(schema, doc, &field(label, name), ask)?;
        obj.insert(name.as_str(), value);
    }
    Ok(obj.into())
}

/// A map's value: a name-keyed object for scalar keys, else an array of
/// `{ k, v }` pairs — the two layouts the encoder distinguishes.
fn build_map(
    key: &Schema,
    value: &Schema,
    doc: &SchemaDoc,
    label: &str,
    ask: &mut dyn Ask,
) -> Result<Value> {
    if is_scalar_schema(resolve(key, doc)) {
        let mut obj = VObject::new();
        while ask.confirm(&format!("add an entry to {label}?"), false)? {
            let k = nonempty(ask, &format!("{label} key"))?;
            let v = build_value(value, doc, &field(label, &k), ask)?;
            obj.insert(k.as_str(), v);
        }
        return Ok(obj.into());
    }
    let mut arr = VArray::new();
    while ask.confirm(&format!("add an entry to {label}?"), false)? {
        let base = index(label, arr.len());
        let k = build_value(key, doc, &field(&base, "k"), ask)?;
        let v = build_value(value, doc, &field(&base, "v"), ask)?;
        let mut pair = VObject::new();
        pair.insert("k", k);
        pair.insert("v", v);
        arr.push(Value::from(pair));
    }
    Ok(arr.into())
}

fn build_enum(
    variants: &BTreeMap<String, VariantKind>,
    doc: &SchemaDoc,
    label: &str,
    ask: &mut dyn Ask,
) -> Result<Value> {
    let names: Vec<&str> = variants.keys().map(String::as_str).collect();
    let name = names[ask.select(&format!("{label} variant"), &names, 0)?];
    let kind = &variants[name];
    let vlabel = field(label, name);
    let payload = match kind {
        VariantKind::Unit => Value::NULL,
        VariantKind::Newtype(inner) => build_value(inner, doc, &vlabel, ask)?,
        VariantKind::Tuple(elems) => {
            let mut arr = VArray::new();
            for (i, elem) in elems.iter().enumerate() {
                arr.push(build_value(elem, doc, &index(&vlabel, i), ask)?);
            }
            arr.into()
        }
        VariantKind::Struct(fields) => build_struct(fields, doc, &vlabel, ask)?,
    };
    let mut obj = VObject::new();
    obj.insert(name, payload);
    Ok(obj.into())
}

/// One schema node, chosen from the type menu under the prompt `what`.
fn build_schema_node(ask: &mut dyn Ask, what: &str) -> Result<Schema> {
    const TYPES: &[&str] = &[
        "string", "bool", "int", "uint", "float", "char", "bytes", "struct", "enum", "list",
        "optional", "map", "dynamic",
    ];
    Ok(match TYPES[ask.select(what, TYPES, 0)?] {
        "string" => Schema::String,
        "bool" => Schema::Bool,
        "int" => width(
            ask,
            &["i64", "i32", "i16", "i8", "i128", "isize"],
            &[
                Schema::I64,
                Schema::I32,
                Schema::I16,
                Schema::I8,
                Schema::I128,
                Schema::ISize,
            ],
        )?,
        "uint" => width(
            ask,
            &["u64", "u32", "u16", "u8", "u128", "usize"],
            &[
                Schema::U64,
                Schema::U32,
                Schema::U16,
                Schema::U8,
                Schema::U128,
                Schema::USize,
            ],
        )?,
        "float" => width(ask, &["f64", "f32"], &[Schema::F64, Schema::F32])?,
        "char" => Schema::Char,
        "bytes" => Schema::Bytes,
        "struct" => Schema::Struct(collect_fields(ask)?),
        "enum" => enum_schema(ask)?,
        "list" => Schema::List(Box::new(build_schema_node(ask, "element type")?)),
        "optional" => Schema::Optional(Box::new(build_schema_node(ask, "inner type")?)),
        "map" => Schema::Map {
            key: Box::new(build_schema_node(ask, "key type")?),
            value: Box::new(build_schema_node(ask, "value type")?),
        },
        "dynamic" => Schema::Dynamic,
        other => unreachable!("unlisted type {other}"),
    })
}

/// Pick one width, defaulting to the first (widest common) option.
fn width(ask: &mut dyn Ask, names: &[&str], schemas: &[Schema]) -> Result<Schema> {
    Ok(schemas[ask.select("width", names, 0)?].clone())
}

/// Named fields for a struct or struct enum variant, gathered until the user
/// declines to add another (an empty set is a valid unit-like struct).
fn collect_fields(ask: &mut dyn Ask) -> Result<BTreeMap<String, Schema>> {
    let mut fields = BTreeMap::new();
    while ask.confirm("add a field?", fields.is_empty())? {
        let name = nonempty(ask, "field name")?;
        let schema = build_schema_node(ask, "field type")?;
        fields.insert(name, schema);
    }
    Ok(fields)
}

fn enum_schema(ask: &mut dyn Ask) -> Result<Schema> {
    let mut variants = BTreeMap::new();
    loop {
        let first = variants.is_empty();
        let q = if first {
            "add a variant?"
        } else {
            "add another variant?"
        };
        if !ask.confirm(q, first)? {
            if first {
                bail!("an enum needs at least one variant");
            }
            return Ok(Schema::Enum(variants));
        }
        let name = nonempty(ask, "variant name")?;
        let kind = variant_kind(ask)?;
        variants.insert(name, kind);
    }
}

fn variant_kind(ask: &mut dyn Ask) -> Result<VariantKind> {
    Ok(
        match ask.select("variant kind", &["unit", "newtype", "tuple", "struct"], 0)? {
            0 => VariantKind::Unit,
            1 => VariantKind::Newtype(Box::new(build_schema_node(ask, "payload type")?)),
            2 => {
                let mut elems = vec![build_schema_node(ask, "element type")?];
                while ask.confirm("add another tuple element?", false)? {
                    elems.push(build_schema_node(ask, "element type")?);
                }
                VariantKind::Tuple(elems)
            }
            _ => VariantKind::Struct(collect_fields(ask)?),
        },
    )
}

/// Prompt until the answer is non-empty, for a name or key that cannot be blank.
fn nonempty(ask: &mut dyn Ask, prompt: &str) -> Result<String> {
    loop {
        let s = ask.text(prompt)?;
        let s = s.trim();
        if !s.is_empty() {
            return Ok(s.to_owned());
        }
        eprintln!("a value is required");
    }
}

/// `label.name`, or bare `name` at the root.
fn field(label: &str, name: &str) -> String {
    if label.is_empty() {
        name.to_owned()
    } else {
        format!("{label}.{name}")
    }
}

fn index(label: &str, i: usize) -> String {
    format!("{label}[{i}]")
}
