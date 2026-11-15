//! What the heartbeat says: the failed beats (once for each distinct cause), the lapse of the
//! registration and its return, each asserted as the whole line.
//!
//! As in `heartbeat_tests`, the loop waits through a sleeper the test steps by hand and the
//! monotonic clock is a [`Mono`] the test advances, so no test waits and the elapsed times in the
//! lines are values the test set.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::Level;
use tracing::subscriber::set_default;

use super::heartbeat_log::{BeatLog, Said};
use super::heartbeat_tests::Stepped;
use crate::faulty::Op;
use crate::journal::proxy_key;
use crate::server::build_info::stderr_log::LineLog;
use crate::{Error, GateBackend, MemoryBackend, Mono, Phase, Proxy, ProxyConfig, TestClock};

const ENDPOINT: &str = "http://192.0.2.10:9200";

/// A writer whose bytes the test reads back: the operator's stderr.
#[derive(Clone, Default)]
struct Stderr(Arc<Mutex<Vec<u8>>>);

impl Stderr {
    fn text(&self) -> String {
        let bytes = self.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }
}

impl Write for Stderr {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn down(why: &str) -> Result<(), Error> {
    Err(Error::Unavailable(why.to_owned()))
}

fn warn(text: &str) -> Said {
    Said {
        warn: true,
        text: text.to_owned(),
    }
}

/// Asserts that a failure is said once for its cause and not again while the same failure goes on
/// (whatever the time it takes), that another cause is said, and that a beat that is acknowledged
/// makes the same cause worth saying again.
#[test]
fn a_failed_beat_is_said_once_for_each_distinct_cause() {
    let mut log = BeatLog::new(ENDPOINT);
    let refused = "tierstore: heartbeat: the beat to http://192.0.2.10:9200 failed after 40 ms: \
                   backend unavailable: connection refused";
    assert_eq!(
        log.after(1_000, 1_040, &down("connection refused"), false),
        [warn(refused)]
    );
    assert_eq!(
        log.after(11_000, 11_900, &down("connection refused"), false),
        []
    );
    let slow = "tierstore: heartbeat: the beat to http://192.0.2.10:9200 failed after 10000 ms: \
                backend unavailable: timed out";
    assert_eq!(
        log.after(21_000, 31_000, &down("timed out"), false),
        [warn(slow)]
    );
    assert_eq!(log.after(41_000, 41_010, &Ok(()), false), []);
    assert_eq!(
        log.after(51_000, 51_040, &down("connection refused"), false),
        [warn(refused)]
    );
}

/// Asserts that the first acknowledged beat says nothing, that the lapse is said once when a beat
/// ends with the registration no longer live (with the time since the last acknowledged beat was
/// sent), and the return once when a beat is acknowledged after it, with the time between the two
/// acknowledged beats; and that a registration that is live says nothing when a beat fails.
#[test]
fn the_lapse_and_the_return_of_the_registration_are_said_once_each() {
    let mut log = BeatLog::new(ENDPOINT);
    assert_eq!(log.after(0, 5, &Ok(()), true), []);
    let failed = "tierstore: heartbeat: the beat to http://192.0.2.10:9200 failed after 5 ms: \
                  backend unavailable: down";
    assert_eq!(
        log.after(10_000, 10_005, &down("down"), true),
        [warn(failed)]
    );
    let lapsed = "tierstore: heartbeat: the registration has lapsed: no beat to \
                  http://192.0.2.10:9200 has been acknowledged for 60005 ms, and this proxy \
                  refuses writes until one is";
    assert_eq!(
        log.after(60_000, 60_005, &down("down"), false),
        [warn(lapsed)]
    );
    assert_eq!(log.after(70_000, 70_005, &down("down"), false), []);
    let back = "tierstore: heartbeat: the registration is live again: a beat to \
                http://192.0.2.10:9200 was acknowledged after 80000 ms without one";
    assert_eq!(
        log.after(80_000, 80_005, &Ok(()), true),
        [Said {
            warn: false,
            text: back.to_owned()
        }]
    );
    assert_eq!(log.after(90_000, 90_005, &Ok(()), true), []);
}

/// Asserts, through the loop and the operator's log, that a beat the store holds for 3 s and then
/// fails is logged with the endpoint and 3000 ms, that the same failure at the next beat is not,
/// that a different failure that holds the beat past the end of the registration is logged with
/// the lapse, and that the beat that is acknowledged after it logs the return.
#[tokio::test]
async fn the_loop_logs_the_failures_the_lapse_and_the_return() {
    let stderr = Stderr::default();
    let _guard = set_default(LineLog::new(stderr.clone(), Level::INFO));
    let memory = Arc::new(MemoryBackend::new());
    let hot = Arc::new(GateBackend::new(memory));
    let mono = Arc::new(Mono::new());
    let mut config = ProxyConfig::new("p1", 1 << 20, Arc::new(TestClock::new()));
    config.mono = mono.clone();
    config.hot_endpoint = ENDPOINT.to_owned();
    let proxy = Proxy::open(hot.clone(), Arc::new(MemoryBackend::new()), config)
        .await
        .unwrap();
    let sleeper = Stepped::new();
    assert!(proxy.start_heartbeat(sleeper.clone()));
    sleeper.beat_done().await;
    assert_eq!(stderr.text(), "", "an acknowledged beat says nothing");

    let held = |error: &str| {
        (
            hot.gate(Op::PutIfMatch, &proxy_key("p1"), Phase::Pre),
            error.to_owned(),
        )
    };
    let (gate, error) = held("connection refused");
    sleeper.next();
    gate.parked().await;
    mono.advance(Duration::from_secs(3));
    assert!(gate.release_with(Error::Unavailable(error)));
    sleeper.beat_done().await;
    let refused = "tierstore: heartbeat: the beat to http://192.0.2.10:9200 failed after 3000 ms: \
                   backend unavailable: connection refused\n";
    assert_eq!(stderr.text(), refused);

    mono.advance(Duration::from_secs(7));
    let (gate, error) = held("connection refused");
    sleeper.next();
    gate.parked().await;
    assert!(gate.release_with(Error::Unavailable(error)));
    sleeper.beat_done().await;
    assert_eq!(stderr.text(), refused, "the same cause is not said again");

    mono.advance(Duration::from_secs(10));
    let (gate, error) = held("timed out");
    sleeper.next();
    gate.parked().await;
    mono.advance(Duration::from_secs(35));
    assert!(gate.release_with(Error::Unavailable(error)));
    sleeper.beat_done().await;
    let lapsed = "tierstore: heartbeat: the beat to http://192.0.2.10:9200 failed after 35000 ms: \
                  backend unavailable: timed out\n\
                  tierstore: heartbeat: the registration has lapsed: no beat to \
                  http://192.0.2.10:9200 has been acknowledged for 55000 ms, and this proxy \
                  refuses writes until one is\n";
    assert_eq!(stderr.text(), format!("{refused}{lapsed}"));
    assert!(!proxy.is_live());

    sleeper.next();
    sleeper.beat_done().await;
    assert!(proxy.is_live());
    let back = "tierstore: heartbeat: the registration is live again: a beat to \
                http://192.0.2.10:9200 was acknowledged after 55000 ms without one\n";
    assert_eq!(stderr.text(), format!("{refused}{lapsed}{back}"));
    proxy.stop_heartbeat().await;
}
