//! `git-store`: a git external subcommand (`git store …`) that stores anything
//! in Git as a real tree. JSON lives only here, at the CLI boundary; the
//! [`Store`] underneath is oid-in/oid-out.
//!
//! Bare `git store` lists kinds. Writing is an explicit `git store put <kind>
//! [<name>]`, taking content from `-F <file>`, stdin, `$EDITOR`, or — with
//! `-i` — an interactive prompt walking the kind's schema; reading is `git
//! store get <kind> <name>`, where `<name>` may carry a git revision
//! (`<name>~1`, `<name>@{date}`) to read a past version. Everything else —
//! `list`, `log`, `rm`, and the `schema` subgroup — mirrors git porcelain.

mod interactive;

use std::io::{IsTerminal, Read};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use facet_git_tree::{FieldSchema, Schema, SchemaDoc, VariantKind};
use facet_value::Value;
use gix_store::{ObjectId, Store};

#[derive(Parser)]
#[command(
    name = "git-store",
    about = "Store anything in Git as a real tree",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Store an entity. Content comes from `-F <file>`, stdin, or `$EDITOR`
    /// (at a terminal with neither); prints the commit id.
    Put(PutArgs),
    /// Read an entity back as JSON. Append a revision to read a past version:
    /// `carbonara~1`, `carbonara@{yesterday}`, or `carbonara@<oid>` (the `@`
    /// separates any revision from the name; see `log`).
    Get { kind: String, name: String },
    /// List kinds, or the entity names within a kind.
    #[command(visible_alias = "ls")]
    List { kind: Option<String> },
    /// Show an entity's version history, newest first.
    Log { kind: String, name: String },
    /// Delete an entity.
    Rm { kind: String, name: String },
    /// Define, read, or trace a kind's schema.
    Schema {
        #[command(subcommand)]
        command: SchemaCommand,
    },
}

/// Arguments for `put`.
#[derive(clap::Args)]
struct PutArgs {
    /// The kind: a schema published at `refs/schema/<kind>`.
    kind: String,
    /// Entity name → `refs/store/<kind>/<name>`. Defaults to the kind, one
    /// canonical entity per kind.
    name: Option<String>,
    /// JSON file to store; stdin or `$EDITOR` is used when omitted.
    #[arg(short = 'F', long = "file", value_name = "FILE")]
    file: Option<PathBuf>,
    /// Commit message for this version (a `Schema:` trailer is always added).
    #[arg(short = 'm', long = "message", value_name = "MSG")]
    message: Option<String>,
    /// Edit the content in `$VISUAL`/`$EDITOR` before storing.
    #[arg(short = 'e', long = "edit")]
    edit: bool,
    /// Build the value by prompting for each field the schema names, instead
    /// of taking JSON from a file, stdin, or the editor.
    #[arg(short = 'i', long = "interactive", conflicts_with_all = ["file", "edit"])]
    interactive: bool,
}

#[derive(Subcommand)]
enum SchemaCommand {
    /// Define or evolve a kind from a JSON schema (`-F <file>`, stdin, or an
    /// interactive `-i` prompt that walks the type grammar).
    Put {
        kind: String,
        #[arg(short = 'F', long = "file", value_name = "FILE")]
        file: Option<PathBuf>,
        /// Build the schema by prompting for each type, instead of reading
        /// JSON from a file or stdin.
        #[arg(short = 'i', long = "interactive", conflicts_with = "file")]
        interactive: bool,
    },
    /// Print a kind's current schema as JSON.
    Get { kind: String },
    /// Show a kind's field layout, human-readable (all kinds when omitted).
    Show { kind: Option<String> },
    /// List kinds that have a published schema.
    #[command(visible_alias = "ls")]
    List,
    /// Show a kind's schema evolution history, newest first.
    Log { kind: String },
}

