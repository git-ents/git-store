//! A [`RefStore`]/[`Committer`] backed by a `gix::Repository`'s own refs.

use std::{collections::BTreeSet, time::Duration};

use gix::actor::Signature;
use gix::refs::transaction::{Change, PreviousValue, RefEdit as GixRefEdit, RefLog};
use gix::refs::{FullName, Target};
use gix_hash::ObjectId;

use crate::edit::RefEdit;
use crate::name::{RefName, RefPrefix};
use crate::store::{ApplyError, Committer, RefStore};

/// Our per-ref lock lives under `<git-dir>/<LOCK_DIR>/`, deliberately not
/// at `<ref>.lock` — that path is git's own, and holding it would deadlock
/// against the very transaction it guards.
const LOCK_DIR: &str = "gix-refstore-locks";

/// How long to block, with backoff, for a contended per-ref lock.
const LOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// A [`RefStore`] over a `gix` repository's refs.
pub struct GixRefStore<'r> {
    repo: &'r gix::Repository,
}

impl<'r> GixRefStore<'r> {
    /// Borrow `repo`'s refs as a store.
    pub fn new(repo: &'r gix::Repository) -> Self {
        Self { repo }
    }

    /// Serialize this ref's compare-and-swap against every other writer that
    /// goes through this type, in this process or any other.
    ///
    /// gix compares an edit's expectation against a value it read *before*
    /// acquiring the ref's own lock, and waits out `core.filesRefLockTimeout`
    /// to acquire it, so a writer committing inside that window is
    /// overwritten rather than detected. This closes the window; the lock is
    /// held until the returned marker drops.
    fn lock(&self, name: &RefName) -> Result<gix::lock::Marker, GixError> {
        // Pre-create the lock directory once and leave it in place: letting
        // the lock's rollback remove empty parents races the next writer's
        // creation of the same directory. With the directory persistent, only
        // the `.lock` files themselves churn.
        let dir = self.repo.git_dir().join(LOCK_DIR);
        std::fs::create_dir_all(&dir).map_err(GixError::git)?;
        gix::lock::Marker::acquire_to_hold_resource(
            dir.join(encode_ref(name)),
            gix::lock::acquire::Fail::AfterDurationWithBackoff(LOCK_TIMEOUT),
            None,
        )
        .map_err(GixError::git)
    }

    /// Refuse a batch the ref store cannot publish atomically.
    ///
    /// gix commits a multi-ref transaction one `rename(2)` at a time and its
    /// own docs say the commit "may have been performed partially". A ref
    /// that is a path prefix of another — `refs/x` and `refs/x/y` — must be a
    /// file and a directory at once, so whichever rename runs second fails
    /// once the first has created the clashing name, leaving earlier refs of
    /// the batch already published. git cannot represent the pair, so reject
    /// the whole batch up front rather than commit part of it.
    fn ensure_namespace_representable(&self, edits: &[RefEdit]) -> Result<(), ApplyError<GixError>> {
        let renamed: Vec<&RefName> = edits
            .iter()
            .filter(|edit| matches!(edit, RefEdit::Create { .. } | RefEdit::Update { .. }))
            .map(RefEdit::name)
            .collect();

        // Two edits whose names nest cannot both be renamed into place.
        for &outer in &renamed {
            let prefix =
                RefPrefix::new(outer.as_str()).expect("a validated ref name is a valid prefix");
            for &inner in &renamed {
                if outer != inner && inner.is_under(&prefix) {
                    return Err(ApplyError::Backend(GixError::DirFileConflict {
                        name: outer.clone(),
                        nested: inner.to_string(),
                    }));
                }
            }
        }

        // A ref already nested under a name being renamed into place makes
        // that rename land on a directory. The reverse — a name nested under
        // an existing ref — is caught by gix's own prepare, which rolls back
        // before committing anything.
        for &outer in &renamed {
            if let Some(nested) = self.first_ref_under(outer).map_err(ApplyError::Backend)? {
                return Err(ApplyError::Backend(GixError::DirFileConflict {
                    name: outer.clone(),
                    nested,
                }));
            }
        }

        Ok(())
    }

