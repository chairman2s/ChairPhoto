//! Job ownership — one implementation of the protocol every background job family runs.
//!
//! AGENTS.md ("Background work and ownership") requires four things of every job family:
//! a newer start or a catalog switch must make older workers abortable and unreachable as
//! owners; status/progress/terminal mutation must be scoped to the current job id; a
//! queryable status slot must be cleared before the terminal event and only by its owner;
//! and every lock an ownership transition needs must be acquired before its first mutation,
//! in one documented order.
//!
//! Those four rules used to be re-derived per command — three near-identical `begin_*_job`
//! functions, five inlined copies of the "only if this job still owns the slot" comparison,
//! and two hand-written lists of abort locks inside the catalog-switch phases. This module
//! is the single implementation they all consume (issue #13). It subsumes the narrower
//! `JobStatusSlots` grouping that landed for issue #51.
//!
//! # The pieces
//!
//! * [`AbortGeneration`] — a **swappable** `Arc<AtomicBool>` plus the family's monotonic
//!   job-id source. Workers hold a clone of the generation they started with, so replacing
//!   the installed flag can never revive an already-tripped worker.
//! * [`JobSlot`] — a worker's handle on its own status slot. Every write is scoped to the
//!   job id that claimed it, so a superseded run can neither overwrite nor clear the slot
//!   a newer run owns.
//! * [`JobFamily`] — an abort generation plus a status slot, and the [`JobFamily::begin`]
//!   start transition that claims both.
//! * [`JobRegistry`] — every family in the app, with the two catalog-switch transitions
//!   ([`JobRegistry::lock_for_detach`], [`JobRegistry::lock_for_publish`]).
//!
//! # Lock order
//!
//! **catalog → abort generations → status slots**, and within each of the last two groups
//! the declaration order of [`JobRegistry`]: scan, face indexing, face matching, sharpness,
//! pHash, trash, import, reconcile, Smart Tagging, identity repair, burst analysis, export,
//! bundle export, slideshow, LocalSend send, the Flickr, SmugMug and Instagram uploads.
//!
//! Every nested acquisition in the backend obeys it:
//!
//! | Site | Takes |
//! |---|---|
//! | [`JobFamily::begin`] | catalog → that family's abort → that family's slot |
//! | [`JobRegistry::lock_for_detach`] (switch phase one) | every abort, then every slot |
//! | [`JobRegistry::lock_for_publish`] (switch phase two) | every abort |
//! | [`AbortGeneration::install_fresh`] (sharpness / pHash starts), `scans::claim_import` | one abort, released before the catalog is read |
//! | a scan start (`scans::scan_two_phase`), `storage::empty_trash_as`, `storage::restore_trashed_as` | catalog → that family's abort (the catalog identity checked before the abort is touched) |
//! | `storage::claim_reconcile` (a back-up drain or offload-policy start) | catalog → the reconcile abort |
//! | `slideshow::claim_slideshow` (a slideshow render start) | catalog → the slideshow abort |
//! | `localsend::claim_send` (a LocalSend send start) | catalog → the LocalSend abort |
//! | `uploads::claim_upload` (a Flickr/SmugMug upload or an Instagram post start) | catalog → that service's upload abort, joined ([`AbortGeneration::join_numbered`]), never tripped |
//! | [`AbortGeneration::install_fresh_if_owner`] (a card import committing to index) | the scan abort → the import abort |
//! | [`AbortGeneration::install_fresh_if_newer`] (a burst-analysis worker's claim) | one abort, released before the catalog is read |
//! | `exports::claim_export`, `exports::claim_bundle_export` | one abort, released before the catalog is read |
//! | `exports::export_bundle_claimed_with`'s publish (its last abort check and the bundle's rename into place) | the bundle-export abort alone, after the catalog was released |
//! | [`AbortGeneration::trip`] (every Cancel command) | one abort |
//! | [`JobSlot`] writes (workers) | one slot |
//! | `develop`'s resident set (`develop::with_resident`) | a leaf: after any of the above, never across another lock |
//! | a sidecar's file lock (`xmp::document::SidecarDocument`, open to commit; `xmp::lock`) | a leaf: taken under the catalog by the face-region, GPS and identity-Overwrite writes, and with no lock by the rest; nothing is acquired while it is held |
//! | `xmp::lock::WriteOrder::reserve` (`app::iptc::reserve`, the geocoder's `fill_in`) | catalog → that sidecar's write order, reserved without blocking |
//! | `xmp::lock::WriteOrder::wait`, then the turn held (the IPTC save, the geocoder's fill) | no lock while it waits; with the turn held: catalog (the store), released, then the sidecar's file lock. No catalog holder ever waits on a turn |
//!
//! The sharpness and pHash starts never hold two of these at once, so they cannot
//! invert against the order; phase two covers them instead by tripping whatever it finds
//! installed before replacing it. See [`JobRegistry::lock_for_publish`].
//!
//! Every transition here acquires **all** of its guards before its first mutation, so a
//! poisoned mutex fails the whole transition rather than leaving a prefix of it applied.
//!
//! # Writes keyed by ids read earlier
//!
//! Abort generations stop *workers*. A front end's own write keyed by row ids, such as a
//! culling mark, a burst or stack accept, or a trash delete, needs one more guard. The
//! switch publishes the new catalog before `catalog:switched` reaches the UI, so such a write
//! can land on the new catalog's rows. For these, capture [`super::CatalogIdentity`] with the
//! snapshot (`with_catalog_identified`) and write through `with_catalog_as`. That fails closed
//! (`CATALOG_CHANGED`) when the open catalog is no longer the one read. It takes no lock of
//! its own: the identity is checked under the catalog lock the write already holds, so it
//! adds nothing to the order above.

use crate::catalog::Catalog;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use super::IdentityRepairJobStatus;
#[cfg(feature = "faces")]
use super::{FacesJobStatus, FacesMatchJobStatus};
#[cfg(feature = "smarttags")]
use super::SmarttagsJobStatus;

// ── Abort generations ────────────────────────────────────────────────────────

/// The currently-installed abort flag for one job family, plus that family's monotonic
/// job-id source.
///
/// The flag is **swappable**, never merely reset: a start trips the installed flag and
/// installs a fresh one, and each worker holds a clone of the flag it started with. That is
/// what makes a superseded worker permanently aborted — a later Cancel or switch acts on the
/// new generation and cannot un-abort the old one, and the old worker cannot be reached by
/// anything that only knows the installed flag.
///
/// Job ids live here rather than beside the status slot because the families without a slot
/// (sharpness, pHash) still number their jobs — their progress and terminal events carry the
/// id so the UI can drop a superseded run's stragglers.
#[derive(Default)]
pub struct AbortGeneration {
    flag: Mutex<Arc<AtomicBool>>,
    seq: AtomicU64,
    /// The newest job id that claimed this generation through
    /// [`Self::install_fresh_if_newer`]; written only under `flag`'s lock. `0`: none yet.
    newest_claim: AtomicU64,
}

