//! The struct authors the schema. Each demo kind is an ordinary
//! `#[derive(Facet)]` struct; [`schema_of`](facet_git_tree::schema_of) turns
//! its shape into the [`Schema`](facet_git_tree::Schema) that
//! `git store schema put` publishes.
//!
//! ```console
//! $ cargo run -p git-store --example schemas -- recipe | git store schema put recipe
//! ```

use facet::Facet;
use facet_git_tree::{Schema, SchemaError, schema_of};

/// A cooking recipe.
#[derive(Facet)]
struct Recipe {
    title: String,
    serves: u32,
    ingredients: Vec<String>,
    steps: Vec<String>,
}

/// A book in a personal library.
#[derive(Facet)]
struct Book {
    title: String,
    author: String,
    year: u16,
    read: bool,
}

/// A task on a to-do list.
#[derive(Facet)]
struct Task {
    summary: String,
    done: bool,
    priority: Priority,
    tags: Vec<String>,
}

/// A task's priority.
#[derive(Facet)]
#[repr(u8)]
enum Priority {
    Low,
    Medium,
    High,
}

fn main() {
    let kind = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "recipe".to_owned());
    let doc = match schema_for(&kind) {
        Ok(doc) => doc,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    };
    println!(
        "{}",
        facet_json::to_string_pretty(&doc).expect("schema serializes to JSON")
    );
}

fn schema_for(kind: &str) -> Result<Schema, SchemaError> {
    let doc = match kind {
        "recipe" => schema_of::<Recipe>(),
        "book" => schema_of::<Book>(),
        "task" => schema_of::<Task>(),
        other => {
            eprintln!("unknown kind {other:?}; try recipe, book, or task");
            std::process::exit(2);
        }
    }?;
    // `schema_of` labels the schema with its Rust type name; the schema
    // document carries the kind it is published under, so name it for the ref
    // the caller will use. Rebuilding only relabels the opaque kind, so the
    // well-formedness just proven by `schema_of` still holds.
    Ok(
        Schema::new(kind.to_owned(), doc.root().clone(), doc.defs().clone())
            .expect("relabelling a well-formed schema keeps it well-formed"),
    )
}
