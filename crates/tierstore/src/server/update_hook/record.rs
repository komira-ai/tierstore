//! The record: `state.json` in the hook's state directory ([`super::STATE_DIR`]).
//!
//! ```json
//! {"schema":1,"kind":"single","updating":true,"markers":["swapped"],
//!  "from":"/var/lib/hostkeeper/packages/tierstore/0.4.0","to":"/var/lib/hostkeeper/packages/tierstore/0.5.0",
//!  "version":"0.5.0","live":"a","next":"b","mode":"handover"}
//! ```
//!
//! One record for both service managers: the launchd arm writes the keys of the previous
//! release's single-instance record and nothing else, and the systemd arm adds three optional
//! keys. A key this hook does not know is ignored, as the previous release ignored them, so a
//! record of this hook is read by the previous release's hook and the other way round (the
//! design's rule that a hook's record is readable one version back).
//!
//! - `kind`: always `single`, the tag the previous release gave this record.
//! - `updating`: an update is in flight: `prepare` recorded it. `rollback` and the end of
//!   `cleanup` clear it.
//! - `from`: while an update is in flight, the release the host ran before
//!   (`updater.previous_release`); absent on a first install.
//! - `to` and `version`: the staged release and the version it must report. Every later step
//!   refuses params that name another release or version. They stay after the commit, for the
//!   `verify` that watches the promotion.
//! - `markers`: `swapped` (the new release serves on the public port), `verified` (a `verify`
//!   succeeded), `stopping` (`cleanup` has begun: the commit). Each needs the one before it.
//!   After `cleanup` has finished, `updating` is false and the three markers stay; the next
//!   `prepare` starts them afresh. `swapped` and `verified` follow their acts, `stopping`
//!   precedes its act.
//! - `live` (systemd only): the instance that serves when no update is in flight; while one is,
//!   the instance that served before it (absent when none did: a first install).
//! - `next` (systemd only, while an update is in flight): the instance `cutover` starts.
//! - `mode` (systemd only, while an update is in flight): `handover` (the new instance takes the
//!   listening sockets of the old) or `restart` (the old one is stopped first: a gap).
//!
//! An absent `live` with no update in flight means no instance is known to serve: launchd's one
//! job has no letter, and a systemd host that was never installed has none.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::json;

use super::ports::Instance;

/// The schema this hook writes and reads.
pub const SCHEMA: u32 = 1;

/// The most bytes of a record that are read. A real record is under a kilobyte.
pub const RECORD_MAX: usize = 64 * 1024;

/// The one kind of record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// The tag of the record, as the previous release wrote it.
    Single,
}

/// How the new release of the systemd arm takes over.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// The new instance takes the old one's listening sockets; no gap.
    Handover,
    /// The old instance is stopped, then the new one started: a gap.
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
    /// The new release answered on the public port.
    Swapped,
    /// A `verify` succeeded.
    Verified,
    /// `cleanup` has begun: the commit.
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

/// The rule a record broke.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Invalid {
    /// A key is set without the one it follows from.
    #[error("{mark} is set but {needs} is not")]
    Marks {
        /// What is set.
        mark: &'static str,
        /// What it needs.
        needs: &'static str,
    },
    /// Markers with no update in flight and no commit.
    #[error("markers are set with no update in flight")]
    Orphan,
    /// `live` and `next` name the same instance.
    #[error("live and next are the same instance")]
    Same,
}

/// Only the schema of a record, read before the rest is trusted.
#[derive(Deserialize)]
struct Header {
    schema: u32,
}

/// The hook record.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Record {
    /// The format; see [`SCHEMA`].
    pub schema: u32,
    /// Always [`Kind::Single`].
    pub kind: Kind,
    /// Whether an update is in flight.
    pub updating: bool,
    /// What has been done.
    pub markers: BTreeSet<Marker>,
    /// The release the host ran before the update in flight; `None` on a first install.
    #[serde(default)]
    pub from: Option<PathBuf>,
    /// The staged release `prepare` recorded.
    #[serde(default)]
    pub to: Option<PathBuf>,
    /// The version `to` must report.
    #[serde(default)]
    pub version: Option<String>,
    /// systemd: the instance that serves at rest, or that served before the update in flight.
    #[serde(default)]
    pub live: Option<Instance>,
    /// systemd: the instance the update in flight starts.
    #[serde(default)]
    pub next: Option<Instance>,
    /// systemd: how the update in flight takes over.
    #[serde(default)]
    pub mode: Option<Mode>,
}