fn main() -> Result<()> {
    // Install signal handlers before any lock is taken, so an interrupted
    // write cleans up its per-ref lock file (a gix_tempfile) instead of
    // leaving a stale one that wedges the ref. grace_count 0 → the first
    // SIGINT/SIGTERM cleans up and exits. (A SIGKILL or power loss can still
    // orphan a lock — nothing short of pid-aware lock breaking covers that.)
    //
    // SAFETY: the interrupt callback runs in a signal handler and does nothing
    // — no allocation, no locks — as required.
    #[allow(unsafe_code)]
    unsafe {
        gix::interrupt::init_handler(0, || {})?;
    }

    let cli = Cli::parse();
    let repo = gix::discover(".").context("not inside a git repository")?;
    let store = Store::open(&repo);

    match cli.command {
        // Bare `git store` lists kinds — a read-only default, like `git remote`.
        None => print_lines(store.kinds()?),
        Some(Command::Put(args)) => put(&store, args)?,
        Some(Command::Get { kind, name }) => {
            let (name, rev) = split_name_rev(&name);
            let value = match rev {
                Some(rev) => {
                    let oid = resolve_at(&repo, &kind, name, rev)?;
                    // Only read a commit that is actually a version of this
                    // entity, so a stray oid can't return an unrelated value.
                    if !store.history(&kind, name)?.contains(&oid) {
                        bail!("{rev} is not a version of {kind}/{name}");
                    }
                    store.retrieve_at(oid)?
                }
                None => store
                    .retrieve(&kind, name)?
                    .with_context(|| format!("no entity {kind}/{name}"))?,
            };
            println!("{}", to_json(&value)?);
        }
        Some(Command::List { kind }) => print_lines(match &kind {
            Some(kind) => store.list(kind)?,
            None => store.kinds()?,
        }),
        Some(Command::Log { kind, name }) => print_log(&repo, store.history(&kind, &name)?)?,
        Some(Command::Rm { kind, name }) => {
            if !store.delete(&kind, &name)? {
                bail!("no entity {kind}/{name}");
            }
        }
        Some(Command::Schema { command }) => match command {
            SchemaCommand::Put {
                kind,
                file,
                interactive,
            } => {
                let doc = if interactive {
                    interactive::build_schema()?
                } else {
                    facet_json::from_str(&read_source(file.as_ref())?)
                        .map_err(|e| anyhow::anyhow!("invalid schema JSON: {e}"))?
                };
                println!("{}", store.put_schema(&kind, &doc)?);
            }
            SchemaCommand::Get { kind } => {
                let doc = store
                    .schema(&kind)?
                    .with_context(|| format!("no schema published for kind {kind:?}"))?;
                println!("{}", to_json(&doc)?);
            }
            SchemaCommand::Show { kind } => {
                let kinds = match kind {
                    Some(kind) => vec![kind],
                    None => store.kinds()?,
                };
                for kind in kinds {
                    if let Some(doc) = store.schema(&kind)? {
                        print_type(&kind, &doc);
                    }
                }
            }
            SchemaCommand::List => print_lines(store.kinds()?),
            SchemaCommand::Log { kind } => print_log(&repo, store.schema_history(&kind)?)?,
        },
    }
    Ok(())
}

/// The store action: gather JSON (file, stdin, or the editor), then commit it
/// forward under the kind at the chosen name.
fn put(store: &Store, args: PutArgs) -> Result<()> {
    let PutArgs {
        kind,
        name,
        file,
        message,
        edit,
        interactive,
    } = args;
    let name = name.as_deref().unwrap_or(&kind);

    let value = if interactive {
        interactive::value_for_kind(store, &kind)?
    } else {
        // Content source, in order: an explicit `-F <file>`, piped stdin, or —
        // at a terminal with neither — the editor. This mirrors `git notes add`.
        let base = if let Some(path) = &file {
            Some(read_file(path)?)
        } else if !std::io::stdin().is_terminal() {
            Some(read_stdin()?)
        } else {
            None
        };

        let json = match base {
            Some(content) if !edit => content,
            Some(content) => edit_in_editor(&content)?,
            None => edit_in_editor(&schema_skeleton(store, &kind)?)?,
        };
        facet_json::from_str(&json).map_err(|e| anyhow::anyhow!("invalid JSON: {e}"))?
    };

    println!("{}", store.store(&kind, name, &value, message.as_deref())?);
    Ok(())
}

/// Split an entity argument into its name and an optional revision.
///
/// A revision may be written as a bare git ancestry suffix (`carbonara~1`,
/// `carbonara^2`), as git's own date/reflog syntax (`carbonara@{yesterday}`),
/// or attached with an explicit `@` separator (`carbonara@<rev>`, where `<rev>`
/// is any revision — an oid, `~1`, a ref). Without a suffix the whole token is
/// the name. A literal `@`/`~`/`^` in a name isn't addressable this way, the
/// same trade-off git makes for `<rev>@{…}`.
fn split_name_rev(token: &str) -> (&str, Option<&str>) {
    let Some(i) = token.find(['~', '^', '@']) else {
        return (token, None);
    };
    let (name, marker) = token.split_at(i);
    let rev = match marker.strip_prefix('@') {
        // `@{…}` is git's date/reflog syntax — keep it attached to the ref.
        Some(rest) if rest.starts_with('{') => marker,
        // `@` is an explicit separator: the revision is whatever follows.
        Some(rest) => rest,
        // `~`/`^`: a bare git ancestry suffix, relative to the entity ref.
        None => marker,
    };
    (name, (!rev.is_empty()).then_some(rev))
}

