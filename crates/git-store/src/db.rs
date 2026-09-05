//! `git store db`: the Dolt command surface over the [`gix_database`]
//! versioned database.
//!
//! The command names and shapes mirror Dolt's CLI — `init`, `status`, `ls`,
//! `add`, `commit`, `table {create,rm,mv,import,export}`, `schema show`,
//! `log`, `show`, `branch`, `checkout`, `merge`, `reset`, `diff`, `tag` —
//! backed entirely by ordinary Git refs, commits, and trees. The SQL
//! commands are the one deliberate omission: schema and query land in a
//! later plan step, so the key/value extensions (`get`, `put`, `rm`) stand
//! in for them until then.

use std::io::Read as _;
use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;
use facet_json::from_str;
use facet_value::{VArray, VObject, Value};
use gix::ObjectId;
use gix::objs::{CommitRef, Find as _};
use gix_database::{ChangeKind, Database, TableStatus};

use crate::{
    ExitClass, ListItem, OutputFormat, cli_error, emit_list, emit_single, oid_value, string_array,
};

/// What a table-level change is called in output — Dolt's wording.
fn change_verb(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "new table",
        ChangeKind::Removed => "deleted table",
        ChangeKind::Modified => "modified",
    }
}

/// The row-level diff symbol a change renders as.
fn row_symbol(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "+",
        ChangeKind::Removed => "-",
        ChangeKind::Modified => "~",
    }
}

/// The stable failure class of a database error, for the process exit code.
pub(crate) fn db_exit_class(error: &gix_database::Error) -> ExitClass {
    use gix_database::Error as E;
    match error {
        E::TableNotFound(_) | E::BranchNotFound(_) | E::TagNotFound(_) | E::KeyNotFound(_) => {
            ExitClass::NotFound
        }
        // A lost compare-and-swap race is always retryable, whatever wrote.
        E::Write(gix_database::WriteStateError::Conflict(_))
        | E::Merge(gix_database::MergeError::Write(gix_database::WriteStateError::Conflict(_)))
        | E::Merge(gix_database::MergeError::Conflicts { .. }) => ExitClass::Cas,
        E::Merge(gix_database::MergeError::BranchNotFound(_)) => ExitClass::NotFound,
        E::Prolly(git_prolly::Error::KeyNotFound(_)) => ExitClass::NotFound,
        // Malformed keys are caller mistakes, not storage failures.
        E::Prolly(git_prolly::Error::Key(_)) | E::Prolly(git_prolly::Error::DuplicateKey(_)) => {
            ExitClass::Invalid
        }
        E::TableExists(_)
        | E::BranchExists(_)
        | E::BranchNotMerged(_)
        | E::TagExists(_)
        | E::CurrentBranch(_)
        | E::InvalidTable(_)
        | E::Checkout(_)
        | E::BranchName(_)
        | E::EmptyDatabase
        | E::Commit(gix_database::CommitError::Detached) => ExitClass::Invalid,
        E::Snapshot(gix_database::SnapshotError::ConfigMismatch { .. })
        | E::State(gix_database::ReadStateError::Snapshot(
            gix_database::SnapshotError::ConfigMismatch { .. },
        )) => ExitClass::Schema,
        E::Snapshot(
            gix_database::SnapshotError::MetadataMissing { .. }
            | gix_database::SnapshotError::UnknownFormat(_),
        )
        | E::State(gix_database::ReadStateError::Snapshot(
            gix_database::SnapshotError::MetadataMissing { .. }
            | gix_database::SnapshotError::UnknownFormat(_),
        )) => ExitClass::Invalid,
        _ => ExitClass::Other,
    }
}

/// Wrap a database error with its stable exit class.
fn db_error<E: Into<gix_database::Error>>(error: E) -> anyhow::Error {
    let error: gix_database::Error = error.into();
    let class = db_exit_class(&error);
    cli_error(class, error.to_string())
}

