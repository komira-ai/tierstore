//! The start of the front waits for the proxy's first beat.
//!
//! A put or a delete on an entry needs a proxy that is live, and a proxy is live from its first
//! acknowledged beat. A front that took requests before that would answer the first writes of a
//! restarted process with 503. So the start waits for the beat, but for one heartbeat period at
//! most: a hot tier that does not answer is what the beats and the reads are for, and the process
//! serves reads all the same.

use std::time::Duration;

use crate::Proxy;

/// How long the wait looks again at the proxy after it found it not live.
const POLL: Duration = Duration::from_millis(1);

/// Waits until `proxy` is live, or `limit` has passed. Gives whether it is live.
pub(super) async fn wait_first_beat(proxy: &Proxy, limit: Duration) -> bool {
    let live = async {
        while !proxy.is_live() {
            tokio::time::sleep(POLL).await;
        }
    };
    tokio::time::timeout(limit, live).await.is_ok()
}

#[cfg(test)]
#[path = "first_beat_tests.rs"]
mod tests;
