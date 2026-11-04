//! The looks at the public port: readiness, the build probe, and what they make of a start.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use super::look::{Look, verified};
use super::{
    CacheMode, Clock, Instances, Outcome, POLL_INTERVAL, PROBE_ATTEMPTS, PROBE_HITS, PUBLIC_PORT,
    PortError, PortKind, READY_ATTEMPTS, Reason, Reply,
};

type Answer = Box<dyn Fn(u32) -> Result<Reply, PortError>>;

/// The public port's answers, one function of the number of the request for each path, and a log
/// of what was asked.
struct Port {
    readyz: Answer,
    metricz: Answer,
    asked: RefCell<Vec<(u16, String)>>,
}

impl Port {
    fn new(readyz: Answer, metricz: Answer) -> Port {
        Port {
            readyz,
            metricz,
            asked: RefCell::new(Vec::new()),
        }
    }

    fn count(&self, path: &str) -> u32 {
        let asked = self.asked.borrow();
        u32::try_from(asked.iter().filter(|(_, asked)| asked == path).count()).unwrap()
    }
}

impl Instances for Port {
    fn get(&self, port: u16, path: &str) -> Result<Reply, PortError> {
        let n = self.count(path);
        self.asked.borrow_mut().push((port, path.to_owned()));
        match path {
            "/readyz" => (self.readyz)(n),
            "/metricz" => (self.metricz)(n),
            other => panic!("unexpected path {other}"),
        }
    }
}

/// A clock that waits for nothing and counts the waits.
#[derive(Default)]
struct CountingClock {
    now: Cell<u64>,
    pauses: RefCell<Vec<Duration>>,
}

impl Clock for CountingClock {
    fn now_ms(&self) -> u64 {
        self.now.get()
    }

    fn pause(&self, span: Duration) {
        self.now
            .set(self.now.get() + u64::try_from(span.as_millis()).unwrap());
        self.pauses.borrow_mut().push(span);
    }
}

fn reply(status: u16, body: &str) -> Result<Reply, PortError> {
    Ok(Reply {
        status,
        body: body.to_owned(),
    })
}

fn gauge(version: &str, commit: &str) -> String {
    format!(
        "# TYPE tierstore_build_info gauge\ntierstore_build_info{{version=\"{version}\",commit=\"{commit}\"}} 1\n"
    )
}

fn ready(_: u32) -> Result<Reply, PortError> {
    reply(200, "ready read-write")
}

fn serving(version: &'static str) -> Answer {
    Box::new(move |_| reply(200, &gauge(version, "0123456789ab")))
}

fn refused() -> Result<Reply, PortError> {
    Err(PortError::failed(PortKind::Instances, "refused"))
}

fn look<'a>(port: &'a Port, clock: &'a CountingClock) -> Look<'a> {
    Look {
        instances: port,
        clock,
    }
}

/// Asserts that one look asks `/readyz` of the public port once, waits for nothing, and gives the
/// mode of a 200 and none for a refusal, a 503 (draining) or a body that is not tierstore's.
#[test]
fn one_look_reads_readyz_of_the_public_port() {
    let clock = CountingClock::default();
    let port = Port::new(
        Box::new(|n| match n {
            0 => reply(200, "ready read-only"),
            1 => reply(200, "ready"),
            2 => reply(503, "draining"),
            3 => reply(200, "ready read-sideways"),
            _ => refused(),
        }),
        serving("0.5.0"),
    );
    let look = look(&port, &clock);
    assert_eq!(look.ready(), Some(CacheMode::ReadOnly));
    assert_eq!(look.ready(), Some(CacheMode::Plain));
    assert_eq!(look.ready(), None);
    assert_eq!(look.ready(), None);
    assert_eq!(look.ready(), None);
    assert_eq!(
        *port.asked.borrow(),
        vec![(PUBLIC_PORT, "/readyz".to_owned()); 5]
    );
    assert!(clock.pauses.borrow().is_empty());
}