    /// The first ref strictly under `name`, if any — the evidence that
    /// `name` is a directory a rename onto it cannot replace.
    fn first_ref_under(&self, name: &RefName) -> Result<Option<String>, GixError> {
        let namespace = format!("{name}/");
        let platform = self.repo.references().map_err(GixError::git)?;
        let mut refs = platform
            .prefixed(namespace.as_str())
            .map_err(GixError::git)?;
        Ok(match refs.next() {
            Some(Ok(reference)) => {
                Some(String::from_utf8_lossy(reference.name().as_bstr()).into_owned())
            }
            Some(Err(_)) | None => None,
        })
    }
}

/// A flat, filesystem-safe lock filename for a ref: `%` and `/` are
/// percent-escaped so the whole ref becomes one path segment, never a nested
/// directory tree.
fn encode_ref(name: &RefName) -> String {
    name.as_str().replace('%', "%25").replace('/', "%2F")
}

/// A failure of the underlying repository.
#[derive(Debug, thiserror::Error)]
pub enum GixError {
    /// No `user.name`/`user.email` (or `committer.*`) is configured, so a
    /// write cannot be attributed.
    #[error("no committer identity configured; set user.name and user.email")]
    NoCommitter,
    /// No `user.name`/`user.email` (or `author.*`) is configured, so a
    /// write cannot be attributed.
    #[error("no author identity configured; set user.name and user.email")]
    NoAuthor,
    /// A batch names two refs that git cannot both store, because one is a
    /// path prefix of the other: `refs/x` alongside `refs/x/y` would be a
    /// file and a directory at once. gix commits a multi-ref transaction one
    /// rename at a time, so it could publish the nested ref and then fail on
    /// the outer one; the batch is refused before anything is written instead.
    #[error(
        "ref {name} conflicts with the nested ref {nested}: a ref store cannot hold a ref and a directory of the same name"
    )]
    DirFileConflict {
        /// The ref the batch would rename into place.
        name: RefName,
        /// The ref nested beneath `name`, from the same batch or already on disk.
        nested: String,
    },
    /// Any other `gix` failure, kept as the source.
    #[error(transparent)]
    Git(Box<dyn std::error::Error + Send + Sync + 'static>),
}

impl GixError {
    /// Collapse a `gix` error into [`GixError::Git`], preserving it as the source.
    fn git<E>(err: E) -> Self
    where
        E: Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    {
        GixError::Git(err.into())
    }
}

/// Convert a gix transaction precondition failure into the ref-store error
/// that names the failed expectation. Lock contention is retryable too.
fn classify_batch(err: gix::reference::edit::Error, edits: &[RefEdit]) -> ApplyError<GixError> {
    use gix::refs::file::transaction::prepare::Error as Prepare;

    let name = match err {
        gix::reference::edit::Error::FileTransactionPrepare(
            Prepare::MustNotExist { full_name, .. }
            | Prepare::MustExist { full_name, .. }
            | Prepare::ReferenceOutOfDate { full_name, .. }
            | Prepare::DeleteReferenceMustExist { full_name }
            | Prepare::LockAcquire { full_name, .. },
        ) => String::from_utf8_lossy(full_name.as_ref()).into_owned(),
        gix::reference::edit::Error::FileTransactionPrepare(Prepare::PackedTransactionAcquire(
            _,
        )) => edits.first().map_or_else(
            || String::from("refs/invalid"),
            |edit| edit.name().to_string(),
        ),
        other => return ApplyError::Backend(GixError::git(other)),
    };

    let Ok(name) = RefName::new(name) else {
        return ApplyError::Backend(GixError::git(
            "gix returned an invalid reference name".to_owned(),
        ));
    };
    let Some(edit) = edits.iter().find(|edit| edit.name() == &name) else {
        return ApplyError::Backend(GixError::git(
            "gix reported a reference outside the submitted transaction".to_owned(),
        ));
    };
    ApplyError::LostRace {
        name,
        expected: edit.expectation(),
    }
}

fn to_gix_edit(edit: &RefEdit) -> Result<GixRefEdit, GixError> {
    let name = FullName::try_from(edit.name().as_str()).map_err(GixError::git)?;
    let change = match edit {
        RefEdit::Create { new, .. } => Change::Update {
            expected: PreviousValue::MustNotExist,
            new: Target::Object(*new),
            log: Default::default(),
        },
        RefEdit::Update { expected, new, .. } => Change::Update {
            expected: PreviousValue::MustExistAndMatch(Target::Object(*expected)),
            new: Target::Object(*new),
            log: Default::default(),
        },
        RefEdit::Delete { expected, .. } => Change::Delete {
            expected: PreviousValue::MustExistAndMatch(Target::Object(*expected)),
            log: RefLog::AndReference,
        },
    };
    Ok(GixRefEdit {
        change,
        name,
        deref: false,
    })
}

