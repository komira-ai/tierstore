//! The record of a single-instance host: `state.json` in the hook's state directory.
//!
//! ```json
//! {"schema":1,"kind":"single","updating":true,"markers":["swapped"],
//!  "from":"/usr/local/tierstore/releases/a","to":"/usr/local/tierstore/releases/b","version":"0.4.0"}
//! ```
//!
//! - `kind`: always `single`. A record of the two-slot hook of Linux has no such key and is
//!   refused here, as this one is refused there, so neither hook acts on the other's record.
//! - `updating`: an update is in flight: `prepare` has recorded it. `rollback` and the end of
//!   `cleanup` clear it.
//! - `from`: while an update is in flight, the release `current` named when `prepare` first
//!   recorded it: the release a rollback goes back to. The link `previous` mirrors it for a person;
//!   the steps go by the record, not by the link.
//! - `to` and `version`: the staged release `prepare` checked and the version it must report.
//!   Every later step refuses params that name another release or version, so that no step acts
//!   on a release `prepare` did not check. They stay after the commit, for the `verify` that
//!   watches the promotion.
//! - `markers`: `swapped` (the service runs the staged release and answered with its version),
//!   `verified` (a `verify` succeeded), `stopping` (`cleanup` has begun: the commit). Each needs
//!   the one before it. After `cleanup` has finished, `updating` is false and the three markers
//!   stay, so that the later `verify` knows it is watching a promotion; the next `prepare`
//!   starts them afresh.
//!
//! Each marker is written relative to its act as the two-slot record's are: `swapped` and
//! `verified` follow their acts, `stopping` precedes its act. There is no boot to record: a
//! reboot starts the release `current` points at, which is what was running before it, so it
//! voids nothing.
//!
//! A key this hook does not know is ignored, so that a later hook may add an optional one; the
//! kind, the schema and the rules of [`SingleRecord::check`] are what a record is held to.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::json;

use crate::server::update_hook::record::{Marker, RecordError, SCHEMA, decode_schema};
use crate::server::update_hook::validate::Invalid;

/// The one kind of record this hook writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// One instance, restarted in place.
    Single,
}

/// The record of a single-instance host.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct SingleRecord {
    /// The format; see [`SCHEMA`].
    pub schema: u32,
    /// Always [`Kind::Single`].
    pub kind: Kind,
    /// Whether an update is in flight.
    pub updating: bool,
    /// What has been done.
    pub markers: BTreeSet<Marker>,
    /// The release a rollback goes back to, while an update is in flight.
    #[serde(default)]
    pub from: Option<PathBuf>,
    /// The staged release `prepare` checked.
    #[serde(default)]
    pub to: Option<PathBuf>,
    /// The version `to` must report.
    #[serde(default)]
    pub version: Option<String>,
}

impl SingleRecord {
    /// The record of a host with no update in flight.
    #[must_use]
    pub fn idle() -> SingleRecord {
        SingleRecord {
            schema: SCHEMA,
            kind: Kind::Single,
            updating: false,
            markers: BTreeSet::new(),
            from: None,
            to: None,
            version: None,
        }
    }

    /// The record of an update from `from` to `to`, which must report `version`, that `prepare`
    /// has just recorded.
    #[must_use]
    pub fn updating(from: PathBuf, to: PathBuf, version: String) -> SingleRecord {
        SingleRecord {
            updating: true,
            from: Some(from),
            to: Some(to),
            version: Some(version),
            ..SingleRecord::idle()
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
    pub fn with(&self, marker: Marker) -> SingleRecord {
        let mut next = self.clone();
        next.markers.insert(marker);
        next
    }

    /// The bytes to store: pretty JSON and a newline.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let markers: Vec<&str> = self.markers.iter().map(|m| m.as_str()).collect();
        let mut value = json!({
            "schema": self.schema,
            "kind": "single",
            "updating": self.updating,
            "markers": markers,
        });
        let paths = [("from", &self.from), ("to", &self.to)];
        for (key, path) in paths {
            if let Some(path) = path {
                value[key] = json!(path.to_string_lossy());
            }
        }
        if let Some(version) = &self.version {
            value["version"] = json!(version);
        }
        let mut text = format!("{value:#}");
        text.push('\n');
        text.into_bytes()
    }

    /// Reads stored bytes, as [`Record::decode`](crate::server::update_hook::Record::decode)
    /// does: the size and the schema first.
    ///
    /// # Errors
    /// [`RecordError`]: too big, not a record of this kind, or of another schema.
    pub fn decode(bytes: &[u8]) -> Result<SingleRecord, RecordError> {
        decode_schema(bytes)
    }

    /// Holds the record to the order of its markers and to what it must name: an update in
    /// flight names `from`, `to` and `version`; a committed one `to` and `version`; a record at
    /// rest none of them.
    ///
    /// # Errors
    /// The first [`Invalid`] rule the record breaks.
    pub fn check(&self) -> Result<(), Invalid> {
        if self.has(Marker::Verified) && !self.has(Marker::Swapped) {
            return Err(Invalid::Marks {
                mark: "verified",
                needs: "swapped",
            });
        }
        if self.has(Marker::Stopping) && !self.has(Marker::Verified) {
            return Err(Invalid::Marks {
                mark: "stopping",
                needs: "verified",
            });
        }
        if !self.updating && !self.markers.is_empty() && !self.committed() {
            return Err(Invalid::Orphan);
        }
        let named = self.to.is_some() && self.version.is_some();
        if self.updating && !(named && self.from.is_some()) {
            return Err(Invalid::Marks {
                mark: "updating",
                needs: "from, to and version",
            });
        }
        if self.committed() && !named {
            return Err(Invalid::Marks {
                mark: "stopping",
                needs: "to and version",
            });
        }
        if !self.updating && self.from.is_some() {
            return Err(Invalid::Marks {
                mark: "from",
                needs: "updating",
            });
        }
        if !self.updating && !self.committed() && (self.to.is_some() || self.version.is_some()) {
            return Err(Invalid::Marks {
                mark: "to or version",
                needs: "updating or stopping",
            });
        }
        Ok(())
    }
}