/// Asserts that waiting for `/readyz` pauses one interval between looks and stops at the first
/// that answers, and that a port that never answers is looked at READY_ATTEMPTS times with a pause
/// after each, then gives up. It would catch a wait that gives up early, or one that never does.
#[test]
fn waiting_for_readiness_looks_until_it_answers_or_the_attempts_run_out() {
    let clock = CountingClock::default();
    let port = Port::new(
        Box::new(|n| if n < 3 { refused() } else { ready(n) }),
        serving("0.5.0"),
    );
    assert_eq!(look(&port, &clock).wait_ready(), Some(CacheMode::ReadWrite));
    assert_eq!(port.count("/readyz"), 4);
    assert_eq!(*clock.pauses.borrow(), vec![POLL_INTERVAL; 3]);
    assert_eq!(POLL_INTERVAL, Duration::from_secs(1));

    let clock = CountingClock::default();
    let port = Port::new(Box::new(|_| refused()), serving("0.5.0"));
    assert_eq!(look(&port, &clock).wait_ready(), None);
    assert_eq!(port.count("/readyz"), READY_ATTEMPTS);
    assert_eq!(clock.pauses.borrow().len(), READY_ATTEMPTS as usize);
}

/// Asserts that a probe needs PROBE_HITS answers in a row from a build of the version, as the
/// version alone or with its commit; that the probe returns at the third without a pause after
/// it; that a miss of any kind (another version, no gauge, a refusal, a status other than 200)
/// starts the count again; and that a miss between hits is not forgiven. It would catch a probe
/// that takes one answer of the new release, among those of the old, for a cutover.
#[test]
fn a_probe_needs_hits_in_a_row_from_the_version() {
    let clock = CountingClock::default();
    let port = Port::new(Box::new(ready), serving("0.5.0"));
    assert!(look(&port, &clock).probe("0.5.0"));
    assert_eq!(port.count("/metricz"), PROBE_HITS);
    assert_eq!(clock.pauses.borrow().len(), PROBE_HITS as usize - 1);
    assert!(look(&port, &clock).probe("0.5.0+0123456789ab"));
    assert!(!look(&port, &CountingClock::default()).probe("0.4.0"));
    assert!(!look(&port, &CountingClock::default()).probe("0.5.0+other"));

    // hit, hit, miss, then three: the miss resets the count.
    let script = [
        Some("0.5.0"),
        Some("0.5.0"),
        None,
        Some("0.5.0"),
        Some("0.5.0"),
        Some("0.5.0"),
    ];
    let clock = CountingClock::default();
    let port = Port::new(
        Box::new(ready),
        Box::new(move |n| match script[n as usize] {
            Some(version) => reply(200, &gauge(version, "abc")),
            None => refused(),
        }),
    );
    assert!(look(&port, &clock).probe("0.5.0"));
    assert_eq!(port.count("/metricz"), 6);
}

/// Asserts that each kind of answer that is not a hit is no hit, even when its body names the
/// version: an old build in between, a 503 with the right gauge, a body with no gauge and a
/// refusal. The probe then gives up after PROBE_ATTEMPTS looks with a pause after each.
#[test]
fn an_answer_that_is_not_a_hit_is_never_counted() {
    let misses: [Answer; 4] = [
        serving("0.4.0"),
        Box::new(|_| reply(503, &gauge("0.5.0", "abc"))),
        Box::new(|_| reply(200, "tierstore_hits_total 3\n")),
        Box::new(|_| refused()),
    ];
    for metricz in misses {
        let clock = CountingClock::default();
        let port = Port::new(Box::new(ready), metricz);
        assert!(!look(&port, &clock).probe("0.5.0"));
        assert_eq!(port.count("/metricz"), PROBE_ATTEMPTS);
        assert_eq!(clock.pauses.borrow().len(), PROBE_ATTEMPTS as usize);
    }
}

/// Asserts that hits in the last looks the probe has still count: the version first answers at
/// look PROBE_ATTEMPTS - PROBE_HITS + 1.
#[test]
fn a_probe_counts_the_last_looks() {
    let first = PROBE_ATTEMPTS - PROBE_HITS;
    let port = Port::new(
        Box::new(ready),
        Box::new(move |n| {
            let version = if n < first { "0.4.0" } else { "0.5.0" };
            reply(200, &gauge(version, "abc"))
        }),
    );
    assert!(look(&port, &CountingClock::default()).probe("0.5.0"));
    assert_eq!(port.count("/metricz"), PROBE_ATTEMPTS);
}

