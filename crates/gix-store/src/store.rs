//! [`Store`]: kinds, schemas, and entities as Git refs and commits.

use std::time::Duration;

use facet_git_tree::{
    ObjectId, SchemaDoc, deserialize, deserialize_value_with_schema, serialize_into,
    serialize_value_with_schema,
};
use facet_value::Value;

use crate::Error;
use crate::refname::check_component;

/// Where entity refs live: `refs/store/<kind>/<name>`.
const DATA_PREFIX: &str = "refs/store";
/// Where schema refs live: `refs/schema/<kind>`.
const SCHEMA_PREFIX: &str = "refs/schema";
/// Our per-ref lock files live under `<git-dir>/<LOCK_DIR>/`, kept separate
/// from git's own `<ref>.lock` so holding one never blocks gix's own ref
/// transaction (which would deadlock against us).
const LOCK_DIR: &str = "gix-store-locks";
/// How long to block, with backoff, for a contended per-ref lock before
/// giving up.
const LOCK_TIMEOUT: Duration = Duration::from_secs(30);
/// A belt-and-suspenders bound on retries once the lock is held; serialized
/// writers should land on the first attempt.
const MAX_CAS_ATTEMPTS: u32 = 8;

/// A content-addressed store layered over a `gix` repository.
///
/// Every kind is defined by a [`SchemaDoc`] published to `refs/schema/<kind>`;
/// every entity is a commit chain at `refs/store/<kind>/<name>` whose tree is
/// the schema-directed encoding of a [`Value`] and whose tip names, in a
/// `Schema:` trailer, the exact schema commit it was validated against. Every
/// write is a commit; history is the audit trail.
pub struct Store<'r> {
    repo: &'r gix::Repository,
}

