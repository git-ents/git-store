//! Store a code review — comments anchored to a `{file, line}` with reply
//! threads — as a Git tree, then inspect the resulting objects directly to see
//! how nested collections are laid out.
//!
//! Run with: `cargo run --example code_review`

use facet::Facet;
use facet_git_tree::{GitObject, ObjectId, ObjectStore, deserialize, serialize};

#[derive(Debug, Facet, PartialEq)]
struct Reply {
    author: String,
    body: String,
}

#[derive(Debug, Facet, PartialEq)]
struct ReviewComment {
    file: String,
    line: u32,
    author: String,
    body: String,
    replies: Vec<Reply>,
}

#[derive(Debug, Facet, PartialEq)]
struct Review {
    comments: Vec<ReviewComment>,
}

/// Print the object graph rooted at `oid`: trees recurse, blobs show their text.
fn dump(oid: &ObjectId, store: &ObjectStore, depth: usize) {
    let pad = "  ".repeat(depth);
    match store.get(oid) {
        Some(GitObject::Tree(tree)) => {
            for entry in tree.entries {
                println!("{pad}{} ({:?}):", entry.filename, entry.mode.kind());
                dump(&entry.oid, store, depth + 1);
            }
        }
        Some(GitObject::Blob(blob)) => {
            // Every leaf blob carries a mandatory trailing newline; strip the
            // one the encoder added so the dump stays one line per value.
            let text = String::from_utf8_lossy(&blob.data);
            println!("{pad}= {}", text.strip_suffix('\n').unwrap_or(&text));
        }
        _ => println!("{pad}<missing>"),
    }
}

fn main() {
    let review = Review {
        comments: vec![
            ReviewComment {
                file: "src/lib.rs".to_string(),
                line: 308,
                author: "alice".to_string(),
                body: "Guard against an empty tree here.".to_string(),
                replies: vec![Reply {
                    author: "bob".to_string(),
                    body: "Good catch, fixed.".to_string(),
                }],
            },
            ReviewComment {
                file: "src/lib.rs".to_string(),
                line: 416,
                author: "alice".to_string(),
                body: "Why sort before writing?".to_string(),
                replies: vec![],
            },
        ],
    };

    let (root, store) = serialize(&review).expect("serialize");
    println!("review root tree: {root}\n");

    let decoded: Review = deserialize(&root, &store).expect("deserialize");
    assert_eq!(review, decoded);

    println!("object graph:");
    dump(&root, &store, 0);
}