/// Asserts that `serves` waits for readiness, then probes, and answers the cache mode; that a port
/// that never answers is `not_ready` (the release's fault) without a probe, and a port that
/// answers as another version is `probe_failed` with the version named. It would catch a start
/// that is called good because the old release still answers.
#[test]
fn serves_waits_for_readiness_and_then_for_the_version() {
    let clock = CountingClock::default();
    let port = Port::new(
        Box::new(|n| {
            if n < 2 {
                refused()
            } else {
                reply(200, "ready read-only")
            }
        }),
        serving("0.5.0"),
    );
    assert_eq!(
        look(&port, &clock).serves("0.5.0").unwrap(),
        CacheMode::ReadOnly
    );
    assert_eq!(port.count("/readyz"), 3);

    let port = Port::new(Box::new(|_| refused()), serving("0.5.0"));
    let done = look(&port, &CountingClock::default())
        .serves("0.5.0")
        .unwrap_err();
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::NotReady));
    assert_eq!(
        done.report.detail.as_deref(),
        Some("port 9300 does not answer /readyz")
    );
    assert_eq!(port.count("/metricz"), 0);

    let port = Port::new(Box::new(ready), serving("0.4.0"));
    let done = look(&port, &CountingClock::default())
        .serves("0.5.0")
        .unwrap_err();
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::ProbeFailed));
    assert_eq!(
        done.report.detail.as_deref(),
        Some("no request to port 9300 was answered by 0.5.0")
    );
}

/// Asserts that `observed` makes one look at `/readyz` and no wait: a port that does not answer
/// is `not_ready` after one request and no pause, and one that answers as another build is
/// `wrong_build` with the version named. It would catch a verify that waits out a release that
/// does not serve, or calls the old release the new.
#[test]
fn observed_looks_once_and_names_what_it_did_not_see() {
    let clock = CountingClock::default();
    let port = Port::new(Box::new(|_| refused()), serving("0.5.0"));
    let done = look(&port, &clock).observed("0.5.0").unwrap_err();
    assert_eq!(done.report.reason, Some(Reason::NotReady));
    assert_eq!(port.count("/readyz"), 1);
    assert_eq!(port.count("/metricz"), 0);
    assert!(clock.pauses.borrow().is_empty());

    let port = Port::new(Box::new(ready), serving("0.4.0"));
    let done = look(&port, &CountingClock::default())
        .observed("0.5.0")
        .unwrap_err();
    assert_eq!(done.outcome, Outcome::Failure);
    assert_eq!(done.report.reason, Some(Reason::WrongBuild));
    assert_eq!(
        done.report.detail.as_deref(),
        Some("port 9300 is not answered by 0.5.0")
    );
    assert_eq!(port.count("/readyz"), 1);

    let port = Port::new(Box::new(|_| reply(200, "ready")), serving("0.5.0"));
    assert_eq!(
        look(&port, &CountingClock::default())
            .observed("0.5.0")
            .unwrap(),
        CacheMode::Plain
    );
}

/// Asserts that a verify that saw a build node still read-only is a success with
/// `promotion_pending` set and says so, and that a node that is read-write or has no cache lock
/// is a plain success. It would catch a promotion taken for finished before the cache lock is held.
#[test]
fn a_read_only_node_is_a_verified_with_a_promotion_pending() {
    let pending = verified(CacheMode::ReadOnly, "0.5.0");
    assert_eq!(pending.outcome, Outcome::Success);
    assert!(pending.report.promotion_pending);
    assert!(pending.report.reversible);
    assert_eq!(
        pending.report.detail.as_deref(),
        Some("0.5.0 serves on port 9300 read-only: the promotion is pending")
    );
    for mode in [CacheMode::ReadWrite, CacheMode::Plain] {
        let done = verified(mode, "0.5.0");
        assert_eq!(done.outcome, Outcome::Success);
        assert!(!done.report.promotion_pending);
        assert_eq!(
            done.report.detail.as_deref(),
            Some("0.5.0 serves on port 9300")
        );
    }
}
