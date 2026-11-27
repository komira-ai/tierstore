//! Endpoints set aside, against fronts of one S3 service on loopback and a gate: an endpoint on
//! loopback that accepts every connection and reads its request, and answers nothing until a
//! test opens it, after which it relays every connection, the ones it held included, to a front.
//!
//! # The clock
//!
//! Each test runs on tokio's paused clock, held from its start ([`Hold`]): it moves only when the
//! test advances it, never on its own. A test waits for the client by events (the gate counts
//! the requests it holds), and a stall is the test advancing the clock by the request limit once
//! the request is held. So a read that completes while the clock stands still waited for no
//! limit at all, and a read that went to the silent gate would never complete:
//! [`read_avoiding`] fails the test as soon as the gate holds one more request than it expects.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use crate::s3::fake::{Failure, FakeS3};
use crate::s3::{EndpointState, S3Store, S3Tuning, Stalled};
use crate::{ByteRange, Capabilities, ObjectKey, ObjectStore, ObjectStoreError};

/// The `request_timeout` of the stores: the default. It passes on the test's clock only.
const LIMIT: Duration = Duration::from_secs(60);

/// What the object of the tests holds.
const BODY: &[u8] = b"hello";

const CAPS: Capabilities = Capabilities {
    conditional_put: true,
    object_lock: false,
};

/// Keeps the paused clock from auto-advancing while it lives: tokio does not auto-advance a
/// paused clock while a `spawn_blocking` task runs, and this one runs until the hold is dropped.
struct Hold {
    _dropped_ends_it: std::sync::mpsc::Sender<()>,
}

impl Hold {
    fn new() -> Self {
        let (hold, dropped) = std::sync::mpsc::channel::<()>();
        drop(tokio::task::spawn_blocking(move || {
            let _ = dropped.recv();
        }));
        Self {
            _dropped_ends_it: hold,
        }
    }
}

/// An endpoint that holds every request it is sent, unanswered, until [`Gate::open_to`] names a
/// front; from then on it relays every connection to that front, the held ones included.
/// [`Gate::release`] relays one held request alone.
struct Gate {
    endpoint: String,
    /// The requests held so far: their head arrived while the gate was shut.
    held: watch::Receiver<usize>,
    /// The connections whose client went away while the gate held them.
    dropped: watch::Receiver<usize>,
    relay: watch::Sender<Relay>,
}

/// Where a [`Gate`] relays its connections.
#[derive(Clone, Default)]
struct Relay {
    /// Every connection, held or new.
    all: Option<SocketAddr>,
    /// The held requests named by their number (the first held is 1), each alone.
    one: BTreeMap<usize, SocketAddr>,
}

fn addr_of(front: &FakeS3) -> SocketAddr {
    front
        .endpoint()
        .trim_start_matches("http://")
        .parse()
        .unwrap()
}

impl Gate {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (held_tx, held) = watch::channel(0);
        let (dropped_tx, dropped) = watch::channel(0);
        let (relay, relay_rx) = watch::channel(Relay::default());
        let (held_tx, dropped_tx) = (Arc::new(held_tx), Arc::new(dropped_tx));
        tokio::spawn(async move {
            loop {
                let (sock, _) = listener.accept().await.unwrap();
                let counts = (Arc::clone(&held_tx), Arc::clone(&dropped_tx));
                tokio::spawn(serve(sock, relay_rx.clone(), counts));
            }
        });
        Self {
            endpoint,
            held,
            dropped,
            relay,
        }
    }

    /// From now on every connection, held or new, is relayed to `front`.
    fn open_to(&self, front: &FakeS3) {
        self.relay.send_modify(|r| r.all = Some(addr_of(front)));
    }

    /// Relays the `n`th request the gate held (the first is 1) to `front`, and no other.
    fn release(&self, n: usize, front: &FakeS3) {
        self.relay.send_modify(|r| {
            r.one.insert(n, addr_of(front));
        });
    }

    fn held(&self) -> usize {
        *self.held.borrow()
    }

    /// Returns once the gate has held `count` requests.
    async fn held_reaches(&self, count: usize) {
        let mut held = self.held.clone();
        held.wait_for(|n| *n >= count).await.unwrap();
    }

    /// Returns once `count` clients went away from connections the gate held.
    async fn dropped_reaches(&self, count: usize) {
        let mut dropped = self.dropped.clone();
        dropped.wait_for(|n| *n >= count).await.unwrap();
    }
}

