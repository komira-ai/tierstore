//! What the requests of the S3 front may hold in memory at once, how a request over a bound is
//! answered, and the gauges of each bound on `/metricz`.
//!
//! - **Connections.** At most [`ServerConfig::max_connections`] (`server.max_connections`) are
//!   open at once. The accept loop takes the next connection only once fewer than that are open,
//!   so a client over the bound waits in the listening socket's queue (`server.listen_backlog`),
//!   as for a process that is slow to accept; nothing is refused or reset. Each open connection
//!   holds its task, hyper's buffers and the request on it. The connections that the closing door
//!   of a drain takes are not held to the bound: it closes them. Gauge:
//!   `tierstore_s3_connections_open`.
//! - **Reads.** At most [`ServerConfig::max_concurrent_reads`] (`server.max_concurrent_reads`)
//!   GETs of an object hold a permit at once, from before the object is opened until the body of
//!   the answer ends, fails or is dropped with its connection. A read holds what its backend
//!   streams it by, up to two chunks (on a build node, two of `fill_chunk`, 8 MiB each). A GET
//!   that finds every permit taken is answered 503 `SlowDown` with `Retry-After`, nothing is
//!   opened, and it is counted. Gauges: `tierstore_s3_reads_in_flight`, and the counter
//!   `tierstore_s3_reads_refused_total`.
//! - **Bodies held whole.** A body that is not streamed to the backend (that of a completion of
//!   an upload, a DeleteObjects, a copy, a CreateBucket, a hydrate call, a read or a delete) is at
//!   most [`WHOLE_BODY_LIMIT`](super::ops::WHOLE_BODY_LIMIT) bytes (DeleteObjects:
//!   [`DELETE_BODY_LIMIT`](super::delete_objects::DELETE_BODY_LIMIT)), over which it is refused
//!   with 413 `EntityTooLarge`. From its first byte until the call has answered, it holds a permit
//!   of the bodies the server takes (`server.max_concurrent_puts`, which the streamed bodies hold
//!   too): one over them waits up to `server.put_permit_wait` and is then answered 503 `SlowDown`
//!   with `Retry-After`. A request with no body takes no permit. Gauge:
//!   `tierstore_s3_bodies_in_flight`, the streamed and the whole bodies that hold a permit.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use hyper::body::Body;
use tokio::sync::OwnedSemaphorePermit;

use super::Server;
use super::reply::S3Error;
use super::request::{BoxError, Check, Reader};

#[cfg(doc)]
use super::ServerConfig;

/// The default of [`ServerConfig::max_connections`]: a few thousand pooled connections of the
/// clients of a node, twice over. An idle connection holds some tens of KiB (its task and hyper's
/// buffers), so the bound is a few hundred MiB of them.
pub const MAX_CONNECTIONS: usize = 8192;

/// The default of [`ServerConfig::max_concurrent_reads`]: a read holds at most two chunks of its
/// read (two of `fill_chunk`, 8 MiB, on a build node), so the reads of a build node hold at most
/// 8 GiB. A node that answers several thousand GETs a minute, each streamed for a second or less,
/// has some tens of them in flight.
pub const MAX_CONCURRENT_READS: usize = 512;

/// The gauge of the connections open.
const CONNECTIONS_OPEN: &str = "tierstore_s3_connections_open";

/// The gauge of the reads that hold a permit.
const READS_IN_FLIGHT: &str = "tierstore_s3_reads_in_flight";

/// The counter of the reads refused for want of a permit.
const READS_REFUSED: &str = "tierstore_s3_reads_refused_total";

/// The gauge of the bodies, streamed or held whole, that hold a permit.
const BODIES_IN_FLIGHT: &str = "tierstore_s3_bodies_in_flight";

impl Server {
    /// Resolves once fewer than [`ServerConfig::max_connections`] connections are open, so that
    /// the accept loop may take one more. A connection that closes wakes it (`Connected`).
    pub(super) async fn room_for_a_connection(&self) {
        loop {
            // Made and enabled before the look, so that a close between the look and the wait is
            // not missed.
            let closed = self.drain.idle.notified();
            let mut closed = std::pin::pin!(closed);
            let _ = closed.as_mut().enable();
            if self.open_connections() < self.config.max_connections {
                return;
            }
            closed.await;
        }
    }

    /// The permit of a GET of an object, held by the body of its answer: 503 `SlowDown`, counted,
    /// if every one of [`ServerConfig::max_concurrent_reads`] is taken.
    pub(super) fn read_permit(&self) -> Result<OwnedSemaphorePermit, S3Error> {
        Arc::clone(&self.reads).try_acquire_owned().map_err(|_| {
            self.counts.reads_refused.fetch_add(1, Ordering::Relaxed);
            S3Error::busy_reading()
        })
    }

    /// The whole body of `reader`, if it has at most the reader's limit and comes to the hashes
    /// of `check`, and the permit of the bodies it took at its first byte (none for a body of no
    /// bytes), which the caller holds until it has answered.
    ///
    /// # Errors
    /// As [`Reader::next`], the error of [`Check::finish`], and 503 `SlowDown` when no permit was
    /// freed within `put_permit_wait`.
    pub(super) async fn whole_body<B>(
        &self,
        reader: Reader<B>,
        check: Check,
    ) -> Result<(Bytes, Option<OwnedSemaphorePermit>), S3Error>
    where
        B: Body + Send,
        B::Data: Send,
        B::Error: Into<BoxError>,
    {
        reader.read_whole(check, || self.permit()).await
    }

    /// The families of the front on `/metricz`: the requests that came too late in a drain, the
    /// failed puts, and the gauges of the bounds (module docs).
    pub(super) fn front_families(&self) -> String {
        let late = self.late_requests();
        let mut text = format!(
            "# TYPE tierstore_drain_late_requests_total counter\n\
             tierstore_drain_late_requests_total {late}\n"
        );
        text.push_str(&self.put_failure_family());
        let taken = |most: usize, free: usize| most.saturating_sub(free);
        let reads = taken(
            self.config.max_concurrent_reads,
            self.reads.available_permits(),
        );
        let bodies = taken(
            self.config.max_concurrent_puts,
            self.puts.available_permits(),
        );
        let refused = self.counts.reads_refused.load(Ordering::Relaxed);
        text.push_str(&bound_families(
            self.open_connections(),
            reads,
            refused,
            bodies,
        ));
        text
    }
}

/// The text of the families of the bounds: the connections `open`, the `reads` and the `bodies`
/// that hold a permit, gauges, and the reads `refused`, a counter.
pub(super) fn bound_families(open: usize, reads: usize, refused: u64, bodies: usize) -> String {
    format!(
        "# TYPE {CONNECTIONS_OPEN} gauge\n{CONNECTIONS_OPEN} {open}\n\
         # TYPE {READS_IN_FLIGHT} gauge\n{READS_IN_FLIGHT} {reads}\n\
         # TYPE {READS_REFUSED} counter\n{READS_REFUSED} {refused}\n\
         # TYPE {BODIES_IN_FLIGHT} gauge\n{BODIES_IN_FLIGHT} {bodies}\n"
    )
}

#[cfg(test)]
#[path = "bounds_tests.rs"]
mod tests;
