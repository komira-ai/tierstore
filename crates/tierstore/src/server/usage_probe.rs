//! The answer of `/usagez`, the probe of the age of the usage the reconciler publishes.

use std::time::Duration;

use hyper::StatusCode;

use super::Server;

impl Server {
    /// The answer of `/usagez`: 200 and the age of the usage object of the reconciler while it is
    /// within the limit, 503 and the reason once it is older, or missing for longer than the
    /// limit since the server started. It does not depend on the drain.
    pub(super) async fn usage_probe(&self) -> (StatusCode, String) {
        let Some(proxy) = &self.config.proxy else {
            return (StatusCode::OK, "usage not watched".to_owned());
        };
        let limit = self.config.usage_stale_after;
        let published = proxy.usage_age().await;
        let since_start = self.config.clock.now().saturating_sub(self.started);
        let age = published.unwrap_or(Duration::from_millis(since_start));
        let (secs, max) = (age.as_secs(), limit.as_secs());
        let down = StatusCode::SERVICE_UNAVAILABLE;
        match (age <= limit, published.is_some()) {
            (true, true) => (StatusCode::OK, format!("usage published {secs}s ago")),
            (true, false) => (StatusCode::OK, format!("no usage yet, {secs}s since start")),
            (false, true) => (
                down,
                format!("usage of the hot tier is stale: published {secs}s ago, limit {max}s"),
            ),
            (false, false) => (
                down,
                format!(
                    "usage of the hot tier is missing: none published in {secs}s, limit {max}s"
                ),
            ),
        }
    }
}
