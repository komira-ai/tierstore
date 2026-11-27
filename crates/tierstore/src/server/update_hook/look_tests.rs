//! The looks at the public port: readiness, the build probe, and what they make of a start.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::time::Duration;

use super::look::{Look, READ_WRITE_ATTEMPTS, verified, writable};
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

/// Asserts that `serves` pauses one interval between looks and stops waiting at the first that
/// `/readyz` answers with the version serving, and that a port that never answers is looked at
/// READY_ATTEMPTS times with a pause after each and no request of the build, then is
/// `not_ready`. It would catch a wait that gives up early, or one that never does.
#[test]
fn serves_looks_until_it_answers_or_the_attempts_run_out() {
    let clock = CountingClock::default();
    let port = Port::new(
        Box::new(|n| if n < 3 { refused() } else { ready(n) }),
        serving("0.5.0"),
    );
    assert_eq!(
        look(&port, &clock).serves("0.5.0").unwrap(),
        CacheMode::ReadWrite
    );
    assert_eq!(port.count("/readyz"), 4);
    // Three pauses of the wait, then two between the probe's three hits.
    assert_eq!(*clock.pauses.borrow(), vec![POLL_INTERVAL; 5]);
    assert_eq!(POLL_INTERVAL, Duration::from_secs(1));

    let clock = CountingClock::default();
    let port = Port::new(Box::new(|_| refused()), serving("0.5.0"));
    let done = look(&port, &clock).serves("0.5.0").unwrap_err();
    assert_eq!(done.report.reason, Some(Reason::NotReady));
    assert_eq!(port.count("/readyz"), READY_ATTEMPTS);
    assert_eq!(port.count("/metricz"), 0);
    assert_eq!(clock.pauses.borrow().len(), READY_ATTEMPTS as usize);
}

/// Asserts that in a hand-over, where the old release answers `/readyz` and the build gauge until
/// the new instance has opened its cache and taken the sockets, `serves` waits for the new
/// version and not for any answer: a new instance that first answers at look 66 (a cache open of
/// 65 s, as a build node took) serves, and one that first answers at the last of the
/// READY_ATTEMPTS looks still does. Catches the wait that ended at the old release's `/readyz`
/// and left the start only the 60 looks of the probe, which failed such a start with
/// `probe_failed`.
#[test]
fn serves_waits_out_the_old_release_until_the_new_one_answers() {
    for first in [65, READY_ATTEMPTS - 1] {
        let clock = CountingClock::default();
        let port = Port::new(
            Box::new(ready),
            Box::new(move |n| {
                let version = if n < first { "0.4.0" } else { "0.5.0" };
                reply(200, &gauge(version, "abc"))
            }),
        );
        assert_eq!(
            look(&port, &clock).serves("0.5.0").unwrap(),
            CacheMode::ReadWrite,
            "first answer at look {}",
            first + 1
        );
        assert_eq!(port.count("/readyz"), first + 1);
        // The wait's looks, the first hit among them, then the probe's own hits.
        assert_eq!(port.count("/metricz"), first + 1 + PROBE_HITS);
        assert_eq!(
            clock.pauses.borrow().len(),
            (first + PROBE_HITS - 1) as usize
        );
    }
}

