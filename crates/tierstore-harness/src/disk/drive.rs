//! A fake drive: the ops of a [`FakeDisk`] served on an injected [`ManualClock`] with scripted
//! service times, errors, hangs and crashes (section 10.7: the drive scheduler, the watchdog and
//! `stall_after`; section 14: "Group commit (fake NVMe, injected clock)", "Hung drive").
//!
//! Each request takes the next [`DriveStep`] scripted for its op kind, or the default step (an
//! `Ok` after the default service time, 0 ms unless set) when the script is empty. The op takes
//! effect on the disk when the request completes, not when it starts. While a request waits for
//! its service time it is a sleeper of the clock, so a test waits for `clock.sleepers()` to reach
//! the number of requests it expects and then advances the clock.
//!
//! - `Ok`: completes after `service_ms` and runs the op on the disk.
//! - `Eio`: completes after `service_ms` with [`DiskError::Eio`]; the op has no effect.
//! - `Hang`: never completes until [`FakeDrive::release_hangs`], then runs the op at once as an
//!   `Ok`. A hang started after a release hangs again until the next release.
//! - `Crash`: the disk crashes at once (no wait) and the request fails with
//!   [`DiskError::Crashed`].
//!
//! The drive tracks the requests in flight (started, not yet completed or dropped) with their
//! start time on the clock, the peak number in flight and the completions per op kind, and marks
//! the events `"start"` and `"done"` on a [`Progress`] for every request.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tierstore_fetch_api::clock::{Clock, ManualClock};

use super::{DiskError, FakeDisk, OpKind};
use crate::progress::{Counter, Progress};

/// What a scripted request does after its service time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveOutcome {
    /// The op runs on the disk and its result is returned.
    Ok,
    /// The request fails with [`DiskError::Eio`] and the op has no effect.
    Eio,
    /// The request never completes until [`FakeDrive::release_hangs`].
    Hang,
    /// The disk crashes and the request fails with [`DiskError::Crashed`].
    Crash,
}

/// One scripted request: its service time on the injected clock and what it does after.
///
/// A `Hang` and a `Crash` act at once, without waiting for the service time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriveStep {
    /// Milliseconds on the injected clock before the request completes.
    pub service_ms: u64,
    /// What happens then.
    pub outcome: DriveOutcome,
}

impl DriveStep {
    /// A successful request after `service_ms`.
    #[must_use]
    #[inline(never)]
    pub fn ok(service_ms: u64) -> Self {
        Self {
            service_ms,
            outcome: DriveOutcome::Ok,
        }
    }

    /// An EIO after `service_ms`.
    #[must_use]
    #[inline(never)]
    pub fn eio(service_ms: u64) -> Self {
        Self {
            service_ms,
            outcome: DriveOutcome::Eio,
        }
    }

    /// A request that hangs until released.
    #[must_use]
    #[inline(never)]
    pub fn hang() -> Self {
        Self {
            service_ms: 0,
            outcome: DriveOutcome::Hang,
        }
    }

    /// A crash of the disk.
    #[must_use]
    #[inline(never)]
    pub fn crash() -> Self {
        Self {
            service_ms: 0,
            outcome: DriveOutcome::Crash,
        }
    }
}

#[derive(Debug, Default)]
struct State {
    default_ms: u64,
    scripts: BTreeMap<OpKind, VecDeque<DriveStep>>,
    next_id: u64,
    /// Each request in flight by id: its start time on the clock.
    in_flight: BTreeMap<u64, u64>,
    peak: usize,
    completed: BTreeMap<OpKind, u64>,
}

/// A fake drive over a [`FakeDisk`], on a [`ManualClock`]. Clones share everything.
#[derive(Debug, Clone)]
pub struct FakeDrive {
    disk: FakeDisk,
    clock: ManualClock,
    progress: Progress,
    releases: Counter,
    state: Arc<Mutex<State>>,
}

