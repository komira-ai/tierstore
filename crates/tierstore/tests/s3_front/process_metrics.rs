//! The `tierstore` binary's `/metricz` shows the process's own figures under the standard names,
//! and the gauge of a bound of memory with its limit: read from a running binary, so that what
//! is checked is the figures of a real process and not of a fake procfs.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures::StreamExt;
use futures::channel::mpsc::unbounded;
use objstore_s3::{BodyStream, ObjectKey, ObjectStore, ObjectStoreError};
use rustix::process::Signal;
use tierstore_testkit::http::client as http_client;

use crate::binary::{Running, start};
use crate::files::Files;

/// The longest a test waits for the binary to show a value. It only stops a test whose process
/// never shows it, and then the test fails: no test asserts how long it takes.
const SAFETY: Duration = Duration::from_secs(60);

/// The five figures of the process, each with the kind of its `# TYPE` line.
const FIGURES: [(&str, &str); 5] = [
    ("process_resident_memory_bytes", "gauge"),
    ("process_virtual_memory_bytes", "gauge"),
    ("process_start_time_seconds", "gauge"),
    ("process_cpu_seconds_total", "counter"),
    ("process_open_fds", "gauge"),
];

/// The time now, in seconds since the Unix epoch: the clock the binary's start is told in.
#[expect(
    clippy::disallowed_methods,
    reason = "the start the binary reports is a time of the machine's clock; the test reads the \
              same clock before and after the binary runs to bound it, and asserts no duration"
)]
fn wall_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after the epoch")
        .as_secs_f64()
}

/// The bytes of memory of the machine.
#[cfg(target_os = "linux")]
fn machine_memory() -> u64 {
    let info = rustix::system::sysinfo();
    info.totalram * u64::from(info.mem_unit)
}

/// The bytes of memory of the machine.
#[cfg(target_os = "macos")]
fn machine_memory() -> u64 {
    use sysinfo::{MemoryRefreshKind, RefreshKind, System};
    let kind = RefreshKind::nothing().with_memory(MemoryRefreshKind::nothing().with_ram());
    System::new_with_specifics(kind).total_memory()
}

/// The text of the binary's `/metricz`.
async fn metricz(running: &Running) -> String {
    let url = format!("http://{}/metricz", running.addr());
    let reply = http_client().get(&url).send().await.unwrap();
    assert_eq!(reply.status(), 200);
    reply.text().await.unwrap()
}

/// The value of the sample `name` (no labels) in `text`, which must be a number.
#[track_caller]
fn value(text: &str, name: &str) -> f64 {
    let prefix = format!("{name} ");
    let line = text
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("no {name} in:\n{text}"));
    line.parse()
        .unwrap_or_else(|_| panic!("{name} is {line:?}, not a number"))
}

/// Scrapes the binary until the sample `name` is `wanted`, for [`SAFETY`] at most.
async fn shows(running: &Running, name: &str, wanted: f64) {
    let looking = async {
        while value(&metricz(running).await, name) != wanted {
            tokio::task::yield_now().await;
        }
    };
    let seen = tokio::time::timeout(SAFETY, looking).await;
    assert!(seen.is_ok(), "{name} never showed {wanted}");
}

/// Asserts that the `/metricz` of a running binary has each figure of the process with its type,
/// as a number: its resident memory above zero and below the machine's, its virtual memory at
/// least its resident memory, its start within the test's own run (told in whole seconds on
/// both systems, so up to a second and a clock tick before the true start), its CPU time at least
/// zero, and some descriptors open (its listener and its standard streams at the least).
#[tokio::test]
async fn metricz_of_the_binary_shows_its_own_memory_start_cpu_and_descriptors() {
    let files = Files::new("bin-process-figures");
    let before = wall_seconds();
    let mut running = start(&files.flags(&["--drain-lb-wait", "50ms"]));
    let text = metricz(&running).await;
    let after = wall_seconds();
    for (name, kind) in FIGURES {
        assert!(
            text.contains(&format!("\n# TYPE {name} {kind}\n{name} ")),
            "no {kind} {name} in:\n{text}"
        );
        let figure = value(&text, name);
        assert!(figure.is_finite() && figure >= 0.0, "{name} {figure}");
    }
    let resident = value(&text, "process_resident_memory_bytes");
    assert!(resident > 0.0, "{resident}");
    assert!(resident < machine_memory() as f64, "{resident}");
    assert!(value(&text, "process_virtual_memory_bytes") >= resident);
    let started = value(&text, "process_start_time_seconds");
    assert!(started >= before.floor() - 1.01, "{started} before {before}");
    assert!(started <= after, "{started} after {after}");
    assert!(value(&text, "process_open_fds") >= 3.0, "{text}");
    running.signal(Signal::TERM);
    assert_eq!(running.exit_code().await, Some(0));
}

/// Asserts that the gauge of the request bodies held, one bound of the S3 front's memory, rises
/// to one under its limit (`server.max_concurrent_puts`, 32 by default) while a put's body is in
/// flight, half sent, and returns to zero once the put has been answered; the limit stays.
#[tokio::test]
async fn the_bodies_held_rise_under_their_limit_while_a_put_is_in_flight() {
    let files = Files::new("bin-bodies-held");
    let mut running = start(&files.flags(&["--drain-lb-wait", "50ms"]));
    let text = metricz(&running).await;
    assert_eq!(value(&text, "tierstore_s3_bodies_in_flight"), 0.0);
    assert_eq!(value(&text, "tierstore_s3_bodies_limit"), 32.0);

    let (feed, body) = unbounded::<Result<Bytes, ObjectStoreError>>();
    let body: BodyStream = body.boxed();
    let store = running.store();
    let putting = tokio::spawn(async move {
        let key = ObjectKey::new("held/body").unwrap();
        store.put_stream(&key, 8, body).await
    });
    feed.unbounded_send(Ok(Bytes::from_static(b"1234"))).unwrap();
    shows(&running, "tierstore_s3_bodies_in_flight", 1.0).await;
    let held = metricz(&running).await;
    assert_eq!(value(&held, "tierstore_s3_bodies_limit"), 32.0);

    feed.unbounded_send(Ok(Bytes::from_static(b"5678"))).unwrap();
    drop(feed);
    putting.await.unwrap().unwrap();
    shows(&running, "tierstore_s3_bodies_in_flight", 0.0).await;
    let after = metricz(&running).await;
    assert_eq!(value(&after, "tierstore_s3_bodies_limit"), 32.0);
    running.signal(Signal::TERM);
    assert_eq!(running.exit_code().await, Some(0));
}