#[derive(Subcommand)]
pub(crate) enum DbCommand {
    /// Initialize an empty database, pointing `refs/db/HEAD` at an unborn
    /// `main`; idempotent.
    Init,
    /// Show the branch and the staged and unstaged table changes.
    Status,
    /// List every table in the working set; `-v` adds row counts.
    Ls {
        /// Show row counts alongside table names.
        #[arg(short = 'v', long)]
        verbose: bool,
    },
    /// Stage tables into the index. `-A`/`.` stages every changed table.
    Add {
        /// The tables to stage.
        tables: Vec<String>,
        /// Stage every table with unstaged changes.
        #[arg(short = 'A', long)]
        all: bool,
    },
    /// Commit the index snapshot onto the current branch.
    Commit {
        /// The commit message.
        #[arg(short = 'm', long = "message", value_name = "MSG")]
        message: String,
        /// Stage every unstaged change first, like `--all` of `add`.
        #[arg(short = 'a', long)]
        all: bool,
    },
    /// Create, drop, rename, import, or list tables.
    Table {
        #[command(subcommand)]
        command: DbTableCommand,
    },
    /// Show the schema a table carries.
    Schema {
        #[command(subcommand)]
        command: DbSchemaCommand,
    },
    /// Show the current branch's database commits, newest first.
    Log {
        /// Limit the walk to the newest `<N>` commits.
        #[arg(short = 'n', long = "number", value_name = "N")]
        number: Option<usize>,
    },
    /// Show a commit's message and the table data it holds.
    Show { commit: String },
    /// Create, delete, rename, or list branches.
    Branch {
        /// The branch to create, delete, or list around.
        name: Option<String>,
        /// Delete `<name>`; refused for the current branch.
        #[arg(short = 'd', requires = "name")]
        delete: bool,
        /// Delete `<name>`, ignoring its merge state.
        #[arg(short = 'D', requires = "name", conflicts_with = "delete")]
        delete_force: bool,
        /// Rename: `-m <new>` renames the current branch, `-m <old> <new>`
        /// renames `<old>`.
        #[arg(
            short = 'm',
            long = "move",
            num_args = 1..=2,
            value_names = ["FROM", "TO"],
            conflicts_with = "name"
        )]
        rename: Vec<String>,
    },
    /// Switch to a branch, or — when `<target>` is not a branch — discard
    /// unstaged changes to a table, restoring the staged version.
    Checkout {
        /// The branch to switch to, or the table to restore.
        target: String,
        /// Create `<target>` at the current tip and switch to it.
        #[arg(short = 'b')]
        new_branch: bool,
        /// Check out even when the working snapshot differs from the current
        /// tip.
        #[arg(long)]
        force: bool,
    },
    /// Merge a branch into the current one: row-level three-way, refused
    /// with explicit conflicts when both sides changed the same key.
    Merge { branch: String },
    /// Unstage tables back to the branch tip; `--hard` also discards
    /// unstaged changes.
    Reset {
        /// The tables to unstage; every staged table when empty.
        #[arg(conflicts_with = "hard")]
        tables: Vec<String>,
        /// Move the working and index snapshots back to the branch tip,
        /// discarding everything.
        #[arg(long)]
        hard: bool,
    },
    /// Show row-level changes: working vs index, tip vs index with
    /// `--staged`, and commits with `<from> [<to>]`.
    Diff {
        /// Compare the branch tip against the index instead.
        #[arg(long, visible_alias = "cached", conflicts_with_all = ["from", "to"])]
        staged: bool,
        /// The older commit to diff from.
        #[arg(value_name = "FROM")]
        from: Option<String>,
        /// The newer commit to diff to; defaults to the working set.
        #[arg(value_name = "TO", requires = "from")]
        to: Option<String>,
    },
    /// Create, delete, or list lightweight tags on database commits.
    Tag {
        /// The tag to create or delete.
        name: Option<String>,
        /// The commit to tag; defaults to the current tip.
        #[arg(conflicts_with = "delete")]
        commit: Option<String>,
        /// Delete `<name>`.
        #[arg(short = 'd', requires = "name")]
        delete: bool,
    },
    /// Read a key's value from a table as JSON. A key/value stand-in for
    /// `sql` until queries land.
    Get { table: String, key: String },
    /// Set a key's value in a table, in the working snapshot. A key/value
    /// stand-in for `sql` until queries land.
    Put {
        table: String,
        key: String,
        /// An inline JSON value.
        value: String,
    },
    /// Remove a key from a table, in the working snapshot. A key/value
    /// stand-in for `sql` until queries land.
    Rm { table: String, key: String },
}

#[derive(Subcommand)]
pub(crate) enum DbTableCommand {
    /// Create a table in the working snapshot.
    ///
    /// Tables are key/value for now: schemas are a later plan step, so no
    /// column list is taken yet.
    Create { name: String },
    /// Drop a table from the working snapshot.
    #[command(visible_alias = "drop")]
    Rm { name: String },
    /// Rename a table in the working snapshot, keeping its rows.
    Mv { from: String, to: String },
    /// Import rows from a JSON `{key: value}` document into a table.
    ///
    /// With `-c`, create the table first (refused if it exists); with `-r`,
    /// replace its contents. The document comes from `-F <file>` or stdin.
    Import {
        table: String,
        /// Create the table first.
        #[arg(short = 'c')]
        create: bool,
        /// Truncate the table before importing.
        #[arg(short = 'r', conflicts_with = "create")]
        replace: bool,
        /// JSON file to import; stdin when omitted.
        #[arg(short = 'F', long = "file", value_name = "FILE")]
        file: Option<PathBuf>,
    },
    /// Export a table's rows as a JSON `{key: value}` document.
    Export {
        table: String,
        /// JSON file to write; stdout when omitted.
        #[arg(short = 'F', long = "file", value_name = "FILE")]
        file: Option<PathBuf>,
    },
    /// List every table in the working snapshot.
    List,
}

