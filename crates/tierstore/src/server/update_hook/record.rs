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
//!   when either release lacks the `handover-v1` capability, or the params say `mode = "restart"`).
//! - `mode_reason`: why `prepare` chose the mode ([`ModeReason`]): `handover_capable`,
//!   `not_handover_capable` or `declared_restart`; absent in a record no `prepare` has written.
//! - `boot_id`: the boot of the host at the last successful `verify`; `guard_placed`: this update
//!   placed the guard table.
//! - `markers`: `swapped` (the redirect names v-next and v-next answers through the public
//!   port), `verified` (a `verify` succeeded), `stopping` (`cleanup` has begun to retire
//!   v-current: the commit; nothing is reversible from here). Each needs the one before it. After
//!   `cleanup` has finished, `previous` is gone and the three markers stay, so that the later
//!   `verify` knows it is watching a promotion; the next `prepare` starts them afresh.
//!
//! Each marker is written relative to its act as follows. `swapped` follows its act (the swap and
//! the probe have happened; a crash before it re-runs `cutover`, which only re-applies). `verified`
//! follows its act (the observations passed), together with `boot_id`. `stopping` precedes its act
//! (it is written, synced, and only then is the link removed and the unit stopped), so a crash at
//! any later point leaves the commit on record. `guard_placed` follows the loading of the guard
//! (a crash between leaves a guard that nothing removes, which is the safe way round).
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

/// Why `prepare` chose the mode: a token, in the spelling of the report's reasons.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModeReason {
    /// Both releases list `handover-v1`: a handover.
    HandoverCapable,
    /// A release lacks `handover-v1`: a restart.
    NotHandoverCapable,
    /// The params say `mode = "restart"`: a restart, whatever the releases can do.
    DeclaredRestart,
}

impl ModeReason {
    /// Every reason.
    pub const ALL: [ModeReason; 3] = [
        ModeReason::HandoverCapable,
        ModeReason::NotHandoverCapable,
        ModeReason::DeclaredRestart,
    ];

    /// The word in the record.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ModeReason::HandoverCapable => "handover_capable",
            ModeReason::NotHandoverCapable => "not_handover_capable",
            ModeReason::DeclaredRestart => "declared_restart",
        }
    }

    /// The mode this reason chooses.
    #[must_use]
    pub const fn mode(self) -> Mode {
        match self {
            ModeReason::HandoverCapable => Mode::Handover,
            ModeReason::NotHandoverCapable | ModeReason::DeclaredRestart => Mode::Restart,
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
    /// Why `prepare` chose `mode`; `None` before any `prepare` has.
    #[serde(default)]
    pub mode_reason: Option<ModeReason>,
    /// What has been done.
    pub markers: BTreeSet<Marker>,
    /// The boot of the host when `verify` last observed the new instance: `cleanup` commits only
    /// in the same boot, since a reboot sends `restore` to the previous slot.
    #[serde(default)]
    pub boot_id: Option<String>,
    /// Whether this update placed the guard table (it was not there before): `rollback` and a
    /// deferred `prepare` remove what they placed, and leave what was there.
    #[serde(default)]
    pub guard_placed: bool,
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
            mode_reason: None,
            markers: BTreeSet::new(),
            boot_id: None,
            guard_placed: false,
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
            "mode_reason": self.mode_reason.map(ModeReason::as_str),
            "markers": markers,
            "boot_id": self.boot_id,
            "guard_placed": self.guard_placed,
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
