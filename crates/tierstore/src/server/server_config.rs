//! The defaults of [`ServerConfig`]: what [`ServerConfig::new`] sets.

use std::sync::Arc;
use std::time::Duration;

use crate::{Clock, MonotonicClock};

use super::ServerConfig;

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
            put_permit_wait: Duration::from_secs(15),
            put_min_bytes_per_sec: 1024,
            put_rate_grace: Duration::from_secs(30),
            header_timeout: Duration::from_secs(60),
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
