//! The settings of a [`Server`](super::Server), [`ServerConfig`]: moved here from `server/mod.rs`
//! unchanged, and re-exported there as `server::ServerConfig`.

use std::sync::Arc;
use std::time::Duration;

use super::handover::PIECES_QUEUED;
use super::{
    AccessKey, GcKey, ListenTuning, LocalCache, OCI_HANDSHAKE, PresignKey, ScopedKey, oci,
};
use crate::{Clock, MonotonicClock, Proxy};

/// The settings of a [`Server`].
#[derive(Clone)]
pub struct ServerConfig {
    /// The one bucket the server serves.
    pub bucket: String,
    /// The region in the credential scope of a signature.
    pub region: String,
    /// When the bucket was made, in milliseconds since the Unix epoch, for the `CreationDate` of
    /// ListBuckets: the time [`ServerConfig::new`] was called at, and in the binary the time the
    /// store's `_tiering/format` was written, where it has one.
    pub bucket_created: u64,
    /// The id of the full key, which may do everything the server serves.
    pub access_key_id: String,
    /// The secret of the full key.
    pub secret_access_key: String,
    /// The read-only keys, which may read objects and list the bucket and do nothing else (see
    /// the module docs). None by default. Their ids differ from each other's and from the full
    /// key's: if one is the full key's, the full key is the one that is meant.
    pub read_only_keys: Vec<AccessKey>,
    /// The keys that sign presigned PUT URLs (see [`presign`]); a URL is accepted only if one of
    /// them signed it. None by default. Their ids differ from each other's and from the other keys'.
    pub presign_keys: Vec<PresignKey>,
    /// The GC keys (see [`gc`]): header-signed keys that list, head and delete under one prefix.
    /// None by default. Their ids differ from each other's and from the other keys'.
    pub gc_keys: Vec<GcKey>,
    /// The scoped keys (see [`scoped`]): header-signed keys that read, create and delete under
    /// the prefixes of three lists. None by default. Their ids differ from each other's and from
    /// the other keys'.
    pub scoped_keys: Vec<ScopedKey>,
    /// The longest `X-Amz-Expires` a presigned URL may have, whatever the ceiling of its key says
    /// (a presigned GET has no ceiling: this is its limit).
    pub presign_max_expires: Duration,
    /// The most bytes of a request body: the longest object that one put takes, and the
    /// longest part of an upload. It is an upper bound on a length, not on memory: the body of
    /// a put or of a part is streamed to the backend and is never held whole. (The body of a
    /// request that is not streamed, the XML of a completion, is held, up to this many bytes.)
    pub max_request_bytes: usize,
    /// The most keys of one DeleteObjects call that are deleted at once (`server.delete_concurrency`).
    pub delete_concurrency: usize,
    /// How long a request body may go without a byte arriving, and how long the client may
    /// go without taking a byte of an answer, before the request ends: a body that arrives
    /// slowly but steadily is not cut, and neither is a slow reader that keeps taking bytes.
    /// See the module docs.
    pub body_idle_timeout: Duration,
    /// The most streamed bodies (puts and parts of uploads) taken at once. A request over the
    /// limit waits up to `put_permit_wait` for one to end, and is then answered 503 `SlowDown`
    /// with a `Retry-After`; its body is not read either way. Each
    /// streamed body holds a few pieces (`PIECES_QUEUED`, of the size the connection delivers),
    /// and an S3 tier up to 16 MiB more, so the memory of the puts is bounded by this number
    /// times that.
    pub max_concurrent_puts: usize,
    /// How long a streamed body that arrives with `max_concurrent_puts` bodies already being
    /// taken waits for one of them to end, before it is answered 503 `SlowDown`. It is the
    /// time of the runtime (`tokio::time`), and the body is not read while the request waits.
    /// Zero answers 503 at once.
    pub put_permit_wait: Duration,
    /// The least average rate, in bytes a second, at which a request body is taken, counted
    /// over the time the server spends waiting for the client to send the next piece once
    /// that time has passed `put_rate_grace`; a body that goes slower is refused with 400
    /// `RequestTimeout` and nothing of it is stored (an upload in progress at the store is
    /// aborted). Time the server spends not waiting for the client does not count, so a tier
    /// that is slow to take the body, or to answer before it takes it, is not blamed on the
    /// client. Zero turns the check off. It catches a sender that trickles a byte inside every
    /// idle timeout.
    pub put_min_bytes_per_sec: u64,
    /// How much time spent waiting for the client a body is let to go before its rate is held
    /// to `put_min_bytes_per_sec`.
    pub put_rate_grace: Duration,
    /// How long a client has to send the head of a request. It also bounds how long a
    /// connection may wait, idle, for its next request, so keep it above the time a client
    /// keeps an idle connection in its pool, and how long the drain waits for a client to close
    /// a connection that the server has shut for writing.
    pub header_timeout: Duration,
    /// The listening sockets and the pauses of the accept loop and of the closing door.
    pub listen: ListenTuning,
    /// How many pieces of a streamed put's body wait for the backend to take them.
    pub put_pieces_queued: usize,
    /// How long a client has to finish the TLS handshake with the OCI front.
    pub oci_handshake_timeout: Duration,
    /// The settle: how long the server goes on accepting and serving after the drain began and
    /// `/readyz` said 503.
    pub drain_lb_wait: Duration,
    /// How long the server waits for requests in flight, for the answers it is sending, and for
    /// the connections it closes to be closed by their clients, after the settle and its closing
    /// door (see `door`).
    pub drain_deadline: Duration,
    /// The proxy whose usage object `/usagez` judges: none by default, and then `/usagez` says
    /// the usage is not watched. The binary sets the proxy it serves.
    pub proxy: Option<Arc<Proxy>>,
    /// The local cache the server is the front of, if it is one: none by default. With one the
    /// server is a read cache: every call but GET and HEAD is refused 403 `AccessDenied` before
    /// its body is read, and `/metricz` shows the cache's counters. The binary sets the cache it
    /// serves, which is also the server's backend.
    pub cache: Option<LocalCache>,
    /// Whether a server that is the front of a local cache takes writes: false by default, and
    /// then the refusal of `cache` stands, before the role of the key is looked at. With it the
    /// roles of the keys decide, as on a storage node. It does nothing without `cache`. The
    /// binary sets it from `write_through` in `[local_cache]`, the setting that also lets the
    /// cache send the writes on.
    pub cache_writes: bool,
    /// How long the usage object may stand unchanged (by this process's clock) before `/usagez`
    /// says 503; a server that has run this long without finding one is judged the same way.
    /// Longer than the lease, so a takeover does not show.
    pub usage_stale_after: Duration,
    /// The counters of the OCI read front, which `GET /metricz` shows; none by default, and then
    /// `/metricz` has no block of it. The binary sets them when it runs the front.
    pub oci_metrics: Option<Arc<oci::OciMetrics>>,
    /// The slot port this instance runs as (`--slot`), which `tierstore_build_info` carries as its
    /// `slot` label so that the update hook can tell which instance answered through the public
    /// port. None for a process that is not a slot, and then the gauge has no such label.
    pub slot: Option<u16>,
    /// The time, for the age of a signature and of the usage object.
    pub clock: Arc<dyn Clock>,
    /// The time, for the rate of a request body (`put_min_bytes_per_sec`). It is a clock that
    /// only goes forward ([`MonotonicClock`](crate::MonotonicClock) from [`ServerConfig::new`]),
    /// not `clock`, so that a step of the clock of the machine (the time service setting it)
    /// while the server waits for a piece does not count for or against the client.
    pub rate_clock: Arc<dyn Clock>,
    /// The TLS of the listeners: `Some` serves every connection of [`Server::serve_all`] over
    /// HTTPS, and `None` (the default) serves plain HTTP.
    pub tls: Option<tokio_rustls::TlsAcceptor>,
}

