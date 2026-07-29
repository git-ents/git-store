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
use facet_git_tree::{Node, Schema, VariantKind};
use facet_value::{Value, from_value};
use gix_store::{Dynamic, GixRefStore, Kind, ObjectId, RefPath, RefSegment, RepoStore};

/// A handle on one kind, over the CLI's own repo-backed store.
pub(crate) type DynKind<'s, 'r> = Kind<'s, Dynamic, GixRefStore<'r>, &'r gix::OdbHandle>;

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
    // write cleans up gix-refstore's per-ref lock file (a gix_tempfile under
    // `<git-dir>/gix-refstore-locks/`) instead of leaving a stale one that
    // wedges the ref. grace_count 0 → the first SIGINT/SIGTERM cleans up and
    // exits. (A SIGKILL or power loss can still orphan a lock — nothing short
    // of pid-aware lock breaking covers that.)
    //
    // SAFETY: the interrupt callback runs in a signal handler and does nothing
    // — no allocation, no locks — as required.
    #[allow(unsafe_code)]
    unsafe {
        gix::interrupt::init_handler(0, || {})?;
    }

    let cli = Cli::parse();
    let repo = gix::discover(".").context("not inside a git repository")?;
    let store = RepoStore::open(&repo);

    match cli.command {
        // Bare `git store` lists kinds — a read-only default, like `git remote`.
        None => print_lines(store.kinds()?),
        Some(Command::Put(args)) => put(&store, args)?,
        Some(Command::Get { kind, name }) => {
            let (name, rev) = split_name_rev(&name);
            let handle = store.dynamic(segment("kind", &kind)?);
            let name_seg = entity(name)?;
            let value = match rev {
                Some(rev) => {
                    let oid = resolve_at(&repo, &handle, &name_seg, rev)?;
                    // Only read a commit that is actually a version of this
                    // entity, so a stray oid can't return an unrelated value.
                    if !handle.history(&name_seg)?.contains(&oid) {
                        bail!("{rev} is not a version of {kind}/{name}");
                    }
                    handle.get_at(oid)?
                }
                None => handle
                    .get(&name_seg)?
                    .with_context(|| format!("no entity {kind}/{name}"))?,
            };
            println!("{}", to_json(&value)?);
        }
        Some(Command::List { kind: Some(kind) }) => {
            print_lines(store.dynamic(segment("kind", &kind)?).list()?)
        }
        Some(Command::List { kind: None }) => print_lines(store.kinds()?),
        Some(Command::Log { kind, name }) => {
            let name_seg = entity(&name)?;
            print_log(
                &repo,
                store.dynamic(segment("kind", &kind)?).history(&name_seg)?,
            )?
        }
        Some(Command::Rm { kind, name }) => {
            let name_seg = entity(&name)?;
            if !store.dynamic(segment("kind", &kind)?).remove(&name_seg)? {
                bail!("no entity {kind}/{name}");
            }
        }
        Some(Command::Schema { command }) => match command {
            SchemaCommand::Put {
                kind,
                file,
                interactive,
            } => {
                let handle = store.dynamic(segment("kind", &kind)?);
                let doc = if interactive {
                    interactive::build_schema()?
                } else {
                    schema_doc_from_json(&read_source(file.as_ref())?)?
                };
                println!("{}", handle.schema().put(&doc)?);
            }
            SchemaCommand::Get { kind } => {
                let doc = store
                    .dynamic(segment("kind", &kind)?)
                    .schema()
                    .get()?
                    .with_context(|| format!("no schema published for kind {kind:?}"))?;
                println!("{}", to_json(&doc)?);
            }
            SchemaCommand::Show { kind } => {
                let kinds = match kind {
                    Some(kind) => vec![segment("kind", &kind)?],
                    None => store.kinds()?,
                };
                for seg in kinds {
                    let handle = store.dynamic(seg);
                    if let Some(doc) = handle.schema().get()? {
                        print_type(handle.name().as_str(), &doc);
                    }
                }
            }
            SchemaCommand::List => print_lines(store.kinds()?),
            SchemaCommand::Log { kind } => print_log(
                &repo,
                store.dynamic(segment("kind", &kind)?).schema().history()?,
            )?,
        },
    }
    Ok(())
}