impl<'r> Store<'r> {
    /// Open a store over `repo` with the default `refs/store` and
    /// `refs/schema` prefixes.
    pub fn open(repo: &'r gix::Repository) -> Store<'r> {
        Store { repo }
    }

    // ── schemas ──────────────────────────────────────────────────────────

    /// Publish (or evolve) the schema for `kind`, committing it forward over
    /// the current schema tip. Returns the new schema commit id.
    pub fn put_schema(&self, kind: &str, doc: &SchemaDoc) -> Result<ObjectId, Error> {
        check_component("kind", kind)?;
        let tree = serialize_into(doc, &self.repo.objects)?;
        self.commit_forward(&schema_ref(kind), &format!("schema {kind}\n"), tree)
    }

    /// The current schema for `kind`, or `None` when never published.
    pub fn schema(&self, kind: &str) -> Result<Option<SchemaDoc>, Error> {
        check_component("kind", kind)?;
        Ok(self.current_schema(kind)?.map(|(_, doc)| doc))
    }

    /// The schema evolution history for `kind`, tip-first. Empty when the kind
    /// has no published schema.
    pub fn schema_history(&self, kind: &str) -> Result<Vec<ObjectId>, Error> {
        check_component("kind", kind)?;
        self.ref_history(&schema_ref(kind))
    }

    // ── entities ─────────────────────────────────────────────────────────

    /// Schema-directed serialize of `value`, committed forward at
    /// `refs/store/<kind>/<name>` with a `Schema:` trailer naming the schema
    /// commit it was validated against.
    ///
    /// `message` sets the commit summary; when `None`, a default `store
    /// <kind>/<name>` summary is used. The `Schema:` trailer is always
    /// appended regardless, and reads only trust the last such trailer, so a
    /// caller-supplied message cannot spoof it.
    ///
    /// Fails if no schema is published for `kind`, or if `value` does not
    /// conform (with the offending path). Retries internally on a lost
    /// compare-and-swap race.
    pub fn store(
        &self,
        kind: &str,
        name: &str,
        value: &Value,
        message: Option<&str>,
    ) -> Result<ObjectId, Error> {
        check_component("kind", kind)?;
        check_component("name", name)?;

        let (schema_commit, doc) = self
            .current_schema(kind)?
            .ok_or_else(|| Error::NoSchema { kind: kind.to_owned() })?;

        let tree = serialize_value_with_schema(value, &doc, &self.repo.objects)?;
        let default_summary = format!("store {kind}/{name}");
        let summary = message.unwrap_or(&default_summary);
        let msg = format!("{summary}\n\nSchema: {schema_commit}\n");
        self.commit_forward(&data_ref(kind, name), &msg, tree)
    }

    /// The current value at `refs/store/<kind>/<name>`, or `None` when absent.
    pub fn retrieve(&self, kind: &str, name: &str) -> Result<Option<Value>, Error> {
        check_component("kind", kind)?;
        check_component("name", name)?;
        let Some(tip) = self.tip(&data_ref(kind, name))? else {
            return Ok(None);
        };
        Ok(Some(self.retrieve_at(tip)?))
    }

    /// The value as of a specific data commit (from [`history`](Self::history)):
    /// resolve the commit's `Schema:` trailer to the schema commit, read that
    /// schema, and do a schema-directed read of the data commit's tree.
    /// Self-contained — no `kind` needed — so a version written under an old
    /// schema stays readable.
    pub fn retrieve_at(&self, commit: ObjectId) -> Result<Value, Error> {
        let commit = self.repo.find_commit(commit).map_err(Error::git)?;
        let tree = commit.tree_id().map_err(Error::git)?.detach();
        let schema_commit = schema_trailer(&commit)?;
        let doc = self.read_schema(self.commit_tree(schema_commit)?)?;
        Ok(deserialize_value_with_schema(&tree, &doc, &self.repo.objects)?)
    }

    /// The entity names published under `kind`, sorted.
    pub fn list(&self, kind: &str) -> Result<Vec<String>, Error> {
        check_component("kind", kind)?;
        self.names_under(&format!("{DATA_PREFIX}/{kind}/"))
    }

    /// Every kind that has a published schema, sorted.
    pub fn kinds(&self) -> Result<Vec<String>, Error> {
        self.names_under(&format!("{SCHEMA_PREFIX}/"))
    }

    /// The commit history of an entity, tip-first along first parents. Empty
    /// when the entity does not exist.
    pub fn history(&self, kind: &str, name: &str) -> Result<Vec<ObjectId>, Error> {
        check_component("kind", kind)?;
        check_component("name", name)?;
        self.ref_history(&data_ref(kind, name))
    }

    /// Delete an entity's ref. Returns whether it existed. Schema refs are not
    /// deletable through this API: their trees are what makes old data
    /// readable.
    pub fn delete(&self, kind: &str, name: &str) -> Result<bool, Error> {
        check_component("kind", kind)?;
        check_component("name", name)?;
        match self
            .repo
            .try_find_reference(data_ref(kind, name).as_str())
            .map_err(Error::git)?
        {
            Some(reference) => {
                reference.delete().map_err(Error::git)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    // ── internals ────────────────────────────────────────────────────────

    /// The current object a ref points at, or `None` when the ref is absent.
    fn tip(&self, refname: &str) -> Result<Option<ObjectId>, Error> {
        match self.repo.try_find_reference(refname).map_err(Error::git)? {
            Some(mut reference) => {
                let id = reference.peel_to_id().map_err(Error::git)?;
                Ok(Some(id.detach()))
            }
            None => Ok(None),
        }
    }

    /// The tree of the commit `oid` points at.
    fn commit_tree(&self, oid: ObjectId) -> Result<ObjectId, Error> {
        let commit = self.repo.find_commit(oid).map_err(Error::git)?;
        Ok(commit.tree_id().map_err(Error::git)?.detach())
    }

    /// Deserialize the tree `oid` as a stored [`SchemaDoc`].
    fn read_schema(&self, oid: ObjectId) -> Result<SchemaDoc, Error> {
        Ok(deserialize(&oid, &self.repo.objects)?)
    }

    /// The current schema for `kind` as `(schema-commit-oid, doc)`, or `None`
    /// when the kind has no published schema. The commit oid is what `store`
    /// records in the `Schema:` trailer: the schema *tree* already
    /// content-addresses which version, so the commit is chosen for its
    /// provenance — who committed that schema and when — and its place in the
    /// schema's history.
    fn current_schema(&self, kind: &str) -> Result<Option<(ObjectId, SchemaDoc)>, Error> {
        let Some(commit) = self.tip(&schema_ref(kind))? else {
            return Ok(None);
        };
        let doc = self.read_schema(self.commit_tree(commit)?)?;
        Ok(Some((commit, doc)))
    }

    /// Commit `tree` forward over the current tip of `refname`, under a
    /// per-ref lock so writers serialize instead of forking the ref.
    ///
    /// `gix::Repository::commit` derives the ref transaction's expected-previous
    /// value from the first parent — `ExistingMustMatch` on a named ref,
    /// `MustNotExist` when parentless — but that precondition alone does not
    /// stop two concurrent writers from both appending to the same tip and
    /// orphaning one commit. Holding [`lock_ref`](Self::lock_ref) around the
    /// tip read and the commit makes each write a fast-forward, so history
    /// stays linear across threads and processes. The retry loop is then just
    /// a guard against a transient error while the lock is held.
    fn commit_forward(&self, refname: &str, msg: &str, tree: ObjectId) -> Result<ObjectId, Error> {
        let _lock = self.lock_ref(refname)?;
        let mut attempts = 0;
        loop {
            let parent = self.tip(refname)?;
            match self.repo.commit(refname, msg, tree, parent) {
                Ok(id) => return Ok(id.detach()),
                Err(err) if is_retryable(&err) => {
                    attempts += 1;
                    if attempts >= MAX_CAS_ATTEMPTS {
                        return Err(Error::CasExhausted {
                            refname: refname.to_owned(),
                            attempts,
                        });
                    }
                }
                Err(err) => return Err(Error::git(err)),
            }
        }
    }

    /// Acquire the exclusive per-ref lock, blocking with backoff up to
    /// [`LOCK_TIMEOUT`]. The returned marker holds the lock until dropped.
    ///
    /// The lock resource lives under `<git-dir>/<LOCK_DIR>/` — deliberately not
    /// `<ref>.lock`, which git itself uses — so our serialization never
    /// contends with gix's own ref transaction. It is a real on-disk lock, so
    /// separate `git store` processes serialize too.
    fn lock_ref(&self, refname: &str) -> Result<gix::lock::Marker, Error> {
        // Pre-create the lock directory once and leave it in place: letting the
        // lock's rollback remove empty parents races the next writer's
        // creation of the same directory. With the directory persistent, only
        // the `.lock` files themselves churn.
        let dir = self.repo.git_dir().join(LOCK_DIR);
        std::fs::create_dir_all(&dir).map_err(Error::git)?;
        gix::lock::Marker::acquire_to_hold_resource(
            dir.join(encode_ref(refname)),
            gix::lock::acquire::Fail::AfterDurationWithBackoff(LOCK_TIMEOUT),
            None,
        )
        .map_err(Error::git)
    }

    /// First-parent walk of a ref's commits, tip-first; empty when absent.
    fn ref_history(&self, refname: &str) -> Result<Vec<ObjectId>, Error> {
        let mut out = Vec::new();
        let mut cursor = self.tip(refname)?;
        while let Some(id) = cursor {
            out.push(id);
            let commit = self.repo.find_commit(id).map_err(Error::git)?;
            cursor = commit.parent_ids().next().map(|id| id.detach());
        }
        Ok(out)
    }

    /// The final path segment of every ref directly under `prefix`, sorted.
    fn names_under(&self, prefix: &str) -> Result<Vec<String>, Error> {
        let platform = self.repo.references().map_err(Error::git)?;
        let mut out = Vec::new();
        for reference in platform.prefixed(prefix).map_err(Error::git)? {
            let reference = reference.map_err(Error::git)?;
            let full = reference.name().as_bstr();
            if let Some(rest) = full.strip_prefix(prefix.as_bytes())
                && let Ok(name) = std::str::from_utf8(rest)
            {
                out.push(name.to_owned());
            }
        }
        out.sort();
        Ok(out)
    }
}

/// `refs/store/<kind>/<name>`.
fn data_ref(kind: &str, name: &str) -> String {
    format!("{DATA_PREFIX}/{kind}/{name}")
}

/// `refs/schema/<kind>`.
fn schema_ref(kind: &str) -> String {
    format!("{SCHEMA_PREFIX}/{kind}")
}

/// A flat, filesystem-safe lock filename for a ref: `%` and `/` are
/// percent-escaped so the whole ref becomes one path segment, never a nested
/// directory tree.
fn encode_ref(refname: &str) -> String {
    refname.replace('%', "%25").replace('/', "%2F")
}

/// The schema commit id named by a data commit's `Schema:` trailer.
///
/// The *last* `Schema:` line wins: `store` always appends the real trailer at
/// the very end, so a caller-supplied commit message containing its own
/// `Schema:` line cannot shadow it.
fn schema_trailer(commit: &gix::Commit<'_>) -> Result<ObjectId, Error> {
    let message = commit.message_raw_sloppy();
    let hex = message
        .split(|&b| b == b'\n')
        .filter_map(|line| line.strip_prefix(b"Schema: "))
        .map(<[u8]>::trim_ascii)
        .next_back();
    match hex {
        Some(hex) => ObjectId::from_hex(hex).map_err(|_| Error::InvalidTrailer {
            commit: commit.id,
            text: String::from_utf8_lossy(hex).into_owned(),
        }),
        None => Err(Error::MissingTrailer { commit: commit.id }),
    }
}

/// Whether a failed commit should be retried by re-reading the tip: either a
/// lost compare-and-swap race — the ref moved (or appeared) between our tip
/// read and the ref transaction — or contention on the ref lock itself while
/// another writer held it. Both resolve on retry; any other error is genuine.
fn is_retryable(err: &gix::commit::Error) -> bool {
    use gix::refs::file::transaction::prepare::Error as Prepare;
    matches!(
        err,
        gix::commit::Error::ReferenceEdit(gix::reference::edit::Error::FileTransactionPrepare(
            Prepare::ReferenceOutOfDate { .. }
                | Prepare::MustNotExist { .. }
                | Prepare::LockAcquire { .. }
                | Prepare::PackedTransactionAcquire(_)
        ))
    )
}