impl AbortGeneration {
    /// Lock the installed flag **without** touching it, so a caller can acquire it alongside
    /// the other locks of a transition before that transition's first mutation.
    pub fn lock(&self) -> Result<MutexGuard<'_, Arc<AtomicBool>>, String> {
        self.flag.lock().map_err(|e| e.to_string())
    }

    /// Allocate this family's next job id. Ids start at 1, so `0` is never a live job.
    pub fn next_job_id(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// How many job ids this family has issued. Only useful to assert that a *rejected*
    /// start consumed none.
    pub fn job_ids_issued(&self) -> u64 {
        self.seq.load(Ordering::Relaxed)
    }

    /// The installed flag. A clone of the `Arc`, so the caller can compare identity
    /// (`Arc::ptr_eq`) against a flag a start handed out.
    pub fn installed(&self) -> Result<Arc<AtomicBool>, String> {
        Ok(self.lock()?.clone())
    }

    /// Trip the installed generation and install a fresh, un-tripped one, returning it.
    ///
    /// This is the whole start transition for the families that publish no status slot
    /// (scan, sharpness, pHash). They take this lock and release it **before** reading the
    /// catalog, so they never hold two locks at once — see the module-level lock order for
    /// why that is safe and how a switch covers them.
    pub fn install_fresh(&self) -> Result<Arc<AtomicBool>, String> {
        let mut guard = self.lock()?;
        Ok(trip_and_replace(&mut guard))
    }

    /// [`Self::install_fresh`], also allocating the new job's id under the same lock, so the
    /// id order matches the generation order (the import start: its progress carries the id).
    pub fn install_fresh_numbered(&self) -> Result<(Arc<AtomicBool>, u64), String> {
        let mut guard = self.lock()?;
        let job = self.next_job_id();
        Ok((trip_and_replace(&mut guard), job))
    }

    /// Join the installed generation **without** tripping it, allocating a job id under the
    /// same lock: for a family whose jobs run side by side rather than superseding each other
    /// (the publish uploads). Every joined job holds the same flag, so a catalog switch, which
    /// trips the installed flag, stops all of them; each keeps a cancel flag of its own for
    /// its own Cancel. Nothing but a switch may trip a joined generation — a [`Self::trip`]
    /// would cancel every job that joined it.
    pub fn join_numbered(&self) -> Result<(Arc<AtomicBool>, u64), String> {
        let guard = self.lock()?;
        let job = self.next_job_id();
        Ok((guard.clone(), job))
    }

    /// Replace this generation (trip the running job, install a fresh flag) **only if**
    /// `owner_flag` — a job of the `owner` family — is still un-tripped, checked while holding
    /// both locks. `None` when the owner was already stopped; then nothing is tripped.
    ///
    /// For one job superseding another family's job only once it commits: a card import
    /// replaces a running scan when it starts indexing. Checking and replacing under both
    /// locks means a Cancel of the import (which trips under `owner`'s lock) lands either
    /// before (nothing is tripped here) or after (the import stops at its next check).
    /// `self` must precede `owner` in the [`JobRegistry`] declaration order (scan before
    /// import), which is the order these locks are taken in.
    pub fn install_fresh_if_owner(
        &self,
        owner: &AbortGeneration,
        owner_flag: &AtomicBool,
    ) -> Result<Option<Arc<AtomicBool>>, String> {
        let mut guard = self.lock()?;
        let _owner = owner.lock()?;
        if owner_flag.load(Ordering::Relaxed) {
            return Ok(None);
        }
        Ok(Some(trip_and_replace(&mut guard)))
    }

    /// Claim the generation for `job` — an id from [`Self::next_job_id`] — **only if** no
    /// newer job has claimed it already: trip the installed flag and install a fresh one for
    /// `job`. `None` when a newer job got here first; then nothing is tripped, and `job` must
    /// not start.
    ///
    /// For a start whose id is allocated where the user acted (the UI thread, lock-free) but
    /// whose claim runs on a worker: two workers may reach their claims in either order, and
    /// comparing ids makes the later *start* the owner whichever worker arrives first (burst
    /// analysis). A catalog switch replaces the flag without touching the claim record, so a
    /// claim older than one already made stays refused across it.
    pub fn install_fresh_if_newer(&self, job: u64) -> Result<Option<Arc<AtomicBool>>, String> {
        let mut guard = self.lock()?;
        if self.newest_claim.load(Ordering::Relaxed) >= job {
            return Ok(None);
        }
        self.newest_claim.store(job, Ordering::Relaxed);
        Ok(Some(trip_and_replace(&mut guard)))
    }

    /// Trip the installed generation. Every Cancel command is exactly this; a no-op when
    /// nothing is running, since the installed flag is then already tripped or unread.
    pub fn trip(&self) -> Result<(), String> {
        self.lock()?.store(true, Ordering::Relaxed);
        Ok(())
    }
}

/// Trip whatever `guard` holds and replace it with a fresh, un-tripped flag, returning the
/// new one. Takes an already-held guard so a transition that must not release the lock
/// between the trip and the replacement can use it too.
fn trip_and_replace(guard: &mut MutexGuard<'_, Arc<AtomicBool>>) -> Arc<AtomicBool> {
    guard.store(true, Ordering::Relaxed);
    let fresh = Arc::new(AtomicBool::new(false));
    **guard = fresh.clone();
    fresh
}

// ── Status slots ─────────────────────────────────────────────────────────────
//
// This section used to be gated on `any(feature = "faces", feature = "smarttags")`, the two
// features that then owned every slot-publishing family; under `--no-default-features` the
// whole group was dead code. Identity repair (#34) is core — every catalog can owe identity
// debt, feature-gated or not — so the gate is gone and this compiles in every configuration.
// The slot-less families (scan, sharpness, pHash) still use `AbortGeneration` directly.

/// A queryable job-status snapshot. The job id is what scopes every write to its owner.
pub trait JobStatus: Copy {
    /// Which job this snapshot describes.
    fn job_id(&self) -> u64;
}

/// One job's handle on its family's status slot.
///
/// Every method is scoped to the job that claimed the slot, which is the AGENTS.md rule
/// "job status/progress/terminal mutation is scoped to the current job id". A superseded
/// run reaches these calls routinely — starting a new run trips the old one's abort flag,
/// but the in-flight chunk still emits its stragglers — and without the scoping it would
/// rewrite the slot back to itself and then clear it on the way out, leaving the newer,
/// genuinely-running job invisible to status queries.
///
/// Handed out by [`JobFamily::begin`]; cloneable so a worker can give its progress callback
/// one and keep another for the terminal path.
pub struct JobSlot<S> {
    slot: Arc<Mutex<Option<S>>>,
    job: u64,
}

// Manual, because `derive(Clone)` would demand `S: Clone` for no reason — only the `Arc` and
// the id are cloned.
impl<S> Clone for JobSlot<S> {
    fn clone(&self) -> Self {
        Self { slot: self.slot.clone(), job: self.job }
    }
}

impl<S: JobStatus> JobSlot<S> {
    /// Attach to `slot` as `job`. Production handles come from [`JobFamily::begin`]; this is
    /// for tests that need to drive the ownership guards without starting a job.
    pub(crate) fn attach(slot: Arc<Mutex<Option<S>>>, job: u64) -> Self {
        Self { slot, job }
    }

    /// This handle's job id.
    pub fn job(&self) -> u64 {
        self.job
    }

    /// Whether this job still owns the slot. A poisoned slot reads as not-owned, matching
    /// what [`Self::publish`] and [`Self::clear`] would then do.
    pub fn owns(&self) -> bool {
        self.slot
            .lock()
            .map(|s| s.map(|x| x.job_id()) == Some(self.job))
            .unwrap_or(false)
    }

    /// Publish a status snapshot, but only while this job still owns the slot.
    ///
    /// `build` is handed the owning job id so the snapshot cannot be written under a
    /// different one; it runs under the slot lock, so it must not take another lock.
    pub fn publish(&self, build: impl FnOnce(u64) -> S) {
        if let Ok(mut s) = self.slot.lock() {
            if s.map(|x| x.job_id()) == Some(self.job) {
                *s = Some(build(self.job));
            }
        }
    }

    /// Release the slot, but only while this job still owns it.
    ///
    /// Always call this **before** emitting the terminal event, never after. A reattaching
    /// panel registers its listener and then re-reads status; if the slot were still set once
    /// the one terminal event had already been emitted, that panel would adopt a job it can
    /// never see finish and sit in "indexing" forever. Clearing first means a missed terminal
    /// necessarily reads back as idle, or as a newer job that is genuinely still running.
    pub fn clear(&self) {
        if let Ok(mut s) = self.slot.lock() {
            if s.map(|x| x.job_id()) == Some(self.job) {
                *s = None;
            }
        }
    }
}

// ── Job families ─────────────────────────────────────────────────────────────

/// A job family that publishes a queryable status slot: its abort generation, its job-id
/// source and the slot itself, so the start transition can claim all three together.
pub struct JobFamily<S> {
    abort: AbortGeneration,
    slot: Arc<Mutex<Option<S>>>,
}

// Manual, because `derive(Default)` would demand `S: Default`; an idle slot is `None`.
impl<S> Default for JobFamily<S> {
    fn default() -> Self {
        Self { abort: AbortGeneration::default(), slot: Arc::new(Mutex::new(None)) }
    }
}

/// What a start transition hands its worker: where the catalog it claimed lives, the abort
/// flag to poll, the job id its events carry, and its owner-scoped status slot.
pub struct JobClaim<S> {
    /// The claimed catalog's database file — open a secondary connection to it, never the
    /// primary handle, so the worker cannot block UI reads.
    pub db_path: PathBuf,
    /// The claimed catalog's root.
    pub root: PathBuf,
    /// This job's generation of the family's abort flag.
    pub abort: Arc<AtomicBool>,
    /// This job's id. Progress and terminal events must carry it.
    pub job: u64,
    /// This job's handle on the status slot it claimed.
    pub slot: JobSlot<S>,
}

// Manual, because `derive(Debug)` would demand `S: Debug` and the slot is a live mutex, not
// something worth formatting. Exists so `Result<JobClaim<S>, String>::unwrap_err()` compiles
// in the tests that assert a start was *rejected*.
impl<S> std::fmt::Debug for JobClaim<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobClaim")
            .field("db_path", &self.db_path)
            .field("root", &self.root)
            .field("aborted", &self.abort.load(Ordering::Relaxed))
            .field("job", &self.job)
            .finish()
    }
}