/// Validate a CLI argument as a [`RefSegment`], with context naming which
/// argument it was.
fn segment(what: &str, value: &str) -> Result<RefSegment> {
    RefSegment::new(value).with_context(|| format!("invalid {what} {value:?}"))
}

/// Validate an entity-name argument, which may name a nested entity
/// (`<a>/<b>`) as well as a flat one.
fn entity(value: &str) -> Result<RefPath> {
    RefPath::new(value).with_context(|| format!("invalid name {value:?}"))
}

/// The store action: gather JSON (file, stdin, or the editor), then commit it
/// forward under the kind at the chosen name.
fn put(store: &RepoStore<'_>, args: PutArgs) -> Result<()> {
    let PutArgs {
        kind,
        name,
        file,
        message,
        edit,
        interactive,
    } = args;
    let name = name.as_deref().unwrap_or(&kind);
    let name_seg = entity(name)?;
    let handle = store.dynamic(segment("kind", &kind)?);

    let value = if interactive {
        interactive::value_for_kind(&handle)?
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
            None => edit_in_editor(&schema_skeleton(&handle)?)?,
        };
        facet_json::from_str(&json).map_err(|e| anyhow::anyhow!("invalid JSON: {e}"))?
    };

    let mut write = handle.write(&value);
    if let Some(summary) = message {
        write = write.message(summary);
    }
    println!("{}", write.at(&name_seg)?);
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
fn resolve_at(
    repo: &gix::Repository,
    kind: &DynKind<'_, '_>,
    name: &RefPath,
    rev: &str,
) -> Result<ObjectId> {
    let spec = if rev.starts_with(['~', '^', '@']) {
        format!("{}{rev}", kind.reference(name))
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

/// Parse a hand-authored schema JSON document into a [`Schema`].
fn schema_doc_from_json(json: &str) -> Result<Schema> {
    let value: Value =
        facet_json::from_str(json).map_err(|e| anyhow::anyhow!("invalid schema JSON: {e}"))?;
    from_value(value).map_err(|e| anyhow::anyhow!("invalid schema JSON: {e}"))
}

/// Content from `-F <file>`, or stdin when no file is given.
fn read_source(file: Option<&PathBuf>) -> Result<String> {
    match file {
        Some(path) => read_file(path),
        None => read_stdin(),
    }
}

/// A pretty schema-seeded skeleton for a kind, or an error when the kind has
/// no published schema (nothing to compose against).
fn schema_skeleton(kind: &DynKind<'_, '_>) -> Result<String> {
    match kind.schema().get()? {
        Some(doc) => Ok(pretty_skeleton(&doc)),
        None => bail!("no schema published for kind {:?}", kind.name().as_str()),
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
fn print_lines<T: std::fmt::Display>(items: Vec<T>) {
    for item in items {
        println!("{item}");
    }
}

/// A pretty-printed JSON skeleton for a kind's schema, or `{}` if it cannot be
/// rendered.
fn pretty_skeleton(doc: &Schema) -> String {
    let compact = skeleton(&doc.root, doc);
    match facet_json::from_str::<Value>(&compact) {
        Ok(value) => to_json(&value).unwrap_or(compact),
        Err(_) => "{}\n".to_owned(),
    }
}

/// A compact placeholder JSON snippet matching `schema`.
fn skeleton(schema: &Node, doc: &Schema) -> String {
    match resolve(schema, doc) {
        Node::Bool => "false".into(),
        Node::Char | Node::String | Node::Bytes => "\"\"".into(),
        Node::F32 | Node::F64 => "0.0".into(),
        Node::I8 | Node::I16 | Node::I32 | Node::I64 | Node::I128 | Node::ISize => "0".into(),
        Node::U8 | Node::U16 | Node::U32 | Node::U64 | Node::U128 | Node::USize => "0".into(),
        Node::Struct(fields) => {
            let body: Vec<_> = fields
                .iter()
                .map(|(name, schema)| format!("{name:?}:{}", skeleton(schema, doc)))
                .collect();
            format!("{{{}}}", body.join(","))
        }
        Node::Tuple(elems) => {
            let body: Vec<_> = elems.iter().map(|s| skeleton(s, doc)).collect();
            format!("[{}]", body.join(","))
        }
        Node::Array { elem, len } => {
            let body: Vec<_> = (0..*len).map(|_| skeleton(elem, doc)).collect();
            format!("[{}]", body.join(","))
        }
        // A scalar-keyed map reads back as a JSON object; a composite-keyed one
        // as an array of `{ k, v }` pairs — mirror that in the seed.
        Node::Map { key, .. } if is_scalar_schema(resolve(key, doc)) => "{}".into(),
        Node::List(_) | Node::Map { .. } => "[]".into(),
        Node::Optional(_) | Node::Unit | Node::RawTree | Node::Dynamic => "null".into(),
        Node::Enum(variants) => match variants.first_key_value() {
            Some((name, kind)) => {
                let payload = match kind {
                    VariantKind::Unit => "null".to_owned(),
                    VariantKind::Newtype(inner) => skeleton(inner, doc),
                    VariantKind::Tuple(elems) => {
                        let body: Vec<_> = elems.iter().map(|s| skeleton(s, doc)).collect();
                        format!("[{}]", body.join(","))
                    }
                    VariantKind::Struct(fields) => skeleton(&Node::Struct(fields.clone()), doc),
                };
                format!("{{{name:?}:{payload}}}")
            }
            None => "null".into(),
        },
        Node::Ref(_) => "null".into(),
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
fn print_type(kind: &str, doc: &Schema) {
    println!("{kind}");
    match resolve(&doc.root, doc) {
        Node::Struct(fields) => {
            for (name, schema) in fields {
                println!("  {name}: {}", label(schema));
            }
        }
        other => println!("  {}", label(other)),
    }
}

/// Whether a schema node is a scalar — the same classification that decides
/// map layout (name-keyed object vs. `{ k, v }` pair array) in
/// `serialize_value_with_schema`.
pub(crate) fn is_scalar_schema(schema: &Node) -> bool {
    matches!(
        schema,
        Node::Bool
            | Node::Char
            | Node::String
            | Node::I8
            | Node::I16
            | Node::I32
            | Node::I64
            | Node::I128
            | Node::ISize
            | Node::U8
            | Node::U16
            | Node::U32
            | Node::U64
            | Node::U128
            | Node::USize
            | Node::F32
            | Node::F64
    )
}

/// Follow a `Ref` to the definition it names; any other node is returned as-is.
pub(crate) fn resolve<'d>(schema: &'d Node, doc: &'d Schema) -> &'d Node {
    match schema {
        Node::Ref(name) => doc.defs.get(name).map_or(schema, |s| resolve(s, doc)),
        other => other,
    }
}

/// A short, one-line label for a schema node.
fn label(schema: &Node) -> String {
    match schema {
        Node::Unit => "unit".into(),
        Node::Bool => "bool".into(),
        Node::Char => "char".into(),
        Node::String => "string".into(),
        Node::Bytes => "bytes".into(),
        Node::I8 | Node::I16 | Node::I32 | Node::I64 | Node::I128 | Node::ISize => "int".into(),
        Node::U8 | Node::U16 | Node::U32 | Node::U64 | Node::U128 | Node::USize => "uint".into(),
        Node::F32 | Node::F64 => "float".into(),
        Node::List(elem) | Node::Array { elem, .. } => format!("[{}]", label(elem)),
        Node::Tuple(elems) => {
            let inner: Vec<_> = elems.iter().map(label).collect();
            format!("({})", inner.join(", "))
        }
        Node::Map { key, value } => format!("{{{}: {}}}", label(key), label(value)),
        Node::Optional(inner) => format!("{}?", label(inner)),
        Node::Struct(_) => "struct".into(),
        Node::Enum(variants) => {
            let names: Vec<_> = variants
                .iter()
                .map(|(name, kind)| match kind {
                    VariantKind::Unit => name.clone(),
                    _ => format!("{name}(…)"),
                })
                .collect();
            names.join(" | ")
        }
        Node::RawTree => "tree".into(),
        Node::Dynamic => "dynamic".into(),
        Node::Ref(name) => name.clone(),
    }
}