type Counts = (Arc<watch::Sender<usize>>, Arc<watch::Sender<usize>>);

/// One connection of a [`Gate`]: relayed at once if the gate is open; else held, every byte the
/// client sends kept, until the gate opens or releases it (and then relayed from its first
/// byte) or the client goes away.
async fn serve(mut sock: TcpStream, mut relay: watch::Receiver<Relay>, (held, dropped): Counts) {
    let mut sent = Vec::new();
    let mut mine: Option<usize> = None;
    let mut chunk = [0u8; 4096];
    let to = loop {
        let target = {
            let r = relay.borrow_and_update();
            r.all.or_else(|| mine.and_then(|n| r.one.get(&n).copied()))
        };
        if let Some(to) = target {
            break to;
        }
        tokio::select! {
            changed = relay.changed() => {
                // The gate is gone: the test is over.
                if changed.is_err() {
                    return;
                }
            }
            n = sock.read(&mut chunk) => {
                let n = n.unwrap_or(0);
                if n == 0 {
                    dropped.send_modify(|d| *d += 1);
                    return;
                }
                sent.extend_from_slice(&chunk[..n]);
                if mine.is_none() && sent.windows(4).any(|w| w == b"\r\n\r\n") {
                    held.send_modify(|h| {
                        *h += 1;
                        mine = Some(*h);
                    });
                }
            }
        }
    };
    let mut front = TcpStream::connect(to).await.unwrap();
    if front.write_all(&sent).await.is_ok() {
        let _ = tokio::io::copy_bidirectional(&mut sock, &mut front).await;
    }
}

/// A service with three fronts: two the stores reach directly and one for a gate to open to.
struct Service {
    fronts: [FakeS3; 3],
}

impl Service {
    /// The service, holding [`BODY`] at [`key`].
    async fn start() -> Self {
        let first = FakeS3::start().await;
        first.require_credentials("fake-key", "fake-secret");
        let second = first.start_front().await;
        let third = second.start_front().await;
        first
            .store(CAPS)
            .put_new(&key(), Bytes::from_static(BODY), None)
            .await
            .unwrap();
        Self {
            fronts: [first, second, third],
        }
    }

    /// The GETs of the object its fronts took.
    fn reads(&self) -> usize {
        self.fronts
            .iter()
            .flat_map(FakeS3::requests)
            .filter(|r| r.starts_with("GET /tierstore-fake/k"))
            .count()
    }
}

fn key() -> ObjectKey {
    ObjectKey::new("k").unwrap()
}

fn all() -> ByteRange {
    ByteRange::new(0, BODY.len() as u64).unwrap()
}

/// A store of the service reached through `endpoints`, in that order, with the default limits
/// and no wait before the look after a PUT that got no answer.
fn store(service: &Service, endpoints: &[&str]) -> Arc<S3Store> {
    let mut config = service.fronts[0].config(CAPS);
    config.endpoint = endpoints[0].to_owned();
    config.request_timeout = LIMIT;
    let also: Vec<String> = endpoints[1..].iter().map(|e| (*e).to_owned()).collect();
    let tuning = S3Tuning {
        cut_put_look_after: Duration::ZERO,
        ..S3Tuning::default()
    };
    Arc::new(S3Store::new_rotating(config, tuning, &also).unwrap())
}

/// For each endpoint of `store`, whether it is set aside.
fn aside(store: &S3Store) -> Vec<bool> {
    store.endpoints().iter().map(|e| e.set_aside).collect()
}