/// Resolve a revision to a commit id. A leading revision operator (`~`, `^`,
/// `@`) is relative to the entity's ref; anything else stands alone (an oid or
/// a full ref).
fn resolve_at(repo: &gix::Repository, kind: &str, name: &str, rev: &str) -> Result<ObjectId> {
    let spec = if rev.starts_with(['~', '^', '@']) {
        format!("refs/store/{kind}/{name}{rev}")
    } else {
        rev.to_owned()
    };
    let id = repo
        .rev_parse_single(spec.as_str())
        .with_context(|| format!("cannot resolve revision {rev:?}"))?;
    Ok(id.detach())
}

/// Read `path`, or all of stdin.
fn read_file(path: &PathBuf) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

fn read_stdin() -> Result<String> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("reading stdin")?;
    Ok(buf)
}

/// Content from `-F <file>`, or stdin when no file is given.
fn read_source(file: Option<&PathBuf>) -> Result<String> {
    match file {
        Some(path) => read_file(path),
        None => read_stdin(),
    }
}

/// A pretty schema-seeded skeleton for `kind`, or an error when the kind has
/// no published schema (nothing to compose against).
fn schema_skeleton(store: &Store, kind: &str) -> Result<String> {
    match store.schema(kind)? {
        Some(doc) => Ok(pretty_skeleton(&doc)),
        None => bail!("no schema published for kind {kind:?}"),
    }
}

/// Open `$VISUAL`/`$EDITOR` on `seed` and return what the user saved.
fn edit_in_editor(seed: &str) -> Result<String> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_owned());

    let mut path = std::env::temp_dir();
    path.push(format!("git-store-{}.json", std::process::id()));
    std::fs::write(&path, seed).with_context(|| format!("writing {}", path.display()))?;

    let status = std::process::Command::new(&editor)
        .arg(&path)
        .status()
        .with_context(|| format!("launching editor {editor:?}"))?;
    if !status.success() {
        let _ = std::fs::remove_file(&path);
        bail!("editor {editor:?} exited without saving");
    }

    let content = std::fs::read_to_string(&path)?;
    let _ = std::fs::remove_file(&path);
    Ok(content)
}

/// Print one item per line.
fn print_lines(items: Vec<String>) {
    for item in items {
        println!("{item}");
    }
}

/// A pretty-printed JSON skeleton for a kind's schema, or `{}` if it cannot be
/// rendered.
fn pretty_skeleton(doc: &SchemaDoc) -> String {
    let compact = skeleton(&doc.root, doc);
    match facet_json::from_str::<Value>(&compact) {
        Ok(value) => to_json(&value).unwrap_or(compact),
        Err(_) => "{}\n".to_owned(),
    }
}

/// A compact placeholder JSON snippet matching `schema`.
fn skeleton(schema: &Schema, doc: &SchemaDoc) -> String {
    match resolve(schema, doc) {
        Schema::Bool => "false".into(),
        Schema::Char | Schema::String | Schema::Bytes => "\"\"".into(),
        Schema::F32 | Schema::F64 => "0.0".into(),
        Schema::I8 | Schema::I16 | Schema::I32 | Schema::I64 | Schema::I128 | Schema::ISize => {
            "0".into()
        }
        Schema::U8 | Schema::U16 | Schema::U32 | Schema::U64 | Schema::U128 | Schema::USize => {
            "0".into()
        }
        Schema::Struct(fields) => {
            let body: Vec<_> = fields
                .iter()
                .map(|FieldSchema { name, schema }| format!("{name:?}:{}", skeleton(schema, doc)))
                .collect();
            format!("{{{}}}", body.join(","))
        }
        Schema::Tuple(elems) => {
            let body: Vec<_> = elems.iter().map(|s| skeleton(s, doc)).collect();
            format!("[{}]", body.join(","))
        }
        Schema::Array { elem, len } => {
            let body: Vec<_> = (0..*len).map(|_| skeleton(elem, doc)).collect();
            format!("[{}]", body.join(","))
        }
        // A scalar-keyed map reads back as a JSON object; a composite-keyed one
        // as an array of `{ k, v }` pairs — mirror that in the seed.
        Schema::Map { key, .. } if is_scalar_schema(resolve(key, doc)) => "{}".into(),
        Schema::List(_) | Schema::Map { .. } => "[]".into(),
        Schema::Optional(_) | Schema::Unit | Schema::RawTree | Schema::Dynamic => "null".into(),
        Schema::Enum(variants) => match variants.first() {
            Some(variant) => {
                let payload = match &variant.kind {
                    VariantKind::Unit => "null".to_owned(),
                    VariantKind::Newtype(inner) => skeleton(inner, doc),
                    VariantKind::Tuple(elems) => {
                        let body: Vec<_> = elems.iter().map(|s| skeleton(s, doc)).collect();
                        format!("[{}]", body.join(","))
                    }
                    VariantKind::Struct(fields) => skeleton(&Schema::Struct(fields.clone()), doc),
                };
                format!("{{{:?}:{}}}", variant.name, payload)
            }
            None => "null".into(),
        },
        Schema::Ref(_) => "null".into(),
    }
}

