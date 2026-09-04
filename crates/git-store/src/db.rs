//! `git store db`: porcelain over the [`gix_database`] versioned database.

use anyhow::Result;
use clap::Subcommand;
use facet_json::from_str;
use facet_value::{VObject, Value};
use gix_database::{ChangeKind, Database, TableStatus};

use crate::{
    ExitClass, ListItem, OutputFormat, cli_error, emit_list, emit_single, oid_value, string_array,
};

/// What a table-level change is called in text output.
fn change_verb(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Added => "added",
        ChangeKind::Removed => "removed",
        ChangeKind::Modified => "modified",
    }
}

/// The stable failure class of a database error, for the process exit code.
pub(crate) fn db_exit_class(error: &gix_database::Error) -> ExitClass {
    use gix_database::Error as E;
    match error {
        E::TableNotFound(_) | E::BranchNotFound(_) | E::KeyNotFound(_) => ExitClass::NotFound,
        E::Merge(gix_database::MergeError::Conflicts { .. }) => ExitClass::Cas,
        E::Prolly(git_prolly::Error::KeyNotFound(_)) => ExitClass::NotFound,
        E::TableExists(_)
        | E::BranchExists(_)
        | E::InvalidTable(_)
        | E::Checkout(_)
        | E::BranchName(_) => ExitClass::Invalid,
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
    /// Point `refs/db/HEAD` at an unborn `main` branch; idempotent.
    Init,
    /// Show staged and unstaged table and row changes.
    Status,
    /// Create, drop, or list tables.
    Table {
        #[command(subcommand)]
        command: DbTableCommand,
    },
    /// Read a key's value from a table as JSON.
    Get { table: String, key: String },
    /// Set a key's value in a table, in the working snapshot.
    ///
    /// The table must exist; create it first with `db table create`.
    Put {
        table: String,
        key: String,
        /// An inline JSON value.
        value: String,
    },
    /// Remove a key from a table, in the working snapshot.
    Rm { table: String, key: String },
    /// Stage a table's working root into the index snapshot.
    Add { table: String },
    /// Show row-level changes: index vs working, or branch tip vs index with
    /// `--staged`.
    Diff {
        /// Compare the branch tip against the index instead.
        #[arg(long)]
        staged: bool,
    },
    /// Commit the index snapshot onto the current branch, resetting the
    /// working and index snapshots to the new tip.
    Commit {
        /// The commit message.
        #[arg(short = 'm', long = "message", value_name = "MSG")]
        message: String,
    },
    /// Show the current branch's database commits, newest first.
    Log,
    /// Create a branch at the current tip.
    Branch { name: String },
    /// Switch to a branch, resetting the working and index snapshots.
    Checkout {
        branch: String,
        /// Check out even when the working snapshot differs from the current
        /// tip.
        #[arg(long)]
        force: bool,
    },
    /// Merge a branch into the current one: row-level three-way, refused
    /// with explicit conflicts when both sides changed the same key.
    Merge { branch: String },
}

#[derive(Subcommand)]
pub(crate) enum DbTableCommand {
    /// Create a table in the working snapshot.
    Create { name: String },
    /// Drop a table from the working snapshot.
    Drop { name: String },
    /// List every table in the working snapshot.
    List,
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
        DbCommand::Status => {
            let status = db.status().map_err(db_error)?;
            let mut all = status_items(&status.staged, "staged");
            all.extend(status_items(&status.unstaged, "unstaged"));
            match output {
                OutputFormat::Text => {
                    if all.is_empty() {
                        println!("clean");
                    }
                    for item in all {
                        println!("{}", item.text);
                    }
                    Ok(())
                }
                _ => emit_list(output, "changes", all),
            }
        }
        DbCommand::Table { command } => match command {
            DbTableCommand::Create { name } => {
                let root = db.create_table(&name).map_err(db_error)?;
                let mut fields = VObject::new();
                fields.insert("table", name.clone());
                fields.insert("root", oid_value(root));
                emit_single(output, fields, || format!("table {name} created"))
            }
            DbTableCommand::Drop { name } => {
                let root = db.drop_table(&name).map_err(db_error)?;
                let mut fields = VObject::new();
                fields.insert("table", name.clone());
                fields.insert("root", oid_value(root));
                emit_single(output, fields, || format!("table {name} dropped"))
            }
            DbTableCommand::List => {
                let snapshot = db.working_state().map_err(db_error)?.snapshot;
                let items: Vec<ListItem> = snapshot
                    .tables()
                    .iter()
                    .map(|(name, root)| ListItem {
                        fields: {
                            let mut fields = VObject::new();
                            fields.insert("table", name.as_str().to_owned());
                            fields.insert("root", oid_value(*root));
                            fields
                        },
                        text: format!("{} {}", name, root),
                    })
                    .collect();
                emit_list(output, "tables", items)
            }
        },
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
                cli_error(ExitClass::Schema, format!("invalid JSON value: {error}"))
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
        DbCommand::Add { table } => {
            let root = db.stage(&table).map_err(db_error)?;
            let mut fields = VObject::new();
            fields.insert("table", table.clone());
            fields.insert("root", oid_value(root));
            emit_single(output, fields, || format!("staged {table} at {root}"))
        }
        DbCommand::Diff { staged } => {
            let changes = if staged {
                db.diff_staged().map_err(db_error)?
            } else {
                db.diff_working().map_err(db_error)?
            };
            let items = status_items(&changes, if staged { "staged" } else { "unstaged" });
            match output {
                OutputFormat::Text => {
                    for item in &items {
                        println!("{}", item.text);
                    }
                    Ok(())
                }
                _ => emit_list(output, "changes", items),
            }
        }
        DbCommand::Commit { message } => {
            let commit = db.commit(&message).map_err(db_error)?;
            let mut fields = VObject::new();
            fields.insert("commit", oid_value(commit));
            emit_single(output, fields, || commit.to_string())
        }
        DbCommand::Log => {
            let entries = db.log().map_err(db_error)?;
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
        DbCommand::Branch { name } => {
            let commit = db.create_branch(&name).map_err(db_error)?;
            let mut fields = VObject::new();
            fields.insert("branch", name.clone());
            fields.insert("commit", oid_value(commit));
            emit_single(output, fields, || format!("branch {name} at {commit}"))
        }
        DbCommand::Checkout { branch, force } => {
            let commit = db.checkout(&branch, force).map_err(db_error)?;
            let mut fields = VObject::new();
            fields.insert("branch", branch.clone());
            fields.insert("commit", oid_value(commit));
            emit_single(output, fields, || {
                format!("checked out {branch} at {commit}")
            })
        }
        DbCommand::Merge { branch } => {
            let commit = db.merge(&branch).map_err(db_error)?;
            let mut fields = VObject::new();
            fields.insert("branch", branch.clone());
            fields.insert("commit", oid_value(commit));
            emit_single(output, fields, || format!("merged {branch} as {commit}"))
        }
    }
}

fn db_fields_init() -> VObject {
    let mut fields = VObject::new();
    fields.insert("head", gix_database::HEAD_REF);
    fields.insert("branch", gix_database::DEFAULT_BRANCH);
    fields
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
        text: format!(
            "{}: {} {} ({} row(s))",
            stage,
            change_verb(status.kind),
            status.table,
            status.rows.len()
        ),
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