#[derive(Subcommand)]
pub(crate) enum DbSchemaCommand {
    /// Show what schema a table carries — every table is key/value until
    /// schemas land, and this reports the pinned tree configuration.
    Show {
        /// One table, or every table when omitted.
        table: Option<String>,
    },
}

/// Dispatch one `db` subcommand.
pub(crate) fn run(repo: &gix::Repository, command: DbCommand, output: OutputFormat) -> Result<()> {
    let db = Database::open(repo);
    match command {
        DbCommand::Init => {
            db.init().map_err(db_error)?;
            emit_single(output, db_fields_init(), || {
                "initialized database (branch main)".to_owned()
            })
        }
        DbCommand::Status => status(&db, output),
        DbCommand::Ls { verbose } => ls(&db, verbose, output),
        DbCommand::Add { tables, all } => {
            if all {
                stage_all(&db)?;
            }
            stage_named(&db, &tables)?;
            emit_single(output, VObject::new(), || "staged".to_owned())
        }
        DbCommand::Commit { message, all } => {
            if all {
                stage_all(&db)?;
            }
            let commit = db.commit(&message).map_err(db_error)?;
            let mut fields = VObject::new();
            fields.insert("commit", oid_value(commit));
            emit_single(output, fields, || commit.to_string())
        }
        DbCommand::Table { command } => table(&db, command, output),
        DbCommand::Schema { command } => schema(&db, command, output),
        DbCommand::Log { number } => log(&db, number, output),
        DbCommand::Show { commit } => show(repo, &db, &commit, output),
        DbCommand::Branch {
            name,
            delete,
            delete_force,
            rename,
        } => branch(&db, name, delete || delete_force, rename, output),
        DbCommand::Checkout {
            target,
            new_branch,
            force,
        } => checkout(&db, &target, new_branch, force, output),
        DbCommand::Merge { branch } => {
            let commit = match db.merge(&branch) {
                Ok(commit) => commit,
                Err(gix_database::MergeError::Conflicts { conflicts }) => {
                    let mut message = format!(
                        "merge refused: {} conflicting row(s); nothing was written",
                        conflicts.len()
                    );
                    for entry in &conflicts {
                        message.push('\n');
                        message.push_str(&entry.describe());
                    }
                    return Err(cli_error(ExitClass::Cas, message));
                }
                Err(error) => return Err(db_error(error)),
            };
            let mut fields = VObject::new();
            fields.insert("branch", branch.clone());
            fields.insert("commit", oid_value(commit));
            emit_single(output, fields, || format!("merged {branch} as {commit}"))
        }
        DbCommand::Reset { tables, hard } => reset(&db, tables, hard, output),
        DbCommand::Diff { staged, from, to } => diff(repo, &db, staged, from, to, output),
        DbCommand::Tag {
            name,
            commit,
            delete,
        } => tag(repo, &db, name, commit, delete, output),
        DbCommand::Get { table, key } => {
            let value = db.get(&table, key.as_bytes()).map_err(db_error)?;
            let mut fields = VObject::new();
            fields.insert("table", table.clone());
            fields.insert("key", key.clone());
            fields.insert(
                "value",
                value
                    .as_ref()
                    .map(json_string)
                    .unwrap_or_else(|| "null".to_owned()),
            );
            fields.insert("found", value.is_some());
            emit_single(output, fields, || match &value {
                Some(value) => format!("{key} = {}", json_string(value)),
                None => format!("{key} is not set"),
            })
        }
        DbCommand::Put { table, key, value } => {
            let value: Value = from_str(&value).map_err(|error| {
                cli_error(ExitClass::Invalid, format!("invalid JSON value: {error}"))
            })?;
            let root = db.put(&table, key.as_bytes(), &value).map_err(db_error)?;
            let mut fields = VObject::new();
            fields.insert("table", table.clone());
            fields.insert("key", key.clone());
            fields.insert("root", oid_value(root));
            emit_single(output, fields, || format!("{table}/{key} -> {root}"))
        }
        DbCommand::Rm { table, key } => {
            let root = db.remove(&table, key.as_bytes()).map_err(db_error)?;
            let mut fields = VObject::new();
            fields.insert("table", table.clone());
            fields.insert("key", key.clone());
            fields.insert("root", oid_value(root));
            emit_single(output, fields, || format!("{table}/{key} removed"))
        }
    }
}

