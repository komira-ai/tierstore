//! The pause of writes: the proxy reads `_tiering/format` (at most once for each
//! [`ProxyConfig::format_refresh`](super::ProxyConfig::format_refresh)) before each write and
//! answers it with [`Error::Unavailable`] while the format pauses writes, and until it says
//! `{"epoch": 2, "phase": "active"}`, from when a write is made on the entry of its key, or
//! `{"phase": "active-marks", "marks_bucket": ...}`, from when a put is made on the marker of its
//! key (`marked_put.rs`). Reads are never paused; a read that misses the hot tier reads the phase
//! from the same kept answer (`mark_reads.rs`). Which of the two a put takes is decided here, once
//! for the put, from the one read of the format object: [`WriteRoute`].

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Mutex as AsyncMutex;
use tokio::time::Instant;

use super::{Inner, locked, refuse_reserved};
use crate::format::{Format, FormatPhase, read_format};
use crate::marks::Markers;
use crate::{Backend, Error};

/// What the proxy knows of the phase of the format object.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Known {
    /// No read of the object has succeeded yet.
    #[default]
    Unread,
    /// The last read that succeeded found no object.
    Missing,
    /// The last read that succeeded found this phase.
    Phase(FormatPhase),
}

/// What the proxy last read of the format object.
#[derive(Default)]
pub(super) struct Pause {
    /// When the object was last read (or tried), by the monotonic clock of the proxy.
    read: Option<Instant>,
    /// Whether the last object read paused writes. Kept when a read fails.
    paused: bool,
    /// Whether the last object read says that writes are made on entries. Kept when a read fails.
    entries: bool,
    /// The phase the last object read says. Kept when a read fails.
    phase: Known,
    /// The marks bucket the last object read says writes are made in, if it says that they are
    /// made on markers (`active-marks` at epoch 3 or later, with the bucket named). Kept when a
    /// read fails.
    marks: Option<String>,
}

impl Pause {
    /// Keeps what `format` (none if there is no object) says.
    fn keep(&mut self, format: Option<&Format>) {
        self.paused = format.is_some_and(Format::pauses_writes);
        self.entries = format.is_some_and(Format::runs_entries);
        self.phase = format.map_or(Known::Missing, |format| Known::Phase(format.phase));
        self.marks = format.and_then(Format::runs_marks).map(str::to_owned);
    }

    fn state(&self) -> FormatState {
        FormatState {
            paused: self.paused,
            entries: self.entries,
            phase: self.phase,
            marks: self.marks.clone(),
        }
    }
}

/// What a read of the format object, kept for up to `format_refresh`, says to a proxy.
#[derive(Clone, Debug)]
pub(super) struct FormatState {
    /// Writes are paused (`drain` or `drained`).
    pub(super) paused: bool,
    /// Writes are made on entries (`active` at epoch 2 or later).
    pub(super) entries: bool,
    /// The phase the object names.
    pub(super) phase: Known,
    /// The format opens writes on markers, in this marks bucket.
    pub(super) marks: Option<String>,
}

impl FormatState {
    /// Whether a read that missed the hot tier reads the marker of its key
    /// (`docs/design/put-without-lock.md`, 5.7). It does in every phase from which a marker can be
    /// in force or can still be being run to completion (`drain`, `drained`, `active-marks`, and a
    /// phase this release does not know), and when no read of the object has succeeded yet, so
    /// that a proxy that cannot see the format does not serve the cold copy of a deleted key. It
    /// does not when the object is missing or says `active`: the entry protocol is in force, no
    /// marker exists, and the marks bucket need not.
    pub(super) fn reads_markers(&self) -> bool {
        !matches!(
            self.phase,
            Known::Missing | Known::Phase(FormatPhase::Active)
        )
    }

    /// Whether the markers are in force (`active-marks`): the only phase in which a read writes
    /// one.
    pub(super) fn marks_in_force(&self) -> bool {
        self.phase == Known::Phase(FormatPhase::ActiveMarks)
    }
}

/// The refusal of a write that waits for the format to say `active` at epoch 2.
fn waits_for_entries() -> Error {
    let why = "writes wait until the format object says active at epoch 2 \
               (tierstore format-epoch --activate); reads are served";
    Error::Unavailable(why.to_owned())
}

