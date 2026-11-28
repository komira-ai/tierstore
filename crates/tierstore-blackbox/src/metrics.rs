//! The gauges of a node's `/metricz` (the Prometheus text format) that say whether its queues of
//! preparations and reads are idle: what the case of a stalled upstream waits for after the stall.

/// The heap bytes held by the reads of preparations not yet sent upstream.
pub(crate) const QUEUED_BYTES: &str = "tierstore_provider_prefetch_queued_bytes";

/// The reads of the cache's prefetch queue.
pub(crate) const QUEUE_DEPTH: &str = "tierstore_cache_prefetch_queue_depth";

/// The preparations under way.
pub(crate) const PREPARING: &str = "tierstore_provider_preparing";

/// The gauges of one `/metricz` text; a gauge the text does not have is none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Gauges {
    /// [`QUEUED_BYTES`].
    pub queued_bytes: Option<u64>,
    /// [`QUEUE_DEPTH`].
    pub queue_depth: Option<u64>,
    /// [`PREPARING`].
    pub preparing: Option<u64>,
}

impl Gauges {
    /// The gauges of `text`, each the sum of its series.
    #[must_use]
    pub fn of(text: &str) -> Self {
        Self {
            queued_bytes: value(text, QUEUED_BYTES),
            queue_depth: value(text, QUEUE_DEPTH),
            preparing: value(text, PREPARING),
        }
    }

    /// Whether every queue is empty.
    ///
    /// # Errors
    /// Each gauge that is not 0, or that the node does not show, with its value.
    pub fn idle(&self) -> Result<(), String> {
        let busy: Vec<String> = [
            (QUEUED_BYTES, self.queued_bytes),
            (QUEUE_DEPTH, self.queue_depth),
            (PREPARING, self.preparing),
        ]
        .into_iter()
        .filter_map(|(name, value)| match value {
            Some(0) => None,
            Some(value) => Some(format!("{name} {value}")),
            None => Some(format!("{name} not shown")),
        })
        .collect();
        if busy.is_empty() {
            Ok(())
        } else {
            Err(busy.join(", "))
        }
    }
}

/// The sum of the series of the metric `name` in `text`, rounded down; none when it has none.
fn value(text: &str, name: &str) -> Option<u64> {
    let series = text.lines().filter(|line| !line.starts_with('#'));
    let values = series.filter_map(|line| {
        let (metric, rest) = line.split_once([' ', '{'])?;
        let number = rest.rsplit(' ').next()?;
        (metric == name)
            .then(|| number.parse::<f64>().ok())
            .flatten()
    });
    values
        .reduce(|sum, value| sum + value)
        .map(|sum| sum.max(0.0) as u64)
}

#[cfg(test)]
#[path = "metrics_tests.rs"]
mod tests;