fn db_fields_init() -> VObject {
    let mut fields = VObject::new();
    fields.insert("head", gix_database::HEAD_REF);
    fields.insert("branch", gix_database::DEFAULT_BRANCH);
    fields
}

/// `status`: the branch, then staged and unstaged tables, Dolt-style.
fn status(db: &Database, output: OutputFormat) -> Result<()> {
    let state = db.status().map_err(db_error)?;
    let branch = branch_name(db)?;
    match output {
        OutputFormat::Text => {
            println!("On branch {branch}");
            if state.staged.is_empty() && state.unstaged.is_empty() {
                println!("nothing to commit, working tree clean");
                return Ok(());
            }
            if !state.staged.is_empty() {
                println!("Changes to be committed:");
                for item in table_lines(&state.staged) {
                    println!("  {item}");
                }
            }
            if !state.unstaged.is_empty() {
                println!("Changes not staged for commit:");
                for item in table_lines(&state.unstaged) {
                    println!("  {item}");
                }
            }
            Ok(())
        }
        _ => {
            let mut all = status_items(&state.staged, "staged");
            all.extend(status_items(&state.unstaged, "unstaged"));
            emit_list(output, "changes", all)
        }
    }
}

/// The checked-out branch's short name, for headers.
fn branch_name(db: &Database) -> Result<String> {
    match db.head().map_err(db_error)? {
        gix_database::Head::Branch { branch, .. } | gix_database::Head::Unborn { branch } => {
            Ok(branch
                .as_str()
                .strip_prefix(gix_database::HEADS_PREFIX)
                .and_then(|rest| rest.strip_prefix('/'))
                .unwrap_or(branch.as_str())
                .to_owned())
        }
        gix_database::Head::Detached { commit } => Ok(format!("(detached at {commit})")),
        gix_database::Head::Missing => Err(db_error(gix_database::Error::State(
            gix_database::ReadStateError::NoDatabase,
        ))),
    }
}

/// One line per changed table, for status text.
fn table_lines(statuses: &[TableStatus]) -> Vec<String> {
    statuses
        .iter()
        .map(|status| format!("{}: {}", change_verb(status.kind), status.table))
        .collect()
}

/// `ls [-v]`: every table in the working set.
fn ls(db: &Database, verbose: bool, output: OutputFormat) -> Result<()> {
    let snapshot = db.working_state().map_err(db_error)?.snapshot;
    let mut items = Vec::new();
    for (name, root) in snapshot.tables() {
        let mut fields = VObject::new();
        fields.insert("table", name.as_str().to_owned());
        fields.insert("root", oid_value(*root));
        let rows = if verbose {
            let rows = db.scan(name.as_str()).map_err(db_error)?.count();
            fields.insert("rows", i64::try_from(rows).unwrap_or(i64::MAX));
            Some(rows)
        } else {
            None
        };
        items.push(ListItem {
            text: match rows {
                Some(rows) => format!("{name} ({rows} rows)"),
                None => name.as_str().to_owned(),
            },
            fields,
        });
    }
    emit_list(output, "tables", items)
}

/// Stage every table with unstaged changes; returns how many were staged.
fn stage_all(db: &Database) -> Result<usize> {
    let unstaged = db.status().map_err(db_error)?.unstaged;
    for status in &unstaged {
        db.stage(status.table.as_str()).map_err(db_error)?;
    }
    Ok(unstaged.len())
}

/// Stage the named tables.
fn stage_named(db: &Database, names: &[String]) -> Result<()> {
    for name in names {
        if name == "." {
            stage_all(db)?;
            continue;
        }
        db.stage(name).map_err(db_error)?;
    }
    Ok(())
}

