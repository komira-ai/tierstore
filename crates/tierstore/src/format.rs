//! The format object of the store, `_tiering/format`: what every proxy and the reconciler read to
//! know whether writes are paused for a change of the format of what is stored, and whether
//! writes are made on entries.
//!
//! The body is a JSON object `{"phase": "drain"}`, `{"phase": "drained"}`,
//! `{"epoch": 2, "phase": "active"}` or
//! `{"epoch": 3, "marks_bucket": "<name>", "phase": "active-marks"}`; a reader ignores fields it
//! does not know and takes a phase it does not know to be no pause. No object means the format of
//! every release before the bridge release, and no pause; a proxy of this release makes no write
//! on it (see `active`).
//!
//! - `drain`: asked for by the operator. A proxy answers every write with
//!   [`Error::Unavailable`] (a 503 at the S3 front) and keeps serving reads; the reconciler of the
//!   bridge release applied the journals until each one was applied to its end.
//! - `drained`: written by the reconciler of the bridge release (and no one else) when it holds the
//!   lease, has seen `drain` for the settle time, and every journal it knows is applied to its end.
//!   Writes stay paused. This release's reconciler never writes it.
//! - `active-marks`: the phase of the protocol of `docs/design/put-without-lock.md`, which keeps a
//!   marker in the marks bucket in place of the entry of a key. This release reads the word and
//!   the optional `marks_bucket` that names the bucket. A read that misses the hot tier reads the
//!   marker of its key beside the cold read (and a listing reads the marker names) in this phase,
//!   in `drain` and `drained`, and in a phase it does not know. Under it, with epoch 3 or more and
//!   a `marks_bucket` that is the one the proxy is configured with, a proxy makes its puts (a put,
//!   a streamed put, a presigned put, and the parts and the completion of a multipart upload) and
//!   its deletes on markers, and a read of a key only the cold tier has writes the `WANT` marker
//!   that asks for a copy-back; a proxy with another marks bucket, or none, answers every write
//!   [`Error::Unavailable`], as for a phase it does not take as active. Nothing pauses, and the
//!   entry reconciler does nothing. A release before it takes the phase to be one it does not
//!   know, which is the same as the second.
//! - `active`: the change is done. With `"epoch": 2` or more a proxy makes its writes on the
//!   entries of their keys; it never pauses writes for it. A proxy of this release makes no write
//!   under any other format: no object, `active` below epoch 2 and a phase it does not know are
//!   answered with [`Error::Unavailable`] and a text that says writes wait for the active format.
//!   A delete that finds the entry of its key held or sealed by another operation, and that it can
//!   neither amend nor claim, waits for the entry to be released before it changes either tier. A
//!   delete that amends a put the reconciler holds for a copy, or claims a handed-over delete, is
//!   the exception: it deletes both tiers, hands the entry over and waits for the release before it
//!   is acknowledged.

#[cfg(feature = "server")]
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::journal::FORMAT_KEY;
#[cfg(feature = "server")]
use crate::journal::to_body;
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
    /// The markers of the put-without-lock design are in force. The word has a hyphen, so it is
    /// named; a release that does not know it reads [`FormatPhase::Unknown`].
    #[serde(rename = "active-marks")]
    ActiveMarks,
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
    /// The name of the marks bucket of the hot store, set with the phase `active-marks`: a proxy
    /// whose `hot_store.marks_bucket` is another does not write. A reader before this field
    /// ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) marks_bucket: Option<String>,
}

impl Format {
    /// Whether a proxy that reads this answers writes with 503.
    pub(crate) fn pauses_writes(&self) -> bool {
        matches!(self.phase, FormatPhase::Drain | FormatPhase::Drained)
    }
}

/// The first epoch whose writes are made on entries.
pub(crate) const ENTRIES_EPOCH: u64 = 2;

/// The first epoch whose writes are made on markers.
pub(crate) const MARKS_EPOCH: u64 = 3;

impl Format {
    /// Whether a proxy that reads this makes its writes on entries: the phase is `active` and the
    /// epoch is [`ENTRIES_EPOCH`] or later.
    pub(crate) fn runs_entries(&self) -> bool {
        self.phase == FormatPhase::Active && self.epoch.is_some_and(|epoch| epoch >= ENTRIES_EPOCH)
    }
}

impl Format {
    /// The name of the marks bucket if the phase is `active-marks` at [`MARKS_EPOCH`] or later and
    /// the record names the bucket: then the writes that are built are made on markers, in that
    /// bucket. A record that lacks the epoch or the name is not one a proxy writes under.
    pub(crate) fn runs_marks(&self) -> Option<&str> {
        let runs = self.phase == FormatPhase::ActiveMarks
            && self.epoch.is_some_and(|epoch| epoch >= MARKS_EPOCH);
        self.marks_bucket.as_deref().filter(|_| runs)
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
#[cfg(feature = "server")]
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
#[cfg(feature = "server")]
pub(crate) fn format_body(format: &Format) -> Bytes {
    to_body(format)
}
