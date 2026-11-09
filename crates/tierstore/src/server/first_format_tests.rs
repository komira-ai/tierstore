//! The format a first start writes into a hot tier that has none.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use tracing::Level;
use tracing::subscriber::set_default;

use super::first_format;
use crate::TokioSleeper;
use crate::journal::FORMAT_KEY;
use crate::server::build_info::stderr_log::LineLog;
use crate::server::cli::Cli;
use crate::server::config_testing::MINIMAL;
use crate::server::first_beat::wait_first_beat;
use crate::{Backend, Error, FaultyBackend, MemoryBackend, Op, Proxy, ProxyConfig, TestClock};

/// A writer whose bytes the test reads back: the operator's stderr.
#[derive(Clone, Default)]
struct Stderr(Arc<Mutex<Vec<u8>>>);

impl Stderr {
    fn text(&self) -> String {
        let bytes = self.0.lock().expect("the buffer is not poisoned").clone();
        String::from_utf8(bytes).expect("the lines are text")
    }
}

impl Write for Stderr {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut held = self.0.lock().expect("the buffer is not poisoned");
        held.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

const LIMIT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(1);

/// A node's settings over the hot tier `table` of `[hot_store]` (a body of keys).
fn cli(table: &str) -> Cli {
    let memory = "[hot_store]\nkind   = \"memory\"";
    assert!(MINIMAL.contains(memory));
    let text = MINIMAL.replacen(memory, &format!("[hot_store]\n{table}"), 1);
    Cli::from_config_text(&text).unwrap()
}

fn s3_cli(marks: Option<&str>) -> Cli {
    let named = marks.map_or_else(String::new, |name| format!("marks_bucket = {name:?}\n"));
    cli(&format!(
        "kind = \"s3\"\nbucket = \"hotb\"\n{named}endpoint = \"http://192.0.2.10:9200\"\n\
         region = \"us-east-1\"\ncredentials_file = \"/etc/tierstore/hot.credentials\""
    ))
}

fn dir_cli() -> Cli {
    cli("kind = \"dir\"\npath = \"/var/lib/tierstore/hot\"")
}

/// A proxy over `hot` that has made its first beat, with the marks bucket `marks` if given.
async fn proxy_over(hot: Arc<dyn Backend>, marks: Option<&str>) -> Proxy {
    let clock = Arc::new(TestClock::new());
    let cold = Arc::new(MemoryBackend::new());
    let mut config = ProxyConfig::new("p1", 1000, clock);
    if let Some(name) = marks {
        config.marks = Some(Arc::new(MemoryBackend::new()));
        config.marks_name = Some(name.to_owned());
    }
    let proxy = Proxy::open(hot, cold, config).await.unwrap();
    assert!(proxy.start_heartbeat(Arc::new(TokioSleeper)));
    assert!(wait_first_beat(&proxy, LIMIT, POLL).await);
    proxy
}

async fn format_of(hot: &dyn Backend) -> Option<Bytes> {
    hot.get(FORMAT_KEY, None).await.ok()
}

fn is_unavailable(result: Result<(), Error>) -> bool {
    matches!(result, Err(Error::Unavailable(_)))
}

/// Asserts that an `s3:` hot tier with no format object is given `active-marks` at epoch 3 naming
/// the marks bucket of the settings (the default `<bucket>-marks`, and a named one), and that a
/// write is refused before and accepted after.
#[tokio::test(start_paused = true)]
async fn an_s3_hot_tier_with_no_format_is_made_active_marks() {
    for (named, marks) in [(None, "hotb-marks"), (Some("mine"), "mine")] {
        let hot = Arc::new(MemoryBackend::new());
        let proxy = proxy_over(hot.clone(), Some(marks)).await;
        assert!(is_unavailable(proxy.put("k", Bytes::from("v")).await));

        first_format(&s3_cli(named), &proxy, hot.as_ref(), LIMIT)
            .await
            .unwrap();
        let want = format!(r#"{{"phase":"active-marks","epoch":3,"marks_bucket":"{marks}"}}"#);
        assert_eq!(format_of(hot.as_ref()).await.unwrap(), want.as_bytes());
        proxy.put("k", Bytes::from("v")).await.unwrap();
    }
}

/// Asserts that a `dir:` hot tier with no format object is given `active` at epoch 2, as
/// `format-epoch --activate` writes it, and that a write is accepted after.
#[tokio::test(start_paused = true)]
async fn a_dir_hot_tier_with_no_format_is_made_active() {
    let hot = Arc::new(MemoryBackend::new());
    let proxy = proxy_over(hot.clone(), None).await;
    assert!(is_unavailable(proxy.put("k", Bytes::from("v")).await));

    first_format(&dir_cli(), &proxy, hot.as_ref(), LIMIT)
        .await
        .unwrap();
    let body = format_of(hot.as_ref()).await.unwrap();
    assert_eq!(&body[..], br#"{"phase":"active","epoch":2}"#);
    proxy.put("k", Bytes::from("v")).await.unwrap();
}

/// Asserts that a format object that exists is never written over, whatever its phase, and that
/// the proxy follows it: the pause of `drain` and `drained` stays, `active` at epoch 2 over a
/// `dir:` tier and a record that names another marks bucket are as they were.
#[tokio::test(start_paused = true)]
async fn a_format_object_that_exists_is_left_as_it_is() {
    let bodies = [
        r#"{"phase":"drain","epoch":2}"#,
        r#"{"phase":"drained"}"#,
        r#"{"phase":"drain","epoch":3,"marks_bucket":"hotb-marks"}"#,
        r#"{"phase":"active-marks","epoch":3,"marks_bucket":"other"}"#,
        r#"{"phase":"active","epoch":2}"#,
    ];
    for body in bodies {
        for cli in [s3_cli(None), dir_cli()] {
            let hot = Arc::new(FaultyBackend::new(Arc::new(MemoryBackend::new())));
            hot.put(FORMAT_KEY, Bytes::from(body)).await.unwrap();
            let proxy = proxy_over(hot.clone(), Some("hotb-marks")).await;
            first_format(&cli, &proxy, hot.as_ref(), LIMIT)
                .await
                .unwrap();
            assert_eq!(format_of(hot.as_ref()).await.unwrap(), body.as_bytes());
            assert_eq!(hot.calls_on(Op::Put, FORMAT_KEY), 1, "{body}");
        }
    }
}

/// Asserts that a `memory` hot tier is left alone here (its format is written before the proxy
/// opens), and that a hot tier that cannot be read is not written to: the read decides.
#[tokio::test(start_paused = true)]
async fn a_memory_tier_and_an_unreadable_tier_are_not_written_to() {
    let hot = Arc::new(FaultyBackend::new(Arc::new(MemoryBackend::new())));
    let proxy = proxy_over(hot.clone(), None).await;
    let memory = cli("kind = \"memory\"");
    first_format(&memory, &proxy, hot.as_ref(), LIMIT)
        .await
        .unwrap();
    assert_eq!(hot.calls_on(Op::Put, FORMAT_KEY), 0);

    hot.fail_next_on(Op::Get, FORMAT_KEY, Error::Unavailable("down".to_owned()));
    first_format(&dir_cli(), &proxy, hot.as_ref(), LIMIT)
        .await
        .unwrap();
    assert_eq!(hot.calls_on(Op::Put, FORMAT_KEY), 0);
    assert!(format_of(hot.as_ref()).await.is_none());
}

/// Asserts that two proxies that start at once over one hot tier leave one format object, the one
/// both of their settings name, and that both write.
#[tokio::test(start_paused = true)]
async fn two_proxies_that_start_at_once_leave_one_object_and_both_follow_it() {
    let hot = Arc::new(FaultyBackend::new(Arc::new(MemoryBackend::new())));
    let (a, b) = (
        proxy_over(hot.clone(), Some("hotb-marks")).await,
        proxy_over(hot.clone(), Some("hotb-marks")).await,
    );
    let cli = s3_cli(None);
    let (first, second) = tokio::join!(
        first_format(&cli, &a, hot.as_ref(), LIMIT),
        first_format(&cli, &b, hot.as_ref(), LIMIT)
    );
    first.unwrap();
    second.unwrap();
    let want = br#"{"phase":"active-marks","epoch":3,"marks_bucket":"hotb-marks"}"#;
    assert_eq!(&format_of(hot.as_ref()).await.unwrap()[..], &want[..]);
    a.put("a", Bytes::from("v")).await.unwrap();
    b.put("b", Bytes::from("v")).await.unwrap();
}

/// Asserts that a create-only put that finds the object there (the loser of a race) reads it and
/// follows it, and does not write again.
#[tokio::test(start_paused = true)]
async fn the_loser_of_the_race_follows_the_object_of_the_winner() {
    let hot = Arc::new(FaultyBackend::new(Arc::new(MemoryBackend::new())));
    let proxy = proxy_over(hot.clone(), Some("hotb-marks")).await;
    // The winner's object lands between the loser's read and its put.
    let winner = br#"{"phase":"drain","epoch":2}"#;
    hot.put(FORMAT_KEY, Bytes::from_static(winner))
        .await
        .unwrap();
    // The read of the loser saw no object: take it away for that read only.
    hot.fail_next_on(Op::Get, FORMAT_KEY, Error::NotFound(FORMAT_KEY.to_owned()));
    first_format(&s3_cli(None), &proxy, hot.as_ref(), LIMIT)
        .await
        .unwrap();
    assert_eq!(&format_of(hot.as_ref()).await.unwrap()[..], &winner[..]);
    assert!(is_unavailable(proxy.put("k", Bytes::from("v")).await));
}

/// Asserts that an ambiguous answer to the put (a 409 that reaches the caller as `Unavailable`) is
/// not retried and does not stop the start, and that the proxy follows the object when it is
/// there, at the next read of the format.
#[tokio::test(start_paused = true)]
async fn an_ambiguous_answer_is_not_retried_and_the_object_is_followed_on_read() {
    let hot = Arc::new(FaultyBackend::new(Arc::new(MemoryBackend::new())));
    let proxy = proxy_over(hot.clone(), None).await;
    let stderr = Stderr::default();
    let _log = set_default(LineLog::new(stderr.clone(), Level::WARN));
    hot.fail_next_on(Op::Put, FORMAT_KEY, Error::Unavailable("409".to_owned()));
    first_format(&dir_cli(), &proxy, hot.as_ref(), LIMIT)
        .await
        .unwrap();
    assert_eq!(hot.calls_on(Op::Put, FORMAT_KEY), 1);
    assert_eq!(
        stderr.text(),
        "tierstore: the answer to writing the format of the new hot tier is not known \
         (409); reading it at each beat\n"
    );
    assert!(is_unavailable(proxy.put("k", Bytes::from("v")).await));

    // The put had been applied, or a peer's was: the next read finds the object.
    hot.put(
        FORMAT_KEY,
        Bytes::from_static(br#"{"phase":"active","epoch":2}"#),
    )
    .await
    .unwrap();
    proxy.read_format_now(LIMIT).await;
    proxy.put("k", Bytes::from("v")).await.unwrap();
}

/// Asserts that any other failure of the put stops the start with a text that names the format of
/// the new hot tier.
#[tokio::test(start_paused = true)]
async fn another_failure_of_the_put_stops_the_start() {
    let hot = Arc::new(FaultyBackend::new(Arc::new(MemoryBackend::new())));
    let proxy = proxy_over(hot.clone(), None).await;
    hot.fail_next_on(Op::Put, FORMAT_KEY, Error::Other("denied".to_owned()));
    let why = first_format(&dir_cli(), &proxy, hot.as_ref(), LIMIT)
        .await
        .unwrap_err();
    assert!(
        why.to_string().contains("format of the new hot tier")
            && why.to_string().contains("denied"),
        "{why}"
    );
}