/// `table {create,rm,mv,import,export,list}`.
fn table(db: &Database, command: DbTableCommand, output: OutputFormat) -> Result<()> {
    match command {
        DbTableCommand::Create { name } => {
            let root = db.create_table(&name).map_err(db_error)?;
            let mut fields = VObject::new();
            fields.insert("table", name.clone());
            fields.insert("root", oid_value(root));
            emit_single(output, fields, || format!("table {name} created"))
        }
        DbTableCommand::Rm { name } => {
            let root = db.drop_table(&name).map_err(db_error)?;
            let mut fields = VObject::new();
            fields.insert("table", name.clone());
            fields.insert("root", oid_value(root));
            emit_single(output, fields, || format!("table {name} dropped"))
        }
        DbTableCommand::Mv { from, to } => {
            let root = db.move_table(&from, &to).map_err(db_error)?;
            let mut fields = VObject::new();
            fields.insert("from", from.clone());
            fields.insert("to", to.clone());
            fields.insert("root", oid_value(root));
            emit_single(output, fields, || format!("table {from} renamed to {to}"))
        }
        DbTableCommand::Import {
            table,
            create,
            replace,
            file,
        } => {
            let rows = import(db, &table, create, replace, file.as_ref())?;
            let mut fields = VObject::new();
            fields.insert("table", table.clone());
            fields.insert("rows", i64::try_from(rows).unwrap_or(i64::MAX));
            emit_single(output, fields, || {
                format!("imported {rows} row(s) into {table}")
            })
        }
        DbTableCommand::Export { table, file } => {
            let mut object = VObject::new();
            for row in db.scan(&table).map_err(db_error)? {
                let (key, value) = row.map_err(db_error)?;
                object.insert(String::from_utf8_lossy(&key).into_owned().as_str(), value);
            }
            let value: Value = object.into();
            let json = facet_json::to_string(&value)
                .map_err(|error| anyhow::anyhow!("encoding JSON: {error}"))?;
            match file {
                Some(path) => {
                    std::fs::write(&path, json + "\n")
                        .map_err(|error| anyhow::anyhow!("writing {}: {error}", path.display()))?;
                    emit_single(output, VObject::new(), || {
                        format!("exported {table} to {}", path.display())
                    })
                }
                None if output.machine() => {
                    let mut fields = VObject::new();
                    fields.insert("table", table.clone());
                    fields.insert("rows", value);
                    emit_single(output, fields, || json.clone())
                }
                None => {
                    println!("{json}");
                    Ok(())
                }
            }
        }
        DbTableCommand::List => ls(db, false, output),
    }
}

/// Parse a JSON `{key: value}` document into ordered key/value pairs.
fn rows_from_json(json: &str) -> Result<Vec<(String, Value)>> {
    let value: Value = from_str(json)
        .map_err(|error| cli_error(ExitClass::Invalid, format!("invalid JSON: {error}")))?;
    let object = value.as_object().ok_or_else(|| {
        cli_error(
            ExitClass::Invalid,
            "expected a JSON object of {\"key\": value}",
        )
    })?;
    let mut rows = Vec::new();
    for (key, value) in object {
        rows.push((key.as_str().to_owned(), value.clone()));
    }
    Ok(rows)
}

/// `table import`: create/replace as asked, then put every row.
fn import(
    db: &Database,
    table: &str,
    create: bool,
    replace: bool,
    file: Option<&PathBuf>,
) -> Result<usize> {
    let json = read_source(file)?;
    let rows = rows_from_json(&json)?;
    db.put_rows(
        table,
        rows.iter().map(|(k, v)| (k.as_str(), v.clone())),
        create,
        replace,
    )
    .map_err(db_error)?;
    Ok(rows.len())
}

/// Read `path`, or all of stdin.
fn read_source(file: Option<&PathBuf>) -> Result<String> {
    match file {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|error| anyhow::anyhow!("reading {}: {error}", path.display())),
        None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|error| anyhow::anyhow!("reading stdin: {error}"))?;
            Ok(buf)
        }
    }
}

/// `schema show [<table>]`: every table is key/value until schemas land.
fn schema(db: &Database, command: DbSchemaCommand, output: OutputFormat) -> Result<()> {
    let DbSchemaCommand::Show { table } = command;
    let snapshot = db.working_state().map_err(db_error)?.snapshot;
    let config = gix_database::config_summary(snapshot.config());
    if let Some(table) = table {
        let Some(root) = snapshot.table(&table) else {
            return Err(db_error(gix_database::Error::TableNotFound(
                gix_database::TableName::new(&table).map_err(db_error)?,
            )));
        };
        let mut fields = VObject::new();
        fields.insert("table", table.clone());
        fields.insert("root", oid_value(root));
        fields.insert("schema", "key/value");
        fields.insert("config", config.clone());
        emit_single(output, fields, || {
            format!("{table}: no schema yet (key/value), pinned config {config}")
        })
    } else {
        let items = snapshot
            .tables()
            .iter()
            .map(|(name, root)| {
                let mut fields = VObject::new();
                fields.insert("table", name.as_str().to_owned());
                fields.insert("root", oid_value(*root));
                fields.insert("schema", "key/value");
                fields.insert("config", config.clone());
                ListItem {
                    text: format!("{}: no schema yet (key/value)", name.as_str()),
                    fields,
                }
            })
            .collect();
        emit_list(output, "tables", items)
    }
}

