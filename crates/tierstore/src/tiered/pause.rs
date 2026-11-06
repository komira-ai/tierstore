//! The pause of writes: the proxy reads `_tiering/format` (at most once for each
//! [`ProxyConfig::format_refresh`](super::ProxyConfig::format_refresh)) before each write and
//! answers it with [`Error::Unavailable`] while the format pauses writes, and until it says
//! `{"epoch": 2, "phase": "active"}`, from when a write is made on the entry of its key. Reads are
//! never paused.
//!
//! A format that says `active-marks` (epoch 3 or more, with the name of the marks bucket) opens
//! the writes that are made on markers, to a proxy whose own marks bucket is that one
//! ([`Inner::write_mode`]); a proxy whose is another, or has none, answers them 503.

use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::Mutex as AsyncMutex;
use tokio::time::Instant;

use std::sync::Arc;

use super::{Inner, locked, refuse_reserved};
use crate::format::{Format, read_format};
use crate::{Backend, Error, Markers};

/// What the proxy last read of the format object.
#[derive(Default)]
pub(super) struct Pause {
    /// When the object was last read (or tried), by the monotonic clock of the proxy.
    read: Option<Instant>,
    /// Whether the last object read paused writes. Kept when a read fails.
    paused: bool,
    /// Whether the last object read says that writes are made on entries. Kept when a read fails.
    entries: bool,
    /// The marks bucket the last object read says writes are made in, if it says that they are made
    /// on markers. Kept when a read fails.
    marks: Option<String>,
}

/// What the format says of writes, as of one read.
#[derive(Clone)]
struct FormatState {
    paused: bool,
    entries: bool,
    marks: Option<String>,
}

impl Pause {
    /// Keeps what `format` (none if there is no object) says.
    fn keep(&mut self, format: Option<&Format>) {
        self.paused = format.is_some_and(Format::pauses_writes);
        self.entries = format.is_some_and(Format::runs_entries);
        self.marks = format.and_then(Format::runs_marks).map(str::to_owned);
    }

    fn state(&self) -> FormatState {
        FormatState {
            paused: self.paused,
            entries: self.entries,
            marks: self.marks.clone(),
        }
    }
}

/// Where a write of a key is made.
pub(super) enum WriteMode {
    /// On the entry of the key (format `active`).
    Entries,
    /// On the marker of the key (format `active-marks`), with the proxy's calls on markers.
    Marks(Arc<Markers>),
}

const PAUSED: &str = "writes are paused while the store changes format; retry later";
const NOT_ACTIVE: &str = "writes wait until the format object says active at epoch 2 \
                          (tierstore format-epoch --activate); reads are served";

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
    /// Checks that a write of `key` may go on: the key is not reserved, writes are not paused, and
    /// the format says that writes are made on entries.
    pub(super) async fn admit_write(&self, key: &str) -> Result<(), Error> {
        refuse_reserved(key)?;
        let state = self.format_state().await;
        if state.paused {
            return Err(Error::Unavailable(PAUSED.to_owned()));
        }
        if !state.entries {
            return Err(Error::Unavailable(NOT_ACTIVE.to_owned()));
        }
        Ok(())
    }

    /// Checks that a write of `key` that is built on markers as well as on entries may go on, and
    /// says which it is made on: the key is not reserved and writes are not paused, and the format
    /// says `active` (on entries) or `active-marks` with the name of this proxy's marks bucket (on
    /// markers). Under `active-marks` a proxy with no marks bucket, or another one, answers
    /// [`Error::Unavailable`] and logs why: two names must not be in use at once.
    pub(super) async fn write_mode(&self, key: &str) -> Result<WriteMode, Error> {
        refuse_reserved(key)?;
        let state = self.format_state().await;
        if state.paused {
            return Err(Error::Unavailable(PAUSED.to_owned()));
        }
        if state.entries {
            return Ok(WriteMode::Entries);
        }
        let Some(recorded) = state.marks else {
            return Err(Error::Unavailable(NOT_ACTIVE.to_owned()));
        };
        let own = self.config.marks_bucket.as_deref();
        match (&self.markers, own) {
            (Some(markers), Some(own)) if own == recorded => {
                Ok(WriteMode::Marks(Arc::clone(markers)))
            }
            _ => {
                let why = format!(
                    "the format record puts the markers in the bucket {recorded:?}, which is not \
                     this proxy's marks bucket; writes are refused until they are the same"
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

    /// Whether writes are paused and whether they are made on entries, as of a read of the format
    /// object that is at most `format_refresh` old.
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
