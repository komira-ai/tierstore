//! The pause of writes: the proxy reads `_tiering/format` (at most once for each
//! [`ProxyConfig::format_refresh`](super::ProxyConfig::format_refresh)) before each write and
//! answers it with [`Error::Unavailable`] while the format pauses writes. Reads are never paused.
//! The same read says which way a write is made: through the journal, or, once the format says
//! `{"epoch": 2, "phase": "active"}`, on the entry of its key.

use tokio::time::Instant;

use super::{Inner, locked, refuse_reserved};
use crate::Error;
use crate::format::{Format, read_format};

/// What the proxy last read of the format object.
#[derive(Default)]
pub(super) struct Pause {
    /// When the object was last read (or tried), by the monotonic clock of the proxy.
    read: Option<Instant>,
    /// Whether the last object read paused writes. Kept when a read fails.
    paused: bool,
    /// Whether the last object read says that writes are made on entries. Kept when a read fails.
    entries: bool,
}

/// The way a write is made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WriteMode {
    /// Appended to the journal of the proxy.
    Journal,
    /// Made on the entry of the key.
    Entries,
}

impl Inner {
    /// Checks that a write of `key` may go on: the key is not reserved and writes are not paused.
    pub(super) async fn admit_write(&self, key: &str) -> Result<(), Error> {
        self.admit_write_mode(key).await.map(|_| ())
    }

    /// As [`Inner::admit_write`], and says how the write is made.
    pub(super) async fn admit_write_mode(&self, key: &str) -> Result<WriteMode, Error> {
        refuse_reserved(key)?;
        let (paused, entries) = self.format_state().await;
        if paused {
            let why = "writes are paused while the store changes format; retry later";
            return Err(Error::Unavailable(why.to_owned()));
        }
        Ok(if entries {
            WriteMode::Entries
        } else {
            WriteMode::Journal
        })
    }

    /// Whether writes are paused and whether they are made on entries, as of a read of the format
    /// object that is at most `format_refresh` old.
    async fn format_state(&self) -> (bool, bool) {
        let now = Instant::now();
        let (read, paused, entries) = {
            let seen = locked(&self.pause);
            (seen.read, seen.paused, seen.entries)
        };
        if read.is_some_and(|at| now.duration_since(at) < self.config.format_refresh) {
            return (paused, entries);
        }
        let found = read_format(self.hot.as_ref()).await;
        let mut seen = locked(&self.pause);
        seen.read = Some(now);
        if let Ok(format) = found {
            seen.paused = format.as_ref().is_some_and(Format::pauses_writes);
            seen.entries = format.as_ref().is_some_and(Format::runs_entries);
        }
        (seen.paused, seen.entries)
    }
}