impl Record {
    /// The record of a host with no update in flight, `live` serving (`None` for launchd's one
    /// job, or for a systemd host nothing serves on).
    #[must_use]
    pub fn idle(live: Option<Instance>) -> Record {
        Record {
            schema: SCHEMA,
            kind: Kind::Single,
            updating: false,
            markers: BTreeSet::new(),
            from: None,
            to: None,
            version: None,
            live,
            next: None,
            mode: None,
        }
    }

    /// Whether `cleanup` has begun: nothing is reversible.
    #[must_use]
    pub fn committed(&self) -> bool {
        self.markers.contains(&Marker::Stopping)
    }

    /// Whether `marker` is set.
    #[must_use]
    pub fn has(&self, marker: Marker) -> bool {
        self.markers.contains(&marker)
    }

    /// The same record with `marker` set.
    #[must_use]
    pub fn with(&self, marker: Marker) -> Record {
        let mut next = self.clone();
        next.markers.insert(marker);
        next
    }

    /// The bytes to store: pretty JSON and a newline. An absent optional key is left out, so a
    /// record of the launchd arm is byte for byte the previous release's single-instance record.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let markers: Vec<&str> = self.markers.iter().map(|m| m.as_str()).collect();
        let mut value = json!({
            "schema": self.schema,
            "kind": "single",
            "updating": self.updating,
            "markers": markers,
        });
        for (key, path) in [("from", &self.from), ("to", &self.to)] {
            if let Some(path) = path {
                value[key] = json!(path.to_string_lossy());
            }
        }
        if let Some(version) = &self.version {
            value["version"] = json!(version);
        }
        for (key, instance) in [("live", self.live), ("next", self.next)] {
            if let Some(instance) = instance {
                value[key] = json!(instance.letter());
            }
        }
        if let Some(mode) = self.mode {
            value["mode"] = json!(mode.as_str());
        }
        let mut text = format!("{value:#}");
        text.push('\n');
        text.into_bytes()
    }

    /// Reads stored bytes: the size and the schema first, the rest only then.
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

    /// Holds the record to the order of its markers and to what it must name: an update in
    /// flight names `to` and `version`; a committed one too; a record at rest names neither,
    /// nor `from`, `next` or `mode`.
    ///
    /// # Errors
    /// The first [`Invalid`] rule the record breaks.
    pub fn check(&self) -> Result<(), Invalid> {
        let marks = |mark, needs| Err(Invalid::Marks { mark, needs });
        if self.has(Marker::Verified) && !self.has(Marker::Swapped) {
            return marks("verified", "swapped");
        }
        if self.has(Marker::Stopping) && !self.has(Marker::Verified) {
            return marks("stopping", "verified");
        }
        if !self.updating && !self.markers.is_empty() && !self.committed() {
            return Err(Invalid::Orphan);
        }
        let named = self.to.is_some() && self.version.is_some();
        if self.updating && !named {
            return marks("updating", "to and version");
        }
        if self.committed() && !named {
            return marks("stopping", "to and version");
        }
        if !self.updating && self.from.is_some() {
            return marks("from", "updating");
        }
        if !self.updating && self.next.is_some() {
            return marks("next", "updating");
        }
        if !self.updating && self.mode.is_some() {
            return marks("mode", "updating");
        }
        if !self.updating && !self.committed() && (self.to.is_some() || self.version.is_some()) {
            return marks("to or version", "updating or stopping");
        }
        if self.live.is_some() && self.live == self.next {
            return Err(Invalid::Same);
        }
        Ok(())
    }
}
