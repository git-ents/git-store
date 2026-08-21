//! `git-store`: a git external subcommand (`git store …`) that stores anything
//! in Git as a real tree. JSON lives only here, at the CLI boundary; the
//! [`Store`] underneath is oid-in/oid-out.
//!
//! Bare `git store` prints help, like any clap app; `git store list` (alias
//! `ls`) lists kinds. `git store compile <kind> [<value>]` compiles `<value>`
//! under `<kind>`'s schema into the `{value/, schema/}` tree and prints its
//! hash — the document's identity — without advancing any ref. `git store put
//! <kind> <name> [<value>]` does the same, then publishes it under `<name>`,
//! advancing that name's ref. Reading mirrors writing: `git store cat
//! <tree-ish>` decodes any tree of that shape back to JSON, content-addressed
//! like `git cat-file`; `git store get <kind> <name>` resolves a name first,
//! then decodes it. `git store check <tree-ish> <schema>` validates a bare
//! value tree against a schema without decoding it. `<value>` may be
//! omitted, taking content from `-F <file>`, stdin, `$EDITOR`, or — with
//! `-i` — an interactive prompt walking the schema.
//!
//! Named, ref-addressed, versioned entities remain reachable: `list`, `log`,
//! `rm`, and the `schema` subgroup mirror git porcelain over them, and any
//! entity ref under the selected data prefix is itself a valid `<tree-ish>`
//! for `cat`/`check`.

mod interactive;

use std::fmt;
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use facet_git_tree::{Node, Schema, VariantKind, validate_with_schema};
use facet_value::{VArray, VObject, Value, from_value};
use gix_store::{
    ApplyError, DeleteResult, DocumentInspection, DocumentTree, Dynamic, EntityState, Expectation,
    GixRefStore, Kind, Layout, ObjectId, Publication, PublishOptions, RefName, RefPath, RefPrefix,
    RefSegment, RefStore, RepoStore, SchemaTree, ValueTree,
};

/// A handle on one kind, over the CLI's own repo-backed store.
pub(crate) type DynKind<'s, 'r> = Kind<'s, Dynamic, GixRefStore<'r>, &'r gix::OdbHandle>;

#[derive(Parser)]
#[command(
    name = "git-store",
    about = "Store anything in Git as a real tree",
    version,
    arg_required_else_help = true
)]
struct Cli {
    /// Output format, honored by every command.
    #[arg(long, global = true, value_enum)]
    format: Option<OutputFormat>,
    /// Shorthand for `--format json`.
    #[arg(long, global = true, hide = true)]
    json: bool,
    /// Ref namespace containing data kinds and compatibility entity aliases.
    #[arg(
        long,
        global = true,
        default_value = "refs/store",
        value_name = "PREFIX"
    )]
    data_prefix: String,
    /// Ref namespace containing published kind schemas.
    #[arg(
        long,
        global = true,
        default_value = "refs/schema",
        value_name = "PREFIX"
    )]
    schema_prefix: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
    Ndjson,
}

impl OutputFormat {
    fn from_cli(format: Option<Self>, json: bool) -> Self {
        if json {
            Self::Json
        } else {
            format.unwrap_or(Self::Text)
        }
    }

    fn machine(self) -> bool {
        !matches!(self, Self::Text)
    }
}