/// `log [-n N]`: the branch's commits, newest first.
fn log(db: &Database, number: Option<usize>, output: OutputFormat) -> Result<()> {
    let mut entries = db.log().map_err(db_error)?;
    if let Some(number) = number {
        entries.truncate(number);
    }
    let items: Vec<ListItem> = entries
        .iter()
        .map(|entry| ListItem {
            fields: {
                let mut fields = VObject::new();
                fields.insert("commit", oid_value(entry.commit));
                fields.insert("tree", oid_value(entry.tree));
                fields.insert(
                    "parents",
                    string_array(entry.parents.iter().map(ToString::to_string)),
                );
                fields.insert("message", entry.message.clone());
                fields
            },
            text: format!("{} {}", entry.commit, entry.message),
        })
        .collect();
    emit_list(output, "commits", items)
}

/// Resolve a commit-ish argument to a commit object id.
fn resolve_commit(repo: &gix::Repository, spec: &str) -> Result<ObjectId> {
    let id = repo
        .rev_parse_single(spec)
        .map_err(|_| cli_error(ExitClass::NotFound, format!("cannot resolve {spec:?}")))?;
    let object = id
        .object()
        .map_err(|_| cli_error(ExitClass::NotFound, format!("cannot resolve {spec:?}")))?;
    if object.kind != gix::objs::Kind::Commit {
        return Err(cli_error(
            ExitClass::Invalid,
            format!("{spec:?} is not a commit"),
        ));
    }
    Ok(id.detach())
}

/// `show <commit>`: the commit's message and the table data it holds.
fn show(repo: &gix::Repository, db: &Database, commit: &str, output: OutputFormat) -> Result<()> {
    let id = resolve_commit(repo, commit)?;
    let mut buf = Vec::new();
    let data = repo
        .try_find(&id, &mut buf)
        .map_err(|error| anyhow::anyhow!("reading {id}: {error}"))?
        .ok_or_else(|| cli_error(ExitClass::NotFound, format!("object {id} not found")))?;
    let parsed = CommitRef::from_bytes(data.data, data.object_hash)
        .map_err(|error| anyhow::anyhow!("parsing commit {id}: {error}"))?;
    let message = parsed.message().summary().to_string();
    let snapshot = db.snapshot_at(id).map_err(db_error)?;
    let mut tables = Vec::new();
    for (name, root) in snapshot.tables() {
        tables.push((name.as_str().to_owned(), root_rows(db, *root)?));
    }
    if output.machine() {
        let mut fields = VObject::new();
        fields.insert("commit", oid_value(id));
        fields.insert("message", message);
        let mut list = VArray::new();
        for (name, rows) in &tables {
            let mut item = VObject::new();
            item.insert("table", name.clone());
            let mut rows_field = VArray::new();
            for (key, value) in rows {
                let mut row = VObject::new();
                row.insert("key", key.clone());
                row.insert("value", value.clone());
                rows_field.push(Value::from(row));
            }
            item.insert("rows", rows_field);
            list.push(Value::from(item));
        }
        fields.insert("tables", list);
        emit_single(output, fields, || id.to_string())
    } else {
        println!("commit {id}");
        println!("{message}");
        for (name, rows) in &tables {
            println!();
            println!("  {name} ({} rows)", rows.len());
            for (key, value) in rows {
                println!("    {key} = {}", json_string(value));
            }
        }
        Ok(())
    }
}

/// A table root's rows as `(key, value)` pairs, newest-first by key.
fn root_rows(db: &Database, root: ObjectId) -> Result<Vec<(String, Value)>> {
    let mut rows = Vec::new();
    for row in db.store().iter(root).map_err(db_error)? {
        let (key, value) = row.map_err(db_error)?;
        rows.push((String::from_utf8_lossy(&key).into_owned(), value));
    }
    Ok(rows)
}

