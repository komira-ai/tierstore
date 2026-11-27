//! The admission of a write: the checks of its head and the permit it holds while it is stored.

use std::sync::Arc;

use tokio::sync::OwnedSemaphorePermit;

use crate::server::Server;
use crate::server::presign::Grant;
use crate::server::reply::S3Error;

impl Server {
    /// Lets a streamed body in, before any byte of it is read: refuses a body of unknown length
    /// where the backend needs the length (411), one that says it is longer than the limit
    /// (413, so that nothing is stored and no upload is begun), and one that comes when the
    /// server is taking as many as it may and none ends within
    /// [`put_permit_wait`](crate::server::ServerConfig::put_permit_wait) (503 `SlowDown`, with
    /// `Retry-After`). The checks of the head come first, so a request that is refused for them
    /// does not wait; the wait is on the permit alone and no byte of the body is read during it.
    /// Gives the permit that is held until the body is stored or refused.
    pub(super) async fn admit(
        &self,
        len: Option<u64>,
        presign: Option<&Grant>,
    ) -> Result<OwnedSemaphorePermit, S3Error> {
        let limit = self.config.max_request_bytes;
        let cap = presign.map_or(u64::MAX, |grant| grant.max_object_bytes);
        let limit = limit.min(usize::try_from(cap).unwrap_or(usize::MAX));
        match len {
            None if self.backend.needs_length() => return Err(S3Error::length_required()),
            Some(declared) if declared > limit as u64 => return Err(S3Error::too_large(limit)),
            _ => {}
        }
        self.permit().await
    }

    /// The permit of a write, which [`admit`](Self::admit) gives a body and a copy of an object
    /// holds while it is made, and a copy into a part while it puts the bytes it reads: at once if the server takes fewer writes than it may,
    /// otherwise the first that is freed within
    /// [`put_permit_wait`](crate::server::ServerConfig::put_permit_wait), or 503 `SlowDown`.
    pub(in crate::server) async fn permit(&self) -> Result<OwnedSemaphorePermit, S3Error> {
        if let Ok(permit) = Arc::clone(&self.puts).try_acquire_owned() {
            return Ok(permit);
        }
        let wait = self.config.put_permit_wait;
        if wait.is_zero() {
            return Err(S3Error::busy());
        }
        match tokio::time::timeout(wait, Arc::clone(&self.puts).acquire_owned()).await {
            Ok(Ok(permit)) => Ok(permit),
            _ => Err(S3Error::busy()),
        }
    }
}
