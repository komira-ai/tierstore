//! The format object of the store, `_tiering/format`: what every proxy and the reconciler read to
//! know whether writes are paused for a change of the format of what is stored.
//!
//! The body is a JSON object `{"phase": "drain"}`, `{"phase": "drained"}` or
//! `{"epoch": 2, "phase": "active"}`; a reader ignores fields it does not know and takes a phase it
//! does not know to be no pause. No object means the format of every release before this one, and
//! no pause.
//!
//! - `drain`: asked for by the operator. A proxy answers every write with
//!   [`Error::Unavailable`] (a 503 at the S3 front) and keeps serving reads; the reconciler
//!   applies the journals until each one is applied to its end.
//! - `drained`: written by the reconciler (and no one else) when it holds the lease, has seen
//!   `drain` for the settle time, and every journal it knows is applied to its end. The settle time
//!   lets a proxy whose format reads succeed read `drain` again; a proxy whose reads fail keeps its
//!   last answer, so `drained` does not show that every proxy has seen the pause. It does not wait
//!   for a write that a proxy admitted earlier and is still running. Writes stay paused.
//! - `active`: the change is done. With `"epoch": 2` or more a proxy makes its writes on the
//!   entries of their keys; it never pauses writes for it.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::journal::{FORMAT_KEY, to_body};
use crate::{Backend, Error};

/// Where the store is in a change of format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum FormatPhase {
    /// Writes are paused until the journals are applied to their end.
    Drain,
    /// The journals are applied to their end; writes stay paused.
    Drained,
    /// The change is done (a later release writes it).
    Active,
    /// Anything a newer writer adds.
    #[serde(other)]
    Unknown,
}

/// The body of `_tiering/format`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Format {
    pub(crate) phase: FormatPhase,
    /// The number of the format, set by the phase `active`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) epoch: Option<u64>,
}

impl Format {
    /// Whether a proxy that reads this answers writes with 503.
    pub(crate) fn pauses_writes(&self) -> bool {
        matches!(self.phase, FormatPhase::Drain | FormatPhase::Drained)
    }
}

/// The first epoch whose writes are made on entries.
pub(crate) const ENTRIES_EPOCH: u64 = 2;

impl Format {
    /// Whether a proxy that reads this makes its writes on entries: the phase is `active` and the
    /// epoch is [`ENTRIES_EPOCH`] or later.
    pub(crate) fn runs_entries(&self) -> bool {
        self.phase == FormatPhase::Active && self.epoch.is_some_and(|epoch| epoch >= ENTRIES_EPOCH)
    }
}

fn decode(body: &[u8]) -> Result<Format, Error> {
    serde_json::from_slice(body)
        .map_err(|error| Error::Other(format!("corrupt format object: {error}")))
}

/// The format in the hot tier, or none if the object is missing.
///
/// # Errors
/// What the hot tier gives; [`Error::Other`] if the body is not a format.
pub(crate) async fn read_format(hot: &dyn Backend) -> Result<Option<Format>, Error> {
    match hot.get(FORMAT_KEY, None).await {
        Ok(body) => decode(&body).map(Some),
        Err(Error::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

/// As [`read_format`], with the content tag of the object. The tag is read first, so that if the
/// object changes between the two calls the tag is the older one, and a write over it by that tag
/// fails.
pub(crate) async fn read_format_tagged(
    hot: &dyn Backend,
) -> Result<Option<(Format, String)>, Error> {
    let read = async {
        let tag = hot.head(FORMAT_KEY).await?.tag;
        let body = hot.get(FORMAT_KEY, None).await?;
        Ok((decode(&body)?, tag))
    };
    match read.await {
        Ok(found) => Ok(Some(found)),
        Err(Error::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

/// The body of a format object.
pub(crate) fn format_body(format: &Format) -> Bytes {
    to_body(format)
}
