//! The queue gauges of a `/metricz` text.

use super::*;

/// Asserts that each gauge is read from its own series, summed over its labels, whatever its
/// help and type lines and the metrics around it, with a gauge the text lacks left none.
#[test]
fn each_gauge_is_the_sum_of_its_series() {
    let text = "\
# HELP tierstore_provider_prefetch_queued_bytes Heap bytes.
# TYPE tierstore_provider_prefetch_queued_bytes gauge
tierstore_provider_prefetch_queued_bytes 1048576
tierstore_provider_prefetch_queued_bytes_total 5
tierstore_cache_prefetch_queue_depth{class=\"a\"} 3
tierstore_cache_prefetch_queue_depth{class=\"b\"} 4
tierstore_cache_bytes 99
";
    assert_eq!(
        Gauges::of(text),
        Gauges {
            queued_bytes: Some(1_048_576),
            queue_depth: Some(7),
            preparing: None,
        }
    );
    assert_eq!(
        Gauges::of("tierstore_provider_preparing 2.0\nbroken\ntierstore_provider_preparing x\n"),
        Gauges {
            queued_bytes: None,
            queue_depth: None,
            preparing: Some(2),
        }
    );
    assert_eq!(Gauges::of(""), Gauges::default());
}

/// Asserts that the queues are idle when every gauge is 0, and otherwise that each one that is
/// not, or that the node does not show, is named with its value.
#[test]
fn the_queues_are_idle_when_every_gauge_is_zero() {
    let idle = Gauges {
        queued_bytes: Some(0),
        queue_depth: Some(0),
        preparing: Some(0),
    };
    assert_eq!(idle.idle(), Ok(()));
    let busy = Gauges {
        queued_bytes: Some(512),
        queue_depth: None,
        preparing: Some(0),
    };
    assert_eq!(
        busy.idle(),
        Err(format!("{QUEUED_BYTES} 512, {QUEUE_DEPTH} not shown"))
    );
}