impl<S: JobStatus> JobFamily<S> {
    /// Claim ownership of this family: snapshot the catalog, allocate the job id, trip the
    /// previous job, install this job's abort flag and claim the status slot as **one**
    /// transition, holding catalog → abort → slot throughout.
    ///
    /// All three locks are held across the whole claim, and each of them is load-bearing:
    ///
    /// * **abort + slot together.** Two starts can run concurrently, since Tauri dispatches
    ///   commands onto its runtime. If the abort lock were released before the slot write
    ///   they could interleave: A installs its flag, B trips A and claims the slot, then A —
    ///   already aborted — overwrites the slot with itself and the panel tracks a dead job.
    ///   Job ids cannot arbitrate that, because they are allocated after the flag is
    ///   installed.
    /// * **catalog.** Held against a catalog switch rather than against another start, and
    ///   it is what brings the job under catalog-switch ownership. The switch holds that same
    ///   lock while it trips the installed flag and drops the handle
    ///   ([`JobRegistry::lock_for_detach`]), and again while it publishes the new catalog and
    ///   fresh flags ([`JobRegistry::lock_for_publish`]), so only three interleavings exist:
    ///   the switch completes first and this job snapshots the new catalog; it runs entirely
    ///   after and trips the generation installed here; or it is mid-switch, in which case
    ///   the catalog reads as `None` here and this job returns having touched nothing.
    ///   Snapshotting the catalog outside this block admits a fourth — install an un-tripped
    ///   generation after the switch's only abort signal, then index the catalog it is about
    ///   to close, with a handle no Cancel, later start or subsequent switch can reach.
    ///
    /// Everything fallible is read-only and precedes the first mutation, so an error cannot
    /// leave the previous job aborted with no successor, and a rejected start consumes no
    /// job id.
    ///
    /// `build_status` produces the slot's initial snapshot from the new job id (`total` is
    /// normally still unknown — claiming anyway is what lets a status query between "command
    /// returned" and "first progress event" already see the job running). It runs under all
    /// three locks, so it must not take another one.
    pub fn begin(
        &self,
        catalog: &Mutex<Option<Catalog>>,
        build_status: impl FnOnce(u64) -> S,
    ) -> Result<JobClaim<S>, String> {
        self.begin_as(catalog, None, build_status)
    }

    /// [`Self::begin`], bound to the catalog a front end showed when the user asked for the
    /// job: with `expected`, a start that finds another catalog open fails closed with
    /// [`super::CATALOG_CHANGED`] before its first mutation — nothing tripped, no slot
    /// claimed, no job id consumed. The identity is checked under the catalog lock the claim
    /// takes first anyway, so it adds no lock and no switch fits between check and claim.
    pub fn begin_as(
        &self,
        catalog: &Mutex<Option<Catalog>>,
        expected: Option<super::CatalogIdentity>,
        build_status: impl FnOnce(u64) -> S,
    ) -> Result<JobClaim<S>, String> {
        let cat_guard = catalog.lock().map_err(|e| e.to_string())?;
        let c = cat_guard.as_ref().ok_or("No catalog is open")?;
        if expected.is_some_and(|e| !e.is(c)) {
            return Err(super::CATALOG_CHANGED.into());
        }
        let (db_path, root) = (c.db_path().to_path_buf(), c.root().to_path_buf());

        let mut abort_guard = self.abort.lock()?;
        let mut slot_guard = self.slot.lock().map_err(|e| e.to_string())?;
        let job = self.abort.next_job_id();
        let fresh = trip_and_replace(&mut abort_guard);
        *slot_guard = Some(build_status(job));
        Ok(JobClaim {
            db_path,
            root,
            abort: fresh,
            job,
            slot: JobSlot::attach(self.slot.clone(), job),
        })
    }

    /// The family's abort generation — for Cancel commands, and for tests that need to park
    /// a start inside its claim.
    pub fn abort(&self) -> &AbortGeneration {
        &self.abort
    }

    /// Trip the running job's abort flag. The worker stops at its next cancellation point.
    pub fn cancel(&self) -> Result<(), String> {
        self.abort.trip()
    }

    /// Cancel `job` only: trip the installed flag while the status slot still names `job`,
    /// checked under the abort and slot locks (the order [`Self::begin`] takes them), so a
    /// newer job — started by another view, or after a catalog switch — is never stopped by
    /// a Cancel meant for the one before it. Returns whether `job` was running.
    pub fn cancel_job(&self, job: u64) -> Result<bool, String> {
        let abort = self.abort.lock()?;
        let slot = self.slot.lock().map_err(|e| e.to_string())?;
        if slot.map(|s| s.job_id()) != Some(job) {
            return Ok(false);
        }
        abort.store(true, Ordering::Relaxed);
        Ok(true)
    }

    /// The running job's status, or `None` when idle — what the `*_index_status` commands
    /// return so a remounted panel can re-attach instead of believing it is idle.
    pub fn status(&self) -> Result<Option<S>, String> {
        Ok(*self.slot.lock().map_err(|e| e.to_string())?)
    }

    /// The installed abort generation. See [`AbortGeneration::installed`].
    pub fn installed(&self) -> Result<Arc<AtomicBool>, String> {
        self.abort.installed()
    }
}

// ── The registry ─────────────────────────────────────────────────────────────

/// Every background job family in the app.
///
/// Grouped rather than left as loose `AppState` fields because a catalog switch has to reach
/// all of them, and loose fields made that a list someone had to remember to extend. It was
/// not extended: slot-clearing entered `switch_catalog` in `1741b09` scoped to Smart Tagging,
/// the face-*matching* slot added in `526b5a0` copied the new pattern, and face *indexing* —
/// which predates both — was never backfilled, so a switch left the face-indexing status
/// describing a catalog the app had already replaced (issue #51).
///
/// The grouping is what makes that structural rather than remembered: [`Self::lock_for_detach`]
/// and [`Self::lock_for_publish`] name every field with no `..`, and so do the guards'
/// mutation methods, so adding a family here does not compile until every transition handles
/// it. **Declare new job families here, never directly on `AppState`.**
///
/// Field order is the acquisition order documented at the top of this module.
#[derive(Default)]
pub struct JobRegistry {
    /// The scan generation (I4b/I6c). Phase A and a detached Phase B share it, so a second
    /// scan aborts the earlier scan's still-running Phase B instead of racing it on the same
    /// catalog. No status slot: scan progress is event-only.
    pub scan: AbortGeneration,
    /// Face indexing (H13b).
    #[cfg(feature = "faces")]
    pub faces: JobFamily<FacesJobStatus>,
    /// Face matching (H13c) — a separate family from [`Self::faces`] so cancelling a match
    /// does not stop an index and vice versa, and so the two never collide on a job id.
    #[cfg(feature = "faces")]
    pub faces_match: JobFamily<FacesMatchJobStatus>,
    /// Sharpness indexing (H16b). No status slot; its events carry the job id.
    pub sharpness: AbortGeneration,
    /// Perceptual-hash indexing (H15a). No status slot; its events carry the job id.
    pub phash: AbortGeneration,
    /// The Smart Tagging embedding index (H7b).
    #[cfg(feature = "smarttags")]
    pub smarttags: JobFamily<SmarttagsJobStatus>,
    /// Emptying the trash (cluster B, B2) — the only job that destroys originals.
    ///
    /// It has a generation for two reasons the other families do not share. A catalog
    /// switch trips every family, which is what makes an in-flight delete stop applying
    /// one catalog's photo ids to another's rows. And **Restore trips it too**: a user who
    /// pulls a photo back out of the trash while a delete is walking the filesystem must
    /// win that race, because the alternative is destroying something they just asked to
    /// keep. No status slot — the delete reports its own terminal result.
    pub trash: AbortGeneration,
    /// Importing a card or a `.chairphoto` bundle: the copy into the library root, then the
    /// indexing of the copies. A newer import start trips the older one, Cancel trips it, and
    /// a catalog switch trips it, so an import never indexes into a catalog the user has
    /// left. No status slot — progress is `import:progress`, and the start reports its own
    /// terminal result.
    pub import: AbortGeneration,
    /// Draining the reconcile queue and applying the offload policy (E4) — backups,
    /// offloads and restores across volumes. Claimed under the catalog lock
    /// (`storage::claim_reconcile`), so a catalog switch trips every drain that read the
    /// catalog it is replacing; a newer drain trips an older one. No status slot — a drain
    /// reports its own terminal summary.
    pub reconcile: AbortGeneration,
    /// The sidecar-identity repair pass (#34) — retries `pending_sidecar_identity`.
    ///
    /// Not feature-gated, and the first family here that isn't: identity debt is core, so
    /// this is the family that made the status-slot machinery above unconditional. It
    /// publishes a slot because the debt panel is a modal that remounts, and a pass over a
    /// 74k-row queue on a NAS long outlives one open/close of it.
    pub identity: JobFamily<IdentityRepairJobStatus>,
    /// Burst-relative sharpness analysis (H16e, `burst_analysis`). A newer run trips an
    /// older one — whichever worker claims first ([`AbortGeneration::install_fresh_if_newer`])
    /// — so a slower, superseded run over an overlapping set never writes its flags over the
    /// newer run's. No status slot: each run reports its own terminal result, tagged with its
    /// job id so a front end drops a superseded run's.
    pub burst: AbortGeneration,
    /// Exporting photos to a folder (`exports`, the Export dialog). A newer export, Cancel or
    /// a catalog switch trips it; the worker stops before its next photo. No status slot —
    /// progress is `export:progress` and the start reports its own terminal result.
    pub export: AbortGeneration,
    /// Writing an import batch as a `.chairphoto` bundle (`exports`). Its own family, so an
    /// export to a folder never cancels a bundle being written, and vice versa. A tripped
    /// bundle write removes its temporary file and places nothing at the destination.
    pub bundle_export: AbortGeneration,
    /// Rendering a slideshow movie (`app::slideshow`): the frame export and the ffmpeg
    /// encode. Claimed under the catalog lock (`slideshow::claim_slideshow`); a newer render,
    /// Cancel or a catalog switch trips it, which kills ffmpeg. No status slot — the render
    /// returns its own terminal result, and its `slideshow:progress` events carry the job id.
    #[cfg(feature = "slideshow")]
    pub slideshow: AbortGeneration,
    /// Sending photos to a device on the LAN (`app::localsend`): the full-resolution renders and
    /// the LocalSend upload. Claimed under the catalog lock (`localsend::claim_send`); a newer
    /// send, Cancel or a catalog switch trips it — the send stops before its next render or
    /// file and cancels the receiver's session. No status slot: the send returns its own
    /// terminal result, and its `localsend:progress` events carry the job id.
    #[cfg(feature = "localsend")]
    pub localsend: AbortGeneration,
    /// Publishing photos to Flickr (`app::uploads`, `app::flickr`): the render and the
    /// upload. Unlike the families above, publishes run side by side: each claim joins the
    /// installed generation under the catalog lock ([`AbortGeneration::join_numbered`],
    /// `uploads::claim_upload`) and keeps its own cancel flag, so a newer publish stops no
    /// older one and a Cancel stops only its own. A catalog switch trips the generation and
    /// so every publish that joined it — each stops before its render and before its upload
    /// (an upload already in flight is not interrupted: the service may already hold it). No
    /// status slot: each job returns its own terminal result, numbered by its own job id.
    ///
    /// The three upload families are not feature-gated, unlike their services: the claim and
    /// render (`app::uploads`) are the shared publish flow's, compiled and tested in every
    /// build, and an unused generation costs one mutex.
    pub upload_flickr: AbortGeneration,
    /// Publishing photos to SmugMug — as [`Self::upload_flickr`], its own family.
    pub upload_smugmug: AbortGeneration,
    /// Supervised Instagram posts (`app::instagram`): the render, then handing it to Chrome —
    /// joined as [`Self::upload_flickr`]. Stopped before Chrome has the render, a post stops;
    /// once the composer has it, the browser window is the cancel (docs/instagram.md).
    pub upload_instagram: AbortGeneration,
    /// The Develop session's working image (docs/plans/raw-foundation): one claim per
    /// opened photo; a switch, exit or catalog change trips it and the image is released.
    #[cfg(all(feature = "raw", feature = "edit"))]
    pub develop: JobFamily<super::DevelopStatus>,
}

