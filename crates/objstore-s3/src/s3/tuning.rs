//! The settings of an [`S3Store`](super::S3Store) that no bucket decides: how long an idle
//! connection is kept, how a read that failed in transport is sent again, and how a write is
//! cut into requests.

use std::time::Duration;

use super::multipart::{PART_BYTES, SINGLE_PUT_MAX};

/// How many times a ranged read is sent again after its transport failed (not timed
/// out by the store's caller or by a limit of the client): a connection the store, or the path to it,
/// closed after the request went out ("connection closed before message completed")
/// says nothing of the next one, a pooled connection closed while idle fails this way,
/// and so does one the kernel gave up on after the request went out (`ETIMEDOUT`,
/// "Connection timed out", mid-body). A read is idempotent. Three sends in all.
/// This is the default of [`S3Tuning::read_retries`].
pub const READ_RETRIES: u32 = 2;

/// The wait before the first resend of a read; each resend waits twice the one before.
/// This is the default of [`S3Tuning::read_retry_wait`].
pub(super) const READ_RETRY_WAIT: Duration = Duration::from_millis(50);

/// How many times at most a request is sent to the endpoints of a store when the sends before it
/// provably never reached a store: the connection to the endpoint could not be made (refused, no
/// route, DNS, connect timeout). Each send goes to the next endpoint in turn. A request that may
/// have reached a store is never sent again by this setting. This is the default of
/// [`S3Tuning::endpoint_attempts`].
pub const ENDPOINT_ATTEMPTS: u32 = 3;

/// How long an unused pooled connection is kept before the client drops it. A request
/// written to a connection the peer has just closed fails with "connection closed before
/// message completed", and it is not sent again because the store may have read it. So
/// the client's idle timeout must be clearly shorter than the idle close of the store and
/// of any proxy in front of it. MinIO closes idle connections after 30 s by default
/// (`--idle-timeout`), so this default is half of that. reqwest's default is 90 s. A
/// proxy that closes sooner than this, or a restart of the store, can still hit the
/// race. This is the default of [`S3Tuning::pool_idle_timeout`].
pub(super) const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(15);

/// The settings of a store that are the same for every bucket. `S3Store::new` takes the
/// defaults; [`S3Store::new_tuned`](super::S3Store::new_tuned) takes these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct S3Tuning {
    /// How long an unused pooled connection is kept before the client drops it.
    pub pool_idle_timeout: Duration,
    /// How many times a read that failed in transport is sent again.
    pub read_retries: u32,
    /// The wait before the first resend of a read; each resend waits twice the one before.
    pub read_retry_wait: Duration,
    /// How many times at most a request is sent when each send before it failed to connect (so
    /// the store never saw it), each to the next endpoint; 1 sends it once. See
    /// [`ENDPOINT_ATTEMPTS`].
    pub endpoint_attempts: u32,
    /// The longest object written with one streamed PUT; a longer one is a multipart upload. At
    /// most [`SINGLE_PUT_MAX`](super::SINGLE_PUT_MAX), S3's limit for one PUT (a larger value is
    /// taken as that).
    pub single_put_max: u64,
    /// The size of a part of a multipart upload, which is held in memory while it is sent. Kept
    /// between [`MIN_PART_BYTES`](super::MIN_PART_BYTES) and
    /// [`MAX_PART_BYTES`](super::MAX_PART_BYTES); an object too large for
    /// [`MAX_PARTS`](super::MAX_PARTS) parts of this size gets larger parts (see
    /// [`part_size_of`](super::part_size_of)).
    pub part_bytes: u64,
}

impl Default for S3Tuning {
    fn default() -> Self {
        Self {
            pool_idle_timeout: POOL_IDLE_TIMEOUT,
            read_retries: READ_RETRIES,
            read_retry_wait: READ_RETRY_WAIT,
            endpoint_attempts: ENDPOINT_ATTEMPTS,
            single_put_max: SINGLE_PUT_MAX,
            part_bytes: PART_BYTES,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_the_values_before_they_were_settings_except_the_pool_idle_timeout() {
        let tuning = S3Tuning::default();
        // The one deliberate change: 15 s, below MinIO's 30 s idle close (it was 30 s).
        assert_eq!(tuning.pool_idle_timeout, Duration::from_secs(15));
        assert_eq!(tuning.read_retries, 2);
        assert_eq!(tuning.read_retry_wait, Duration::from_millis(50));
        assert_eq!(tuning.endpoint_attempts, 3);
        assert_eq!(tuning.single_put_max, 5 << 30);
        assert_eq!(tuning.part_bytes, 16 << 20);
    }
}
