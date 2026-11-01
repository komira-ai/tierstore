//! The record: `state.json` in the hook's state directory.
//!
//! ```json
//! {"schema":1,"serving":9302,"previous":9301,"mode":"handover","markers":["swapped"]}
//! ```
//!
//! - `serving`: the private port that serves once the update is through. `prepare` sets it to the
//!   idle port, so while an update is in flight it names v-next; with no update in flight it
//!   names the one instance the redirect points at.
//! - `previous`: the port of v-current while an update is in flight, else absent. It differs
//!   from `serving`.
//! - `mode`: `handover` (two instances side by side) or `restart` (a plain restart with a gap,
//!   when either release lacks the `handover-v1` capability).
//! - `markers`: `swapped` (the redirect names v-next and v-next answers through the public
//!   port), `verified` (a `verify` succeeded), `stopping` (`cleanup` has begun to retire
//!   v-current: the commit; nothing is reversible from here). Each needs the one before it. After
//!   `cleanup` has finished, `previous` is gone and the three markers stay, so that the later
//!   `verify` knows it is watching a promotion; the next `prepare` starts them afresh.
//!
//! The record is written whole, atomically, by the [`StateDir`](super::StateDir) port. It is
//! read through [`validate`](super::validate) before anything else is done.

use std::collections::BTreeSet;

use serde::Deserialize;
use serde_json::json;

/// The schema this hook writes and reads.
pub const SCHEMA: u32 = 1;

/// The most bytes of a record that are read. A real record is under a kilobyte.
pub const RECORD_MAX: usize = 64 * 1024;

/// How the new release takes over.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Both instances run; the redirect moves; the old one is stopped after the gate.
    Handover,
    /// The old instance is stopped, the new one started: a gap.
    Restart,
}

impl Mode {
    /// The word in the record.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Mode::Handover => "handover",
            Mode::Restart => "restart",
        }
    }
}

/// A step of the update that has been done.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Marker {
    /// The redirect names v-next and v-next answered through the public port.
    Swapped,
    /// A `verify` succeeded.
    Verified,
    /// `cleanup` has begun to retire v-current: the commit.
    Stopping,
}

impl Marker {
    /// The word in the record.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Marker::Swapped => "swapped",
            Marker::Verified => "verified",
            Marker::Stopping => "stopping",
        }
    }
}

/// Why a record could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    /// More than [`RECORD_MAX`] bytes.
    #[error("the record is larger than {RECORD_MAX} bytes")]
    TooBig,
    /// Not a record.
    #[error("the record is not readable: {0}")]
    Corrupt(String),
    /// A schema newer than [`SCHEMA`]: read-only to this hook.
    #[error("the record has schema {found}, newer than {SCHEMA}")]
    Newer {
        /// The record's.
        found: u32,
    },
    /// A schema older than [`SCHEMA`]: none was ever written.
    #[error("the record has schema {found}, which this hook never wrote")]
    Older {
        /// The record's.
        found: u32,
    },
}

/// Only the schema of a record, read before the rest is trusted.
#[derive(Deserialize)]
struct Header {
    schema: u32,
}

/// The hook record.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    /// The format; see [`SCHEMA`].
    pub schema: u32,
    /// The port that serves once the update is through.
    pub serving: u16,
    /// The port of v-current while an update is in flight.
    pub previous: Option<u16>,
    /// How the new release takes over.
    pub mode: Mode,
    /// What has been done.
    pub markers: BTreeSet<Marker>,
}

impl Record {
    /// The record of a host with no update in flight, serving on `serving`.
    #[must_use]
    pub fn idle(serving: u16) -> Record {
        Record {
            schema: SCHEMA,
            serving,
            previous: None,
            mode: Mode::Handover,
            markers: BTreeSet::new(),
        }
    }

    /// Whether `cleanup` has begun: nothing is reversible.
    #[must_use]
    pub fn committed(&self) -> bool {
        self.markers.contains(&Marker::Stopping)
    }

    /// The port of v-current: `previous` while an update is in flight, else `serving`.
    #[must_use]
    pub fn current(&self) -> u16 {
        self.previous.unwrap_or(self.serving)
    }

    /// The bytes to store: pretty JSON and a newline.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let markers: Vec<&str> = self.markers.iter().map(|m| m.as_str()).collect();
        let value = json!({
            "schema": self.schema,
            "serving": self.serving,
            "previous": self.previous,
            "mode": self.mode.as_str(),
            "markers": markers,
        });
        let mut text = format!("{value:#}");
        text.push('\n');
        text.into_bytes()
    }

    /// Reads stored bytes.
    ///
    /// # Errors
    /// [`RecordError`]: too big, not a record, or of another schema.
    pub fn decode(bytes: &[u8]) -> Result<Record, RecordError> {
        if bytes.len() > RECORD_MAX {
            return Err(RecordError::TooBig);
        }
        let header: Header =
            serde_json::from_slice(bytes).map_err(|e| RecordError::Corrupt(e.to_string()))?;
        if header.schema > SCHEMA {
            return Err(RecordError::Newer {
                found: header.schema,
            });
        }
        if header.schema < SCHEMA {
            return Err(RecordError::Older {
                found: header.schema,
            });
        }
        serde_json::from_slice(bytes).map_err(|e| RecordError::Corrupt(e.to_string()))
    }
}