impl JobRegistry {
    /// Lock every abort generation **and** every status slot, mutating nothing — the guards
    /// a catalog switch's phase one needs (see `detach_catalog_and_trip_jobs_with`).
    ///
    /// Two passes: every abort generation in declaration order, then every status slot in
    /// declaration order. That is the module's documented catalog → abort → slot order
    /// extended across families, and it is why this cannot invert against
    /// [`JobFamily::begin`], which takes one family's abort before that same family's slot.
    ///
    /// Locking and mutating are separate steps so the caller can get every guard before its
    /// first mutation — phase one has a fallible caller-supplied step (`set_library_root`
    /// persisting `catalog_root` through the outgoing handle) that must be able to back out
    /// with nothing tripped.
    pub fn lock_for_detach(&self) -> Result<DetachGuards<'_>, String> {
        let aborts = self.lock_for_publish()?;
        // Destructured, not field-accessed, and deliberately without `..`: this is what makes
        // a new family a compile error here rather than a silent omission. Do not add `..`.
        let Self {
            scan: _,
            #[cfg(feature = "faces")]
            faces,
            #[cfg(feature = "faces")]
            faces_match,
            sharpness: _,
            phash: _,
            trash: _,
            import: _,
            reconcile: _,
            #[cfg(feature = "smarttags")]
            smarttags,
            identity,
            burst: _,
            export: _,
            bundle_export: _,
            #[cfg(feature = "slideshow")]
            slideshow: _,
            #[cfg(feature = "localsend")]
            localsend: _,
            upload_flickr: _,
            upload_smugmug: _,
            upload_instagram: _,
            #[cfg(all(feature = "raw", feature = "edit"))]
            develop,
        } = self;
        let slots = SlotGuards {
            #[cfg(feature = "faces")]
            faces: faces.slot.lock().map_err(|e| e.to_string())?,
            #[cfg(feature = "faces")]
            faces_match: faces_match.slot.lock().map_err(|e| e.to_string())?,
            #[cfg(feature = "smarttags")]
            smarttags: smarttags.slot.lock().map_err(|e| e.to_string())?,
            identity: identity.slot.lock().map_err(|e| e.to_string())?,
            #[cfg(all(feature = "raw", feature = "edit"))]
            develop: develop.slot.lock().map_err(|e| e.to_string())?,
        };
        Ok(DetachGuards { aborts, slots })
    }

    /// Lock every abort generation, mutating nothing — the guards a catalog switch's phase
    /// two needs (see `publish_catalog_and_reset_jobs`).
    ///
    /// Status slots are deliberately **not** locked or cleared here. In phase two a newer
    /// start can already own a slot, and clearing there would wipe it; each aborted worker
    /// clears its own slot on the way out, and only if it still owns it.
    pub fn lock_for_publish(&self) -> Result<AbortGuards<'_>, String> {
        // Exhaustive destructuring, no `..` — see `lock_for_detach`.
        let Self {
            scan,
            #[cfg(feature = "faces")]
            faces,
            #[cfg(feature = "faces")]
            faces_match,
            sharpness,
            phash,
            trash,
            import,
            reconcile,
            #[cfg(feature = "smarttags")]
            smarttags,
            identity,
            burst,
            export,
            bundle_export,
            #[cfg(feature = "slideshow")]
            slideshow,
            #[cfg(feature = "localsend")]
            localsend,
            upload_flickr,
            upload_smugmug,
            upload_instagram,
            #[cfg(all(feature = "raw", feature = "edit"))]
            develop,
        } = self;
        Ok(AbortGuards {
            scan: scan.lock()?,
            #[cfg(feature = "faces")]
            faces: faces.abort.lock()?,
            #[cfg(feature = "faces")]
            faces_match: faces_match.abort.lock()?,
            sharpness: sharpness.lock()?,
            phash: phash.lock()?,
            trash: trash.lock()?,
            import: import.lock()?,
            reconcile: reconcile.lock()?,
            #[cfg(feature = "smarttags")]
            smarttags: smarttags.abort.lock()?,
            identity: identity.abort.lock()?,
            burst: burst.lock()?,
            export: export.lock()?,
            bundle_export: bundle_export.lock()?,
            #[cfg(feature = "slideshow")]
            slideshow: slideshow.lock()?,
            #[cfg(feature = "localsend")]
            localsend: localsend.lock()?,
            upload_flickr: upload_flickr.lock()?,
            upload_smugmug: upload_smugmug.lock()?,
            upload_instagram: upload_instagram.lock()?,
            #[cfg(all(feature = "raw", feature = "edit"))]
            develop: develop.abort.lock()?,
        })
    }
}

