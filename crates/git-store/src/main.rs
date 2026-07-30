//! `git-store`: a git external subcommand (`git store …`) that stores anything
//! in Git as a real tree. JSON lives only here, at the CLI boundary; the
//! [`Store`] underneath is oid-in/oid-out.
//!
//! Bare `git store` lists kinds. Writing is `git store put <schema> <value>`,
//! which compiles `<value>` under `<schema>` into the `{value/, schema/}`
//! tree and prints its hash — the document's identity — without advancing
//! any ref; reading is `git store get <tree-ish>`, which decodes any tree of
//! that shape back to JSON; `git store check <tree-ish> <schema>` validates a
//! bare value tree against a schema without decoding it. `<value>` may be
//! omitted, taking content from `-F <file>`, stdin, `$EDITOR`, or — with
//! `-i` — an interactive prompt walking the schema.
//!
//! Named, ref-addressed, versioned entities — the pre-S1 shape of this CLI
//! — remain reachable: `list`, `log`, `rm`, and the `schema` subgroup mirror
//! git porcelain over them, and any entity ref (`refs/store/<kind>/<name>`)
//! is itself a valid `<tree-ish>` for `get`/`check`. `put`/`get` also accept
//! their old two-argument forms as a hidden compatibility path (see
//! [`PutArgs`] and [`Command::Get`]).

mod interactive;

