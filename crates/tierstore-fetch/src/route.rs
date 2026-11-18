//! Which source of a location serves a part (4.6, 10.6): the first choice, the alternative a
//! retry goes to, and whether a demand part may be hedged. Pure: a location, the copies the fill
//! excluded, and what the devices last said.
//!
//! - **First choice:** healthy before unhealthy (a device `stalled`, `failed`, `down`, `suspect`,
//!   `out` or `indexing`, or one that answered `503` within the busy window, is unhealthy), then
//!   a promoted copy, then hot copies, then cold extents and the legacy upstream, then a degraded
//!   read; among equals the lowest advertised queue wait, then the location's order. So a cold
//!   shard on a stalled drive goes to the reconstructor, and a hot read goes to the least-loaded
//!   copy.
//! - **Alternative** (a retry after a stall, a transport error, a `503` or a bad cell): the same
//!   order among sources on another device; else the same source again (a fresh connection).
//! - **Hedge** (only `demand`): another hot copy whose advertised queue wait is not longer than
//!   the current device's (the first choice is already the least-loaded copy, so "shorter" holds
//!   only once the current device's wait has grown; at equal waits the hedge routes around the
//!   one slow device that has not answered), or, for a cold range of at most `degraded_hedge_max`, a degraded read
//!   whose `k` source drives each advertise no queued request; never when either device is in a
//!   decrease state.

use std::collections::{BTreeMap, BTreeSet};

use tierstore_fetch_api::upstream::{
    DeviceRef, DriveSignals, DriveState, Location, Source, SourceKind,
};

/// What the engine last learned about one device.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeviceInfo {
    /// `x-ts-queue-wait-ms` of its last response or ping.
    pub queue_wait_ms: Option<u32>,
    /// `x-ts-drive-queue`.
    pub drive_queue: Option<u32>,
    /// `x-ts-drive-state`.
    pub state: Option<DriveState>,
    /// It answered `503` (a decrease signal): treated as in a decrease state until then.
    pub busy_until: u64,
}

/// What the engine knows of every device it has heard from.
#[derive(Debug, Default, Clone)]
pub struct DeviceBook {
    devices: BTreeMap<DeviceRef, DeviceInfo>,
}

impl DeviceBook {
    /// Takes in a response's or a ping's signals; a field the answer left out keeps its value.
    pub fn signals(&mut self, device: DeviceRef, s: DriveSignals) {
        let d = self.devices.entry(device).or_default();
        d.queue_wait_ms = s.queue_wait_ms.or(d.queue_wait_ms);
        d.drive_queue = s.drive_queue.or(d.drive_queue);
        d.state = s.drive_state.or(d.state);
    }

    /// `device` answered `503` at `now`: in a decrease state until `until`.
    pub fn busy(&mut self, device: DeviceRef, until: u64) {
        let d = self.devices.entry(device).or_default();
        d.busy_until = d.busy_until.max(until);
    }

    /// What is known of `device`.
    #[must_use]
    pub fn get(&self, device: DeviceRef) -> DeviceInfo {
        self.devices.get(&device).copied().unwrap_or_default()
    }

    /// Whether `device` serves reads and is not in a decrease state at `now`.
    #[must_use]
    pub fn healthy(&self, device: DeviceRef, now: u64) -> bool {
        let d = self.get(device);
        let state_ok = matches!(d.state, None | Some(DriveState::Up | DriveState::Drain));
        state_ok && d.busy_until <= now
    }

    /// Whether `device` is in a decrease state (it answered `503` recently) at `now`.
    #[must_use]
    pub fn decreasing(&self, device: DeviceRef, now: u64) -> bool {
        self.get(device).busy_until > now
    }
}

fn tier(kind: &SourceKind) -> u8 {
    match kind {
        SourceKind::HotClean { .. } => 0,
        SourceKind::Hot { .. } => 1,
        SourceKind::Cold { .. } | SourceKind::Legacy => 2,
        SourceKind::Degraded { .. } => 3,
    }
}

