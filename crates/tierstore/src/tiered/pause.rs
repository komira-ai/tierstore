//! The pause of writes: the proxy reads `_tiering/format` (at most once for each
//! [`ProxyConfig::format_refresh`](super::ProxyConfig::format_refresh)) before each write and
//! answers it with [`Error::Unavailable`] while the format pauses writes, and until it says
//! `{"epoch": 2, "phase": "active"}`, from when a write is made on the entry of its key. Reads are
//! never paused. It keeps too whether the format says `active-marks`, which only the plain puts of
//! `marked_put.rs` ask: the writes of the [`Backend`](crate::Backend) trait do not.

use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::Mutex as AsyncMutex;
use tokio::time::Instant;

use super::{Inner, locked, refuse_reserved};
use crate::format::{Format, read_format};
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
    /// Whether the last object read says that writes are made on markers, and if so the
    /// `marks_bucket` it names. Kept when a read fails.
    marks: Option<Option<String>>,
}

/// What the proxy knows of the format object at one moment.
struct FormatState {
    paused: bool,
    entries: bool,
    /// `Some` when the phase is `active-marks`, with the marks bucket the record names.
    marks: Option<Option<String>>,
}

impl Pause {
    fn state(&self) -> FormatState {
        FormatState {
            paused: self.paused,
            entries: self.entries,
            marks: self.marks.clone(),
        }
    }

    /// Keeps what `format` (none if there is no object) says.
    fn keep(&mut self, format: Option<&Format>) {
        self.paused = format.is_some_and(Format::pauses_writes);
        self.entries = format.is_some_and(Format::runs_entries);
        self.marks = format
            .filter(|format| format.runs_marks())
            .map(|format| format.marks_bucket.clone());
    }
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
    /// Checks that a write of `key` may go on: the key is not reserved, writes are not paused, and
    /// the format says that writes are made on entries.
    pub(super) async fn admit_write(&self, key: &str) -> Result<(), Error> {
        refuse_reserved(key)?;
        let FormatState {
            paused, entries, ..
        } = self.format_state().await;
        if paused {
            let why = "writes are paused while the store changes format; retry later";
            return Err(Error::Unavailable(why.to_owned()));
        }
        if !entries {
            let why = "writes wait until the format object says active at epoch 2 \
                       (tierstore format-epoch --activate); reads are served";
            return Err(Error::Unavailable(why.to_owned()));
        }
        Ok(())
    }

    /// Whether the format says `active`, so that a read of a cold object asks for a copy-back on
    /// an entry. Reads are never paused.
    pub(super) async fn entries_active(&self) -> bool {
        self.format_state().await.entries
    }

    /// `None` unless the format says `active-marks` (as of a read of the format object that is at
    /// most `format_refresh` old; nothing a proxy does under another format depends on it).
    /// Otherwise whether the marks bucket the record names is the one this proxy was configured
    /// with (`ProxyConfig::marks_bucket`). A proxy with no configured name
    /// compares nothing; one with a name refuses a record that names another bucket or none.
    pub(super) async fn marks_bucket_agrees(&self) -> Option<bool> {
        let named = self.format_state().await.marks?;
        Some(
            self.config
                .marks_bucket
                .as_ref()
                .is_none_or(|own| named.as_ref() == Some(own)),
        )
    }

    /// Whether writes are paused, whether they are made on entries and whether they are made on
    /// markers, as of a read of the format object that is at most `format_refresh` old.
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