/// `branch [name] [-d|-D] [-m [from] to]`, or list branches when bare.
fn branch(
    db: &Database,
    name: Option<String>,
    delete: bool,
    rename: Vec<String>,
    output: OutputFormat,
) -> Result<()> {
    if delete {
        let Some(name) = name else {
            return Err(cli_error(
                ExitClass::Invalid,
                "branch deletion needs a name",
            ));
        };
        let tip = db.delete_branch(&name).map_err(db_error)?;
        let mut fields = VObject::new();
        fields.insert("branch", name.clone());
        fields.insert("commit", oid_value(tip));
        emit_single(output, fields, || {
            format!("deleted branch {name} (was {tip})")
        })
    } else if !rename.is_empty() {
        let to = rename.last().expect("non-empty").clone();
        let from = if rename.len() == 2 {
            rename[0].clone()
        } else {
            current_branch(db)?
        };
        let tip = db.rename_branch(&from, &to).map_err(db_error)?;
        let mut fields = VObject::new();
        fields.insert("from", from.clone());
        fields.insert("to", to.clone());
        fields.insert("commit", oid_value(tip));
        emit_single(output, fields, || format!("renamed branch {from} to {to}"))
    } else if let Some(name) = name {
        let commit = db.create_branch(&name).map_err(db_error)?;
        let mut fields = VObject::new();
        fields.insert("branch", name.clone());
        fields.insert("commit", oid_value(commit));
        emit_single(output, fields, || format!("branch {name} at {commit}"))
    } else {
        let current = current_branch(db).ok();
        let items = db
            .list_branches()
            .map_err(db_error)?
            .into_iter()
            .map(|(name, commit)| {
                let marked = current.as_deref() == Some(name.as_str());
                let mut fields = VObject::new();
                fields.insert("branch", name.clone());
                fields.insert("commit", oid_value(commit));
                fields.insert("current", marked);
                ListItem {
                    text: format!("{}{name}", if marked { "* " } else { "  " }),
                    fields,
                }
            })
            .collect();
        emit_list(output, "branches", items)
    }
}

/// The checked-out branch's short name, failing when there is none.
fn current_branch(db: &Database) -> Result<String> {
    match db.head().map_err(db_error)? {
        gix_database::Head::Branch { branch, .. } => Ok(strip_heads(branch.as_str()).to_owned()),
        _ => Err(cli_error(ExitClass::Invalid, "no current branch")),
    }
}

/// Strip the `refs/db/heads/` prefix from a full branch ref.
fn strip_heads(name: &str) -> &str {
    name.strip_prefix(gix_database::HEADS_PREFIX)
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(name)
}

/// `checkout [-b] <target>`: branch when the target names one, table
/// restore when it does not.
fn checkout(
    db: &Database,
    target: &str,
    new_branch: bool,
    force: bool,
    output: OutputFormat,
) -> Result<()> {
    let commit = if new_branch {
        db.create_branch(target).map_err(db_error)?;
        db.checkout(target, force).map_err(db_error)?
    } else {
        let is_branch = db
            .list_branches()
            .map_err(db_error)?
            .iter()
            .any(|(name, _)| name == target);
        if is_branch {
            db.checkout(target, force).map_err(db_error)?
        } else {
            match db.restore_table(target) {
                Ok(root) => {
                    let mut fields = VObject::new();
                    fields.insert("table", target.to_owned());
                    fields.insert("root", oid_value(root));
                    return emit_single(output, fields, || format!("restored {target} at {root}"));
                }
                Err(gix_database::Error::TableNotFound(_)) => {
                    return Err(cli_error(
                        ExitClass::NotFound,
                        format!("{target:?} is neither a branch nor a table"),
                    ));
                }
                Err(error) => return Err(db_error(error)),
            }
        }
    };
    let mut fields = VObject::new();
    fields.insert("branch", target.to_owned());
    fields.insert("commit", oid_value(commit));
    emit_single(output, fields, || {
        format!("checked out {target} at {commit}")
    })
}

/// `reset [--hard] [<table>...]`.
fn reset(db: &Database, tables: Vec<String>, hard: bool, output: OutputFormat) -> Result<()> {
    if hard {
        let commit = db.reset_hard().map_err(db_error)?;
        let mut fields = VObject::new();
        fields.insert("commit", oid_value(commit));
        return emit_single(output, fields, || {
            format!("reset working and index to {commit}")
        });
    }
    let names = if tables.is_empty() {
        db.diff_staged()
            .map_err(db_error)?
            .iter()
            .map(|status| status.table.as_str().to_owned())
            .collect::<Vec<_>>()
    } else {
        tables
    };
    for name in &names {
        db.unstage(name).map_err(db_error)?;
    }
    let mut fields = VObject::new();
    fields.insert("tables", string_array(names.iter().cloned()));
    emit_single(output, fields, || {
        if names.is_empty() {
            "nothing to unstage".to_owned()
        } else {
            format!("unstaged {}", names.join(", "))
        }
    })
}