fn is_hot(kind: &SourceKind) -> bool {
    matches!(kind, SourceKind::Hot { .. } | SourceKind::HotClean { .. })
}

/// Whether `source` holds byte `offset` of the object.
fn covers(source: &Source, offset: u64) -> bool {
    let end = source.extent.end().unwrap_or(u64::MAX);
    source.extent.offset <= offset && offset < end
}

/// The sources of `location` that hold byte `offset`, by index, minus `excluded`.
fn candidates<'a>(
    location: &'a Location,
    excluded: &'a BTreeSet<usize>,
    offset: u64,
) -> impl Iterator<Item = (usize, &'a Source)> + 'a {
    location
        .sources
        .iter()
        .enumerate()
        .filter(move |(i, s)| !excluded.contains(i) && covers(s, offset))
}

fn best<'a>(
    it: impl Iterator<Item = (usize, &'a Source)>,
    book: &DeviceBook,
    now: u64,
) -> Option<usize> {
    it.min_by_key(|(i, s)| {
        let unhealthy = !book.healthy(s.device, now);
        let wait = book.get(s.device).queue_wait_ms.unwrap_or(0);
        (unhealthy, tier(&s.kind), wait, *i)
    })
    .map(|(i, _)| i)
}

/// The view a choice is made against: a fill's location, the sources it excluded, what the
/// devices last said, and the time.
#[derive(Debug, Clone, Copy)]
pub struct Routing<'a> {
    /// The fill's location.
    pub location: &'a Location,
    /// The indexes of the sources the fill excluded (another generation, a promoted copy gone).
    pub excluded: &'a BTreeSet<usize>,
    /// The devices.
    pub book: &'a DeviceBook,
    /// The time of the injected clock.
    pub now: u64,
}

impl Routing<'_> {
    /// The first choice for a part starting at `offset`.
    #[must_use]
    pub fn first(&self, offset: u64) -> Option<usize> {
        best(
            candidates(self.location, self.excluded, offset),
            self.book,
            self.now,
        )
    }

    /// The source a retry of a part at `offset` goes to after a failure on `failed`: another
    /// device's, else the first choice (`failed` itself, on a fresh connection, when it is still
    /// a candidate).
    #[must_use]
    pub fn alternative(&self, offset: u64, failed: DeviceRef) -> Option<usize> {
        let other =
            candidates(self.location, self.excluded, offset).filter(|(_, s)| s.device != failed);
        best(other, self.book, self.now).or_else(|| self.first(offset))
    }

    /// The source a hedge of a demand part of `len` bytes at `offset`, now served by `current`,
    /// goes to, if the rules of 4.6 allow one (`degraded_max`: the largest degraded hedge).
    #[must_use]
    pub fn hedge(
        &self,
        offset: u64,
        len: u64,
        current: &Source,
        degraded_max: u64,
    ) -> Option<usize> {
        let book = self.book;
        let now = self.now;
        if book.decreasing(current.device, now) {
            return None;
        }
        let current_wait = book.get(current.device).queue_wait_ms.unwrap_or(0);
        let ok = |s: &Source| {
            let d = book.get(s.device);
            let allowed = match &s.kind {
                SourceKind::Hot { .. } | SourceKind::HotClean { .. } => {
                    is_hot(&current.kind) && d.queue_wait_ms.unwrap_or(0) <= current_wait
                }
                SourceKind::Degraded { sources, .. } => {
                    matches!(current.kind, SourceKind::Cold { .. })
                        && len <= degraded_max
                        && sources
                            .iter()
                            .all(|dev| book.get(*dev).drive_queue == Some(0))
                }
                SourceKind::Cold { .. } | SourceKind::Legacy => false,
            };
            allowed && s.device != current.device && book.healthy(s.device, now)
        };
        let fit = candidates(self.location, self.excluded, offset).filter(|(_, s)| ok(s));
        best(fit, book, now)
    }
}
