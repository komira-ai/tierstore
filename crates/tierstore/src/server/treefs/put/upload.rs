//! The upload of a commit: the fill socket's `put` request for the staged file, asked again with a
//! jittered back-off while the answer says the upstream may take it later, until the commit's
//! deadline (`fs.put.commit_deadline_secs`). The answer of the socket is one line of JSON:
//!
//! ```text
//! {"ok":true,"created":true}     Created
//! {"ok":true,"created":false}    AlreadyStored (the same bytes were there: a put made again)
//! {"ok":false,"error":"<word>"}  exists_different, checksum or toolarge, final;
//!                                upstream, asked again until the deadline;
//!                                stage, or a word this release does not know: Upstream at once
//! ```
//!
//! A socket that is not there, or that gives no answer in time, is asked again like `upstream`. Any
//! other line (the socket's `refused <why>` for a request it did not understand) is `Upstream` at
//! once, and said: asking again would get the same answer.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

use super::commit::PutOutcome;
use super::names::Sha256Digest;
use crate::Clock;
use crate::server::treefs::fill_client::FillClient;

/// The first pause before a put is asked again.
const FIRST_PAUSE: Duration = Duration::from_millis(100);

/// The longest pause between two tries.
const MOST_PAUSE: Duration = Duration::from_secs(5);

/// The answer of the socket to a `put`.
#[derive(Debug, Deserialize)]
struct Answer {
    ok: bool,
    #[serde(default)]
    created: bool,
    #[serde(default)]
    error: String,
}

/// What one try came to.
#[derive(Debug, PartialEq, Eq)]
enum Tried {
    /// The answer, final.
    Done(PutOutcome),
    /// The upstream, or the socket, may take it later.
    Again,
}

/// What the answer `line` says.
fn read_answer(line: &str) -> Tried {
    let Ok(answer) = serde_json::from_str::<Answer>(line) else {
        tracing::warn!("tierstore fs: the put was answered {line:?}, which is not an answer to it");
        return Tried::Done(PutOutcome::Upstream);
    };
    if answer.ok {
        return Tried::Done(if answer.created {
            PutOutcome::Created
        } else {
            PutOutcome::AlreadyStored
        });
    }
    match answer.error.as_str() {
        "exists_different" => Tried::Done(PutOutcome::ExistsDifferent),
        "checksum" => Tried::Done(PutOutcome::Checksum),
        "toolarge" => Tried::Done(PutOutcome::TooLarge),
        "upstream" => Tried::Again,
        word => {
            tracing::warn!("tierstore fs: the put failed for a reason not asked again: {word:?}");
            Tried::Done(PutOutcome::Upstream)
        }
    }
}

/// The pause before try `tries + 1` of the put of `sha256`, in whole milliseconds: from
/// [`FIRST_PAUSE`], doubled at each try, at most [`MOST_PAUSE`], of which the second half is spread
/// by the digest and the try, so that the commits one outage held do not all ask again at once. It
/// is a function of its inputs alone, so a test knows it.
pub(super) fn pause_before(tries: u32, sha256: &Sha256Digest) -> Duration {
    let first = u64::try_from(FIRST_PAUSE.as_millis()).unwrap_or(u64::MAX);
    let most = u64::try_from(MOST_PAUSE.as_millis()).unwrap_or(u64::MAX);
    let full = first.saturating_mul(1 << tries.min(16)).min(most);
    let half = full / 2;
    let spread = u64::from(sha256.0[(tries as usize) % 32]) * u64::from(sha256.0[31]);
    Duration::from_millis(half + half * spread / (255 * 255))
}

/// The upload of commits over the fill socket.
pub(in crate::server) struct Uploader {
    /// A client of the fill socket that does not wait for it itself: the waits are this one's.
    client: FillClient,
    /// The time the deadlines are told by, in milliseconds.
    clock: Arc<dyn Clock>,
    /// Takes a pause between two tries; a test replaces it.
    pause: Arc<dyn Fn(Duration) + Send + Sync>,
}

impl Uploader {
    /// Uploads over the socket at `socket`, waiting at most `answer_timeout` for an answer.
    pub(in crate::server) fn new(
        socket: &Path,
        answer_timeout: Duration,
        clock: Arc<dyn Clock>,
        pause: Arc<dyn Fn(Duration) + Send + Sync>,
    ) -> Self {
        let client = FillClient::pausing(socket.to_owned(), 0, answer_timeout, Arc::new(|_| {}));
        Self {
            client,
            clock,
            pause,
        }
    }

    /// Puts the `size` bytes of the file at `path`, which hash to `sha256`, as the object `key`,
    /// asking again while the answer allows until `deadline` (on the clock), and gives the answer;
    /// [`PutOutcome::Upstream`] when the deadline comes first.
    pub(in crate::server) fn put(
        &self,
        (key, sha256, size): (&str, Sha256Digest, u64),
        path: &Path,
        deadline: u64,
    ) -> PutOutcome {
        let hex = sha256.to_string();
        let mut tries = 0;
        loop {
            let tried = match self.client.put(key, &hex, size, path) {
                Ok(line) => read_answer(&line),
                Err(error) => {
                    tracing::warn!("tierstore fs: the put of {key}: {error}");
                    Tried::Again
                }
            };
            if let Tried::Done(outcome) = tried {
                return outcome;
            }
            let pause = pause_before(tries, &sha256);
            let millis = u64::try_from(pause.as_millis()).unwrap_or(u64::MAX);
            if self.clock.now().saturating_add(millis) >= deadline {
                tracing::warn!("tierstore fs: the put of {key} did not finish by its deadline");
                return PutOutcome::Upstream;
            }
            (self.pause)(pause);
            tries += 1;
        }
    }
}

#[cfg(test)]
#[path = "upload_tests.rs"]
mod tests;
