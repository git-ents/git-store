//! A [`RefStore`]/[`Committer`] backed by a `gix::Repository`'s own refs.

use std::{
    collections::BTreeSet,
    io::Write as _,
    path::{Path, PathBuf},
    time::Duration,
};

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

/// A per-ref sidecar lock that serializes *reclamation* of a dead writer's
/// lock, so two reclaimers can never both decide to remove the same file.
///
/// `~` cannot appear in a ref name, so [`encode_ref`] never emits it and this
/// name can never collide with another ref's lock file.
const REAP_SUFFIX: &str = "~reap";

/// How long a lock file must have gone untouched before a provably-dead
/// owner's leftover may be reclaimed. Combined with the liveness check, never
/// on its own: a live owner is respected no matter how old its file is, and a
/// time skewed into the future reads as "not old".
const STALE_LOCK_GRACE: Duration = Duration::from_secs(10);

/// A [`RefStore`] over a `gix` repository's refs.
///
/// # Read consistency
///
/// [`RefStore::read`] and [`RefStore::prefixed`] take no lock: they read the
/// loose ref files (and `packed-refs`) directly. [`RefStore::apply_batch`]
/// commits a multi-ref transaction one `rename(2)` at a time — gix-ref's own
/// docs call this the "non-atomic nature of multi-file edits" — so a reader
/// running concurrently with a batch can see one ref of the batch moved and
/// another not, and a listing can catch a namespace between renames. This is
/// inherent to refs stored as plain files: it holds across processes and
/// cannot be fixed inside one of them. Callers needing a mutually consistent
/// view must re-read and re-validate. See [`RefStore`] for the exact promise.
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
    /// held until the returned handle drops.
    ///
    /// A lock a *live* writer holds is waited out; one left by a writer that
    /// has provably died is reclaimed (see [`Self::reclaim`]), so a crash does
    /// not leave the ref unwritable forever.
    fn lock(&self, name: &RefName) -> Result<gix::lock::File, GixError> {
        use gix::lock::acquire::Fail;

        // Pre-create the lock directory once and leave it in place: letting
        // the lock's rollback remove empty parents races the next writer's
        // creation of the same directory. With the directory persistent, only
        // the `.lock` files themselves churn.
        let dir = self.repo.git_dir().join(LOCK_DIR);
        std::fs::create_dir_all(&dir).map_err(GixError::git)?;
        let resource = dir.join(encode_ref(name));

        // Fast path: an uncontended lock, by far the common case, is taken
        // without ever paying the timeout.
        match self.acquire(&resource, Fail::Immediately) {
            Ok(file) => return Ok(file),
            Err(gix::lock::acquire::Error::PermanentlyLocked { .. }) => {}
            Err(err) => return Err(GixError::git(err)),
        }

        // The lock is held. If its owner is provably gone it is a crashed
        // writer's leftover — the destructor that would have removed it never
        // ran — and this ref would otherwise stay unwritable for good. Reclaim
        // it and take it. A *live* owner is real contention: wait it out below
        // rather than clobbering it.
        if self.reclaim(&resource)? {
            return self
                .acquire(&resource, Fail::Immediately)
                .map_err(GixError::git);
        }
        self.acquire(&resource, Fail::AfterDurationWithBackoff(LOCK_TIMEOUT))
            .map_err(GixError::git)
    }

    /// Take the lock file at `resource`, recording this process as its owner.
    ///
    /// The owner record is what lets a later writer tell a crashed holder from
    /// a live one. It is written immediately after the file is created, so the
    /// only way to observe its absence is a crash in that microsecond window.
    /// The trailing newline marks the record complete: a reader that catches a
    /// partial write sees no terminator and so never mistakes a truncated pid
    /// for a different, possibly-dead one.
    fn acquire(
        &self,
        resource: &Path,
        mode: gix::lock::acquire::Fail,
    ) -> Result<gix::lock::File, gix::lock::acquire::Error> {
        let mut file = gix::lock::File::acquire_to_update_resource(resource, mode, None)?;
        file.write_all(format!("{}\n", std::process::id()).as_bytes())?;
        file.flush()?;
        Ok(file)
    }

    /// Remove a lock file whose owner is provably dead, so a crashed writer
    /// does not lock its ref forever. Returns whether the caller should retry
    /// acquisition.
    ///
    /// Reclamation is serialized by a per-ref sidecar lock; that is what makes
    /// it race-free. Without it, two reclaimers could interleave such that one
    /// removes the *fresh* lock the other had just taken, breaking mutual
    /// exclusion. While we hold the sidecar nothing else can unlink or replace
    /// the primary: to be eligible the primary's writer must be dead, a dead
    /// writer's destructor never runs, and the only unlinkers that exist are
    /// the writer itself and a sidecar holder. So the file we inspect is the
    /// file we remove.
    fn reclaim(&self, resource: &Path) -> Result<bool, GixError> {
        let sidecar = resource.with_file_name(format!(
            "{}{REAP_SUFFIX}",
            resource
                .file_name()
                .expect("a lock resource has a file name")
                .to_string_lossy(),
        ));
        let _held = match gix::lock::Marker::acquire_to_hold_resource(
            sidecar,
            gix::lock::acquire::Fail::Immediately,
            None,
        ) {
            Ok(marker) => marker,
            // Another reclaimer is running, or a previous one crashed mid-way.
            // Either way, do not reclaim: the caller reports the lock as held,
            // which is no worse than the leak reclamation exists to repair.
            Err(_) => return Ok(false),
        };

        let lock_path = lock_file_of(resource);
        let owner = match std::fs::read(&lock_path) {
            Ok(bytes) => std::str::from_utf8(&bytes).ok().and_then(|text| {
                // Only a terminated record is a complete one.
                text.strip_suffix('\n')?.parse::<u32>().ok()
            }),
            // The holder released it since the timeout: nothing to reclaim.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(err) => return Err(GixError::git(err)),
        };

        // No parseable owner — a writer between creating the file and recording
        // itself, or a foreign file. Never reclaim on a guess.
        let Some(pid) = owner else {
            return Ok(false);
        };
        if !owner_is_dead(pid) || !older_than_grace(&lock_path) {
            return Ok(false);
        }

        match std::fs::remove_file(&lock_path) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(err) => Err(GixError::git(err)),
        }
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

/// The lock file `gix-lock` creates for `resource`: the resource path with
/// `.lock` appended.
fn lock_file_of(resource: &Path) -> PathBuf {
    let mut path = resource.as_os_str().to_owned();
    path.push(".lock");
    PathBuf::from(path)
}

/// Whether `pid` provably belongs to no live process.
///
/// On Linux a missing `/proc/<pid>` is that proof. Elsewhere there is no
/// portable probe without pulling in a dependency, so every owner counts as
/// live and reclamation never happens: a leak may remain, but a live writer is
/// never clobbered.
#[cfg(target_os = "linux")]
fn owner_is_dead(pid: u32) -> bool {
    !Path::new("/proc").join(pid.to_string()).exists()
}

/// See the Linux definition for why this is conservative.
#[cfg(not(target_os = "linux"))]
fn owner_is_dead(_pid: u32) -> bool {
    false
}

/// Whether `path` last changed at least [`STALE_LOCK_GRACE`] ago. A time in the
/// future (clock skew) reads as "not old" and so is never reclaimed.
fn older_than_grace(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|mtime| mtime.elapsed().ok())
        .is_some_and(|age| age >= STALE_LOCK_GRACE)
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
