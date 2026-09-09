//! Detached canonical commits: every byte caller-supplied, no ref touched,
//! never signed.
//!
//! [`Store`]'s own writes take identity from the backend's
//! [`Committer`](gix_refstore::Committer) and may carry the configured
//! signer's signature. Protocol commits — derivation, realization, projection,
//! seal — need the opposite: exact metadata, an explicit multi-parent set, and
//! an object identity that is a function of the declared fields alone, since
//! callers derive cache keys and provenance edges from it.

use gix::actor::Signature;
use gix::objs::{Find, Write};

use crate::error::Error;
use crate::store::Store;
use crate::store::validate_commit_message;
use facet_git_tree::ObjectId;
use gix_refstore::RefStore;

/// A commit whose tree, parents, author, committer, and message are all
/// explicit and all there is.
///
/// Distinct from a store publication commit: no schema binding, no signature,
/// no backend identity. Write it with [`Store::write_canonical`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalCommit {
    tree: ObjectId,
    parents: Vec<ObjectId>,
    author: Signature,
    committer: Signature,
    message: String,
}

impl CanonicalCommit {
    /// A parentless commit over `tree` attributed to `author` and
    /// `committer`, with `message`.
    pub fn new(
        tree: ObjectId,
        author: Signature,
        committer: Signature,
        message: impl Into<String>,
    ) -> Self {
        CanonicalCommit {
            tree,
            parents: Vec::new(),
            author,
            committer,
            message: message.into(),
        }
    }

    /// Add one parent, keeping call order.
    pub fn with_parent(mut self, parent: ObjectId) -> Self {
        self.parents.push(parent);
        self
    }

    /// Add `parents`, keeping iterator order, after any already added.
    pub fn with_parents(mut self, parents: impl IntoIterator<Item = ObjectId>) -> Self {
        self.parents.extend(parents);
        self
    }

    /// The tree this commit records.
    pub const fn tree(&self) -> ObjectId {
        self.tree
    }

    /// The parent commits, in call order.
    pub fn parents(&self) -> &[ObjectId] {
        &self.parents
    }

    /// The exact bytes to store, independent of any encoder: git's commit
    /// format with the fields in git's own order and no signature header.
    fn raw_bytes(&self) -> Vec<u8> {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(out, "tree {}", self.tree);
        for parent in &self.parents {
            let _ = writeln!(out, "parent {}", parent);
        }
        let _ = writeln!(
            out,
            "author {} <{}> {} {}",
            self.author.name,
            self.author.email,
            self.author.time.seconds,
            git_offset(self.author.time.offset)
        );
        let _ = writeln!(
            out,
            "committer {} <{}> {} {}",
            self.committer.name,
            self.committer.email,
            self.committer.time.seconds,
            git_offset(self.committer.time.offset)
        );
        out.push('\n');
        out.push_str(&self.message);
        out.into_bytes()
    }
}

impl<R, O> Store<R, O>
where
    R: RefStore,
    O: Find + Write,
{
    /// Write `commit` as a commit object, touching no ref.
    ///
    /// The stored bytes are exactly [`CanonicalCommit::raw_bytes`], so the
    /// object id is a function of the declared fields alone, and a configured
    /// signer is deliberately not consulted: a signature would change an
    /// identity callers derive keys from.
    ///
    /// The message rules are the store's own: lines starting with a reserved
    /// trailer are refused, since a canonical commit must not be mistakable
    /// for a legacy metadata document. Duplicate parents are refused because
    /// `git fsck` rejects the result.
    pub fn write_canonical(&self, commit: CanonicalCommit) -> Result<ObjectId, Error> {
        validate_commit_message(&commit.message)?;
        let mut seen = std::collections::HashSet::new();
        if let Some(parent) = commit.parents.iter().find(|p| !seen.insert(**p)) {
            return Err(Error::DuplicateParent { parent: *parent });
        }
        self.objects()
            .write_buf(gix::objs::Kind::Commit, &commit.raw_bytes())
            .map_err(Error::backend)
    }
}

/// The offset as git frames it: sign, two-digit hours, two-digit minutes.
fn git_offset(offset: i32) -> String {
    let sign = if offset < 0 { '-' } else { '+' };
    let abs = offset.unsigned_abs();
    format!("{}{:02}{:02}", sign, abs / 3600, (abs % 3600) / 60)
}
