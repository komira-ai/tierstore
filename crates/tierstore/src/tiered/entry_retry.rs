//! The retry of the last step of a put or a delete on its entry.
//!
//! A put ends on its entry with one replace, `(PUT, HELD)` to `(PUT, PENDING)` (the landing, which
//! is also the acknowledgement); a delete ends with a replace too, `(DEL, HELD)` to
//! `(DEL, PENDING)` (the hand-over), or with the removal of its entry (a fresh delete, and a put
//! that failed before its object landed). Before this module each was made once, and one failed
//! call of the hot tier left the entry held: no later put or delete of the key was admitted until
//! the reconciler reaped it, `hot_store.held_ttl` later.
//!
//! Each of these calls is now tried up to [`StepRetry::attempts`] times, with a sleep that starts
//! at [`StepRetry::delay`] and doubles up to [`StepRetry::delay_max`], through the sleeper of the
//! [`ProxyConfig`](super::ProxyConfig). What is retried, and what is not:
//!
//! - Only a call whose answer is [`ambiguous`] (the store was unavailable, or failed in a way
//!   that does not say whether it did the call) is retried. Any other error, and any other
//!   outcome (the entry changed, the step does not apply), ends the retries at once.
//! - A replace is not repeated blindly. The entry is read first: if the body stored has the nonce
//!   of the failed replace, it landed and is [`Outcome::Done`], and nothing is sent again; if the
//!   entry is as it was read, the replace is sent again, naming the same tag; if it is anything
//!   else the replace is [`Outcome::Lost`]. The read is the one of
//!   [`Entries::try_advance`](crate::Entries::try_advance) when it answers, and
//!   [`Entries::settle`](crate::Entries::settle) before the next try when it did not. A replace
//!   that names a tag cannot be applied twice, so a try whose predecessor landed late is `Lost`,
//!   and is not acknowledged.
//! - A removal of an entry names the tag too, so it is sent again as it is: one that landed
//!   already finds nothing at the tag and is `Lost`, which its callers take as done.
//! - A call cut at the margin of the registration is retried like a failed one. No try starts
//!   once the registration no longer allows a call to start (see
//!   [`Deadline::allows_start`](crate::Deadline::allows_start)), so the retries never carry an operation past the point
//!   where the reconciler may take its entry for a departed proxy's.
//!
//! The acknowledgement of a put is unchanged: it is a replace that is known to have landed
//! (`Done`), by the answer of the store or by the nonce read back, and nothing else.

use std::future::Future;
use std::time::Duration;

use super::Inner;
use crate::Error;
use crate::entry::{Attempt, Outcome, Seen, ambiguous};

/// How often and how fast the last step of a put or a delete is tried again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StepRetry {
    /// The most tries of one step, the first included: 1 is no retry.
    pub attempts: u32,
    /// The sleep before the second try.
    pub delay: Duration,
    /// The longest sleep: each sleep is twice the one before, up to this.
    pub delay_max: Duration,
}

impl StepRetry {
    /// Four tries, after sleeps of 100 ms, 200 ms and 400 ms.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            attempts: 4,
            delay: Duration::from_millis(100),
            delay_max: Duration::from_secs(2),
        }
    }
}

impl Default for StepRetry {
    fn default() -> Self {
        Self::new()
    }
}

/// The tries left of one step and the sleep before the next.
struct Backoff {
    left: u32,
    delay: Duration,
    max: Duration,
}

impl Backoff {
    fn new(retry: StepRetry) -> Self {
        Self {
            left: retry.attempts.saturating_sub(1),
            delay: retry.delay,
            max: retry.delay_max,
        }
    }
}

/// A call that was cut or refused before the store answered (`Unavailable`) is a failed replace
/// that did not land, for a step that names its tag.
fn failed(answer: Result<Attempt, Error>) -> Result<Attempt, Error> {
    match answer {
        Err(error) if ambiguous(&error) => Ok(Attempt::Unchanged(error)),
        other => other,
    }
}

impl Inner {
    /// Sleeps the delay of `backoff` and says whether another try may be made: one is left, and
    /// the registration allows a call to start.
    async fn again(&self, backoff: &mut Backoff) -> bool {
        if backoff.left == 0 || !self.heartbeat.deadline.allows_start() {
            return false;
        }
        backoff.left -= 1;
        self.config.sleeper.sleep(backoff.delay).await;
        backoff.delay = backoff.delay.saturating_mul(2).min(backoff.max);
        true
    }

    /// A replace of `seen`, made by `once` (which makes one try), retried while it fails
    /// ambiguously. Returns the outcome of the replace, or the last error when the tries are used
    /// up or the registration no longer allows one.
    ///
    /// # Errors
    /// The error of a try that is not ambiguous, and the last ambiguous one.
    pub(super) async fn advance_retrying<F, Fut>(
        &self,
        seen: &Seen,
        mut once: F,
    ) -> Result<Outcome<Seen>, Error>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<Attempt, Error>>,
    {
        let mut backoff = Backoff::new(self.config.step_retry);
        let mut attempt = failed(once().await)?;
        loop {
            match attempt {
                Attempt::Settled(outcome) => return Ok(outcome),
                Attempt::Unchanged(error) => {
                    if !self.again(&mut backoff).await {
                        return Err(error);
                    }
                    attempt = failed(once().await)?;
                }
                Attempt::Unknown(unsettled) => {
                    if !self.again(&mut backoff).await {
                        return Err(unsettled.into_error());
                    }
                    attempt = match self.entries.settle(seen, unsettled).await {
                        Attempt::Unchanged(_) => failed(once().await)?,
                        read => read,
                    };
                }
            }
        }
    }

    /// The removal of the entry `seen`, each try cut at the margin of the registration, retried
    /// while it fails ambiguously.
    ///
    /// # Errors
    /// The error of a try that is not ambiguous, and the last ambiguous one.
    pub(super) async fn release_retrying(
        &self,
        key: &str,
        seen: &Seen,
    ) -> Result<Outcome<()>, Error> {
        let mut backoff = Backoff::new(self.config.step_retry);
        loop {
            match self.bounded(key, self.entries.release(seen)).await {
                Err(error) if ambiguous(&error) && self.again(&mut backoff).await => {}
                answer => return answer,
            }
        }
    }
}