impl ServerConfig {
    /// The settings for `bucket` and a full access key, in region `us-east-1`, with bodies of at
    /// most 256 MiB, 30 seconds of idle time for a body or an answer, a minute for a request
    /// head (and for an idle connection to send its next request), a drain that waits 10
    /// seconds with `/readyz` at 503 and then at most 15 minutes for the requests in flight.
    #[must_use]
    pub fn new(
        bucket: &str,
        access_key_id: &str,
        secret_access_key: &str,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            bucket: bucket.to_owned(),
            region: "us-east-1".to_owned(),
            bucket_created: clock.now(),
            access_key_id: access_key_id.to_owned(),
            secret_access_key: secret_access_key.to_owned(),
            read_only_keys: Vec::new(),
            presign_keys: Vec::new(),
            gc_keys: Vec::new(),
            scoped_keys: Vec::new(),
            presign_max_expires: Duration::from_secs(900),
            max_request_bytes: 256 << 20,
            body_idle_timeout: Duration::from_secs(30),
            max_concurrent_puts: 32,
            delete_concurrency: 16,
            put_permit_wait: Duration::from_secs(15),
            put_min_bytes_per_sec: 1024,
            put_rate_grace: Duration::from_secs(30),
            header_timeout: Duration::from_secs(60),
            listen: ListenTuning::default(),
            put_pieces_queued: PIECES_QUEUED,
            oci_handshake_timeout: OCI_HANDSHAKE,
            drain_lb_wait: Duration::from_secs(10),
            drain_deadline: Duration::from_secs(900),
            proxy: None,
            cache: None,
            cache_writes: false,
            usage_stale_after: Duration::from_secs(120),
            oci_metrics: None,
            slot: None,
            clock,
            rate_clock: Arc::new(MonotonicClock::new()),
            tls: None,
        }
    }
}