/// Serialize any `Facet` value to pretty JSON.
fn to_json<T: facet::Facet<'static>>(value: &T) -> Result<String> {
    facet_json::to_string_pretty(value).map_err(|e| anyhow::anyhow!("encoding JSON: {e}"))
}

/// Print `<oid> <iso-date>` per commit, newest first.
fn print_log(repo: &gix::Repository, commits: Vec<ObjectId>) -> Result<()> {
    for id in commits {
        let commit = repo.find_commit(id)?;
        let when = commit.time()?.format(gix::date::time::format::ISO8601)?;
        println!("{id} {when}");
    }
    Ok(())
}

/// Print a kind's top-level field layout, resolving the root through `defs`.
fn print_type(kind: &str, doc: &SchemaDoc) {
    println!("{kind}");
    match resolve(&doc.root, doc) {
        Schema::Struct(fields) => {
            for FieldSchema { name, schema } in fields {
                println!("  {name}: {}", label(schema));
            }
        }
        other => println!("  {}", label(other)),
    }
}

/// Whether a schema node is a scalar — the same classification that decides
/// map layout (name-keyed object vs. `{ k, v }` pair array) in
/// `serialize_value_with_schema`.
pub(crate) fn is_scalar_schema(schema: &Schema) -> bool {
    matches!(
        schema,
        Schema::Bool
            | Schema::Char
            | Schema::String
            | Schema::I8
            | Schema::I16
            | Schema::I32
            | Schema::I64
            | Schema::I128
            | Schema::ISize
            | Schema::U8
            | Schema::U16
            | Schema::U32
            | Schema::U64
            | Schema::U128
            | Schema::USize
            | Schema::F32
            | Schema::F64
    )
}

/// Follow a `Ref` to the definition it names; any other node is returned as-is.
pub(crate) fn resolve<'d>(schema: &'d Schema, doc: &'d SchemaDoc) -> &'d Schema {
    match schema {
        Schema::Ref(name) => doc.defs.get(name).map_or(schema, |s| resolve(s, doc)),
        other => other,
    }
}

/// A short, one-line label for a schema node.
fn label(schema: &Schema) -> String {
    match schema {
        Schema::Unit => "unit".into(),
        Schema::Bool => "bool".into(),
        Schema::Char => "char".into(),
        Schema::String => "string".into(),
        Schema::Bytes => "bytes".into(),
        Schema::I8 | Schema::I16 | Schema::I32 | Schema::I64 | Schema::I128 | Schema::ISize => {
            "int".into()
        }
        Schema::U8 | Schema::U16 | Schema::U32 | Schema::U64 | Schema::U128 | Schema::USize => {
            "uint".into()
        }
        Schema::F32 | Schema::F64 => "float".into(),
        Schema::List(elem) | Schema::Array { elem, .. } => format!("[{}]", label(elem)),
        Schema::Tuple(elems) => {
            let inner: Vec<_> = elems.iter().map(label).collect();
            format!("({})", inner.join(", "))
        }
        Schema::Map { key, value } => format!("{{{}: {}}}", label(key), label(value)),
        Schema::Optional(inner) => format!("{}?", label(inner)),
        Schema::Struct(_) => "struct".into(),
        Schema::Enum(variants) => {
            let names: Vec<_> = variants
                .iter()
                .map(|v| match &v.kind {
                    VariantKind::Unit => v.name.clone(),
                    _ => format!("{}(…)", v.name),
                })
                .collect();
            names.join(" | ")
        }
        Schema::RawTree => "tree".into(),
        Schema::Dynamic => "dynamic".into(),
        Schema::Ref(name) => name.clone(),
    }
}