impl RefStore for GixRefStore<'_> {
    type Error = GixError;

    fn read(&self, name: &RefName) -> Result<Option<ObjectId>, Self::Error> {
        match self
            .repo
            .try_find_reference(name.as_str())
            .map_err(GixError::git)?
        {
            Some(mut reference) => Ok(Some(
                reference.peel_to_id().map_err(GixError::git)?.detach(),
            )),
            None => Ok(None),
        }
    }

    /// A ref name that is not valid UTF-8, or that [`RefName::new`] rejects,
    /// cannot have been written through this API, so it is skipped rather
    /// than surfaced as an error.
    fn prefixed(&self, prefix: &RefPrefix) -> Result<Vec<(RefName, ObjectId)>, Self::Error> {
        let namespace = format!("{prefix}/");
        let platform = self.repo.references().map_err(GixError::git)?;
        let mut out = Vec::new();
        for reference in platform
            .prefixed(namespace.as_str())
            .map_err(GixError::git)?
        {
            let mut reference = reference.map_err(GixError::git)?;
            let Ok(text) = std::str::from_utf8(reference.name().as_bstr()) else {
                continue;
            };
            let Ok(name) = RefName::new(text) else {
                continue;
            };
            if !name.is_under(prefix) {
                continue;
            }
            let id = reference.peel_to_id().map_err(GixError::git)?.detach();
            out.push((name, id));
        }
        out.sort_by(|(a, _), (b, _)| a.cmp(b));
        Ok(out)
    }

    fn apply_batch(&self, edits: Vec<RefEdit>) -> Result<(), ApplyError<Self::Error>> {
        if edits.is_empty() {
            return Ok(());
        }

        // This lock is shared by all GixRefStore instances in this process and
        // across processes. Acquire names in order so overlapping batches cannot
        // deadlock before gix acquires its own reference locks.
        let names: BTreeSet<RefName> = edits.iter().map(|edit| edit.name().clone()).collect();
        let _locks: Vec<_> = names
            .iter()
            .map(|name| self.lock(name).map_err(ApplyError::Backend))
            .collect::<Result<_, _>>()?;

        // Preserve the stricter RefEdit::Create contract: gix's MustNotExist
        // accepts an existing ref when it already has the requested value.
        for edit in &edits {
            let current = self.read(edit.name()).map_err(ApplyError::Backend)?;
            let matches = match edit {
                RefEdit::Create { .. } => current.is_none(),
                RefEdit::Update { expected, .. } | RefEdit::Delete { expected, .. } => {
                    current == Some(*expected)
                }
            };
            if !matches {
                return Err(ApplyError::LostRace {
                    name: edit.name().clone(),
                    expected: edit.expectation(),
                });
            }
        }

        // Every expectation holds and we hold this batch's locks, so the
        // namespace is ours to judge: refuse a batch gix could only commit
        // partially (see `ensure_namespace_representable`) before touching disk.
        self.ensure_namespace_representable(&edits)?;

        let gix_edits: Vec<_> = edits
            .iter()
            .map(to_gix_edit)
            .collect::<Result<_, _>>()
            .map_err(ApplyError::Backend)?;
        self.repo
            .edit_references(gix_edits)
            .map(|_| ())
            .map_err(|err| classify_batch(err, &edits))
    }
}

impl Committer for GixRefStore<'_> {
    type Error = GixError;

    fn signature(&self) -> Result<Signature, Self::Error> {
        match self.repo.committer() {
            Some(Ok(sig)) => sig.to_owned().map_err(GixError::git),
            Some(Err(err)) => Err(GixError::git(err)),
            None => Err(GixError::NoCommitter),
        }
    }

    fn author(&self) -> Result<Signature, Self::Error> {
        match self.repo.author() {
            Some(Ok(sig)) => sig.to_owned().map_err(GixError::git),
            Some(Err(err)) => Err(GixError::git(err)),
            None => Err(GixError::NoAuthor),
        }
    }
}