/// Asserts the failures of `serves` once `/readyz` was answered: an old release that answers
/// every one of READY_ATTEMPTS looks is `probe_failed` after exactly those looks; a port that
/// answered `/readyz` once and then only 503 (draining), never with the version, is
/// `probe_failed` and not `not_ready`; and a version seen once and then lost (another build
/// takes the port back) fails the probe that follows with `probe_failed`.
#[test]
fn serves_fails_with_probe_failed_once_readyz_was_answered() {
    let clock = CountingClock::default();
    let port = Port::new(Box::new(ready), serving("0.4.0"));
    let done = look(&port, &clock).serves("0.5.0").unwrap_err();
    assert_eq!(done.report.reason, Some(Reason::ProbeFailed));
    assert_eq!(port.count("/readyz"), READY_ATTEMPTS);
    assert_eq!(port.count("/metricz"), READY_ATTEMPTS);
    assert_eq!(clock.pauses.borrow().len(), READY_ATTEMPTS as usize);

    let port = Port::new(
        Box::new(|n| {
            if n == 0 {
                ready(n)
            } else {
                reply(503, "draining")
            }
        }),
        serving("0.4.0"),
    );
    let done = look(&port, &CountingClock::default())
        .serves("0.5.0")
        .unwrap_err();
    assert_eq!(done.report.reason, Some(Reason::ProbeFailed));
    assert_eq!(
        done.report.detail.as_deref(),
        Some("no request to port 9300 was answered by 0.5.0")
    );
    assert_eq!(port.count("/metricz"), 1);

    let port = Port::new(
        Box::new(ready),
        Box::new(|n| {
            let version = if n == 0 { "0.5.0" } else { "0.4.0" };
            reply(200, &gauge(version, "abc"))
        }),
    );
    let done = look(&port, &CountingClock::default())
        .serves("0.5.0")
        .unwrap_err();
    assert_eq!(done.report.reason, Some(Reason::ProbeFailed));
    assert_eq!(port.count("/readyz"), 1);
    assert_eq!(port.count("/metricz"), 1 + PROBE_ATTEMPTS);
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

/// The answer of `/readyz` for a look by its number: read-write at look `at` and after it, before
/// it read-only, a 503 of the old instance draining on the same sockets, or no answer, in turn.
fn read_write_from(at: u32) -> Answer {
    Box::new(move |n| match (n >= at, n % 3) {
        (true, _) => reply(200, "ready read-write"),
        (false, 0) => reply(200, "ready read-only"),
        (false, 1) => reply(503, "draining"),
        (false, _) => refused(),
    })
}

/// Asserts that the wait for the cache lock looks once and waits for nothing when the store is
/// already read-write, or has no cache lock (`ready`), and gives that mode. Catches a wait that
/// pauses before its first look, or one that does not end on a store with no lock.
#[test]
fn the_wait_for_the_cache_lock_ends_at_once_on_a_store_that_is_not_read_only() {
    for (body, mode) in [
        ("ready read-write", CacheMode::ReadWrite),
        ("ready", CacheMode::Plain),
    ] {
        let port = Port::new(Box::new(move |_| reply(200, body)), Box::new(|_| refused()));
        let clock = CountingClock::default();
        assert_eq!(look(&port, &clock).read_write(), Some(mode));
        assert_eq!(port.count("/readyz"), 1);
        assert_eq!(port.count("/metricz"), 0);
        assert!(clock.pauses.borrow().is_empty());
    }
}

/// Asserts that the wait for the cache lock waits out every answer that is not read-write (read-only,
/// a 503 of the old instance draining, no answer) a second apart, and ends at a store that turns
/// read-write on its last look, after `READ_WRITE_ATTEMPTS` looks and one pause fewer. Catches a
/// wait that ends early on an answer of the old instance, or one a look short of its bound.
#[test]
fn the_wait_for_the_cache_lock_takes_a_read_write_answer_on_its_last_look() {
    let port = Port::new(
        read_write_from(READ_WRITE_ATTEMPTS - 1),
        Box::new(|_| refused()),
    );
    let clock = CountingClock::default();
    assert_eq!(look(&port, &clock).read_write(), Some(CacheMode::ReadWrite));
    assert_eq!(port.count("/readyz"), READ_WRITE_ATTEMPTS);
    let pauses = clock.pauses.borrow();
    assert_eq!(
        pauses.len(),
        usize::try_from(READ_WRITE_ATTEMPTS - 1).unwrap()
    );
    assert!(pauses.iter().all(|pause| *pause == POLL_INTERVAL));
}

/// Asserts that a store still read-only on every look gives `None` after exactly
/// `READ_WRITE_ATTEMPTS` looks, and that one read-write a look after the bound is not waited for.
/// Catches a wait with no bound, which hostkeeper would kill at the step's bound instead of the
/// hook deferring.
#[test]
fn the_wait_for_the_cache_lock_gives_none_once_its_looks_run_out() {
    let port = Port::new(
        read_write_from(READ_WRITE_ATTEMPTS),
        Box::new(|_| refused()),
    );
    let clock = CountingClock::default();
    assert_eq!(look(&port, &clock).read_write(), None);
    assert_eq!(port.count("/readyz"), READ_WRITE_ATTEMPTS);
    assert_eq!(
        clock.pauses.borrow().len(),
        usize::try_from(READ_WRITE_ATTEMPTS - 1).unwrap()
    );
}

/// The `cutover` bound of hostkeeper's manifest, in seconds.
fn cutover_bound() -> u64 {
    let file =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/hostkeeper/manifest.toml.in");
    let text = std::fs::read_to_string(file).unwrap();
    let (_, rest) = text.split_once("[hook.timeout_secs]\n").unwrap();
    let table: toml::Table = rest.split("\n[").next().unwrap().parse().unwrap();
    u64::try_from(table["cutover"].as_integer().unwrap()).unwrap()
}

/// Whether a `cutover` bound of `secs` is above every look the step may make a second apart: the
/// wait for the release to serve, its probe, and the wait for the cache lock.
fn above_the_cutover_looks(secs: u64) -> bool {
    let looks = POLL_INTERVAL * (READY_ATTEMPTS + PROBE_ATTEMPTS + READ_WRITE_ATTEMPTS);
    Duration::from_secs(secs) > looks
}

/// Asserts that the manifest's `cutover` bound is above every look of the step, the wait for the
/// cache lock included, and that the 1500 s it had before that wait is not. Catches a wait raised
/// without its bound, after which hostkeeper would kill a `cutover` that was still waiting for the
/// new store to take the lock.
#[test]
fn the_manifests_cutover_bound_covers_the_wait_for_the_cache_lock() {
    let secs = cutover_bound();
    assert!(above_the_cutover_looks(secs), "cutover = {secs} s");
    assert!(!above_the_cutover_looks(1500));
}

/// Asserts that before the commit a node whose cache is read-only is no verdict: `writable` defers
/// it, `not_ready` with the retry it is given and a detail that names what serves, while a node that
/// is read-write, or has no cache lock (a storage node), passes with its mode. It would catch a
/// verify that lets a node be committed while its jobs cannot fill their trees.
#[test]
fn a_read_only_node_is_deferred_before_the_commit_and_the_others_pass() {
    let deferred = writable(CacheMode::ReadOnly, "tierstore@b", 7).unwrap_err();
    assert_eq!(deferred.outcome, Outcome::Deferred);
    assert_eq!(deferred.report.reason, Some(Reason::NotReady));
    assert_eq!(deferred.report.retry_after_secs, Some(7));
    assert_eq!(
        deferred.report.detail.as_deref(),
        Some("tierstore@b serves read-only: its cache has not taken the write role yet")
    );
    for mode in [CacheMode::ReadWrite, CacheMode::Plain] {
        assert_eq!(writable(mode, "tierstore@b", 7).unwrap(), mode);
    }
}