use std::io::{IsTerminal, Read};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use facet_git_tree::{Node, Schema, VariantKind, validate_with_schema};
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
    /// Compile a value under a schema; prints the document's tree hash.
    /// Content for `<value>` comes from the positional argument itself
    /// (parsed as JSON) when given, else from `-F <file>`, stdin, or
    /// `$EDITOR`.
    ///
    /// Hidden compatibility: when a second argument is given and does *not*
    /// parse as JSON, it is taken as the old `put <kind> <name>` form —
    /// committing forward at `refs/store/<kind>/<name>` and printing the
    /// commit id instead.
    Put(PutArgs),
    /// Decode a document back to JSON from any tree-ish of the
    /// `{value/, schema/}` shape `put` compiles — a bare tree hash, or any
    /// commit/ref whose tree has that shape.
    ///
    /// Hidden compatibility: `get <kind> <name>` (two arguments) is the old
    /// ref-addressed form; `<name>` may carry a revision suffix
    /// (`carbonara~1`, `carbonara@{yesterday}`, `carbonara@<oid>`).
    Get {
        #[arg(num_args = 1..=2, value_name = "TREE-ISH")]
        args: Vec<String>,
    },
    /// Check whether a tree-ish's value conforms to a schema, without
    /// decoding it. Exits non-zero, with a diagnostic, when it does not.
    Check { tree_ish: String, schema: String },
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
    /// The schema: a schema published at `refs/schema/<schema>`.
    schema: String,
    /// An inline JSON value, or — for the hidden old form — an entity name.
    value: Option<String>,
    /// JSON file to store; stdin or `$EDITOR` is used when omitted.
    #[arg(short = 'F', long = "file", value_name = "FILE")]
    file: Option<PathBuf>,
    /// Commit message for the hidden old (named) form (a `Schema:` trailer is
    /// always added). Has no effect on a pure compile.
    #[arg(short = 'm', long = "message", value_name = "MSG")]
    message: Option<String>,
    /// Edit the content in `$VISUAL`/`$EDITOR` before storing.
    #[arg(short = 'e', long = "edit")]
    edit: bool,
    /// Build the value by prompting for each field the schema names, instead
    /// of taking JSON from the positional argument, a file, stdin, or the
    /// editor.
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
        Some(Command::Get { args }) => match <[String; 1]>::try_from(args) {
            // `get <tree-ish>`: decode any tree of the `{value/, schema/}`
            // shape directly, whatever it was reached through.
            Ok([tree_ish]) => {
                let tree = resolve_tree(&repo, &tree_ish)?;
                // `decode` reads entirely out of `tree`'s own embedded
                // schema, so which kind this handle is opened on is
                // irrelevant — any placeholder name will do.
                let value = document_handle(&store)
                    .decode(tree)
                    .with_context(|| format!("{tree_ish} is not a document"))?;
                println!("{}", to_json(&value)?);
            }
            // Hidden old form: `get <kind> <name>`.
            Err(args) => {
                let [kind, name] = <[String; 2]>::try_from(args)
                    .expect("clap enforces 1..=2 positional arguments");
                let (name, rev) = split_name_rev(&name);
                let handle = store.dynamic(segment("kind", &kind)?);
                let name_seg = entity(name)?;
                let value = match rev {
                    Some(rev) => {
                        let oid = resolve_at(&repo, &handle, &name_seg, rev)?;
                        // Only read a commit that is actually a version of
                        // this entity, so a stray oid can't return an
                        // unrelated value.
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
        },
        Some(Command::Check { tree_ish, schema }) => {
            let tree = resolve_tree(&repo, &tree_ish)?;
            let doc = store
                .dynamic(segment("schema", &schema)?)
                .schema()
                .get()?
                .with_context(|| format!("no schema published for {schema:?}"))?;
            validate_with_schema(&tree, &doc, store.objects())
                .with_context(|| format!("{tree_ish} does not conform to {schema:?}"))?;
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

/// A [`Dynamic`] handle whose kind name is irrelevant — only [`Kind::decode`]
/// and [`Kind::compile`] read/write independent of it, since a document's
/// schema travels inline with it rather than through a kind's own ref.
fn document_handle<'s, 'r>(store: &'s RepoStore<'r>) -> DynKind<'s, 'r> {
    store.dynamic(RefSegment::new("_").expect("\"_\" is a valid ref segment"))
}

/// Resolve `spec` to a tree: any revision syntax `rev-parse` accepts
/// (`<oid>`, a ref, `<rev>~1`, `<rev>:<path>`, …) resolved to an object,
/// then peeled down to a tree — a no-op when it already is one.
fn resolve_tree(repo: &gix::Repository, spec: &str) -> Result<ObjectId> {
    let id = repo
        .rev_parse_single(spec)
        .with_context(|| format!("cannot resolve {spec:?}"))?;
    let tree = id
        .object()
        .with_context(|| format!("cannot resolve {spec:?}"))?
        .peel_to_kind(gix::objs::Kind::Tree)
        .with_context(|| format!("{spec:?} is not a tree-ish"))?;
    Ok(tree.id)
}

/// Gather JSON content the same way for either `put` form: an explicit
/// `-F <file>`, piped stdin, or — at a terminal with neither — the editor,
/// seeded from `kind`'s schema. Mirrors `git notes add`.
fn gathered_json(handle: &DynKind<'_, '_>, file: &Option<PathBuf>, edit: bool) -> Result<String> {
    let base = if let Some(path) = file {
        Some(read_file(path)?)
    } else if !std::io::stdin().is_terminal() {
        Some(read_stdin()?)
    } else {
        None
    };
    match base {
        Some(content) if !edit => Ok(content),
        Some(content) => edit_in_editor(&content),
        None => edit_in_editor(&schema_skeleton(handle)?),
    }
}

/// `put`: compile `<value>` under `<schema>`, printing the document's tree
/// hash — a pure operation, advancing no ref.
///
/// The hidden old form (`value` present but not valid JSON) instead commits
/// forward at `refs/store/<schema>/<value>`, printing the commit id — the
/// pre-S1 named-entity write path.
fn put(store: &RepoStore<'_>, args: PutArgs) -> Result<()> {
    let PutArgs {
        schema,
        value,
        file,
        message,
        edit,
        interactive,
    } = args;

    match &value {
        Some(text) if !interactive && !edit && file.is_none() => {
            match facet_json::from_str::<Value>(text) {
                Ok(value) => {
                    let handle = store.dynamic(segment("schema", &schema)?);
                    println!("{}", handle.compile(&value)?);
                }
                // Not JSON: the hidden old `put <kind> <name>` form.
                Err(_) => put_named(store, schema, text, file, message, edit, interactive)?,
            }
            return Ok(());
        }
        // A second argument alongside `-F`/`-i` can only be the old form's
        // entity name — an inline value has nowhere else to come from.
        Some(name) => {
            put_named(store, schema, name, file, message, edit, interactive)?;
            return Ok(());
        }
        None => {}
    }

    let handle = store.dynamic(segment("schema", &schema)?);
    let value = if interactive {
        interactive::value_for_kind(&handle)?
    } else {
        let json = gathered_json(&handle, &file, edit)?;
        facet_json::from_str(&json).map_err(|e| anyhow::anyhow!("invalid JSON: {e}"))?
    };
    println!("{}", handle.compile(&value)?);
    Ok(())
}

/// The hidden old `put <kind> <name>` form: commit the gathered value forward
/// at `refs/store/<kind>/<name>`, printing the commit id.
fn put_named(
    store: &RepoStore<'_>,
    kind: String,
    name: &str,
    file: Option<PathBuf>,
    message: Option<String>,
    edit: bool,
    interactive: bool,
) -> Result<()> {
    let name_seg = entity(name)?;
    let handle = store.dynamic(segment("kind", &kind)?);
    let value = if interactive {
        interactive::value_for_kind(&handle)?
    } else {
        let json = gathered_json(&handle, &file, edit)?;
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

/// A compact placeholder JSON object snippet for a struct's fields, shared by
/// [`Node::Struct`] (fields carry [`StructField`]) and a struct enum
/// variant's payload (bare [`Node`] fields).
fn skeleton_fields<'a>(
    fields: impl Iterator<Item = (&'a String, &'a Node)>,
    doc: &Schema,
) -> String {
    let body: Vec<_> = fields
        .map(|(name, schema)| format!("{name:?}:{}", skeleton(schema, doc)))
        .collect();
    format!("{{{}}}", body.join(","))
}

/// A compact placeholder JSON snippet matching `schema`.
fn skeleton(schema: &Node, doc: &Schema) -> String {
    match resolve(schema, doc) {
        Node::Bool => "false".into(),
        Node::Char | Node::String | Node::Bytes => "\"\"".into(),
        Node::F32 | Node::F64 => "0.0".into(),
        Node::I8 | Node::I16 | Node::I32 | Node::I64 | Node::I128 | Node::ISize => "0".into(),
        Node::U8 | Node::U16 | Node::U32 | Node::U64 | Node::U128 | Node::USize => "0".into(),
        Node::Struct(fields) => skeleton_fields(fields.iter().map(|(n, f)| (n, &f.node)), doc),
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
                    VariantKind::Struct(fields) => skeleton_fields(fields.iter(), doc),
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
            for (name, field) in fields {
                let default = if field.has_default { " = default" } else { "" };
                println!("  {name}: {}{default}", label(&field.node));
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