/// The stable machine-facing class of a CLI failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExitClass {
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

    fn code(self) -> i32 {
        match self {
            Self::Cas => 4,
            Self::NotFound => 3,
            Self::Schema => 5,
            Self::Invalid => 2,
            Self::Other => 1,
        }
    }

    fn prefer(self, other: Self) -> Self {
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
struct ClassifiedContext {
    class: ExitClass,
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

fn cli_error(class: ExitClass, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(ClassifiedContext::new(class, message))
}

fn cli_context(class: ExitClass, message: impl Into<String>) -> ClassifiedContext {
    ClassifiedContext::new(class, message)
}

#[derive(Subcommand)]
enum Command {
    /// Compile a value under a kind's schema; prints the document tree — a
    /// pure operation, advancing no ref. Content for `<value>` comes from
    /// the positional argument itself (parsed as JSON) when given, else
    /// from `-F <file>`, stdin, or `$EDITOR`.
    Compile(CompileArgs),
    /// Store a value under a name, advancing that name's ref.
    Put(PutArgs),
    /// Decode any document tree, ref, or commit — a bare tree hash, or any
    /// commit/ref whose tree has the `{value/, schema/}` shape `put`
    /// compiles. Content-addressed, like `git cat-file`; use `get` to
    /// resolve a name first.
    Cat {
        #[arg(value_name = "TREE-ISH")]
        tree_ish: String,
        /// Opt into decoding pre-newline leaves and pre-`kind` schemas.
        #[arg(long)]
        legacy_leaves: bool,
    },
    /// Read a stored value as JSON, by kind and name.
    ///
    /// `<name>` may carry a revision suffix (`carbonara~1`,
    /// `carbonara@{yesterday}`, `carbonara@<oid>`).
    Get { kind: String, name: String },
    /// Check whether a tree-ish's value conforms to a schema, without
    /// decoding it. Exits non-zero, with a diagnostic, when it does not.
    Check { tree_ish: String, schema: String },
    /// Validate the repository's supported object format and schema bootstrap.
    Doctor,
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
    /// Inspect and resolve Git refs.
    Ref {
        #[command(subcommand)]
        command: RefCommand,
    },
    /// Inspect Git objects and trees.
    Object {
        #[command(subcommand)]
        command: ObjectCommand,
    },
    /// Encode and decode unbound values with explicit schemas.
    Value {
        #[command(subcommand)]
        command: ValueCommand,
    },
    /// Inspect and compose bound documents.
    Document {
        #[command(subcommand)]
        command: DocumentCommand,
    },
    /// Operate on canonical content-derived entity identities.
    Entity {
        #[command(subcommand)]
        command: EntityCommand,
    },
}

/// Arguments for `compile`.
#[derive(clap::Args)]
struct CompileArgs {
    /// The kind: a schema published under the selected schema prefix.
    kind: String,
    /// An inline JSON value. Malformed JSON is rejected without touching a
    /// ref — `compile` never advances one.
    value: Option<String>,
    /// JSON file to compile; stdin or `$EDITOR` is used when omitted.
    #[arg(short = 'F', long = "file", value_name = "FILE")]
    file: Option<PathBuf>,
    /// Edit the content in `$VISUAL`/`$EDITOR` before compiling.
    #[arg(short = 'e', long = "edit")]
    edit: bool,
    /// Build the value by prompting for each field the schema names, instead
    /// of taking JSON from the positional argument, a file, stdin, or the
    /// editor.
    #[arg(short = 'i', long = "interactive", conflicts_with_all = ["file", "edit"])]
    interactive: bool,
}

/// Arguments for `put`.
#[derive(clap::Args)]
struct PutArgs {
    /// The kind: a schema published under the selected schema prefix.
    kind: String,
    /// The entity name; the ref `put` advances.
    name: String,
    /// An inline JSON value; stdin, `-F <file>`, or `$EDITOR` is used when
    /// omitted.
    value: Option<String>,
    /// JSON file to store; stdin or `$EDITOR` is used when omitted.
    #[arg(short = 'F', long = "file", value_name = "FILE")]
    file: Option<PathBuf>,
    /// Commit message. Reserved legacy trailer lines (`Schema:`,
    /// `Schema-Version:`, or `Ents-Ref:`) are rejected.
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
enum RefCommand {
    /// List refs, optionally narrowed by full prefix and kind.
    List {
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long)]
        kind: Option<String>,
    },
    /// Resolve a full ref to its object id.
    Resolve { reference: String },
}

#[derive(Subcommand)]
enum ObjectCommand {
    /// Inspect an object or revision.
    Inspect { object_ish: String },
    /// List the direct entries of a tree or tree-ish.
    Tree { tree_ish: String },
}

#[derive(Subcommand)]
enum ValueCommand {
    /// Decode an unbound value tree under an explicit schema tree or commit.
    Decode {
        value_tree: String,
        #[arg(long)]
        schema: String,
        /// Opt into decoding pre-newline leaves and pre-`kind` schemas.
        #[arg(long)]
        legacy_leaves: bool,
    },
    /// Encode JSON from stdin or a file under an explicit schema.
    Encode {
        #[arg(long)]
        schema: String,
        #[arg(short = 'F', long = "file", value_name = "FILE")]
        file: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum DocumentCommand {
    /// Inspect the root shape of a document tree.
    Inspect { document_tree_ish: String },
    /// Bind an encoded value tree to a schema tree.
    Bind {
        value_tree_ish: String,
        #[arg(long)]
        schema: String,
    },
    /// Publish a prepared document using an explicit compare-and-swap.
    Publish {
        kind: String,
        document_tree_ish: String,
        #[arg(long)]
        alias: Option<String>,
        #[arg(long)]
        expected: String,
        /// Parent commit for a newly written publication commit.
        #[arg(long)]
        parent: Option<String>,
        #[arg(short = 'm', long = "message")]
        message: Option<String>,
    },
}

#[derive(Subcommand)]
enum EntityCommand {
    /// Publish a canonical tombstone for an entity id.
    Delete { kind: String, entity_id: String },
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
    /// Print a kind's current or historical schema as JSON.
    Get {
        kind: String,
        /// Address the schema publication commit directly.
        #[arg(long)]
        at: Option<String>,
        /// Opt into decoding pre-newline leaves and pre-`kind` schemas.
        #[arg(long)]
        legacy_leaves: bool,
    },
    /// Inspect a schema publication commit directly.
    Inspect {
        kind: String,
        #[arg(long)]
        at: String,
        /// Opt into decoding pre-newline leaves and pre-`kind` schemas.
        #[arg(long)]
        legacy_leaves: bool,
    },
    /// Show a kind's field layout, human-readable (all kinds when omitted).
    Show { kind: Option<String> },
    /// List kinds that have a published schema.
    #[command(visible_alias = "ls")]
    List,
    /// Show a kind's schema evolution history, newest first.
    Log { kind: String },
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error:#}");
        std::process::exit(error_exit_code(&error));
    }
}

fn run() -> Result<()> {
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
    let output = OutputFormat::from_cli(cli.format, cli.json);
    let layout = layout_from_cli(&cli.data_prefix, &cli.schema_prefix)?;
    let mut repo = match gix::discover(".") {
        Ok(repo) => repo,
        Err(error) => {
            if raw_repository_object_format(Path::new("."))
                .is_some_and(|format| format.eq_ignore_ascii_case("sha256"))
            {
                bail!(
                    "unsupported Git object format: expected sha1, observed sha256; this build's schema codec and fixed-point digest are SHA-1-only"
                );
            }
            return Err(error).context("not inside a git repository");
        }
    };
    repo.object_cache_size_if_unset(4 * 1024 * 1024);
    let store = RepoStore::open_with_layout(&repo, layout);

    match cli.command {
        Command::Compile(args) => compile(&store, args, output)?,
        Command::Put(args) => put(&store, args, output)?,
        Command::Cat {
            tree_ish,
            legacy_leaves,
        } => cat(&repo, &store, &tree_ish, legacy_leaves, output)?,
        Command::Get { kind, name } => get(&repo, &store, &kind, &name, output)?,
        Command::Check { tree_ish, schema } => check(&repo, &store, &tree_ish, &schema, output)?,
        Command::Doctor => doctor(&repo, &store, output)?,
        Command::List { kind: Some(kind) } => list_entities(&store, &kind, output)?,
        Command::List { kind: None } => list_kinds(&store, output)?,
        Command::Log { kind, name } => log(&repo, &store, &kind, &name, output)?,
        Command::Rm { kind, name } => rm(&store, &kind, &name, output)?,
        Command::Schema { command } => match command {
            SchemaCommand::Put {
                kind,
                file,
                interactive,
            } => {
                let handle = store.dynamic(segment("kind", &kind)?);
                let doc = if interactive {
                    interactive::build_schema(&kind)?
                } else {
                    schema_doc_from_json(&read_source(file.as_ref())?)?
                };
                let commit = handle.schema().put(&doc)?;
                let mut fields = VObject::new();
                fields.insert("commit", oid_value(commit));
                emit_single(output, fields, || commit.to_string())?;
            }
            SchemaCommand::Get {
                kind,
                at,
                legacy_leaves,
            } => {
                let handle = store.dynamic(segment("kind", &kind)?);
                let snapshot = match at {
                    Some(at) => {
                        let commit = resolve_commit(&repo, &at)?;
                        if legacy_leaves {
                            handle.schema().snapshot_at_legacy(commit)?
                        } else {
                            handle.schema().snapshot_at(commit)?
                        }
                    }
                    None => {
                        if legacy_leaves {
                            handle.schema().current_snapshot_legacy()?
                        } else {
                            handle.schema().current_snapshot()?
                        }
                    }
                };
                if output.machine() {
                    emit_record(output, schema_record(&snapshot)?)?;
                } else {
                    println!("{}", to_json(&snapshot.schema)?);
                }
            }
            SchemaCommand::Inspect {
                kind,
                at,
                legacy_leaves,
            } => {
                schema_inspect(&repo, &store, &kind, &at, legacy_leaves, output)?;
            }
            SchemaCommand::Show { kind } => schema_show(&store, kind.as_deref(), output)?,
            SchemaCommand::List => list_kinds(&store, output)?,
            SchemaCommand::Log { kind } => log_commits(
                &repo,
                store.dynamic(segment("kind", &kind)?).schema().history()?,
                output,
            )?,
        },
        Command::Ref { command } => match command {
            RefCommand::List { prefix, kind } => {
                ref_list(&store, prefix.as_deref(), kind.as_deref(), output)?
            }
            RefCommand::Resolve { reference } => ref_resolve(&store, &reference, output)?,
        },
        Command::Object { command } => match command {
            ObjectCommand::Inspect { object_ish } => object_inspect(&repo, &object_ish, output)?,
            ObjectCommand::Tree { tree_ish } => object_tree(&repo, &tree_ish, output)?,
        },
        Command::Value { command } => match command {
            ValueCommand::Decode {
                value_tree,
                schema,
                legacy_leaves,
            } => value_decode(&repo, &store, &value_tree, &schema, legacy_leaves, output)?,
            ValueCommand::Encode { schema, file } => {
                value_encode(&repo, &store, &schema, file.as_ref(), output)?
            }
        },
        Command::Document { command } => match command {
            DocumentCommand::Inspect { document_tree_ish } => {
                document_inspect(&repo, &store, &document_tree_ish, output)?
            }
            DocumentCommand::Bind {
                value_tree_ish,
                schema,
            } => document_bind(&repo, &store, &value_tree_ish, &schema, output)?,
            DocumentCommand::Publish {
                kind,
                document_tree_ish,
                alias,
                expected,
                parent,
                message,
            } => document_publish(
                &repo,
                &store,
                DocumentPublishRequest {
                    kind: &kind,
                    document_spec: &document_tree_ish,
                    alias: alias.as_deref(),
                    expected: &expected,
                    parent: parent.as_deref(),
                    message: message.as_deref(),
                },
                output,
            )?,
        },
        Command::Entity { command } => match command {
            EntityCommand::Delete { kind, entity_id } => {
                entity_delete(&store, &kind, &entity_id, output)?
            }
        },
    }
    Ok(())
}

fn error_exit_code(error: &anyhow::Error) -> i32 {
    let mut class = ExitClass::Other;
    for cause in error.chain() {
        if let Some(context) = cause.downcast_ref::<ClassifiedContext>() {
            class = class.prefer(context.class);
        }
        if cause
            .downcast_ref::<ApplyError<<GixRefStore<'static> as RefStore>::Error>>()
            .is_some()
        {
            class = class.prefer(ExitClass::Cas);
        }
        if let Some(store_error) = cause.downcast_ref::<gix_store::Error>() {
            class = class.prefer(store_error_exit_class(store_error));
        }
        if cause
            .downcast_ref::<facet_git_tree::SchemaReadError>()
            .is_some()
            || cause
                .downcast_ref::<facet_git_tree::SchemaWriteError>()
                .is_some()
            || cause
                .downcast_ref::<facet_git_tree::SchemaPinError>()
                .is_some()
            || cause
                .downcast_ref::<facet_git_tree::MigrationError>()
                .is_some()
            || cause
                .downcast_ref::<facet_git_tree::MigrationPinError>()
                .is_some()
        {
            class = class.prefer(ExitClass::Schema);
        }
        if let Some(error) = cause.downcast_ref::<facet_git_tree::DeserializeError>() {
            class = class.prefer(deserialize_exit_class(error));
        }
        if let Some(error) = cause.downcast_ref::<facet_git_tree::SerializeError>()
            && matches!(error, facet_git_tree::SerializeError::Key(_))
        {
            class = class.prefer(ExitClass::Invalid);
        }
        if cause.downcast_ref::<facet_git_tree::KeyError>().is_some() {
            class = class.prefer(ExitClass::Invalid);
        }
    }
    class.code()
}

fn store_error_exit_class(error: &gix_store::Error) -> ExitClass {
    match error {
        gix_store::Error::NoSchema { .. }
        | gix_store::Error::MissingObject { .. }
        | gix_store::Error::SubtreeMissing { .. } => ExitClass::NotFound,
        gix_store::Error::NotACommit { .. }
        | gix_store::Error::NotATree { .. }
        | gix_store::Error::InvalidTrailer { .. } => ExitClass::Invalid,
        gix_store::Error::NotSubtreeBound { .. }
        | gix_store::Error::MissingTrailer { .. }
        | gix_store::Error::KindMismatch { .. }
        | gix_store::Error::IdentityUniverse { .. }
        | gix_store::Error::Schema(_)
        | gix_store::Error::SchemaWrite(_)
        | gix_store::Error::SchemaRead(_)
        | gix_store::Error::SchemaPin(_)
        | gix_store::Error::MigrationPin(_)
        | gix_store::Error::Migration(_)
        | gix_store::Error::SchemaNotInHistory { .. }
        | gix_store::Error::TargetHistoryEmpty { .. }
        | gix_store::Error::TargetSchemaMismatch { .. }
        | gix_store::Error::TargetSchemaNotInHistory { .. }
        | gix_store::Error::MigrationMissing { .. } => ExitClass::Schema,
        gix_store::Error::ReservedTrailer { .. }
        | gix_store::Error::NameTaken { .. }
        | gix_store::Error::EntityIdCollision { .. }
        | gix_store::Error::Deleted { .. }
        | gix_store::Error::TombstoneSchemaMismatch
        | gix_store::Error::InvalidTombstone
        | gix_store::Error::Signer(_)
        | gix_store::Error::Fingerprint(_)
        | gix_store::Error::Backend(_)
        | gix_store::Error::Serialize(_)
        | gix_store::Error::Deserialize(_)
        | gix_store::Error::UnsupportedObjectFormat { .. } => ExitClass::Other,
    }
}

fn deserialize_exit_class(error: &facet_git_tree::DeserializeError) -> ExitClass {
    match error {
        facet_git_tree::DeserializeError::NotFound(_) => ExitClass::NotFound,
        facet_git_tree::DeserializeError::NotATree(_)
        | facet_git_tree::DeserializeError::InvalidOrdinal(_) => ExitClass::Invalid,
        _ => ExitClass::Other,
    }
}

fn json_record() -> VObject {
    let mut record = VObject::new();
    record.insert("status", "ok");
    record.insert("code", "ok");
    record
}

fn oid_value(oid: impl Into<ObjectId>) -> Value {
    oid.into().to_string().into()
}

fn string_array<I>(items: I) -> Value
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

fn emit_record(format: OutputFormat, record: VObject) -> Result<()> {
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
fn emit_single(format: OutputFormat, fields: VObject, text: impl FnOnce() -> String) -> Result<()> {
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
fn emit_fields(format: OutputFormat, fields: VObject) -> Result<()> {
    let mut record = json_record();
    record.extend(fields);
    emit_record(format, record)
}

/// One row of a [`emit_list`] result: `fields` are the bare, unwrapped
/// machine data for that row and `text` its one-line human rendering.
struct ListItem {
    fields: VObject,
    text: String,
}

/// Print a list-shaped command's result. Text prints one line per item.
/// `ndjson` prints one enveloped record per item. `json` prints one envelope
/// whose `list_field` holds the array of bare item objects — the same shape
/// [`ref_list`] and [`object_tree`] already used, generalized to every
/// list-producing command.
fn emit_list(format: OutputFormat, list_field: &str, items: Vec<ListItem>) -> Result<()> {
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

fn resolve_commit(repo: &gix::Repository, spec: &str) -> Result<ObjectId> {
    let id = repo.rev_parse_single(spec).with_context(|| {
        cli_context(
            ExitClass::NotFound,
            format!("cannot resolve commit {spec:?}"),
        )
    })?;
    let object = id.object().with_context(|| {
        cli_context(
            ExitClass::NotFound,
            format!("cannot resolve commit {spec:?}"),
        )
    })?;
    if object.kind != gix::objs::Kind::Commit {
        return Err(cli_error(
            ExitClass::Invalid,
            format!("{spec:?} is not a commit"),
        ));
    }
    Ok(id.detach())
}

/// Resolve an explicit schema argument as either a schema tree or publication
/// commit. No kind ref, name lookup, or commit trailer is consulted.
fn resolve_schema_tree(repo: &gix::Repository, spec: &str) -> Result<ObjectId> {
    resolve_tree(repo, spec)
}

fn schema_record(snapshot: &gix_store::SchemaSnapshot) -> Result<VObject> {
    let schema_json = facet_json::to_string(&snapshot.schema)
        .map_err(|e| anyhow::anyhow!("encoding schema JSON: {e}"))?;
    let schema: Value = facet_json::from_str(&schema_json)
        .map_err(|e| anyhow::anyhow!("encoding schema JSON: {e}"))?;
    let mut record = json_record();
    record.insert("kind", snapshot.schema.kind.clone());
    record.insert("commit", oid_value(snapshot.commit));
    record.insert("schema_tree", oid_value(snapshot.schema_tree));
    record.insert("schema", schema);
    Ok(record)
}

fn schema_inspect(
    repo: &gix::Repository,
    store: &RepoStore<'_>,
    kind: &str,
    at: &str,
    legacy_leaves: bool,
    format: OutputFormat,
) -> Result<()> {
    let segment = segment("kind", kind)?;
    let commit = resolve_commit(repo, at)?;
    let schema = store.dynamic(segment).schema();
    let snapshot = if legacy_leaves {
        schema.snapshot_at_legacy(commit)?
    } else {
        schema.snapshot_at(commit)?
    };
    if format.machine() {
        emit_record(format, schema_record(&snapshot)?)
    } else {
        println!("kind: {}", snapshot.schema.kind);
        println!("commit: {}", snapshot.commit);
        println!("schema tree: {}", snapshot.schema_tree);
        println!("{}", type_layout(kind, &snapshot.schema));
        Ok(())
    }
}

fn layout_from_cli(data: &str, schema: &str) -> Result<Layout> {
    Ok(Layout {
        data: RefPrefix::new(data).with_context(|| {
            cli_context(
                ExitClass::Invalid,
                format!("invalid data ref prefix {data:?}"),
            )
        })?,
        schema: RefPrefix::new(schema).with_context(|| {
            cli_context(
                ExitClass::Invalid,
                format!("invalid schema ref prefix {schema:?}"),
            )
        })?,
    })
}

fn ref_list(
    store: &RepoStore<'_>,
    prefix: Option<&str>,
    kind: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let prefix = RefPrefix::new(prefix.unwrap_or("refs"))?;
    let kind_prefixes = match kind {
        Some(kind) => {
            let kind = segment("kind", kind)?;
            Some((
                store.layout().data.child(&kind),
                store.layout().schema.join(&kind),
            ))
        }
        None => None,
    };
    let mut refs = store.refs().prefixed(&prefix)?;
    refs.retain(|(name, _)| {
        kind_prefixes
            .as_ref()
            .is_none_or(|(data_kind, schema_ref)| name.is_under(data_kind) || name == schema_ref)
    });
    if format.machine() {
        if matches!(format, OutputFormat::Ndjson) {
            for (name, oid) in refs {
                let mut record = json_record();
                record.insert("ref", name.to_string());
                record.insert("oid", oid_value(oid));
                emit_record(format, record)?;
            }
            Ok(())
        } else {
            let mut values = VArray::new();
            for (name, oid) in &refs {
                let mut item = VObject::new();
                item.insert("ref", name.to_string());
                item.insert("oid", oid_value(*oid));
                values.push(item);
            }
            let mut record = json_record();
            record.insert("refs", values);
            emit_record(format, record)
        }
    } else {
        for (name, oid) in refs {
            println!("{name} {oid}");
        }
        Ok(())
    }
}

fn ref_resolve(store: &RepoStore<'_>, reference: &str, format: OutputFormat) -> Result<()> {
    let reference = RefName::new(reference).with_context(|| {
        cli_context(
            ExitClass::Invalid,
            format!("invalid full ref {reference:?}"),
        )
    })?;
    let oid = store
        .refs()
        .read(&reference)?
        .with_context(|| cli_context(ExitClass::NotFound, format!("ref {reference} not found")))?;
    if format.machine() {
        let mut record = json_record();
        record.insert("ref", reference.to_string());
        record.insert("oid", oid_value(oid));
        emit_record(format, record)
    } else {
        println!("{oid}");
        Ok(())
    }
}

fn object_inspect(repo: &gix::Repository, spec: &str, format: OutputFormat) -> Result<()> {
    let id = repo
        .rev_parse_single(spec)
        .with_context(|| {
            cli_context(
                ExitClass::NotFound,
                format!("cannot resolve object {spec:?}"),
            )
        })?
        .detach();
    let object = repo
        .find_object(id)
        .with_context(|| cli_context(ExitClass::NotFound, format!("object {id} is not present")))?;
    let kind = format!("{:?}", object.kind).to_ascii_lowercase();
    if format.machine() {
        let mut record = json_record();
        record.insert("oid", oid_value(id));
        record.insert("kind", kind);
        record.insert("size", object.data.len() as u64);
        emit_record(format, record)
    } else {
        println!("{kind} {id} {}", object.data.len());
        Ok(())
    }
}

fn object_tree(repo: &gix::Repository, spec: &str, format: OutputFormat) -> Result<()> {
    let tree_id = resolve_tree(repo, spec)?;
    let tree = repo
        .find_object(tree_id)
        .with_context(|| {
            cli_context(
                ExitClass::NotFound,
                format!("tree {tree_id} is not present"),
            )
        })?
        .try_into_tree()
        .with_context(|| cli_context(ExitClass::Invalid, format!("{spec:?} is not a tree-ish")))?;
    let mut entries = Vec::new();
    for entry in tree.iter() {
        let entry = entry?;
        entries.push((
            String::from_utf8_lossy(entry.filename()).into_owned(),
            entry.object_id(),
            format!("{:?}", entry.kind()).to_ascii_lowercase(),
            entry.mode().as_str().to_owned(),
        ));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    if format.machine() {
        if matches!(format, OutputFormat::Ndjson) {
            for (name, oid, kind, mode) in entries {
                let mut record = json_record();
                record.insert("tree", oid_value(tree_id));
                record.insert("name", name);
                record.insert("oid", oid_value(oid));
                record.insert("kind", kind);
                record.insert("mode", mode);
                emit_record(format, record)?;
            }
            Ok(())
        } else {
            let mut values = VArray::new();
            for (name, oid, kind, mode) in entries {
                let mut item = VObject::new();
                item.insert("name", name);
                item.insert("oid", oid_value(oid));
                item.insert("kind", kind);
                item.insert("mode", mode);
                values.push(item);
            }
            let mut record = json_record();
            record.insert("tree", oid_value(tree_id));
            record.insert("entries", values);
            emit_record(format, record)
        }
    } else {
        for (name, oid, kind, mode) in entries {
            println!("{mode} {oid} {kind} {name}");
        }
        Ok(())
    }
}

fn value_decode(
    repo: &gix::Repository,
    store: &RepoStore<'_>,
    value_spec: &str,
    schema_spec: &str,
    legacy_leaves: bool,
    format: OutputFormat,
) -> Result<()> {
    let value_tree = resolve_tree(repo, value_spec)?;
    let schema_tree = resolve_schema_tree(repo, schema_spec)?;
    let value = if legacy_leaves {
        store.decode_value_legacy(value_tree, schema_tree)
    } else {
        store.decode_value(ValueTree::from(value_tree), SchemaTree::from(schema_tree))
    }?;
    if format.machine() {
        let mut record = json_record();
        record.insert("value_tree", oid_value(value_tree));
        record.insert("schema_tree", oid_value(schema_tree));
        record.insert("value", value);
        emit_record(format, record)
    } else {
        println!("{}", to_json(&value)?);
        Ok(())
    }
}

fn value_encode(
    repo: &gix::Repository,
    store: &RepoStore<'_>,
    schema_spec: &str,
    file: Option<&PathBuf>,
    format: OutputFormat,
) -> Result<()> {
    let schema_tree = resolve_schema_tree(repo, schema_spec)?;
    let json = read_source(file)?;
    let value: Value = facet_json::from_str(&json)
        .map_err(|e| cli_error(ExitClass::Invalid, format!("invalid JSON: {e}")))?;
    let value_tree = store.encode_value(&value, SchemaTree::from(schema_tree))?;
    if format.machine() {
        let mut record = json_record();
        record.insert("schema_tree", oid_value(schema_tree));
        record.insert("value_tree", oid_value(value_tree));
        emit_record(format, record)
    } else {
        println!("{value_tree}");
        Ok(())
    }
}

fn document_inspect(
    repo: &gix::Repository,
    store: &RepoStore<'_>,
    spec: &str,
    format: OutputFormat,
) -> Result<()> {
    let document_tree = resolve_tree(repo, spec)?;
    let inspection = store.inspect_document(DocumentTree::from(document_tree))?;
    if format.machine() {
        let mut record = json_record();
        record.insert("document_tree", oid_value(document_tree));
        match inspection {
            DocumentInspection::Bound(prepared) => {
                record.insert("kind", "bound");
                record.insert("value_tree", oid_value(prepared.value_tree()));
                record.insert("schema_tree", oid_value(prepared.schema_tree()));
            }
            DocumentInspection::LegacyValueRoot { value_tree } => {
                record.insert("kind", "legacy_value_root");
                record.insert("value_tree", oid_value(value_tree));
            }
            DocumentInspection::Malformed { found, reason, .. } => {
                record.insert("kind", "malformed");
                record.insert("found", string_array(found));
                record.insert("reason", reason.to_string());
            }
        }
        emit_record(format, record)
    } else {
        match inspection {
            DocumentInspection::Bound(prepared) => {
                println!("bound document {document_tree}");
                println!("  value: {}", prepared.value_tree());
                println!("  schema: {}", prepared.schema_tree());
            }
            DocumentInspection::LegacyValueRoot { value_tree } => {
                println!("legacy value root {value_tree}");
            }
            DocumentInspection::Malformed { found, reason, .. } => {
                println!("malformed document {document_tree}: {reason}");
                println!("  found: {}", found.join(", "));
            }
        }
        Ok(())
    }
}

fn document_bind(
    repo: &gix::Repository,
    store: &RepoStore<'_>,
    value_spec: &str,
    schema_spec: &str,
    format: OutputFormat,
) -> Result<()> {
    let value_tree = resolve_tree(repo, value_spec)?;
    let schema_tree = resolve_schema_tree(repo, schema_spec)?;
    let prepared =
        store.bind_document(ValueTree::from(value_tree), SchemaTree::from(schema_tree))?;
    if format.machine() {
        let mut record = json_record();
        record.insert("document_tree", oid_value(prepared.document_tree()));
        record.insert("value_tree", oid_value(prepared.value_tree()));
        record.insert("schema_tree", oid_value(prepared.schema_tree()));
        emit_record(format, record)
    } else {
        println!("{}", prepared.document_tree());
        Ok(())
    }
}

fn parse_expectation(value: &str) -> Result<Expectation> {
    if value.eq_ignore_ascii_case("absent") {
        Ok(Expectation::Absent)
    } else {
        Ok(Expectation::Exactly(value.parse().with_context(|| {
            cli_context(
                ExitClass::Invalid,
                format!("invalid expected object id {value:?}"),
            )
        })?))
    }
}

struct DocumentPublishRequest<'a> {
    kind: &'a str,
    document_spec: &'a str,
    alias: Option<&'a str>,
    expected: &'a str,
    parent: Option<&'a str>,
    message: Option<&'a str>,
}

fn document_publish(
    repo: &gix::Repository,
    store: &RepoStore<'_>,
    request: DocumentPublishRequest<'_>,
    format: OutputFormat,
) -> Result<()> {
    let DocumentPublishRequest {
        kind,
        document_spec,
        alias,
        expected,
        parent,
        message,
    } = request;
    let document_tree = resolve_tree(repo, document_spec)?;
    let prepared = match store.inspect_document(DocumentTree::from(document_tree))? {
        DocumentInspection::Bound(prepared) => prepared,
        DocumentInspection::LegacyValueRoot { .. } => {
            return Err(cli_error(
                ExitClass::Schema,
                format!("{document_spec:?} is an unbound value tree; bind it before publishing"),
            ));
        }
        DocumentInspection::Malformed { reason, .. } => {
            return Err(cli_error(
                ExitClass::Schema,
                format!("{document_spec:?} is not a publishable document: {reason}"),
            ));
        }
    };
    let expected = parse_expectation(expected)?;
    let mut options = PublishOptions::new(message.unwrap_or("publish prepared document"))
        .with_expectation(expected);
    if let Some(parent) = parent {
        options = options.with_parent(resolve_commit(repo, parent)?);
    }
    if let Some(alias) = alias {
        options = options.with_alias(entity(alias)?);
    }
    let publication = store
        .dynamic(segment("kind", kind)?)
        .publish_prepared(&prepared, options)
        .map_err(|error| {
            if matches!(error, gix_store::Error::Backend(_)) {
                // `gix-store` intentionally erases backend error parameters at
                // its public boundary. This command supplies an explicit CAS,
                // so classify that stable public variant without parsing text.
                cli_error(ExitClass::Cas, error.to_string())
            } else {
                error.into()
            }
        })?;
    emit_publication(format, kind, publication)
}

/// Render a [`Publication`]: text prints the entity id and its publication
/// commit, one per line; `json`/`ndjson` render the same two fields plus
/// `kind` in one record. The id is the document-tree oid by construction, so
/// no separate `document_tree` field is needed.
fn emit_publication(format: OutputFormat, kind: &str, publication: Publication) -> Result<()> {
    let id = publication.entity_id();
    let commit = publication.commit();
    let mut fields = VObject::new();
    fields.insert("kind", kind.to_owned());
    fields.insert("id", id.to_string());
    fields.insert("commit", oid_value(commit));
    emit_single(format, fields, || format!("{id}\n{commit}"))
}

fn entity_delete(
    store: &RepoStore<'_>,
    kind: &str,
    entity_text: &str,
    format: OutputFormat,
) -> Result<()> {
    // Names are application policy: an entity may be published under a
    // content-derived id or any other path, and either addresses it here.
    let name = entity(entity_text)?;
    let result = store.dynamic(segment("kind", kind)?).delete_name(&name)?;
    let (status, code, commit) = match result {
        DeleteResult::Deleted(entry) => ("deleted", "deleted", Some(entry.commit)),
        DeleteResult::AlreadyDeleted(entry) => {
            ("already_deleted", "already_deleted", Some(entry.commit))
        }
        DeleteResult::Absent => {
            return Err(cli_error(
                ExitClass::NotFound,
                format!("no entity {kind}/{entity_text}"),
            ));
        }
    };
    if format.machine() {
        let mut record = json_record();
        record.insert("status", status);
        record.insert("code", code);
        record.insert("kind", kind);
        record.insert("id", entity_text.to_owned());
        if let Some(commit) = commit {
            record.insert("commit", oid_value(commit));
        }
        emit_record(format, record)
    } else {
        println!("{status} {kind}/{entity_text}");
        Ok(())
    }
}

/// Validate a CLI argument as a [`RefSegment`], with context naming which
/// argument it was.
fn segment(what: &str, value: &str) -> Result<RefSegment> {
    RefSegment::new(value)
        .with_context(|| cli_context(ExitClass::Invalid, format!("invalid {what} {value:?}")))
}

/// Validate an entity-name argument, which may name a nested entity
/// (`<a>/<b>`) as well as a flat one.
fn entity(value: &str) -> Result<RefPath> {
    RefPath::new(value)
        .with_context(|| cli_context(ExitClass::Invalid, format!("invalid name {value:?}")))
}

/// Read the object format without asking gix to parse the repository config.
///
/// This is used only when discovery fails: gix builds without SHA-256 support
/// reject the `extensions.objectFormat` key before the doctor command can
/// report the more useful unsupported-format diagnostic.
fn raw_repository_object_format(start: &Path) -> Option<String> {
    let git_dir = raw_git_dir(start)?;
    let config = std::fs::read_to_string(git_dir.join("config")).ok()?;
    let mut in_extensions = false;

    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_extensions = line.eq_ignore_ascii_case("[extensions]");
            continue;
        }
        if !in_extensions {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("objectformat") {
            return Some(value.trim().to_owned());
        }
    }
    None
}

/// Find the Git directory at or above `start` without parsing its config.
fn raw_git_dir(start: &Path) -> Option<PathBuf> {
    let mut dir = std::fs::canonicalize(start).ok()?;
    loop {
        let dot_git = dir.join(".git");
        if dot_git.is_dir() {
            return Some(dot_git);
        }
        if dot_git.is_file() {
            let gitdir_file = std::fs::read_to_string(&dot_git).ok()?;
            let gitdir = gitdir_file
                .lines()
                .find_map(|line| line.trim().strip_prefix("gitdir:"))?
                .trim()
                .to_owned();
            let git_dir = PathBuf::from(gitdir);
            return Some(if git_dir.is_absolute() {
                git_dir
            } else {
                dir.join(git_dir)
            });
        }
        if dir.join("HEAD").is_file()
            && dir.join("config").is_file()
            && dir.join("objects").is_dir()
        {
            return Some(dir);
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

/// Check invariants that depend on the repository object database rather than
/// on a particular kind or ref namespace, and print the result.
///
/// The checks themselves — object format and the meta-schema fixed point —
/// live in [`gix_store::check_repository`]; this only gathers its inputs and
/// formats its outcome.
fn doctor(repo: &gix::Repository, store: &RepoStore<'_>, format: OutputFormat) -> Result<()> {
    let observed = repo.object_hash();
    // A SHA-256 repository is reported as SHA-1 by gix builds without its
    // `sha256` feature. Inspect the raw extension as well so this build does
    // not accidentally accept a repository it cannot encode correctly.
    let configured_sha256 = repo
        .config_snapshot()
        .string("extensions.objectformat")
        .is_some_and(|format| format.as_ref().eq_ignore_ascii_case(b"sha256"));
    doctor_with(
        || gix_store::check_repository(observed, configured_sha256, store.objects()),
        format,
    )
}

/// Format the outcome of one repository check, attaching CLI context to a
/// fixed-point failure specifically.
fn doctor_with(
    check: impl FnOnce() -> Result<gix_store::DoctorReport, gix_store::Error>,
    format: OutputFormat,
) -> Result<()> {
    let report = check().map_err(|error| match error {
        gix_store::Error::SchemaPin(_) => anyhow::Error::new(error).context(cli_context(
            ExitClass::Schema,
            "schema fixed-point validation failed",
        )),
        error => anyhow::Error::new(error),
    })?;
    let mut fields = VObject::new();
    fields.insert("object_format", report.object_format.to_string());
    fields.insert("schema_fixed_point", "valid");
    emit_single(format, fields, || {
        format!(
            "git-store doctor: ok (object format: {}; schema fixed point: valid)",
            report.object_format
        )
    })
}

/// Resolve `spec` to a tree: any revision syntax `rev-parse` accepts
/// (`<oid>`, a ref, `<rev>~1`, `<rev>:<path>`, …) resolved to an object,
/// then peeled down to a tree — a no-op when it already is one.
fn resolve_tree(repo: &gix::Repository, spec: &str) -> Result<ObjectId> {
    let id = repo
        .rev_parse_single(spec)
        .with_context(|| cli_context(ExitClass::NotFound, format!("cannot resolve {spec:?}")))?;
    let tree = id
        .object()
        .with_context(|| cli_context(ExitClass::NotFound, format!("cannot resolve {spec:?}")))?
        .peel_to_kind(gix::objs::Kind::Tree)
        .with_context(|| cli_context(ExitClass::Invalid, format!("{spec:?} is not a tree-ish")))?;
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

/// `compile <kind> [<value>]`: compile a value under a kind's current
/// schema, printing the document tree — a pure operation, advancing no ref.
fn compile(store: &RepoStore<'_>, args: CompileArgs, format: OutputFormat) -> Result<()> {
    let CompileArgs {
        kind,
        value,
        file,
        edit,
        interactive,
    } = args;
    let handle = store.dynamic(segment("kind", &kind)?);
    let value = gather_value(&handle, value, &file, edit, interactive)?;
    emit_tree(format, handle.compile(&value)?)
}

/// Print a bare document-tree hash: `{tree}` under `json`/`ndjson`, the hash
/// alone as text.
fn emit_tree(format: OutputFormat, tree: ObjectId) -> Result<()> {
    let mut fields = VObject::new();
    fields.insert("tree", oid_value(tree));
    emit_single(format, fields, || tree.to_string())
}

/// `put <kind> <name> [<value>]`: compile a value and publish it under
/// `name`, advancing that name's ref. The general form of the pipeline
/// `value encode → document bind → document publish`.
fn put(store: &RepoStore<'_>, args: PutArgs, format: OutputFormat) -> Result<()> {
    let PutArgs {
        kind,
        name,
        value,
        file,
        message,
        edit,
        interactive,
    } = args;
    let name_seg = entity(&name)?;
    let handle = store.dynamic(segment("kind", &kind)?);
    let value = gather_value(&handle, value, &file, edit, interactive)?;
    let tree = handle.compile(&value)?;
    let prepared = match store.inspect_document(DocumentTree::from(tree))? {
        DocumentInspection::Bound(prepared) => prepared,
        other => {
            unreachable!("a just-compiled document is always bound, got {other:?}")
        }
    };
    let message = message.unwrap_or_else(|| format!("store {kind}/{name}"));
    let options = PublishOptions::new(message).with_alias(name_seg);
    let publication = handle.publish_prepared(&prepared, options)?;
    emit_publication(format, &kind, publication)
}

/// Take a value from the positional argument (as JSON), else `-F <file>`,
/// stdin, `$EDITOR`, or — with `interactive` — a prompt walking `handle`'s
/// schema. Shared by [`compile`] and [`put`].
fn gather_value(
    handle: &DynKind<'_, '_>,
    inline: Option<String>,
    file: &Option<PathBuf>,
    edit: bool,
    interactive: bool,
) -> Result<Value> {
    if let Some(inline) = inline {
        if file.is_some() || edit || interactive {
            bail!("a positional value cannot be combined with -F, --edit, or --interactive");
        }
        return facet_json::from_str::<Value>(&inline).map_err(|error| {
            cli_error(ExitClass::Invalid, format!("invalid JSON value: {error}"))
        });
    }
    if interactive {
        return interactive::value_for_kind(handle);
    }
    let json = gathered_json(handle, file, edit)?;
    facet_json::from_str(&json)
        .map_err(|e| cli_error(ExitClass::Invalid, format!("invalid JSON: {e}")))
}

/// `cat <tree-ish>`: decode any tree of the `{value/, schema/}` shape
/// directly, whatever it was reached through — content-addressed, like `git
/// cat-file`.
fn cat(
    repo: &gix::Repository,
    store: &RepoStore<'_>,
    tree_ish: &str,
    legacy_leaves: bool,
    format: OutputFormat,
) -> Result<()> {
    let tree = resolve_tree(repo, tree_ish)?;
    // `decode` reads entirely out of `tree`'s own embedded schema and does
    // not consult any kind or schema ref.
    let value = if legacy_leaves {
        store.decode_legacy(tree)
    } else {
        store.decode(tree)
    }
    .with_context(|| cli_context(ExitClass::Schema, format!("{tree_ish} is not a document")))?;
    emit_value(format, value)
}

/// `get <kind> <name>`: resolve a name, then decode it — the name-addressed
/// mirror of `put`.
fn get(
    repo: &gix::Repository,
    store: &RepoStore<'_>,
    kind: &str,
    name: &str,
    format: OutputFormat,
) -> Result<()> {
    let (name, rev) = split_name_rev(name);
    let handle = store.dynamic(segment("kind", kind)?);
    let name_seg = entity(name)?;
    let state = match rev {
        Some(rev) => {
            let oid = resolve_at(repo, &handle, &name_seg, rev)?;
            // Only read a commit that is actually a version of this entity,
            // so a stray oid can't return an unrelated value.
            if !handle.history(&name_seg)?.contains(&oid) {
                bail!("{rev} is not a version of {kind}/{name}");
            }
            handle.read(oid)?
        }
        None => handle.read(name_seg)?,
    };
    let value = match state {
        EntityState::Present(entry) => entry.value,
        EntityState::Deleted(_) => bail!("entity {kind}/{name} is deleted"),
        EntityState::Absent => {
            return Err(cli_error(
                ExitClass::NotFound,
                format!("no entity {kind}/{name}"),
            ));
        }
    };
    emit_value(format, value)
}

/// Render a decoded document value: `{value}` under `json`/`ndjson`, its
/// pretty JSON alone as text. Shared by `cat` and `get`.
fn emit_value(format: OutputFormat, value: Value) -> Result<()> {
    let mut fields = VObject::new();
    fields.insert("value", value.clone());
    emit_single(format, fields, || to_json(&value).unwrap_or_default())
}

/// `check <tree-ish> <schema>`: validate a value tree against a published
/// schema, without decoding it. Silent on success in text mode, matching
/// `git`'s own validating subcommands; `json`/`ndjson` emit a confirming
/// record so a script never has to infer success from silence.
fn check(
    repo: &gix::Repository,
    store: &RepoStore<'_>,
    tree_ish: &str,
    schema: &str,
    format: OutputFormat,
) -> Result<()> {
    let tree = resolve_tree(repo, tree_ish)?;
    let doc = store
        .dynamic(segment("schema", schema)?)
        .schema()
        .get()?
        .with_context(|| {
            cli_context(
                ExitClass::NotFound,
                format!("no schema published for {schema:?}"),
            )
        })?;
    validate_with_schema(&tree, &doc, store.objects()).with_context(|| {
        cli_context(
            ExitClass::Schema,
            format!("{tree_ish} does not conform to {schema:?}"),
        )
    })?;
    if format.machine() {
        let mut fields = VObject::new();
        fields.insert("tree", oid_value(tree));
        fields.insert("schema", schema.to_owned());
        fields.insert("valid", true);
        emit_fields(format, fields)?;
    }
    Ok(())
}

/// `rm <kind> <name>`: publish a tombstone over a name.
fn rm(store: &RepoStore<'_>, kind: &str, name: &str, format: OutputFormat) -> Result<()> {
    let name_seg = entity(name)?;
    let handle = store.dynamic(segment("kind", kind)?);
    let result = handle.delete_name(&name_seg)?;
    emit_deletion(format, kind, name, result)
}

/// Render a [`DeleteResult`], shared by `rm` and `entity delete`.
fn emit_deletion(
    format: OutputFormat,
    kind: &str,
    target: &str,
    result: DeleteResult,
) -> Result<()> {
    let (status, commit) = match result {
        DeleteResult::Deleted(entry) => ("deleted", Some(entry.commit)),
        DeleteResult::AlreadyDeleted(entry) => ("already_deleted", Some(entry.commit)),
        DeleteResult::Absent => {
            return Err(cli_error(
                ExitClass::NotFound,
                format!("no entity {kind}/{target}"),
            ));
        }
    };
    let mut fields = VObject::new();
    fields.insert("status", status);
    fields.insert("code", status);
    fields.insert("kind", kind.to_owned());
    fields.insert("id", target.to_owned());
    if let Some(commit) = commit {
        fields.insert("commit", oid_value(commit));
    }
    let text = match status {
        "deleted" => format!("deleted {kind}/{target}"),
        _ => format!("already deleted {kind}/{target}"),
    };
    emit_single(format, fields, || text)
}

/// `schema show [<kind>]`: field layout, human-readable, of one kind or every
/// published kind.
fn schema_show(store: &RepoStore<'_>, kind: Option<&str>, format: OutputFormat) -> Result<()> {
    let kinds = match kind {
        Some(kind) => vec![segment("kind", kind)?],
        None => store.kinds()?,
    };
    let mut items = Vec::new();
    for seg in kinds {
        let handle = store.dynamic(seg);
        let Some(doc) = handle.schema().get()? else {
            continue;
        };
        let schema_json = facet_json::to_string(&doc)
            .map_err(|e| anyhow::anyhow!("encoding schema JSON: {e}"))?;
        let schema: Value = facet_json::from_str(&schema_json)
            .map_err(|e| anyhow::anyhow!("encoding schema JSON: {e}"))?;
        let mut fields = VObject::new();
        fields.insert("kind", handle.name().as_str().to_owned());
        fields.insert("schema", schema);
        items.push(ListItem {
            text: type_layout(handle.name().as_str(), &doc),
            fields,
        });
    }
    emit_list(format, "kinds", items)
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
    let id = repo.rev_parse_single(spec.as_str()).with_context(|| {
        cli_context(
            ExitClass::NotFound,
            format!("cannot resolve revision {rev:?}"),
        )
    })?;
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

/// Parse a hand-authored schema JSON document into a [`Schema`]. The
/// publication boundary replaces its embedded kind with the selected CLI
/// kind, so the input may use any valid kind name or the generated default.
fn schema_doc_from_json(json: &str) -> Result<Schema> {
    let value: Value = facet_json::from_str(json)
        .map_err(|e| cli_error(ExitClass::Invalid, format!("invalid schema JSON: {e}")))?;
    from_value(value)
        .map_err(|e| cli_error(ExitClass::Invalid, format!("invalid schema JSON: {e}")))
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
        None => Err(cli_error(
            ExitClass::NotFound,
            format!("no schema published for kind {:?}", kind.name().as_str()),
        )),
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

/// `list`/`ls` with no kind: every kind with a published schema.
fn list_kinds(store: &RepoStore<'_>, format: OutputFormat) -> Result<()> {
    let items = store
        .kinds()?
        .into_iter()
        .map(|kind| {
            let mut fields = VObject::new();
            fields.insert("kind", kind.as_str().to_owned());
            ListItem {
                text: kind.to_string(),
                fields,
            }
        })
        .collect();
    emit_list(format, "kinds", items)
}

/// `list`/`ls <kind>`: the live entity names published under `kind`.
fn list_entities(store: &RepoStore<'_>, kind: &str, format: OutputFormat) -> Result<()> {
    let items = store
        .dynamic(segment("kind", kind)?)
        .list()?
        .into_iter()
        .map(|name| {
            let mut fields = VObject::new();
            fields.insert("name", name.to_string());
            ListItem {
                text: name.to_string(),
                fields,
            }
        })
        .collect();
    emit_list(format, "entities", items)
}

/// `log <kind> <name>`: `name`'s publication history, newest first.
fn log(
    repo: &gix::Repository,
    store: &RepoStore<'_>,
    kind: &str,
    name: &str,
    format: OutputFormat,
) -> Result<()> {
    let name_seg = entity(name)?;
    let commits = store.dynamic(segment("kind", kind)?).history(&name_seg)?;
    log_commits(repo, commits, format)
}

/// Render a first-parent commit walk as a list of `<oid> <iso-date>` rows,
/// shared by [`log`] and `schema log`.
fn log_commits(repo: &gix::Repository, commits: Vec<ObjectId>, format: OutputFormat) -> Result<()> {
    let mut items = Vec::with_capacity(commits.len());
    for id in commits {
        let commit = repo.find_commit(id)?;
        let when = commit.time()?.format(gix::date::time::format::ISO8601)?;
        let mut fields = VObject::new();
        fields.insert("commit", oid_value(id));
        fields.insert("date", when.clone());
        items.push(ListItem {
            text: format!("{id} {when}"),
            fields,
        });
    }
    emit_list(format, "commits", items)
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

/// A kind's top-level field layout, resolving the root through `defs`, as
/// `print_type` used to print directly.
fn type_layout(kind: &str, doc: &Schema) -> String {
    let mut out = format!("{kind}");
    match resolve(&doc.root, doc) {
        Node::Struct(fields) => {
            for (name, field) in fields {
                let default = if field.has_default { " = default" } else { "" };
                out.push_str(&format!("\n  {name}: {}{default}", label(&field.node)));
            }
        }
        other => out.push_str(&format!("\n  {}", label(other))),
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_reports_fixed_point_failure_with_context() {
        let injected = gix_store::Error::SchemaPin(facet_git_tree::SchemaPinError::Unpinned(
            gix::ObjectId::null(gix::hash::Kind::Sha1),
        ));
        let error = doctor_with(|| Err(injected), OutputFormat::Text)
            .expect_err("the injected fixed-point failure must reach the caller");
        let message = format!("{error:#}");
        assert!(message.contains("schema fixed-point validation failed"));
        assert!(message.contains("carries no schema-schema pin"));
    }

    #[test]
    fn doctor_rejects_a_configured_sha256_repository() {
        let error = doctor_with(
            || {
                gix_store::check_repository(
                    gix::hash::Kind::Sha1,
                    true,
                    &facet_git_tree::ObjectStore::default(),
                )
            },
            OutputFormat::Text,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("unsupported Git object format"));
        assert!(message.contains("sha256"));
    }

    #[test]
    fn exit_classification_does_not_parse_human_text() {
        let untyped = anyhow::anyhow!("schema value is invalid");
        assert_eq!(error_exit_code(&untyped), 1);

        let typed = cli_error(ExitClass::Invalid, "schema value is invalid");
        assert_eq!(error_exit_code(&typed), 2);
    }
}