/// Reads the object, failing the test if `gate` holds a request beyond the `expected` it held
/// before: a read sent there would never end on the held clock.
async fn read_avoiding(store: &S3Store, gate: &Gate, expected: usize) -> Bytes {
    let key = key();
    tokio::select! {
        got = store.get_range(&key, all()) => got.unwrap(),
        () = gate.held_reaches(expected + 1) => panic!("a read went to the silent endpoint"),
    }
}

/// Starts a read of the object on a task of its own.
fn spawn_read(store: &Arc<S3Store>) -> tokio::task::JoinHandle<Result<Bytes, ObjectStoreError>> {
    let store = Arc::clone(store);
    tokio::spawn(async move { store.get_range(&key(), all()).await })
}

fn is_stall(err: &ObjectStoreError) -> bool {
    matches!(err, ObjectStoreError::Transport(cause) if cause.is::<Stalled>())
}

/// Turns the runtime over until `done`, a bounded number of times.
async fn until(mut done: impl FnMut() -> bool) {
    for _ in 0..100_000 {
        if done() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the condition never held");
}

/// The first read lands on the silent gate, which holds it; the clock then runs out its limit,
/// and the read, sent on to the next endpoint, comes back whole.
async fn stall_the_first_read(store: &Arc<S3Store>, gate: &Gate) {
    let first = spawn_read(store);
    gate.held_reaches(1).await;
    tokio::time::advance(LIMIT).await;
    assert_eq!(&first.await.unwrap().unwrap()[..], BODY);
}

/// Asserts that with three endpoints of one store and the first accepting
/// connections and never answering, 100 reads all come back whole; the first waits out its
/// limit on the silent endpoint and is sent on to the next, and the other 99 complete with the
/// clock standing still, none sent to the silent endpoint, which is reported set aside. Before
/// endpoints were set aside, the first read failed as a stall, and every third read went to the
/// silent endpoint and waited out its limit.
#[tokio::test(start_paused = true)]
async fn a_silent_endpoint_costs_the_first_read_its_limit_and_the_next_99_reads_nothing() {
    let _hold = Hold::new();
    let service = Service::start().await;
    let gate = Gate::start().await;
    let s = store(
        &service,
        &[
            &gate.endpoint,
            service.fronts[0].endpoint(),
            service.fronts[1].endpoint(),
        ],
    );
    stall_the_first_read(&s, &gate).await;
    for _ in 0..99 {
        assert_eq!(&read_avoiding(&s, &gate, 1).await[..], BODY);
    }
    assert_eq!(gate.held(), 1);
    assert_eq!(service.reads(), 100);
    let states = s.endpoints();
    assert_eq!(
        states
            .iter()
            .map(|e| e.endpoint.as_str())
            .collect::<Vec<_>>(),
        [
            gate.endpoint.as_str(),
            service.fronts[0].endpoint(),
            service.fronts[1].endpoint()
        ]
    );
    assert_eq!(aside(&s), [true, false, false]);
    assert_eq!(
        states[0],
        EndpointState {
            endpoint: gate.endpoint.clone(),
            set_aside: true
        }
    );
}

/// Asserts the probe: when the time aside is up, the next read starts one probe of the endpoint
/// and is itself served by another; while the probe is out no read goes there; a probe with no
/// answer sets the endpoint aside for longer (still aside after the first time, 2 s, again); and
/// once a probe is answered the endpoint is back in turn and takes reads again.
#[tokio::test(start_paused = true)]
async fn a_probe_with_no_answer_sets_the_endpoint_aside_longer_and_an_answered_one_brings_it_back()
{
    let _hold = Hold::new();
    let service = Service::start().await;
    let gate = Gate::start().await;
    let s = store(
        &service,
        &[
            &gate.endpoint,
            service.fronts[0].endpoint(),
            service.fronts[1].endpoint(),
        ],
    );
    stall_the_first_read(&s, &gate).await;
    gate.dropped_reaches(1).await;

    // The first time aside, at most 2 s, is up: this read starts the probe, and is not it.
    tokio::time::advance(super::FIRST_ASIDE).await;
    assert_eq!(&read_avoiding(&s, &gate, 2).await[..], BODY);
    until(|| gate.held() == 2).await;
    for _ in 0..6 {
        assert_eq!(&read_avoiding(&s, &gate, 2).await[..], BODY);
    }
    assert_eq!(aside(&s), [true, false, false]);

    // The probe gets no answer: its client goes away once its limit is out.
    tokio::time::advance(LIMIT).await;
    gate.dropped_reaches(2).await;
    // The second time aside is 3 s at least.
    tokio::time::advance(super::FIRST_ASIDE).await;
    for _ in 0..6 {
        assert_eq!(&read_avoiding(&s, &gate, 2).await[..], BODY);
    }
    assert_eq!(aside(&s), [true, false, false]);

    // It is up after 4 s: the next probe is held, and answered once the gate opens.
    tokio::time::advance(super::FIRST_ASIDE).await;
    assert_eq!(&read_avoiding(&s, &gate, 3).await[..], BODY);
    until(|| gate.held() == 3).await;
    gate.open_to(&service.fronts[2]);
    until(|| aside(&s) == [false, false, false]).await;
    let probes = service.fronts[2].requests();
    assert_eq!(
        probes.first().map(String::as_str),
        Some("GET /?"),
        "{probes:?}"
    );

    for _ in 0..3 {
        assert_eq!(&s.get_range(&key(), all()).await.unwrap()[..], BODY);
    }
    let through_gate = service.fronts[2].requests();
    assert!(
        through_gate
            .iter()
            .any(|r| r.starts_with("GET /tierstore-fake/k")),
        "{through_gate:?}"
    );
}

/// Asserts that the last endpoint in turn is never set aside: with two silent endpoints the
/// first is set aside and the read is sent on to the second, whose stall is then the read's
/// error, and the second stays in turn and takes the next read; with one silent endpoint the
/// stall is the error and the endpoint is never set aside.
#[tokio::test(start_paused = true)]
async fn the_last_endpoint_in_turn_is_never_set_aside_and_its_stall_is_the_error() {
    let _hold = Hold::new();
    let service = Service::start().await;
    let (one, two) = (Gate::start().await, Gate::start().await);
    let s = store(&service, &[&one.endpoint, &two.endpoint]);
    let reading = spawn_read(&s);
    one.held_reaches(1).await;
    tokio::time::advance(LIMIT).await;
    two.held_reaches(1).await;
    tokio::time::advance(LIMIT).await;
    let err = reading.await.unwrap().unwrap_err();
    assert!(is_stall(&err), "{err}");
    assert_eq!(aside(&s), [true, false]);
    let next = spawn_read(&s);
    two.held_reaches(2).await;
    tokio::time::advance(LIMIT).await;
    assert!(is_stall(&next.await.unwrap().unwrap_err()));
    assert_eq!(aside(&s), [true, false]);

    let alone = Gate::start().await;
    let s = store(&service, &[&alone.endpoint]);
    for n in 1..=2 {
        let reading = spawn_read(&s);
        alone.held_reaches(n).await;
        tokio::time::advance(LIMIT).await;
        assert!(is_stall(&reading.await.unwrap().unwrap_err()));
        assert_eq!(aside(&s), [false]);
    }
}

/// Asserts that an endpoint whose connection is refused is set aside and the request lands on
/// the next, and that the requests after it go to the others alone: five writes, five objects,
/// each written once.
#[tokio::test(start_paused = true)]
async fn an_endpoint_that_refuses_the_connection_is_set_aside() {
    let _hold = Hold::new();
    let service = Service::start().await;
    let refused = format!("http://{}", tierstore_testkit::port::dead_addr());
    let s = store(
        &service,
        &[
            &refused,
            service.fronts[0].endpoint(),
            service.fronts[1].endpoint(),
        ],
    );
    for n in 0..5 {
        let key = ObjectKey::new(format!("w{n}")).unwrap();
        s.put_new(&key, Bytes::from_static(BODY), None)
            .await
            .unwrap();
    }
    assert_eq!(aside(&s), [true, false, false]);
    let puts = service
        .fronts
        .iter()
        .flat_map(FakeS3::requests)
        .filter(|r| r.starts_with("PUT /tierstore-fake/w"))
        .count();
    assert_eq!(puts, 5);
}

/// Asserts that a write whose endpoint stalls is not sent again (it may have landed) but sets
/// the endpoint aside, so the next write goes to the other endpoint and lands there; and that a
/// HEAD that stalls is sent on to the next endpoint and answered.
#[tokio::test(start_paused = true)]
async fn a_stalled_write_is_not_sent_again_and_a_stalled_head_is() {
    let _hold = Hold::new();
    let service = Service::start().await;
    let gate = Gate::start().await;
    let s = store(&service, &[&gate.endpoint, service.fronts[0].endpoint()]);
    let writing = {
        let s = Arc::clone(&s);
        tokio::spawn(async move {
            let key = ObjectKey::new("w").unwrap();
            s.put_new(&key, Bytes::from_static(BODY), None).await
        })
    };
    gate.held_reaches(1).await;
    tokio::time::advance(LIMIT).await;
    let err = writing.await.unwrap().unwrap_err();
    assert!(matches!(err, ObjectStoreError::Transport(_)), "{err}");
    assert_eq!(aside(&s), [true, false]);
    let seen = service.fronts[0].requests();
    assert!(
        !seen.iter().any(|r| r.starts_with("PUT /tierstore-fake/w?")),
        "{seen:?}"
    );
    let next = ObjectKey::new("w2").unwrap();
    s.put_new(&next, Bytes::from_static(BODY), None)
        .await
        .unwrap();
    assert_eq!(gate.held(), 1);
    let seen = service.fronts[0].requests();
    assert!(
        seen.iter()
            .any(|r| r.starts_with("PUT /tierstore-fake/w2?")),
        "{seen:?}"
    );

    let s = store(&service, &[&gate.endpoint, service.fronts[0].endpoint()]);
    let heading = {
        let s = Arc::clone(&s);
        tokio::spawn(async move { s.len(&key()).await })
    };
    gate.held_reaches(2).await;
    tokio::time::advance(LIMIT).await;
    assert_eq!(heading.await.unwrap().unwrap(), Some(BODY.len() as u64));
    assert_eq!(aside(&s), [true, false]);
}

/// Asserts that an answer of 500 proves the endpoint alive: the HEAD fails with the store's
/// error, is not sent again, and no endpoint is set aside.
#[tokio::test(start_paused = true)]
async fn an_answer_of_500_does_not_set_the_endpoint_aside() {
    let _hold = Hold::new();
    let service = Service::start().await;
    let s = store(
        &service,
        &[service.fronts[0].endpoint(), service.fronts[1].endpoint()],
    );
    service.fronts[0].fail(Failure::Head(1));
    let err = s.len(&key()).await.unwrap_err();
    assert!(
        matches!(err, ObjectStoreError::Service { status: 500, .. }),
        "{err}"
    );
    assert_eq!(aside(&s), [false, false]);
    let heads = service
        .fronts
        .iter()
        .flat_map(FakeS3::requests)
        .filter(|r| r.starts_with("HEAD "))
        .count();
    assert_eq!(heads, 1);
}

/// Asserts that one stall seen by two reads at once is one failure: both reads are sent on and
/// come back whole, and the endpoint is probed when the first time aside (2 s) is up, not later.
#[tokio::test(start_paused = true)]
async fn two_reads_held_by_one_stall_count_it_once() {
    let _hold = Hold::new();
    let service = Service::start().await;
    let gate = Gate::start().await;
    let s = store(
        &service,
        &[
            &gate.endpoint,
            service.fronts[0].endpoint(),
            service.fronts[1].endpoint(),
        ],
    );
    // Picks 0 to 3: the gate, the two fronts, the gate.
    let reads: Vec<_> = (0..4).map(|_| spawn_read(&s)).collect();
    gate.held_reaches(2).await;
    tokio::time::advance(LIMIT).await;
    for read in reads {
        assert_eq!(&read.await.unwrap().unwrap()[..], BODY);
    }
    assert_eq!(aside(&s), [true, false, false]);
    tokio::time::advance(super::FIRST_ASIDE).await;
    assert_eq!(&read_avoiding(&s, &gate, 3).await[..], BODY);
    until(|| gate.held() == 3).await;
}

/// Asserts that an endpoint answering a request sent to it before it was set aside is back in
/// turn at once: a read held at the gate is answered once the gate opens, after another read
/// stalled there and set it aside.
#[tokio::test(start_paused = true)]
async fn an_answer_to_a_request_sent_before_the_endpoint_was_set_aside_brings_it_back() {
    let _hold = Hold::new();
    let service = Service::start().await;
    let gate = Gate::start().await;
    let s = store(
        &service,
        &[
            &gate.endpoint,
            service.fronts[0].endpoint(),
            service.fronts[1].endpoint(),
        ],
    );
    let early = spawn_read(&s);
    gate.held_reaches(1).await;
    tokio::time::advance(LIMIT / 2).await;
    // Picks 1 and 2 go to the fronts; pick 3 is the gate again, with a later limit.
    for _ in 0..2 {
        assert_eq!(&read_avoiding(&s, &gate, 1).await[..], BODY);
    }
    let late = spawn_read(&s);
    gate.held_reaches(2).await;
    tokio::time::advance(LIMIT / 2).await;
    assert_eq!(&early.await.unwrap().unwrap()[..], BODY);
    assert_eq!(aside(&s), [true, false, false]);
    gate.open_to(&service.fronts[2]);
    assert_eq!(&late.await.unwrap().unwrap()[..], BODY);
    assert_eq!(aside(&s), [false, false, false]);
}

/// Asserts that a probe with no answer changes nothing for an endpoint that an answer brought
/// back while the probe was out: a read held at the gate since before the endpoint was set aside
/// is answered while the probe is held, which puts the endpoint back in turn, and when the
/// probe's limit then runs out the endpoint stays in turn.
#[tokio::test(start_paused = true)]
async fn a_probe_with_no_answer_leaves_an_endpoint_an_answer_brought_back_in_turn() {
    let _hold = Hold::new();
    let service = Service::start().await;
    let gate = Gate::start().await;
    let s = store(
        &service,
        &[
            &gate.endpoint,
            service.fronts[0].endpoint(),
            service.fronts[1].endpoint(),
        ],
    );
    let early = spawn_read(&s);
    gate.held_reaches(1).await;
    tokio::time::advance(LIMIT / 2).await;
    for _ in 0..2 {
        assert_eq!(&read_avoiding(&s, &gate, 1).await[..], BODY);
    }
    // Held second, with a limit that runs out after the probe's start.
    let late = spawn_read(&s);
    gate.held_reaches(2).await;
    tokio::time::advance(LIMIT / 2).await;
    assert_eq!(&early.await.unwrap().unwrap()[..], BODY);
    gate.dropped_reaches(1).await;
    assert_eq!(aside(&s), [true, false, false]);

    // The time aside is up: this read starts the probe, held third.
    tokio::time::advance(super::FIRST_ASIDE).await;
    assert_eq!(&read_avoiding(&s, &gate, 3).await[..], BODY);
    until(|| gate.held() == 3).await;

    // The late read is answered: the endpoint is back in turn while its probe is out.
    gate.release(2, &service.fronts[2]);
    assert_eq!(&late.await.unwrap().unwrap()[..], BODY);
    assert_eq!(aside(&s), [false, false, false]);

    // The probe's limit runs out with no answer: the endpoint stays in turn.
    tokio::time::advance(LIMIT).await;
    gate.dropped_reaches(2).await;
    assert_eq!(aside(&s), [false, false, false]);
}
