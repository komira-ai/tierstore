//! The record of a single-instance host: `state.json` in the hook's state directory.
//!
//! ```json
//! {"schema":1,"kind":"single","updating":true,"markers":["swapped"]}
//! ```
//!
//! - `kind`: always `single`. A record of the two-slot hook of Linux has no such key and is
//!   refused here, as this one is refused there, so neither hook acts on the other's record.
//! - `updating`: an update is in flight: `prepare` has recorded it and the `previous` link names
//!   the release to go back to. `rollback` and the end of `cleanup` clear it.
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

use std::collections::BTreeSet;

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
#[serde(deny_unknown_fields)]
pub struct SingleRecord {
    /// The format; see [`SCHEMA`].
    pub schema: u32,
    /// Always [`Kind::Single`].
    pub kind: Kind,
    /// Whether an update is in flight.
    pub updating: bool,
    /// What has been done.
    pub markers: BTreeSet<Marker>,
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
        }
    }

    /// The record of an update `prepare` has just recorded.
    #[must_use]
    pub fn updating() -> SingleRecord {
        SingleRecord {
            updating: true,
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
        let value = json!({
            "schema": self.schema,
            "kind": "single",
            "updating": self.updating,
            "markers": markers,
        });
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

    /// Holds the record to the order of its markers.
    ///
    /// # Errors
    /// The first [`Invalid`] rule the record breaks: a marker without the one it follows from,
    /// or markers with no update in flight and no commit.
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
        Ok(())
    }
}