/// Every abort generation, locked. Produced by [`JobRegistry::lock_for_publish`].
pub struct AbortGuards<'a> {
    scan: MutexGuard<'a, Arc<AtomicBool>>,
    #[cfg(feature = "faces")]
    faces: MutexGuard<'a, Arc<AtomicBool>>,
    #[cfg(feature = "faces")]
    faces_match: MutexGuard<'a, Arc<AtomicBool>>,
    sharpness: MutexGuard<'a, Arc<AtomicBool>>,
    phash: MutexGuard<'a, Arc<AtomicBool>>,
    trash: MutexGuard<'a, Arc<AtomicBool>>,
    import: MutexGuard<'a, Arc<AtomicBool>>,
    reconcile: MutexGuard<'a, Arc<AtomicBool>>,
    #[cfg(feature = "smarttags")]
    smarttags: MutexGuard<'a, Arc<AtomicBool>>,
    identity: MutexGuard<'a, Arc<AtomicBool>>,
    burst: MutexGuard<'a, Arc<AtomicBool>>,
    export: MutexGuard<'a, Arc<AtomicBool>>,
    bundle_export: MutexGuard<'a, Arc<AtomicBool>>,
    #[cfg(feature = "slideshow")]
    slideshow: MutexGuard<'a, Arc<AtomicBool>>,
    #[cfg(feature = "localsend")]
    localsend: MutexGuard<'a, Arc<AtomicBool>>,
    upload_flickr: MutexGuard<'a, Arc<AtomicBool>>,
    upload_smugmug: MutexGuard<'a, Arc<AtomicBool>>,
    upload_instagram: MutexGuard<'a, Arc<AtomicBool>>,
    #[cfg(all(feature = "raw", feature = "edit"))]
    develop: MutexGuard<'a, Arc<AtomicBool>>,
}

impl AbortGuards<'_> {
    /// Trip every installed generation, leaving each in place. Exhaustive — no `..`.
    fn trip_all(&self) {
        let Self {
            scan,
            #[cfg(feature = "faces")]
            faces,
            #[cfg(feature = "faces")]
            faces_match,
            sharpness,
            phash,
            trash,
            import,
            reconcile,
            #[cfg(feature = "smarttags")]
            smarttags,
            identity,
            burst,
            export,
            bundle_export,
            #[cfg(feature = "slideshow")]
            slideshow,
            #[cfg(feature = "localsend")]
            localsend,
            upload_flickr,
            upload_smugmug,
            upload_instagram,
            #[cfg(all(feature = "raw", feature = "edit"))]
            develop,
        } = self;
        scan.store(true, Ordering::Relaxed);
        #[cfg(feature = "faces")]
        faces.store(true, Ordering::Relaxed);
        #[cfg(feature = "faces")]
        faces_match.store(true, Ordering::Relaxed);
        sharpness.store(true, Ordering::Relaxed);
        phash.store(true, Ordering::Relaxed);
        trash.store(true, Ordering::Relaxed);
        import.store(true, Ordering::Relaxed);
        reconcile.store(true, Ordering::Relaxed);
        #[cfg(feature = "smarttags")]
        smarttags.store(true, Ordering::Relaxed);
        identity.store(true, Ordering::Relaxed);
        burst.store(true, Ordering::Relaxed);
        export.store(true, Ordering::Relaxed);
        bundle_export.store(true, Ordering::Relaxed);
        #[cfg(feature = "slideshow")]
        slideshow.store(true, Ordering::Relaxed);
        #[cfg(feature = "localsend")]
        localsend.store(true, Ordering::Relaxed);
        upload_flickr.store(true, Ordering::Relaxed);
        upload_smugmug.store(true, Ordering::Relaxed);
        upload_instagram.store(true, Ordering::Relaxed);
        #[cfg(all(feature = "raw", feature = "edit"))]
        develop.store(true, Ordering::Relaxed);
    }

    /// Trip every installed generation and replace each with a fresh, un-tripped one,
    /// returning the new scan generation (an auto-resumed Phase B is handed it).
    ///
    /// Whatever is installed at this point is tripped **before** being replaced. The families
    /// that claim under the catalog lock cannot have installed anything while phase two held
    /// it — but the scan, sharpness and pHash starts install their generation *before*
    /// reading the catalog, so one of those can be holding a live, un-tripped generation
    /// right now, blocked on the catalog read. Replacing it silently would leave its worker
    /// running with a handle no Cancel, later start or subsequent switch can reach. Tripping
    /// first means such a start loses this race with an already-aborted flag and stops
    /// immediately; a start that arrives after the replacement becomes the current
    /// generation and proceeds normally.
    ///
    /// The generations tripped in phase one are deliberately not cleared: the old workers
    /// still hold those `Arc`s and must stay aborted. Swapping in new ones lets future jobs
    /// start clean against the new catalog.
    ///
    /// Consumes the guards, so every generation is replaced before any lock is released.
    pub fn trip_and_replace_all(mut self) -> Arc<AtomicBool> {
        self.trip_all();
        let Self {
            ref mut scan,
            #[cfg(feature = "faces")]
                ref mut faces,
            #[cfg(feature = "faces")]
                ref mut faces_match,
            ref mut sharpness,
            ref mut phash,
            ref mut trash,
            ref mut import,
            ref mut reconcile,
            #[cfg(feature = "smarttags")]
                ref mut smarttags,
            ref mut identity,
            ref mut burst,
            ref mut export,
            ref mut bundle_export,
            #[cfg(feature = "slideshow")]
                ref mut slideshow,
            #[cfg(feature = "localsend")]
                ref mut localsend,
            ref mut upload_flickr,
            ref mut upload_smugmug,
            ref mut upload_instagram,
            #[cfg(all(feature = "raw", feature = "edit"))]
                ref mut develop,
        } = self;
        let fresh_scan = Arc::new(AtomicBool::new(false));
        **scan = fresh_scan.clone();
        #[cfg(feature = "faces")]
        {
            **faces = Arc::new(AtomicBool::new(false));
            **faces_match = Arc::new(AtomicBool::new(false));
        }
        **sharpness = Arc::new(AtomicBool::new(false));
        **phash = Arc::new(AtomicBool::new(false));
        **trash = Arc::new(AtomicBool::new(false));
        **import = Arc::new(AtomicBool::new(false));
        **reconcile = Arc::new(AtomicBool::new(false));
        #[cfg(feature = "smarttags")]
        {
            **smarttags = Arc::new(AtomicBool::new(false));
        }
        **identity = Arc::new(AtomicBool::new(false));
        **burst = Arc::new(AtomicBool::new(false));
        **export = Arc::new(AtomicBool::new(false));
        **bundle_export = Arc::new(AtomicBool::new(false));
        #[cfg(feature = "slideshow")]
        {
            **slideshow = Arc::new(AtomicBool::new(false));
        }
        #[cfg(feature = "localsend")]
        {
            **localsend = Arc::new(AtomicBool::new(false));
        }
        **upload_flickr = Arc::new(AtomicBool::new(false));
        **upload_smugmug = Arc::new(AtomicBool::new(false));
        **upload_instagram = Arc::new(AtomicBool::new(false));
        #[cfg(all(feature = "raw", feature = "edit"))]
        {
            **develop = Arc::new(AtomicBool::new(false));
        }
        fresh_scan
    }
}

/// Every status slot, locked. Part of [`DetachGuards`].
pub struct SlotGuards<'a> {
    #[cfg(feature = "faces")]
    faces: MutexGuard<'a, Option<FacesJobStatus>>,
    #[cfg(feature = "faces")]
    faces_match: MutexGuard<'a, Option<FacesMatchJobStatus>>,
    #[cfg(feature = "smarttags")]
    smarttags: MutexGuard<'a, Option<SmarttagsJobStatus>>,
    /// Unconditional, so this struct always borrows `'a` — it used to need a `PhantomData`
    /// to keep the lifetime used under `--no-default-features`, where every other slot here
    /// is compiled out.
    identity: MutexGuard<'a, Option<IdentityRepairJobStatus>>,
    #[cfg(all(feature = "raw", feature = "edit"))]
    develop: MutexGuard<'a, Option<super::DevelopStatus>>,
}

impl SlotGuards<'_> {
    /// Clear every slot. Exhaustive destructuring — no `..` — so a family added to
    /// [`JobRegistry`] fails to compile here until it is handled.
    fn clear_all(self) {
        let Self {
            #[cfg(feature = "faces")]
            mut faces,
            #[cfg(feature = "faces")]
            mut faces_match,
            #[cfg(feature = "smarttags")]
            mut smarttags,
            mut identity,
            #[cfg(all(feature = "raw", feature = "edit"))]
            mut develop,
        } = self;
        #[cfg(feature = "faces")]
        {
            *faces = None;
            *faces_match = None;
        }
        #[cfg(feature = "smarttags")]
        {
            *smarttags = None;
        }
        *identity = None;
        #[cfg(all(feature = "raw", feature = "edit"))]
        {
            *develop = None;
        }
    }
}

/// Every abort generation and every status slot, locked. Produced by
/// [`JobRegistry::lock_for_detach`].
pub struct DetachGuards<'a> {
    aborts: AbortGuards<'a>,
    slots: SlotGuards<'a>,
}

