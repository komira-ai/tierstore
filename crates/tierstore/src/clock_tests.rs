//! The injected monotonic clock: it moves only when told to, only forward, and apart from the
//! clock that stamps objects.

use std::sync::Arc;
use std::time::Duration;

use crate::{Backend, Clock, MemoryBackend, Mono, TestClock};

/// A new clock reads zero and stays there, whatever the machine's time is.
#[test]
fn mono_reads_zero_until_advanced() {
    let mono = Mono::new();
    assert_eq!(mono.now(), 0);
    assert_eq!(mono.now(), 0);
}

/// Each advance adds its whole milliseconds, and a step shorter than a millisecond adds none.
#[test]
fn mono_advance_adds_whole_milliseconds() {
    let mono = Mono::new();
    mono.advance(Duration::from_millis(1500));
    assert_eq!(mono.now(), 1500);
    mono.advance(Duration::from_secs(60));
    assert_eq!(mono.now(), 61_500);
    mono.advance(Duration::from_micros(999));
    assert_eq!(mono.now(), 61_500);
    mono.advance(Duration::from_micros(2_500));
    assert_eq!(mono.now(), 61_502);
}

/// A step too big for the clock stops it at the largest time; the clock does not wrap round to a
/// time before the one it had, and a later step does not move it back.
#[test]
fn mono_stops_at_the_largest_time_and_never_goes_back() {
    let mono = Mono::new();
    mono.advance(Duration::from_millis(u64::MAX - 5));
    mono.advance(Duration::from_millis(10));
    assert_eq!(mono.now(), u64::MAX);
    mono.advance(Duration::MAX);
    assert_eq!(mono.now(), u64::MAX);
    mono.advance(Duration::from_secs(1));
    assert_eq!(mono.now(), u64::MAX);
}

/// Two clocks are two times: advancing one moves nothing else.
#[test]
fn mono_is_apart_from_the_clock_that_stamps_objects() {
    let stamps = Arc::new(TestClock::new());
    let mono = Mono::new();
    let other = Mono::new();
    mono.advance(Duration::from_secs(30));
    assert_eq!(stamps.now(), 0);
    assert_eq!(other.now(), 0);
    assert_eq!(mono.now(), 30_000);
}

/// A holder of the clock as `dyn Clock` reads what the test advanced, which is how the proxy and
/// the reconciler will be given it.
#[tokio::test]
async fn mono_is_read_through_the_clock_trait() {
    let mono = Arc::new(Mono::new());
    let clock: Arc<dyn Clock> = mono.clone();
    mono.advance(Duration::from_millis(250));
    assert_eq!(clock.now(), 250);

    let memory = MemoryBackend::new();
    memory
        .put("k", bytes::Bytes::from_static(b"v"))
        .await
        .unwrap();
    assert_eq!(memory.head("k").await.unwrap().modified, 0);
}
