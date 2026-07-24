//! The struct authors the schema. Each demo kind is an ordinary
//! `#[derive(Facet)]` struct; [`schema_of`](facet_git_tree::schema_of) turns
//! its shape into the [`SchemaDoc`](facet_git_tree::SchemaDoc) that
//! `git store schema put` publishes.
//!
//! ```console
//! $ cargo run -p git-store --example schemas -- recipe | git store schema put recipe
//! ```

use facet::Facet;
use facet_git_tree::{SchemaDoc, SchemaError, schema_of};

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
    let kind = std::env::args().nth(1).unwrap_or_else(|| "recipe".to_owned());
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

fn schema_for(kind: &str) -> Result<SchemaDoc, SchemaError> {
    match kind {
        "recipe" => schema_of::<Recipe>(),
        "book" => schema_of::<Book>(),
        "task" => schema_of::<Task>(),
        other => {
            eprintln!("unknown kind {other:?}; try recipe, book, or task");
            std::process::exit(2);
        }
    }
}