impl DetachGuards<'_> {
    /// Trip every generation and clear every status slot, then release the locks together.
    ///
    /// Clearing the slots is what makes an outgoing job unreachable as an *owner*, not just
    /// abortable. Tripping alone leaves it reachable as a slot's owner: `*_index_status`
    /// keeps reporting it as running, and a panel re-queries on mount, so after a switch it
    /// adopts a job belonging to the catalog the user has left and shows "indexing" against
    /// the new one (issue #51).
    ///
    /// Clearing is safe in *this* phase specifically, which is why
    /// [`AbortGuards::trip_and_replace_all`] does not do it. Phase one already holds the
    /// catalog lock, so no start can be inside its catalog → abort → slot claim; the slot's
    /// owner is necessarily the job being tripped. Between the phases the catalog is `None`,
    /// so every start fails before claiming.
    ///
    /// The develop family's working images go with its slot: an image is only ever
    /// reachable through a token minted by a claim, and phase one trips every claim, so a
    /// resident image would be 800 MB nothing can name — or worse, named by a token whose
    /// photo id now means a different photo in the next catalog. The resident set's lock is
    /// a leaf (see `develop`), taken here after every registry lock is released by
    /// `clear_all`.
    pub fn trip_and_clear_all(self) {
        let Self { aborts, slots } = self;
        aborts.trip_all();
        slots.clear_all();
        #[cfg(all(feature = "raw", feature = "edit"))]
        {
            // Tests only: the resident set is process-global, so a switch in one test would
            // release the images a develop test running in parallel just made resident
            // (#133). Wait for that test instead; see `develop::serial`.
            #[cfg(test)]
            let _serial = crate::develop::serial();
            crate::develop::release_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// A status shape standing in for the real ones, so these tests pin the shared module
    /// rather than one family's use of it. `Copy`, as every real status slot is.
    #[derive(Debug, Clone, Copy, PartialEq)]
    struct TestStatus {
        job: u64,
        done: usize,
    }

    impl JobStatus for TestStatus {
        fn job_id(&self) -> u64 {
            self.job
        }
    }

    fn status(job: u64, done: usize) -> TestStatus {
        TestStatus { job, done }
    }

    /// A catalog switch must make every in-flight worker stop being an owner — including
    /// the one that deletes originals. Without this, an `empty_trash` worker planned
    /// against catalog A would carry on deleting A's files and then apply A's numeric photo
    /// ids to catalog B's rows.
    ///
    /// The registry is destructured without `..` precisely so a new family cannot skip
    /// this; the test is here so the *behaviour* is pinned as well as the compile error.
    #[test]
    fn a_switch_trips_the_trash_generation_along_with_every_other() {
        let jobs = JobRegistry::default();
        let held = jobs.trash.installed().unwrap();
        let scan_held = jobs.scan.installed().unwrap();
        assert!(!held.load(Ordering::Relaxed), "a fresh generation is not tripped");

        jobs.lock_for_publish().unwrap().trip_and_replace_all();

        assert!(held.load(Ordering::Relaxed), "the delete worker is no longer the owner");
        assert!(scan_held.load(Ordering::Relaxed), "and so is everyone else");
        assert!(
            !jobs.trash.installed().unwrap().load(Ordering::Relaxed),
            "while a fresh generation is installed for whatever starts next"
        );
    }

    /// An import in flight when the user switches catalogs must not index its copies into
    /// either catalog: both switch phases trip the import generation, and phase two leaves a
    /// fresh one for the next import.
    #[test]
    fn both_switch_phases_trip_the_import_generation() {
        let jobs = JobRegistry::default();
        let copying = jobs.import.install_fresh().unwrap();
        // Phase one's abort half (`trip_and_clear_all` minus the slot clear and the Develop
        // release, which reach process-wide state other tests share).
        jobs.lock_for_detach().unwrap().aborts.trip_all();
        assert!(copying.load(Ordering::Relaxed), "phase one stops the import");

        let racing = jobs.import.install_fresh().unwrap();
        jobs.lock_for_publish().unwrap().trip_and_replace_all();
        assert!(racing.load(Ordering::Relaxed), "phase two stops an import started between the phases");
        assert!(!jobs.import.installed().unwrap().load(Ordering::Relaxed));
    }

    /// Cancelling is the same trip, which is what lets Restore stand a delete down.
    #[test]
    fn tripping_the_trash_family_stops_a_worker_holding_its_flag() {
        let jobs = JobRegistry::default();
        let worker_flag = jobs.trash.install_fresh().unwrap();

        jobs.trash.trip().unwrap();

        assert!(worker_flag.load(Ordering::Relaxed));
    }

    fn open_catalog(tag: &str) -> (Mutex<Option<Catalog>>, crate::test_support::TestSubPath) {
        let dir = crate::test_support::TestTmpDir::new(&format!("jobs-{tag}"));
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let db = dir.join("catalog.chairphoto");
        let catalog = Catalog::open(&db, &root).unwrap();
        (Mutex::new(Some(catalog)), dir.into_subpath("catalog.chairphoto"))
    }

    // ── Status-slot ownership ────────────────────────────────────────────────

    /// The guard that scopes every slot write to its owning job, exercised directly: a
    /// superseded run must be unable to publish into or clear a slot a newer run owns.
    ///
    /// This used to live in `commands::smarttags` against helpers only that module had, so
    /// the identical comparisons inlined in `commands::faces` (five copies across indexing
    /// and matching) were covered by nothing at all. Now there is one implementation and one
    /// test of it.
    ///
    /// Exercised through the guard conditions rather than a live worker, because reproducing
    /// the race in a real run means winning it — the forced-interleaving tests below cover
    /// the transitions that *create* the two competing owners.
    #[test]
    fn slot_writes_are_scoped_to_the_owning_job() {
        let slot: Arc<Mutex<Option<TestStatus>>> = Arc::new(Mutex::new(None));
        let superseded = JobSlot::attach(slot.clone(), 1);
        let owner = JobSlot::attach(slot.clone(), 2);

        // The newer job (id 2) owns the slot.
        *slot.lock().unwrap() = Some(status(2, 5));
        assert!(owner.owns());
        assert!(!superseded.owns());

        // The superseded run reports progress. The guard must reject it — without it the
        // newer, genuinely-running job becomes invisible to status queries and its panel
        // reads idle while indexing is still going.
        superseded.publish(|job| status(job, 42));
        assert_eq!(
            *slot.lock().unwrap(),
            Some(status(2, 5)),
            "a superseded job (1) overwrote the slot owned by a newer job (2)"
        );

        // The owner's own write must still land, or the guard has broken progress entirely.
        owner.publish(|job| status(job, 7));
        assert_eq!(*slot.lock().unwrap(), Some(status(2, 7)));

        // The superseded run finishing must not clear the newer job's slot.
        superseded.clear();
        assert!(
            slot.lock().unwrap().is_some(),
            "a superseded job cleared the slot out from under the running one"
        );

        // The owner clearing on completion must work.
        owner.clear();
        assert!(slot.lock().unwrap().is_none(), "the owner could not clear its own slot");
    }

    /// `publish` builds its snapshot from the *owning* id, so a worker cannot write a
    /// mismatched job id into the slot even by accident — the closure never sees any other.
    #[test]
    fn publish_stamps_the_owning_job_id() {
        let slot: Arc<Mutex<Option<TestStatus>>> = Arc::new(Mutex::new(None));
        let owner = JobSlot::attach(slot.clone(), 9);
        *slot.lock().unwrap() = Some(status(9, 0));
        owner.publish(|job| status(job, 3));
        assert_eq!(*slot.lock().unwrap(), Some(status(9, 3)));
    }

    // ── The start transition ─────────────────────────────────────────────────

    /// A start claims the slot before it returns, so a status query between "command
    /// returned" and "first progress event" already sees the job running.
    #[test]
    fn a_start_claims_the_slot_before_returning() {
        let (catalog, db) = open_catalog("claim");
        let family: JobFamily<TestStatus> = JobFamily::default();

        let claim = family.begin(&catalog, |job| status(job, 0)).unwrap();

        assert_eq!(claim.db_path, db.to_path_buf());
        assert!(!claim.abort.load(Ordering::Relaxed));
        assert_eq!(family.status().unwrap(), Some(status(claim.job, 0)));
        assert!(claim.slot.owns());
        assert!(Arc::ptr_eq(&claim.abort, &family.installed().unwrap()));
    }

    /// A second start trips the first and takes the slot from it, and the superseded job's
    /// handle knows it no longer owns anything.
    #[test]
    fn a_second_start_trips_the_first_and_takes_the_slot() {
        let (catalog, _db) = open_catalog("supersede");
        let family: JobFamily<TestStatus> = JobFamily::default();

        let first = family.begin(&catalog, |job| status(job, 0)).unwrap();
        let second = family.begin(&catalog, |job| status(job, 0)).unwrap();

        assert!(first.abort.load(Ordering::Relaxed), "the superseded run must be tripped");
        assert!(!second.abort.load(Ordering::Relaxed));
        assert_ne!(first.job, second.job);
        assert!(!first.slot.owns());
        assert!(second.slot.owns());
        assert_eq!(family.status().unwrap().map(|s| s.job), Some(second.job));
    }

    /// A start with no catalog open must touch nothing: no generation installed, no job id
    /// consumed, slot untouched. This is the state a start blocked mid-switch observes, and
    /// it is what keeps a switch's abort signal from being stranded behind a fresh live flag.
    #[test]
    fn a_rejected_start_touches_nothing() {
        let catalog: Mutex<Option<Catalog>> = Mutex::new(None);
        let family: JobFamily<TestStatus> = JobFamily::default();
        let before = family.installed().unwrap();

        let err = family.begin(&catalog, |job| status(job, 0)).unwrap_err();

        assert_eq!(err, "No catalog is open");
        assert!(Arc::ptr_eq(&before, &family.installed().unwrap()));
        assert_eq!(family.abort().job_ids_issued(), 0, "a rejected start consumed a job id");
        assert!(family.status().unwrap().is_none());
    }

    /// **Forced race.** A start parked partway through its claim is *still holding the
    /// catalog lock*.
    ///
    /// This is the start-side half of the protocol. Holding the abort lock parks a start
    /// exactly at the seam, because the claim order is catalog → abort → slot: to be waiting
    /// on the abort lock it must already own the catalog lock. A claim that read the catalog
    /// under its own short-lived lock and released it before touching the abort flag would
    /// leave the catalog lock free here — and that shape admits the interleaving this whole
    /// module exists to exclude: snapshot catalog A, let a switch trip every generation, then
    /// install a live generation nothing can reach.
    ///
    /// One failed `try_lock` would prove nothing — even the bad shape holds the catalog lock
    /// for the microseconds it takes to copy two paths out. The observation that
    /// discriminates is that the lock stays held, which only happens when the holder is
    /// parked while owning it.
    #[test]
    fn a_blocked_start_keeps_holding_the_catalog_lock() {
        let (catalog, db) = open_catalog("blocked");
        let family: JobFamily<TestStatus> = JobFamily::default();

        // Take the lock the claim needs second, then park a start behind it.
        let abort_held = family.abort().lock().unwrap();

        std::thread::scope(|scope| {
            let start = scope.spawn(|| family.begin(&catalog, |job| status(job, 0)));

            assert!(
                catalog_stays_locked(&catalog, Duration::from_millis(100), Duration::from_secs(5)),
                "a start waiting on the abort lock must still be holding the catalog lock; \
                 one that reads the catalog and releases it first leaves a window in which a \
                 switch can trip every generation and still be overtaken by this start"
            );

            // Release it and confirm the claim it was parked in the middle of completes
            // normally — the test observes a stalled transition, it does not break one.
            drop(abort_held);
            let claim = start.join().unwrap().unwrap();
            assert_eq!(claim.db_path, db.to_path_buf());
            assert!(Arc::ptr_eq(&claim.abort, &family.installed().unwrap()));
            assert!(claim.slot.owns());
        });
    }

    /// Wait until `catalog` has been locked *continuously* for `window`, giving up after
    /// `timeout`. `try_lock` never blocks, so a caller holding an abort lock can probe from
    /// here without inverting the catalog → abort order.
    fn catalog_stays_locked(
        catalog: &Mutex<Option<Catalog>>,
        window: Duration,
        timeout: Duration,
    ) -> bool {
        let give_up = Instant::now() + timeout;
        while Instant::now() < give_up {
            if catalog.try_lock().is_err() {
                let until = Instant::now() + window;
                let mut still_locked = true;
                while Instant::now() < until {
                    if catalog.try_lock().is_ok() {
                        still_locked = false;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                if still_locked {
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        false
    }

    // ── The switch transitions ───────────────────────────────────────────────
    //
    // These drive a REAL registry field rather than the `TestStatus` stand-in above, because
    // the transitions they exercise are the registry's, not one family's. They used to pick
    // Smart Tagging for that and were `#[cfg(feature = "smarttags")]` as a result, so
    // `--no-default-features` — the configuration where a switch has the fewest families to
    // reach and the most room for one to be missed — ran none of them. Identity repair (#34)
    // is the first slot-publishing family that is always compiled, so they run everywhere
    // now.

    /// Start the identity repair family, the way `commands::storage`'s
    /// `begin_identity_repair_job` does.
    fn begin_identity(
        registry: &JobRegistry,
        catalog: &Mutex<Option<Catalog>>,
    ) -> Result<JobClaim<IdentityRepairJobStatus>, String> {
        registry
            .identity
            .begin(catalog, |job| IdentityRepairJobStatus { job, done: 0, total: 0 })
    }

    /// Phase one trips the running job **and** clears its status slot; phase two installs a
    /// fresh generation without reviving the old one or resurrecting the slot.
    ///
    /// Driven through the registry rather than through a command, so it pins the shared
    /// transition every family now goes through.
    #[test]
    fn a_detach_trips_and_unpublishes_every_running_job() {
        let (catalog, _db) = open_catalog("detach");
        let registry = JobRegistry::default();
        let scan = registry.scan.install_fresh().unwrap();
        let sharpness = registry.sharpness.install_fresh().unwrap();
        let burst = registry.burst.install_fresh_if_newer(registry.burst.next_job_id()).unwrap().unwrap();
        #[cfg(feature = "slideshow")]
        let (slideshow, _) = registry.slideshow.install_fresh_numbered().unwrap();
        #[cfg(feature = "localsend")]
        let (localsend, _) = registry.localsend.install_fresh_numbered().unwrap();
        let uploads = [&registry.upload_flickr, &registry.upload_smugmug, &registry.upload_instagram]
            .map(|g| g.join_numbered().unwrap().0);

        let identity = begin_identity(&registry, &catalog).unwrap();
        #[cfg(feature = "smarttags")]
        let smarttags = registry
            .smarttags
            .begin(&catalog, |job| SmarttagsJobStatus { job, done: 0, total: 0 })
            .unwrap();

        registry.lock_for_detach().unwrap().trip_and_clear_all();

        assert!(scan.load(Ordering::Relaxed), "phase one must trip the scan generation");
        assert!(sharpness.load(Ordering::Relaxed), "phase one must trip sharpness too");
        assert!(burst.load(Ordering::Relaxed), "and a burst analysis");
        #[cfg(feature = "slideshow")]
        assert!(slideshow.load(Ordering::Relaxed), "and a slideshow render");
        #[cfg(feature = "localsend")]
        assert!(localsend.load(Ordering::Relaxed), "and a LocalSend send");
        assert!(uploads.iter().all(|u| u.load(Ordering::Relaxed)), "and every publish upload");
        assert!(identity.abort.load(Ordering::Relaxed), "and the identity repair pass");
        assert!(
            registry.identity.status().unwrap().is_none(),
            "phase one must clear the status slot, or the panel re-adopts a dead job"
        );
        assert!(!identity.slot.owns());
        #[cfg(feature = "smarttags")]
        {
            assert!(smarttags.abort.load(Ordering::Relaxed));
            assert!(registry.smarttags.status().unwrap().is_none());
            assert!(!smarttags.slot.owns());
        }

        let fresh_scan = registry.lock_for_publish().unwrap().trip_and_replace_all();
        assert!(!fresh_scan.load(Ordering::Relaxed), "the new generation must start clean");
        assert!(!Arc::ptr_eq(&fresh_scan, &scan), "phase two must not reuse the tripped flag");
        assert!(scan.load(Ordering::Relaxed), "the old worker's flag must stay tripped");
        assert!(
            registry.identity.status().unwrap().is_none(),
            "phase two must not resurrect a slot phase one cleared"
        );
        #[cfg(feature = "smarttags")]
        assert!(registry.smarttags.status().unwrap().is_none());
    }

    /// **Forced.** A catalog switch trips the develop claim, clears its slot, **and releases
    /// its working image** — the token it minted would otherwise still answer, naming a
    /// photo id that means something else in the next catalog.
    #[cfg(all(feature = "raw", feature = "edit"))]
    #[test]
    fn a_switch_releases_the_develop_working_image() {
        use crate::develop::{resident, resident_bytes, serial, session, test_image};
        use crate::plugins::edit::SourceToken;
        let _serial = serial();
        let (catalog, _db) = open_catalog("develop");
        let registry = JobRegistry::default();
        let claim = registry
            .develop
            .begin(&catalog, |job| super::super::DevelopStatus { job, photo_id: 7, generation: job, resident: false })
            .unwrap();
        let token = SourceToken::Working { photo_id: 7, generation: claim.job };
        assert_eq!(session::publish(&claim, 7, &token, test_image(8, 8)), session::Published::Resident);
        assert!(resident(&token).is_some());

        registry.lock_for_detach().unwrap().trip_and_clear_all();

        assert!(claim.abort.load(Ordering::Relaxed), "the decode is told to stop");
        assert!(registry.develop.status().unwrap().is_none(), "the slot is cleared");
        assert!(resident(&token).is_none(), "the image is released with the slot");
        assert_eq!(resident_bytes(), 0);
        // A straggler that decoded across the switch cannot make itself resident either.
        assert_eq!(session::publish(&claim, 7, &token, test_image(8, 8)), session::Published::Superseded);
        assert!(resident(&token).is_none());
    }

    /// **Forced interleaving** (#133). Another test's switch — its own `AppState`, its own
    /// thread — runs phase one while a develop test holds images resident. The resident set
    /// is process-global, so before phase one took `develop::serial()` this released the
    /// develop test's image (shown with a throwaway test that joined the switch thread
    /// between `publish` and the check: it failed 3 runs of 3). Now the switch waits for the
    /// develop test, and releases only after it.
    #[cfg(all(feature = "raw", feature = "edit"))]
    #[test]
    fn another_tests_switch_waits_for_a_develop_test() {
        use crate::develop::{resident, serial, session, test_image};
        use crate::plugins::edit::SourceToken;
        let serial = serial();
        let (catalog, _db) = open_catalog("develop-parallel-switch");
        let registry = JobRegistry::default();
        let claim = registry
            .develop
            .begin(&catalog, |job| super::super::DevelopStatus { job, photo_id: 7, generation: job, resident: false })
            .unwrap();
        let token = SourceToken::Working { photo_id: 7, generation: claim.job };
        assert_eq!(session::publish(&claim, 7, &token, test_image(8, 8)), session::Published::Resident);
        let switch = std::thread::spawn(|| {
            let state = crate::app::AppState::default();
            crate::app::catalogs::detach_catalog_and_trip_jobs(&state).unwrap();
        });
        // Long enough for an unblocked switch to finish many times over. Under extreme load
        // this can only pass falsely (switch thread not yet scheduled), never fail falsely.
        std::thread::sleep(Duration::from_millis(200));
        assert!(!switch.is_finished(), "the switch did not wait for the develop test");
        assert!(resident(&token).is_some(), "another test's switch released this test's image");
        drop(serial);
        switch.join().unwrap();
        assert!(resident(&token).is_none(), "once the develop test is done, the switch releases");
    }

    /// **Forced race.** A superseded worker's straggler, arriving after a switch has cleared
    /// its slot, must not re-publish it.
    ///
    /// Cleared reads as `None`, and `None` is nobody's slot, so the ownership guard rejects
    /// the write. Without that, a worker that emits one more progress event between the
    /// switch and noticing its abort flag would put the replaced catalog's job back on
    /// display against the new catalog — exactly the #51 symptom, re-entered through the
    /// worker instead of through the switch.
    ///
    /// The interleaving is forced by construction, not by timing: the switch is driven
    /// synchronously and the straggler is then delivered by hand.
    #[test]
    fn a_straggler_cannot_republish_a_slot_a_switch_cleared() {
        let (catalog, _db) = open_catalog("straggler");
        let registry = JobRegistry::default();
        let claim = begin_identity(&registry, &catalog).unwrap();

        registry.lock_for_detach().unwrap().trip_and_clear_all();

        // The worker has not noticed its flag yet and emits one more progress event.
        claim.slot.publish(|job| IdentityRepairJobStatus { job, done: 99, total: 100 });
        assert!(
            registry.identity.status().unwrap().is_none(),
            "a straggler re-published a slot the switch had already cleared"
        );

        // And its terminal clear is a no-op rather than an error.
        claim.slot.clear();
        assert!(registry.identity.status().unwrap().is_none());
    }

    /// **Forced race.** A start that lands between the two switch phases finds no catalog,
    /// touches nothing, and does not survive as a live generation nothing can reach.
    #[test]
    fn a_start_between_the_switch_phases_touches_nothing() {
        let (catalog, _db) = open_catalog("between");
        let registry = JobRegistry::default();

        registry.lock_for_detach().unwrap().trip_and_clear_all();
        *catalog.lock().unwrap() = None; // phase one drops the handle

        let before = registry.identity.installed().unwrap();
        let err = begin_identity(&registry, &catalog).unwrap_err();

        assert_eq!(err, "No catalog is open");
        assert!(Arc::ptr_eq(&before, &registry.identity.installed().unwrap()));
        assert!(before.load(Ordering::Relaxed), "the installed generation is still the tripped one");
        assert_eq!(registry.identity.abort().job_ids_issued(), 0);
    }

    /// **Forced race, real threads.** Starts and switches running concurrently must never
    /// leave a live generation that is not the installed one, and never a live generation
    /// against a catalog the switch has already left.
    ///
    /// The invariant holds under every legal interleaving, so the assertion never depends on
    /// which one the scheduler picks — the deterministic tests above carry the load and this
    /// is a net over the rest. Checked after every round, so a bad install cannot be papered
    /// over by the next round's abort.
    #[test]
    fn concurrent_starts_and_switches_leave_no_unreachable_generation() {
        let dir = crate::test_support::TestTmpDir::new("jobs-race");
        let root_a = dir.join("photos-a");
        let root_b = dir.join("photos-b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        let db_a = dir.join("a.chairphoto");
        let db_b = dir.join("b.chairphoto");
        let catalog = Mutex::new(Some(Catalog::open(&db_a, &root_a).unwrap()));
        let registry = JobRegistry::default();

        let started: Mutex<Vec<(PathBuf, Arc<AtomicBool>)>> = Mutex::new(Vec::new());
        // One uncontended start, so the check below cannot be vacuous even if every racing
        // start lands in the mid-switch window.
        let first = begin_identity(&registry, &catalog).unwrap();
        started.lock().unwrap().push((first.db_path.clone(), first.abort.clone()));

        for round in 0..50 {
            let (next_db, next_root) =
                if round % 2 == 0 { (&db_b, &root_b) } else { (&db_a, &root_a) };
            std::thread::scope(|s| {
                s.spawn(|| {
                    if let Ok(c) = begin_identity(&registry, &catalog) {
                        started.lock().unwrap().push((c.db_path, c.abort));
                    }
                });
                s.spawn(|| {
                    // Phase one: trip and clear under the catalog lock, then drop the handle.
                    let mut guard = catalog.lock().unwrap();
                    registry.lock_for_detach().unwrap().trip_and_clear_all();
                    *guard = None;
                    drop(guard);
                    // Phase two: publish the reopened catalog with fresh generations.
                    let reopened = Catalog::open(next_db, next_root).unwrap();
                    let mut guard = catalog.lock().unwrap();
                    registry.lock_for_publish().unwrap().trip_and_replace_all();
                    *guard = Some(reopened);
                });
            });

            let installed = registry.identity.installed().unwrap();
            let open_db =
                catalog.lock().unwrap().as_ref().map(|c| c.db_path().to_path_buf());
            for (i, (db, abort)) in started.lock().unwrap().iter().enumerate() {
                if abort.load(Ordering::Relaxed) {
                    continue;
                }
                assert!(
                    Arc::ptr_eq(abort, &installed),
                    "round {round}: start #{i} left a live generation that no cancel or \
                     switch can reach"
                );
                assert_eq!(
                    Some(db.clone()),
                    open_db,
                    "round {round}: start #{i} is still live against a catalog the switch \
                     has left"
                );
            }
        }
    }

    // ── Slot-less families ───────────────────────────────────────────────────

    /// `install_fresh` is the whole start transition for a slot-less family: it trips the
    /// previous generation and hands back a fresh one that is the installed generation.
    #[test]
    fn install_fresh_trips_the_previous_generation() {
        let generation = AbortGeneration::default();
        let first = generation.install_fresh().unwrap();
        let second = generation.install_fresh().unwrap();

        assert!(first.load(Ordering::Relaxed), "the previous generation must be tripped");
        assert!(!second.load(Ordering::Relaxed));
        assert!(Arc::ptr_eq(&second, &generation.installed().unwrap()));

        // A Cancel trips the installed generation only — the superseded one is already gone.
        generation.trip().unwrap();
        assert!(second.load(Ordering::Relaxed));
    }

    /// Ids are per family, start at 1 and never repeat, so `0` is never a live job and two
    /// families cannot collide.
    #[test]
    fn job_ids_are_monotonic_and_start_at_one() {
        let generation = AbortGeneration::default();
        assert_eq!(generation.job_ids_issued(), 0);
        assert_eq!(generation.next_job_id(), 1);
        assert_eq!(generation.next_job_id(), 2);
        assert_eq!(generation.job_ids_issued(), 2);
    }
}
