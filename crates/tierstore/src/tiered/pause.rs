//! The pause of writes: the proxy reads `_tiering/format` (at most once for each
//! [`ProxyConfig::format_refresh`](super::ProxyConfig::format_refresh)) before each write and
//! answers it with [`Error::Unavailable`] while the format pauses writes. Reads are never paused.

use tokio::time::Instant;

use super::{Inner, locked, refuse_reserved};
use crate::Error;
use crate::format::read_format;

/// What the proxy last read of the format object.
#[derive(Default)]
pub(super) struct Pause {
    /// When the object was last read (or tried), by the monotonic clock of the proxy.
    read: Option<Instant>,
    /// Whether the last object read paused writes. Kept when a read fails.
    paused: bool,
}

impl Inner {
    /// Checks that a write of `key` may go on: the key is not reserved and writes are not paused.
    pub(super) async fn admit_write(&self, key: &str) -> Result<(), Error> {
        refuse_reserved(key)?;
        if self.writes_paused().await {
            let why = "writes are paused while the store changes format; retry later";
            return Err(Error::Unavailable(why.to_owned()));
        }
        Ok(())
    }

    async fn writes_paused(&self) -> bool {
        let now = Instant::now();
        let (read, paused) = {
            let seen = locked(&self.pause);
            (seen.read, seen.paused)
        };
        if read.is_some_and(|at| now.duration_since(at) < self.config.format_refresh) {
            return paused;
        }
        let found = read_format(self.hot.as_ref()).await;
        let mut seen = locked(&self.pause);
        seen.read = Some(now);
        if let Ok(format) = found {
            seen.paused = format.is_some_and(|format| format.pauses_writes());
        }
        seen.paused
    }
}