/// A request in flight: removes itself from the drive's list when it ends or is dropped.
struct Flight<'a> {
    state: &'a Mutex<State>,
    id: u64,
}

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        lock(self.state).in_flight.remove(&self.id);
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl FakeDrive {
    /// A drive serving `disk` on `clock`, with no script and a default service time of 0.
    #[must_use]
    pub fn new(disk: FakeDisk, clock: ManualClock) -> Self {
        Self {
            disk,
            clock,
            progress: Progress::new(),
            releases: Counter::new(),
            state: Arc::default(),
        }
    }

    /// The disk behind the drive.
    #[must_use]
    #[inline(never)]
    pub fn disk(&self) -> &FakeDisk {
        &self.disk
    }

    /// The events `"start"` and `"done"`, marked once per request.
    #[inline(never)]
    #[must_use]
    pub fn progress(&self) -> &Progress {
        &self.progress
    }

    /// Sets the service time of a request that has no scripted step.
    pub fn set_default_service_ms(&self, ms: u64) {
        lock(&self.state).default_ms = ms;
    }

    /// Queues `steps` for the next requests of `kind`, after any already queued.
    pub fn script(&self, kind: OpKind, steps: impl IntoIterator<Item = DriveStep>) {
        lock(&self.state)
            .scripts
            .entry(kind)
            .or_default()
            .extend(steps);
    }

    /// The number of requests in flight.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        lock(&self.state).in_flight.len()
    }

    /// The clock time at which the oldest request in flight started (what a watchdog compares
    /// to `stall_after`), `None` when nothing is in flight.
    #[must_use]
    pub fn oldest_in_flight_since(&self) -> Option<u64> {
        lock(&self.state).in_flight.values().copied().min()
    }

    /// The most requests that were ever in flight at once.
    #[must_use]
    pub fn peak_in_flight(&self) -> usize {
        lock(&self.state).peak
    }

    /// How many requests of `kind` completed (with any result, a crash included).
    #[must_use]
    pub fn completed(&self, kind: OpKind) -> u64 {
        lock(&self.state).completed.get(&kind).copied().unwrap_or(0)
    }

    /// Lets every request now hanging complete as `Ok`.
    pub fn release_hangs(&self) {
        self.releases.add(1);
    }

    async fn serve<T>(
        &self,
        kind: OpKind,
        path: &str,
        op: impl FnOnce(&FakeDisk) -> Result<T, DiskError>,
    ) -> Result<T, DiskError> {
        let start = self.clock.now_ms();
        let released = self.releases.get();
        let (step, _flight) = {
            let mut s = lock(&self.state);
            let default_ms = s.default_ms;
            let step = s
                .scripts
                .get_mut(&kind)
                .and_then(VecDeque::pop_front)
                .unwrap_or_else(|| DriveStep::ok(default_ms));
            s.next_id += 1;
            let id = s.next_id;
            s.in_flight.insert(id, start);
            s.peak = s.peak.max(s.in_flight.len());
            (
                step,
                Flight {
                    state: &self.state,
                    id,
                },
            )
        };
        self.progress.mark("start");
        let result = match step.outcome {
            DriveOutcome::Hang => {
                self.releases.reached(released + 1).await;
                op(&self.disk)
            }
            DriveOutcome::Crash => {
                self.disk.crash();
                op(&self.disk)
            }
            DriveOutcome::Eio => {
                self.clock.sleep_until(start + step.service_ms).await;
                Err(DiskError::Eio {
                    op: kind,
                    path: path.to_owned(),
                })
            }
            DriveOutcome::Ok => {
                self.clock.sleep_until(start + step.service_ms).await;
                op(&self.disk)
            }
        };
        *lock(&self.state).completed.entry(kind).or_default() += 1;
        self.progress.mark("done");
        result
    }

    /// `create` as a request.
    ///
    /// # Errors
    /// As [`FakeDisk::create`], or the scripted failure.
    pub async fn create(&self, path: &str) -> Result<(), DiskError> {
        self.serve(OpKind::Create, path, |d| d.create(path)).await
    }

    /// `write_at` as a request.
    ///
    /// # Errors
    /// As [`FakeDisk::write_at`], or the scripted failure.
    pub async fn write_at(&self, path: &str, offset: usize, bytes: &[u8]) -> Result<(), DiskError> {
        self.serve(OpKind::WriteAt, path, |d| d.write_at(path, offset, bytes))
            .await
    }

    /// `read_at` as a request.
    ///
    /// # Errors
    /// As [`FakeDisk::read_at`], or the scripted failure.
    #[inline(never)]
    pub async fn read_at(
        &self,
        path: &str,
        offset: usize,
        len: usize,
    ) -> Result<Vec<u8>, DiskError> {
        self.serve(OpKind::ReadAt, path, |d| d.read_at(path, offset, len))
            .await
    }

    /// `set_len` as a request.
    ///
    /// # Errors
    /// As [`FakeDisk::set_len`], or the scripted failure.
    pub async fn set_len(&self, path: &str, len: usize) -> Result<(), DiskError> {
        self.serve(OpKind::SetLen, path, |d| d.set_len(path, len))
            .await
    }

    /// `punch_hole` as a request.
    ///
    /// # Errors
    /// As [`FakeDisk::punch_hole`], or the scripted failure.
    pub async fn punch_hole(&self, path: &str, offset: usize, len: usize) -> Result<(), DiskError> {
        self.serve(OpKind::PunchHole, path, |d| d.punch_hole(path, offset, len))
            .await
    }

    /// `fdatasync` as a request.
    ///
    /// # Errors
    /// As [`FakeDisk::fdatasync`], or the scripted failure.
    pub async fn fdatasync(&self, path: &str) -> Result<(), DiskError> {
        self.serve(OpKind::Fdatasync, path, |d| d.fdatasync(path))
            .await
    }

    /// `rename` as a request.
    ///
    /// # Errors
    /// As [`FakeDisk::rename`], or the scripted failure.
    pub async fn rename(&self, from: &str, to: &str, noreplace: bool) -> Result<(), DiskError> {
        self.serve(OpKind::Rename, from, |d| d.rename(from, to, noreplace))
            .await
    }

    /// `unlink` as a request.
    ///
    /// # Errors
    /// As [`FakeDisk::unlink`], or the scripted failure.
    pub async fn unlink(&self, path: &str) -> Result<(), DiskError> {
        self.serve(OpKind::Unlink, path, |d| d.unlink(path)).await
    }

    /// `fsync_dir` as a request.
    ///
    /// # Errors
    /// As [`FakeDisk::fsync_dir`], or the scripted failure.
    pub async fn fsync_dir(&self, dir: &str) -> Result<(), DiskError> {
        self.serve(OpKind::FsyncDir, dir, |d| d.fsync_dir(dir))
            .await
    }
}
