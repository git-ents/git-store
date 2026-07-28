//! [`Store`]: kinds, schemas, and entities as Git refs and commits.

use std::time::Duration;

use facet_git_tree::{
    ObjectId, SchemaDoc, deserialize, deserialize_value_with_schema, serialize_into,
    serialize_value_with_schema,
};
use facet_value::Value;
use gix::objs::Write as _;

use crate::Error;
use crate::refname::{check_component, check_prefix};

/// Where entity refs live by default: `refs/store/<kind>/<name>`.
const DATA_PREFIX: &str = "refs/store";
/// Where schema refs live by default: `refs/schema/<kind>`.
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
/// Every kind is defined by a [`SchemaDoc`] published to
/// `<schema_prefix>/<kind>` (`refs/schema/<kind>` by default); every entity is
/// a commit chain at `<data_prefix>/<kind>/<name>` (`refs/store/<kind>/<name>`
/// by default). Each commit's tree is a two-entry root, `{value/, schema/}`:
/// the schema-directed encoding of a [`Value`] under `value/`, and the tree of
/// the schema it was validated against — the same object `refs/schema/<kind>`
/// points its commit at — copied in under `schema/`. Binding the schema by
/// subtree rather than by parent or by trailer means ordinary tree
/// reachability keeps it reachable, gc-safe, and fetch-complete: a `git fetch`
/// of just the data ref brings the schema along for free, with no dependence
/// on `refs/schema/*` also having been fetched. The tip additionally names,
/// in a `Schema:` trailer, the schema commit it was validated against — kept
/// for human-readable provenance (`git log`, `git show`), but not load-bearing
/// for reads. Every write is a commit; history is the audit trail.
///
/// Refs are this system's public API surface, so the namespace is
/// configurable: see [`Store::open_with_prefixes`] for a consumer that wants
/// its entities to live under its own domain namespace instead of under
/// `refs/store`, which names the storage mechanism rather than the domain.
pub struct Store<'r> {
    repo: &'r gix::Repository,
    data_prefix: String,
    schema_prefix: String,
}

impl<'r> Store<'r> {
    /// Open a store over `repo` with the default `refs/store` and
    /// `refs/schema` prefixes.
    pub fn open(repo: &'r gix::Repository) -> Store<'r> {
        Store {
            repo,
            data_prefix: DATA_PREFIX.to_owned(),
            schema_prefix: SCHEMA_PREFIX.to_owned(),
        }
    }

    /// Open a store over `repo` with caller-supplied ref-namespace prefixes
    /// in place of the defaults `refs/store` and `refs/schema`. Entities then
    /// live at `<data>/<kind>/<name>` and schemas at `<schema>/<kind>`.
    ///
    /// Use this when the mechanism's own namespace (`refs/store/…`) is the
    /// wrong public name for what is stored — a rule module is a rule module
    /// regardless of what serializes it, so a consumer may prefer e.g.
    /// `refs/meta/rules` for `data` over accepting `refs/store/rules`.
    ///
    /// `data` and `schema` are validated with the same discipline
    /// [`Store::open`]'s `kind`/`name` arguments get on every call: each
    /// `/`-separated segment must be non-empty and must not begin or end with
    /// `.`, end with `.lock`, contain `..` or `@{`, be a lone `@`, or contain
    /// control characters, spaces, or any of `~^:?*[\`. The prefix as a whole
    /// must not begin or end with `/`. Unlike `open`, this can fail — a bad
    /// prefix is rejected here, at open time, rather than surfacing later as
    /// a malformed ref from the first write.
    pub fn open_with_prefixes(
        repo: &'r gix::Repository,
        data: &str,
        schema: &str,
    ) -> Result<Store<'r>, Error> {
        check_prefix("data prefix", data)?;
        check_prefix("schema prefix", schema)?;
        Ok(Store {
            repo,
            data_prefix: data.to_owned(),
            schema_prefix: schema.to_owned(),
        })
    }

    // ── schemas ──────────────────────────────────────────────────────────