/// How a write of a key is made, from the format object.
pub(super) enum WriteRoute<'a> {
    /// On the entry of the key (`entry_put.rs`).
    Entries,
    /// On the marker of the key (`marked_put.rs`), by the calls of these markers.
    Marks(&'a Arc<Markers>),
}

/// Reads the format object of `hot` now, within `limit`, and keeps what it says as a read of a write
/// does, so that the proxy sees a change of the format at its next heartbeat even if no write or
/// read comes. The read goes through `gate`, the one that the reads of the writes go through, so
/// that reads are made one at a time and an older read never replaces a newer one. A read that
/// fails or runs out of time (the wait for the gate counts) changes nothing: what was kept stays,
/// and the next write reads the object itself.
pub(super) async fn refresh(
    pause: &Mutex<Pause>,
    gate: &AsyncMutex<()>,
    hot: &dyn Backend,
    limit: Duration,
) {
    // The gate is held until the answer is kept, so no other read can be kept between this read
    // and its publication, and an older read never replaces a newer one.
    let read = async {
        let _gate = gate.lock().await;
        let at = Instant::now();
        let format = read_format(hot).await?;
        let mut seen = locked(pause);
        seen.read = Some(at);
        seen.keep(format.as_ref());
        Ok::<(), Error>(())
    };
    // A read that fails or runs out of time changes nothing.
    let _ = tokio::time::timeout(limit, read).await;
}

impl Inner {
    /// Checks that a write of `key` may go on and says how it is made: the key is not reserved,
    /// writes are not paused, and the format says either that writes are made on entries, or
    /// `active-marks` with the marks bucket this proxy has.
    pub(super) async fn route_write(&self, key: &str) -> Result<WriteRoute<'_>, Error> {
        refuse_reserved(key)?;
        let state = self.format_state().await;
        if state.paused {
            let why = "writes are paused while the store changes format; retry later";
            return Err(Error::Unavailable(why.to_owned()));
        }
        if let Some(recorded) = state.marks {
            return self.marks_route(&recorded);
        }
        if !state.entries {
            return Err(waits_for_entries());
        }
        Ok(WriteRoute::Entries)
    }

    /// The route of a write under `active-marks`, for a format that records the marks bucket
    /// `recorded`: marks if this proxy has the markers of that bucket, and else a refusal, so
    /// that two names are never in use at once.
    fn marks_route(&self, recorded: &str) -> Result<WriteRoute<'_>, Error> {
        let mine = self.config.marks_name.as_deref();
        match (&self.markers, mine) {
            (Some(markers), Some(mine)) if mine == recorded => Ok(WriteRoute::Marks(markers)),
            // A proxy with no marks bucket does not act on the phase: its refusal is the one it
            // always gave.
            (None, _) => Err(waits_for_entries()),
            _ => {
                let why = format!(
                    "the format record puts the markers in the bucket {recorded:?}, which is not \
                     this proxy's marks bucket ({mine:?}); writes are refused until \
                     hot_store.marks_bucket is the recorded one"
                );
                #[cfg(feature = "server")]
                tracing::warn!("tierstore: {why}");
                Err(Error::Unavailable(why))
            }
        }
    }

    /// Whether the format says `active`, so that a read of a cold object asks for a copy-back on
    /// an entry. Reads are never paused.
    pub(super) async fn entries_active(&self) -> bool {
        self.format_state().await.entries
    }

    /// What the format object says, as of a read of it that is at most `format_refresh` old.
    pub(super) async fn format_state(&self) -> FormatState {
        if let Some(state) = self.fresh_format_state() {
            return state;
        }
        // One caller reads at a time, and the freshness is checked again once the gate is held, so
        // a caller that waited uses the read of the caller before it.
        let _gate = self.format_gate.lock().await;
        if let Some(state) = self.fresh_format_state() {
            return state;
        }
        let now = Instant::now();
        let found = read_format(self.hot.as_ref()).await;
        let mut seen = locked(&self.pause);
        seen.read = Some(now);
        if let Ok(format) = found {
            seen.keep(format.as_ref());
        }
        seen.state()
    }

    /// What the last read of the format object says, if it is at most `format_refresh` old.
    fn fresh_format_state(&self) -> Option<FormatState> {
        let seen = locked(&self.pause);
        seen.read
            .is_some_and(|at| Instant::now().duration_since(at) < self.config.format_refresh)
            .then(|| seen.state())
    }
}
