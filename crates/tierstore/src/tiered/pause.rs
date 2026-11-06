//! The pause of writes: the proxy reads `_tiering/format` (at most once for each
//! [`ProxyConfig::format_refresh`](super::ProxyConfig::format_refresh)) before each write and
//! answers it with [`Error::Unavailable`] while the format pauses writes, and until it says
//! `{"epoch": 2, "phase": "active"}`, from when a write is made on the entry of its key, or
//! `{"phase": "active-marks", "marks_bucket": ...}`, from when a put is made on the marker of its
//! key (`marked_put.rs`). Reads are never paused. Which of the two a put takes is decided here,
//! once for the put, from the one read of the format object: [`WriteRoute`].

use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::Mutex as AsyncMutex;
use tokio::time::Instant;

use super::{Inner, locked, refuse_reserved};
use crate::format::{Format, read_format};
use crate::marks::Markers;
use crate::{Backend, Error};

/// What the proxy last read of the format object.
#[derive(Default)]
pub(super) struct Pause {
    /// When the object was last read (or tried), by the monotonic clock of the proxy.
    read: Option<Instant>,
    /// Whether the last object read paused writes. Kept when a read fails.
    paused: bool,
    /// Whether the last object read says that writes are made on entries. Kept when a read fails.
    entries: bool,
    /// Whether the last object read says `active-marks`, and then the name of the marks bucket it
    /// records (none if it records none). Kept when a read fails.
    marks: Option<Option<String>>,
}

impl Pause {
    /// Keeps what `format` (none if there is no object) says.
    fn keep(&mut self, format: Option<&Format>) {
        self.paused = format.is_some_and(Format::pauses_writes);
        self.entries = format.is_some_and(Format::runs_entries);
        self.marks = format
            .filter(|format| format.runs_marks())
            .map(|format| format.marks_bucket.clone());
    }

    fn state(&self) -> FormatState {
        FormatState {
            paused: self.paused,
            entries: self.entries,
            marks: self.marks.clone(),
        }
    }
}

/// What the format object says, as of a read that is at most `format_refresh` old.
struct FormatState {
    /// Writes are paused.
    paused: bool,
    /// Writes are made on entries.
    entries: bool,
    /// The format says `active-marks`, and records this marks bucket (`None`: none).
    marks: Option<Option<String>>,
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
    Marks(&'a Markers),
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
            return self.marks_route(recorded.as_deref());
        }
        if !state.entries {
            return Err(waits_for_entries());
        }
        Ok(WriteRoute::Entries)
    }

    /// The route of a write under `active-marks`, for a format that records the marks bucket
    /// `recorded`: marks if this proxy has the markers of that bucket, and else a refusal, so
    /// that two names are never in use at once.
    fn marks_route(&self, recorded: Option<&str>) -> Result<WriteRoute<'_>, Error> {
        let mine = self.config.marks_name.as_deref();
        match (&self.markers, recorded) {
            (Some(markers), Some(recorded)) if mine == Some(recorded) => {
                Ok(WriteRoute::Marks(markers))
            }
            // A proxy with no marks bucket does not act on the phase: its refusal is the one it
            // always gave.
            (None, _) => Err(waits_for_entries()),
            _ => {
                let why = format!(
                    "the format object records the marks bucket {recorded:?}, and this proxy's \
                     is {mine:?}: writes wait until hot_store.marks_bucket is the recorded one"
                );
                #[cfg(feature = "server")]
                tracing::warn!("tierstore: {why}");
                Err(Error::Unavailable(why))
            }
        }
    }

    /// Checks that a write of `key` that only the entry protocol makes (a delete) may go on: as
    /// [`Inner::route_write`], and the route is the entries. Under `active-marks` it is refused as
    /// for any phase that is not active, since no delete is made on markers yet.
    pub(super) async fn admit_write(&self, key: &str) -> Result<(), Error> {
        match self.route_write(key).await? {
            WriteRoute::Entries => Ok(()),
            WriteRoute::Marks(_) => Err(waits_for_entries()),
        }
    }

    /// Whether the format says `active`, so that a read of a cold object asks for a copy-back on
    /// an entry. Reads are never paused.
    pub(super) async fn entries_active(&self) -> bool {
        self.format_state().await.entries
    }

    /// What the format object says, as of a read of it that is at most `format_refresh` old.
    async fn format_state(&self) -> FormatState {
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