/// `diff [--staged] [<from> [<to>]]`.
fn diff(
    repo: &gix::Repository,
    db: &Database,
    staged: bool,
    from: Option<String>,
    to: Option<String>,
    output: OutputFormat,
) -> Result<()> {
    let (changes, label) = if staged {
        (db.diff_staged().map_err(db_error)?, "staged")
    } else {
        match (from, to) {
            (None, _) => (db.diff_working().map_err(db_error)?, "unstaged"),
            (Some(from), None) => {
                let base = db
                    .snapshot_at(resolve_commit(repo, &from)?)
                    .map_err(db_error)?;
                let working = db.working_state().map_err(db_error)?.snapshot;
                (
                    db.diff_snapshots(&base, &working).map_err(db_error)?,
                    "diff",
                )
            }
            (Some(from), Some(to)) => {
                let older = db
                    .snapshot_at(resolve_commit(repo, &from)?)
                    .map_err(db_error)?;
                let newer = db
                    .snapshot_at(resolve_commit(repo, &to)?)
                    .map_err(db_error)?;
                (db.diff_snapshots(&older, &newer).map_err(db_error)?, "diff")
            }
        }
    };
    match output {
        OutputFormat::Text => {
            for status in &changes {
                println!("{}: {}", change_verb(status.kind), status.table);
                for row in &status.rows {
                    println!(
                        "  {} {}",
                        row_symbol(row.kind),
                        String::from_utf8_lossy(&row.key)
                    );
                }
            }
            Ok(())
        }
        _ => emit_list(output, "changes", status_items(&changes, label)),
    }
}

/// `tag [name] [commit] [-d name]`.
fn tag(
    repo: &gix::Repository,
    db: &Database,
    name: Option<String>,
    commit: Option<String>,
    delete: bool,
    output: OutputFormat,
) -> Result<()> {
    if delete {
        let Some(name) = name else {
            return Err(cli_error(ExitClass::Invalid, "tag deletion needs a name"));
        };
        let commit = db.delete_tag(&name).map_err(db_error)?;
        let mut fields = VObject::new();
        fields.insert("tag", name.clone());
        fields.insert("commit", oid_value(commit));
        return emit_single(output, fields, || {
            format!("deleted tag {name} (was {commit})")
        });
    }
    if let Some(name) = name {
        let at = match commit {
            Some(spec) => Some(resolve_commit(repo, &spec)?),
            None => None,
        };
        let commit = db.create_tag(&name, at).map_err(db_error)?;
        let mut fields = VObject::new();
        fields.insert("tag", name.clone());
        fields.insert("commit", oid_value(commit));
        return emit_single(output, fields, || format!("tag {name} at {commit}"));
    }
    let items = db
        .list_tags()
        .map_err(db_error)?
        .into_iter()
        .map(|(tag, commit)| {
            let mut fields = VObject::new();
            fields.insert("tag", tag.clone());
            fields.insert("commit", oid_value(commit));
            ListItem {
                text: format!("{tag} {commit}"),
                fields,
            }
        })
        .collect();
    emit_list(output, "tags", items)
}

/// Render one value as compact JSON.
fn json_string(value: &Value) -> String {
    facet_json::to_string(value).unwrap_or_else(|error| format!("<unencodable: {error}>"))
}

/// One table's status as a list item, shared by `status` and `diff`.
fn status_items(statuses: &[TableStatus], stage: &'static str) -> Vec<ListItem> {
    statuses
        .iter()
        .flat_map(|status| status_item(status, stage))
        .collect()
}

fn status_item(status: &TableStatus, stage: &'static str) -> Vec<ListItem> {
    let head = ListItem {
        fields: {
            let mut fields = VObject::new();
            fields.insert("stage", stage);
            fields.insert("table", status.table.as_str().to_owned());
            fields.insert("change", change_verb(status.kind));
            fields.insert("rows", i64::try_from(status.rows.len()).unwrap_or(i64::MAX));
            fields
        },
        text: format!("{}: {}", change_verb(status.kind), status.table),
    };
    let mut items = vec![head];
    for row in &status.rows {
        items.push(ListItem {
            fields: {
                let mut fields = VObject::new();
                fields.insert("stage", stage);
                fields.insert("table", status.table.as_str().to_owned());
                fields.insert("key", String::from_utf8_lossy(&row.key).into_owned());
                fields.insert("change", row.verb());
                fields.insert("old", row.old.map(oid_value).unwrap_or(Value::from("")));
                fields.insert("new", row.new.map(oid_value).unwrap_or(Value::from("")));
                fields
            },
            text: format!("  {} {}", row.verb(), String::from_utf8_lossy(&row.key)),
        });
    }
    items
}
