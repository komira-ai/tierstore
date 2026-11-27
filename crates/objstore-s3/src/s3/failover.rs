//! How a request is sent to the endpoints of a store: [`Conn::failing_over`], which sends it
//! again to the next endpoint after a failure that proves the store never saw it (or, for an
//! idempotent request, after a stall), and [`Conn::failing_over_marked`], which also says whether
//! the store never saw a request that failed.

use std::future::Future;
use std::sync::Arc;

use reqwest::Response;

use super::credentials::Keys;
use super::endpoint::Target;
use super::{Conn, Failed};
use crate::ObjectStoreError;

impl Conn {
    /// Runs `attempt`, which signs a request with the keys it is given for the endpoint it is
    /// given (the next in turn, `health.rs`) and sends it, again after a failure that proves the
    /// store never saw the request ([`Failed::unsent`]), and, for an `idempotent` request, after
    /// a stall of an endpoint that was then set aside, until the sends are
    /// [`S3Tuning::endpoint_attempts`](super::S3Tuning::endpoint_attempts) in all. The last
    /// failure is the error, as it is for a request sent once; any other failure ends the request
    /// at once. An answer of any status
    /// puts its endpoint back in turn; a stall or a failed connect sets it aside unless it is the
    /// last in turn. With one endpoint the next send after a failed connect is to the same
    /// address on a new connection, and a stall is the error. The keys are had again for each
    /// send, so a send never signs with keys that expired; when none can be had, nothing is
    /// sent and the error is [`ObjectStoreError::Credentials`]. An answer, of any status, also
    /// begins a pass over the aborts that failed (`cleanup.rs`).
    pub(super) async fn failing_over<F, Fut>(
        &self,
        idempotent: bool,
        attempt: F,
    ) -> Result<Response, ObjectStoreError>
    where
        F: FnMut(Arc<Keys>, &Target) -> Fut,
        Fut: Future<Output = Result<Response, Failed>>,
    {
        self.failing_over_marked(idempotent, attempt)
            .await
            .map_err(|failed| failed.error)
    }

    /// [`failing_over`](Self::failing_over), whose failure says whether the store never saw the
    /// request: [`Failed::unsent`] for the last send, and for a request that was never signed
    /// for want of keys.
    pub(super) async fn failing_over_marked<F, Fut>(
        &self,
        idempotent: bool,
        mut attempt: F,
    ) -> Result<Response, Failed>
    where
        F: FnMut(Arc<Keys>, &Target) -> Fut,
        Fut: Future<Output = Result<Response, Failed>>,
    {
        let mut sends = 1;
        loop {
            let keys = self.credentials.keys().await.map_err(|e| Failed {
                error: ObjectStoreError::Credentials(e),
                unsent: true,
                stalled: false,
            })?;
            let index = self.pick();
            let failed = match attempt(keys, &self.targets[index]).await {
                Ok(resp) => {
                    self.answered(index);
                    self.cleaner.answered();
                    return Ok(resp);
                }
                Err(failed) => failed,
            };
            let aside =
                (failed.unsent || failed.stalled) && self.endpoint_failed(index, &failed.error);
            let again = failed.unsent || (failed.stalled && idempotent && aside);
            if !again || sends >= self.tuning.endpoint_attempts {
                return Err(failed);
            }
            let why = if failed.unsent {
                "request never reached the store; sending it to the next endpoint"
            } else {
                "request got no answer in time; sending it to the next endpoint"
            };
            tracing::warn!(error = %failed.error, sends, "{why}");
            sends += 1;
        }
    }
}