    /// Publish (or evolve) the schema for `kind`, committing it forward over
    /// the current schema tip. Returns the new schema commit id.
    pub fn put_schema(&self, kind: &str, doc: &SchemaDoc) -> Result<ObjectId, Error> {
        check_component("kind", kind)?;
        let tree = serialize_into(doc, &self.repo.objects)?;
        self.commit_forward(&self.schema_ref(kind), &format!("schema {kind}\n"), tree)
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
        self.ref_history(&self.schema_ref(kind))
    }

    // ── entities ─────────────────────────────────────────────────────────

    /// Schema-directed serialize of `value`, committed forward at
    /// `<data_prefix>/<kind>/<name>` with the schema bound into the commit's
    /// own tree (see the type docs) and named, for provenance, in a
    /// `Schema:` trailer.
    ///
    /// `message` sets the commit summary; when `None`, a default `store
    /// <kind>/<name>` summary is used. The `Schema:` trailer is always
    /// appended regardless; a caller-supplied message cannot spoof it, though
    /// nothing reads it back to resolve the schema — that is what the
    /// `schema/` subtree is for.
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

        let (schema_commit, doc) = self.current_schema(kind)?.ok_or_else(|| Error::NoSchema {
            kind: kind.to_owned(),
        })?;

        let value_tree = serialize_value_with_schema(value, &doc, &self.repo.objects)?;
        let schema_tree = self.commit_tree(schema_commit)?;
        let tree = self.bind_schema(value_tree, schema_tree)?;
        let default_summary = format!("store {kind}/{name}");
        let summary = message.unwrap_or(&default_summary);
        let msg = format!("{summary}\n\nSchema: {schema_commit}\n");
        self.commit_forward(&self.data_ref(kind, name), &msg, tree)
    }

    pub fn store_anonymous(
        &self,
        kind: &str,
        value: &Value,
        message: Option<&str>,
    ) -> Result<(String, ObjectId), Error> {
        check_component("kind", kind)?;

        let (schema_commit, doc) = self.current_schema(kind)?.ok_or_else(|| Error::NoSchema {
            kind: kind.to_owned(),
        })?;

        let value_tree = serialize_value_with_schema(value, &doc, &self.repo.objects)?;
        let schema_tree = self.commit_tree(schema_commit)?;
        let tree = self.bind_schema(value_tree, schema_tree)?;
        let default_summary = format!("store {kind}/<auto>");
        let summary = message.unwrap_or(&default_summary);
        let msg = format!("{summary}\n\nSchema: {schema_commit}\n");

        // Write the commit object first, without touching any ref, so its hash
        // is known before the name it determines is.
        let commit_id = self
            .repo
            .new_commit(&msg, tree, None::<ObjectId>)
            .map_err(Error::git)?
            .id;

        let name = commit_id.to_string()[..8].to_owned();
        self.repo
            .reference(
                self.data_ref(kind, &name),
                commit_id,
                gix::refs::transaction::PreviousValue::MustNotExist,
                "store anonymous",
            )
            .map_err(Error::git)?;

        Ok((name, commit_id))
    }

    /// The current value at `<data_prefix>/<kind>/<name>`, or `None` when absent.
    pub fn retrieve(&self, kind: &str, name: &str) -> Result<Option<Value>, Error> {
        check_component("kind", kind)?;
        check_component("name", name)?;
        let Some(tip) = self.tip(&self.data_ref(kind, name))? else {
            return Ok(None);
        };
        Ok(Some(self.retrieve_at(tip)?))
    }

    /// The value as of a specific data commit (from [`history`](Self::history)):
    /// read the schema straight out of the commit's own `schema/` subtree and
    /// do a schema-directed read of its `value/` subtree. Self-contained — no
    /// `kind` needed, and no other ref needs to be present — so a version
    /// written under an old schema stays readable even when only this one
    /// commit was fetched.
    pub fn retrieve_at(&self, commit: ObjectId) -> Result<Value, Error> {
        let root = self
            .repo
            .find_commit(commit)
            .map_err(Error::git)?
            .tree_id()
            .map_err(Error::git)?
            .detach();
        let (value_tree, schema_tree) = self.split(root, commit)?;
        let doc = self.read_schema(schema_tree)?;
        Ok(deserialize_value_with_schema(
            &value_tree,
            &doc,
            &self.repo.objects,
        )?)
    }

    /// The schema commit named by a data commit's `Schema:` trailer:
    /// provenance only — which schema commit `value` was validated against at
    /// write time — never a read path. [`retrieve_at`](Self::retrieve_at)
    /// does not call this; it reads the schema straight out of the commit's
    /// own `schema/` subtree, which stays reachable even where this trailer,
    /// being plain commit-message text, would not help resolve anything.
    ///
    /// The returned commit is **not** reachable from `commit` and may not
    /// exist in this repository at all — that unreachability is the whole
    /// reason the `schema/` subtree exists. In a repository that fetched only
    /// the data ref, resolving this oid fails exactly as reads did before
    /// subtree binding. Treat it as a label to display, not an object to
    /// follow; anything that needs the schema itself must read `schema/`.
    ///
    /// Fails if `commit` was not written by [`store`](Self::store) or
    /// [`store_anonymous`](Self::store_anonymous) — or any other commit
    /// lacking a well-formed `Schema:` trailer.
    pub fn schema_provenance(&self, commit: ObjectId) -> Result<ObjectId, Error> {
        let commit = self.repo.find_commit(commit).map_err(Error::git)?;
        schema_trailer(&commit)
    }

    /// The entity names published under `kind`, sorted.
    pub fn list(&self, kind: &str) -> Result<Vec<String>, Error> {
        check_component("kind", kind)?;
        self.names_under(&format!("{}/{kind}/", self.data_prefix))
    }

    /// Every kind that has a published schema, sorted.
    pub fn kinds(&self) -> Result<Vec<String>, Error> {
        self.names_under(&format!("{}/", self.schema_prefix))
    }

    /// The commit history of an entity, tip-first along first parents. Empty
    /// when the entity does not exist.
    pub fn history(&self, kind: &str, name: &str) -> Result<Vec<ObjectId>, Error> {
        check_component("kind", kind)?;
        check_component("name", name)?;
        self.ref_history(&self.data_ref(kind, name))
    }

    /// Delete an entity's ref. Returns whether it existed. Schema refs are not
    /// deletable through this API: their trees are what makes old data
    /// readable.
    pub fn delete(&self, kind: &str, name: &str) -> Result<bool, Error> {
        check_component("kind", kind)?;
        check_component("name", name)?;
        match self
            .repo
            .try_find_reference(self.data_ref(kind, name).as_str())
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

    /// Splice an already-written value tree and schema tree into the
    /// two-entry root a data commit's own tree becomes: `value/` and
    /// `schema/`. From here, ordinary tree reachability is what keeps the
    /// schema alongside the value on every fetch, clone, and gc — unlike a
    /// `Schema:` trailer (not part of the object graph) or a second commit
    /// parent (would contaminate the data commit's ancestry with schema
    /// lineage; rejected, see the type docs).
    fn bind_schema(&self, value: ObjectId, schema: ObjectId) -> Result<ObjectId, Error> {
        let mut entries = vec![
            gix::objs::tree::Entry {
                mode: self.entry_mode(value)?,
                filename: "value".into(),
                oid: value,
            },
            gix::objs::tree::Entry {
                mode: self.entry_mode(schema)?,
                filename: "schema".into(),
                oid: schema,
            },
        ];
        entries.sort();
        self.repo
            .objects
            .write(&gix::objs::Tree { entries })
            .map_err(Error::git)
    }

    /// The tree-entry mode for an already-written object: `Tree` when it
    /// decodes as one, `Blob` otherwise. `serialize_value_with_schema` and a
    /// schema's own tree only ever produce a blob or a tree — never an
    /// executable, symlink, or submodule entry — so this fully determines the
    /// mode a wrapping entry in [`bind_schema`](Self::bind_schema) needs.
    fn entry_mode(&self, oid: ObjectId) -> Result<gix::objs::tree::EntryMode, Error> {
        let kind = self.repo.find_header(oid).map_err(Error::git)?.kind();
        Ok(gix::objs::tree::EntryMode::from(
            if kind == gix::objs::Kind::Tree {
                gix::objs::tree::EntryKind::Tree
            } else {
                gix::objs::tree::EntryKind::Blob
            },
        ))
    }

    /// Split a data commit's root tree into `(value, schema)` — the two-way
    /// split [`bind_schema`](Self::bind_schema) writes, which makes a data
    /// commit's own tree self-sufficient to read. `commit` names the data
    /// commit, for the errors.
    ///
    /// The root must hold *exactly* `schema` and `value`, with `schema` a
    /// tree. Requiring the whole shape, rather than looking up each name in
    /// isolation, is what makes a pre-binding commit — whose tree *was* the
    /// value — fail as the single diagnosable [`Error::NotSubtreeBound`]
    /// naming re-storing as the remedy, instead of being half-matched and
    /// misreported.
    ///
    /// One pre-binding shape stays out of reach: a value with exactly two
    /// top-level fields named `value` and `schema` where `schema` is itself a
    /// tree is indistinguishable here from a real binding, and surfaces
    /// further in as a schema that will not deserialize. It still fails, just
    /// less pointedly, and no commit written from here on can take that shape.
    ///
    /// Both objects are then confirmed present, so an incomplete transfer
    /// names the absent subtree instead of collapsing to a bare `gix`
    /// object-not-found — the failure this whole binding exists to prevent.
    fn split(&self, root: ObjectId, commit: ObjectId) -> Result<(ObjectId, ObjectId), Error> {
        let tree = self.repo.find_tree(root).map_err(Error::git)?;
        let found: Vec<_> = tree
            .iter()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.filename().to_string())
            .collect();
        let not_bound = || Error::NotSubtreeBound {
            commit,
            found: found.join(", "),
        };
        if found.len() != 2 {
            return Err(not_bound());
        }
        let value = tree.find_entry("value").ok_or_else(not_bound)?.object_id();
        let schema_entry = tree.find_entry("schema").ok_or_else(not_bound)?;
        // A bound `schema/` is always a tree — it is a schema commit's tree.
        // A pre-binding value could carry top-level fields named exactly
        // `value` and `schema` and so match on names alone; requiring a tree
        // here rejects every such value whose `schema` field is a scalar.
        if !schema_entry.mode().is_tree() {
            return Err(not_bound());
        }
        let schema = schema_entry.object_id();
        self.require_present(value, "value", commit)?;
        self.require_present(schema, "schema", commit)?;
        Ok((value, schema))
    }

    /// Confirm a subtree object is actually in this repository, so an
    /// incomplete transfer reports [`Error::SchemaObjectMissing`] naming the
    /// commit and which half is absent. Only the subtree root is checked;
    /// corruption deeper inside still surfaces as [`Error::Git`].
    fn require_present(
        &self,
        oid: ObjectId,
        subtree: &'static str,
        commit: ObjectId,
    ) -> Result<(), Error> {
        match self.repo.find_header(oid) {
            Ok(_) => Ok(()),
            Err(_) => Err(Error::SchemaObjectMissing {
                subtree,
                oid,
                commit,
            }),
        }
    }

    /// The current schema for `kind` as `(schema-commit-oid, doc)`, or `None`
    /// when the kind has no published schema. The commit oid is what `store`
    /// records in the `Schema:` trailer: the schema *tree* already
    /// content-addresses which version, so the commit is chosen for its
    /// provenance — who committed that schema and when — and its place in the
    /// schema's history.
    fn current_schema(&self, kind: &str) -> Result<Option<(ObjectId, SchemaDoc)>, Error> {
        let Some(commit) = self.tip(&self.schema_ref(kind))? else {
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

    /// `<data_prefix>/<kind>/<name>`.
    fn data_ref(&self, kind: &str, name: &str) -> String {
        format!("{}/{kind}/{name}", self.data_prefix)
    }

    /// `<schema_prefix>/<kind>`.
    fn schema_ref(&self, kind: &str) -> String {
        format!("{}/{kind}", self.schema_prefix)
    }
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
