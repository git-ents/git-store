//! A [`RefStore`]/[`Committer`] backed by a `gix::Repository`'s own refs.

use std::time::Duration;

use gix::actor::Signature;
use gix::refs::transaction::{Change, PreviousValue, RefEdit as GixRefEdit, RefLog};
use gix::refs::{FullName, Target};
use gix_hash::ObjectId;

use crate::edit::{Expectation, RefEdit};
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

/// Whether a failed edit should be reported as [`ApplyError::LostRace`]:
/// either the precondition genuinely failed, or gix's own ref lock was held
/// by a concurrent writer — both resolve the same way, by re-reading the ref
/// and retrying. Anything else is a genuine backend failure.
fn classify(
    name: RefName,
    expected: Expectation,
    err: gix::reference::edit::Error,
) -> ApplyError<GixError> {
    use gix::refs::file::transaction::prepare::Error as Prepare;
    match err {
        gix::reference::edit::Error::FileTransactionPrepare(
            Prepare::MustNotExist { .. }
            | Prepare::MustExist { .. }
            | Prepare::ReferenceOutOfDate { .. }
            | Prepare::DeleteReferenceMustExist { .. }
            | Prepare::LockAcquire { .. }
            | Prepare::PackedTransactionAcquire(_),
        ) => ApplyError::LostRace { name, expected },
        other => ApplyError::Backend(GixError::git(other)),
    }
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
            if name.strip_prefix(prefix).is_none() {
                continue;
            }
            let id = reference.peel_to_id().map_err(GixError::git)?.detach();
            out.push((name, id));
        }
        out.sort_by(|(a, _), (b, _)| a.cmp(b));
        Ok(out)
    }

    fn apply(&self, edit: RefEdit) -> Result<(), ApplyError<Self::Error>> {
        let name = edit.name().clone();
        let expectation = edit.expectation();
        let _lock = self.lock(&name).map_err(ApplyError::Backend)?;
        let result = match edit {
            RefEdit::Create { new, .. } => {
                // gix treats `MustNotExist` as satisfied when the ref already
                // holds exactly `new`, so a create over a live ref would be
                // reported as success. The lock makes this read authoritative.
                if self
                    .repo
                    .try_find_reference(name.as_str())
                    .map_err(|err| ApplyError::Backend(GixError::git(err)))?
                    .is_some()
                {
                    return Err(ApplyError::LostRace {
                        name,
                        expected: expectation,
                    });
                }
                self.repo
                    .reference(
                        name.as_str(),
                        new,
                        PreviousValue::MustNotExist,
                        "gix-refstore: create",
                    )
                    .map(|_| ())
            }
            RefEdit::Update { expected, new, .. } => self
                .repo
                .reference(
                    name.as_str(),
                    new,
                    PreviousValue::MustExistAndMatch(Target::Object(expected)),
                    "gix-refstore: update",
                )
                .map(|_| ()),
            RefEdit::Delete { expected, .. } => {
                let full_name = FullName::try_from(name.as_str())
                    .map_err(|err| ApplyError::Backend(GixError::git(err)))?;
                self.repo
                    .edit_reference(GixRefEdit {
                        change: Change::Delete {
                            expected: PreviousValue::MustExistAndMatch(Target::Object(expected)),
                            log: RefLog::AndReference,
                        },
                        name: full_name,
                        deref: false,
                    })
                    .map(|_| ())
            }
        };
        result.map_err(|err| classify(name, expectation, err))
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
