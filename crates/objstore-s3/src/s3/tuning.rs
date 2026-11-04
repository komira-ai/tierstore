//! The settings of an [`S3Store`](super::S3Store) that no bucket decides: how long an idle
//! connection is kept, and how a read that failed in transport is sent again.

use std::time::Duration;

use super::{MULTIPART_THRESHOLD, PART_BYTES};

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

/// How long an unused pooled connection is kept before the client drops it. reqwest's
/// default is 90 s, longer than many servers and proxies keep an idle connection; a
/// request sent on a connection the peer has already closed fails with "connection
/// closed before message completed". Dropping idle connections after 30 s keeps the
/// client's side shorter than common load-balancer and server idle closes (often 60 s;
/// some paths close at about 20 s). This is the default of [`S3Tuning::pool_idle_timeout`].
pub(super) const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

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
    /// The part size of a multipart upload (a larger object gets larger parts, see
    /// [`part_size_with`](super::part_size_with)), which is also what one part holds in memory
    /// while it is sent. Between [`MIN_PART_BYTES`](super::MIN_PART_BYTES) and
    /// [`MAX_PART_BYTES`](super::MAX_PART_BYTES).
    pub part_bytes: u64,
    /// The largest object written with one PUT; a larger one is a multipart upload. At most
    /// `part_bytes`, so that a streamed write holds no more than a part of its body.
    pub multipart_threshold: u64,
}

impl Default for S3Tuning {
    fn default() -> Self {
        Self {
            pool_idle_timeout: POOL_IDLE_TIMEOUT,
            read_retries: READ_RETRIES,
            read_retry_wait: READ_RETRY_WAIT,
            part_bytes: PART_BYTES,
            multipart_threshold: MULTIPART_THRESHOLD,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_the_values_before_they_were_settings() {
        let tuning = S3Tuning::default();
        assert_eq!(tuning.pool_idle_timeout, Duration::from_secs(30));
        assert_eq!(tuning.read_retries, 2);
        assert_eq!(tuning.read_retry_wait, Duration::from_millis(50));
        assert_eq!(tuning.part_bytes, 16 << 20);
        assert_eq!(tuning.multipart_threshold, 16 << 20);
    }
}
