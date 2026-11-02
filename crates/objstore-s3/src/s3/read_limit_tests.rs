//! The time limits of a ranged read, against a scripted store on loopback: a body that keeps
//! arriving is read whole however long it takes in all, and an answer or a body that stops is
//! cut after `request_timeout` as a timeout, from one request.
//!
//! # The clock
//!
//! The tests run on tokio's paused clock, and only a test moves it, once the client has taken
//! what the store sent. A paused clock left alone auto-advances to the next timer whenever the
//! runtime parks and no I/O event wakes a task, and that includes a park while bytes the store
//! wrote are still on their way to the client: a loopback write on Linux can be read by the
//! peer when `write` returns, but on macOS it may be delivered later, and a few such parks in a
//! row took the clock past the client's limit with the body still arriving. So the clock is held
//! from the start of each test ([`Hold`]), and a test waits for the client by events, not by
//! time: the read path reports what it has taken ([`took_head`], [`took_body`]; test builds
//! only), and [`Harness::until`] turns the runtime over, one idle park at a time, until the
//! report says enough. A test that wants a stall cut lets go of the clock
//! ([`Harness::release`]) only once the client is waiting with nothing on its way to it.
//! [`Relay`] holds every write of the store for many idle parks, to show that the outcome does
//! not depend on when the bytes arrive.

use super::tests::{config, r};
use super::*;
use std::cell::Cell;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// What the client has taken of the store's answer, as the read path reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Taken {
    /// Answers whose headers arrived.
    heads: usize,
    /// Bytes of body that arrived, in all.
    body: usize,
}

thread_local! {
    /// What the client has taken, on the thread whose runtime a [`Harness`] runs; `None`
    /// elsewhere, so a read in any other test reports to nothing.
    static TAKEN: Cell<Option<Taken>> = const { Cell::new(None) };
}

/// Called by `Conn::send_read` (test builds only) when an answer's headers arrived.
pub(super) fn took_head() {
    record(|t| t.heads += 1);
}

/// Called by `Conn::read_body_within` (test builds only) for each piece of a body.
pub(super) fn took_body(bytes: usize) {
    record(|t| t.body += bytes);
}

fn record(add: impl FnOnce(&mut Taken)) {
    TAKEN.with(|cell| {
        if let Some(mut taken) = cell.get() {
            add(&mut taken);
            cell.set(Some(taken));
        }
    });
}

fn taken() -> Taken {
    TAKEN.with(Cell::get).unwrap_or_default()
}

/// Keeps the paused clock from auto-advancing while it lives: tokio does not auto-advance a
/// paused clock while a `spawn_blocking` task runs, and this one runs until the hold is
/// dropped.
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

/// The idle parks of a runtime, counted as they come.
#[derive(Default)]
struct Parks {
    count: Mutex<u64>,
    moved: Condvar,
}

impl Parks {
    fn parked(&self) {
        *self.count.lock().unwrap() += 1;
        self.moved.notify_all();
    }

    fn count(&self) -> u64 {
        *self.count.lock().unwrap()
    }

    /// Blocks until there were more parks than `seen`.
    fn wait_past(&self, seen: u64) {
        let mut count = self.count.lock().unwrap();
        while *count <= seen {
            count = self.moved.wait(count).unwrap();
        }
    }
}

/// A test's runtime: current-thread, its clock paused and held from the start.
struct Harness {
    parks: Arc<Parks>,
    hold: Option<Hold>,
}

impl Harness {
    /// Runs `test` on a current-thread runtime started paused, with the clock held and what
    /// the client takes counted from zero. Each idle park also turns `relay`, if there is
    /// one.
    fn run<F: Future<Output = ()>>(relay: Option<Arc<Relay>>, test: impl FnOnce(Self) -> F) {
        let parks = Arc::new(Parks::default());
        let counted = Arc::clone(&parks);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .on_thread_park(move || {
                // Counted first: a relay that fails then fails the test, not a turn waiting.
                counted.parked();
                if let Some(relay) = &relay {
                    relay.turn();
                }
            })
            .build()
            .unwrap();
        TAKEN.with(|cell| cell.set(Some(Taken::default())));
        runtime.block_on(async {
            let hold = Some(Hold::new());
            test(Self { parks, hold }).await;
        });
        TAKEN.with(|cell| cell.set(None));
    }

    /// Lets the clock auto-advance from now on.
    fn release(&mut self) {
        self.hold = None;
    }

    /// Turns the runtime over until `enough` holds; fails the test if `reading` ends first.
    async fn until(&self, reading: &mut JoinHandle<Answer>, enough: impl Fn() -> bool) {
        while !enough() {
            if reading.is_finished() {
                panic!(
                    "the read ended first, having taken {:?}: {:?}",
                    taken(),
                    reading.await
                );
            }
            self.turn().await;
        }
    }

    /// Returns after the runtime has parked once more with nothing to do.
    async fn turn(&self) {
        let parks = Arc::clone(&self.parks);
        let seen = parks.count();
        tokio::task::spawn_blocking(move || parks.wait_past(seen))
            .await
            .unwrap();
    }
}

/// What a ranged read gives.
type Answer = Result<Bytes, ObjectStoreError>;

/// The `request_timeout` of the reads. It passes on the test's clock, so it costs no time.
const LIMIT: Duration = Duration::from_secs(10);

/// Reads `len` bytes from offset 0 of the store at `endpoint`, on a task of its own, through
/// an `S3Store` whose limit is [`LIMIT`].
fn read(endpoint: &str, len: usize) -> JoinHandle<Answer> {
    let mut c = config(endpoint, "test-bucket");
    c.request_timeout = LIMIT;
    let store = S3Store::new(c).unwrap();
    let key = ObjectKey::new("obj").unwrap();
    tokio::spawn(async move { store.get_range(&key, r(0, len as u64)).await })
}

/// Whether `err` is the client's own cut of a read that made no progress for its limit.
fn is_stall(err: &ObjectStoreError) -> bool {
    matches!(err, ObjectStoreError::Transport(cause) if cause.is::<Stalled>())
}

/// What a scripted store sends for a ranged read of `len` bytes.
#[derive(Clone, Copy)]
enum Script {
    /// Headers, then the body as the test feeds it ([`Store::feed`]).
    Fed,
    /// Headers, half the body, then nothing.
    StallsInBody,
    /// Nothing at all after the request.
    Silent,
    /// A 500 whose body is promised and never sent.
    StallsInError,
}

/// A one-object store on loopback that answers every request as its script says.
struct Store {
    endpoint: String,
    requests: Arc<AtomicUsize>,
    pieces: mpsc::UnboundedSender<Vec<u8>>,
}

impl Store {
    /// A store on an ephemeral loopback port.
    async fn new(len: usize, script: Script) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let (pieces, fed) = mpsc::unbounded_channel();
        let count = Arc::clone(&requests);
        tokio::spawn(async move {
            // The fed body goes to the first request; a second one gets the headers alone.
            let mut fed = Some(fed);
            loop {
                let (sock, _) = listener.accept().await.unwrap();
                let count = Arc::clone(&count);
                tokio::spawn(answer(sock, len, script, count, fed.take()));
            }
        });
        Self {
            endpoint,
            requests,
            pieces,
        }
    }

    /// Writes `piece` of the body of a [`Script::Fed`] answer.
    fn feed(&self, piece: Vec<u8>) {
        self.pieces.send(piece).unwrap();
    }

    /// The requests the store has read.
    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

/// Reads one request from `sock`, counts it, and answers as `script` says.
async fn answer(
    mut sock: tokio::net::TcpStream,
    len: usize,
    script: Script,
    count: Arc<AtomicUsize>,
    fed: Option<mpsc::UnboundedReceiver<Vec<u8>>>,
) {
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
        match sock.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => seen.extend_from_slice(&buf[..n]),
        }
    }
    count.fetch_add(1, Ordering::SeqCst);
    let head = format!(
        "HTTP/1.1 206 Partial Content\r\ncontent-length: {len}\r\n\
         content-range: bytes 0-{}/{len}\r\n\r\n",
        len - 1
    );
    match script {
        Script::Silent => {}
        Script::StallsInError => {
            let _ = sock
                .write_all(b"HTTP/1.1 500 Oops\r\ncontent-length: 100\r\n\r\n")
                .await;
        }
        Script::StallsInBody => {
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(&vec![7u8; len / 2]).await;
        }
        Script::Fed => {
            let _ = sock.write_all(head.as_bytes()).await;
            if let Some(mut fed) = fed {
                while let Some(piece) = fed.recv().await {
                    let _ = sock.write_all(&piece).await;
                }
            }
        }
    }
    // Keep the connection open until the test ends.
    std::future::pending::<()>().await;
}

/// A relay on loopback between a client and a store that only the runtime's idle parks move
/// (a [`Harness`] calls [`Relay::turn`] as its runtime is about to park with nothing to do):
/// at each, what the client sent goes on to the store at once, and what the store sent goes on
/// to the client `delay` parks after the relay read it. Every park in between finds nothing new
/// for the client, as a park may on macOS while a loopback write is still being delivered.
struct Relay {
    state: Mutex<RelayState>,
    addr: std::net::SocketAddr,
}

struct RelayState {
    listener: std::net::TcpListener,
    store: Option<std::net::SocketAddr>,
    /// The client's connection and the relay's own to the store, once the client connected.
    pair: Option<(std::net::TcpStream, std::net::TcpStream)>,
    /// The store's writes not yet passed on, each with the parks it still waits.
    held: VecDeque<(usize, Vec<u8>)>,
    delay: usize,
}

impl Relay {
    fn new(delay: usize) -> Arc<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        Arc::new(Self {
            state: Mutex::new(RelayState {
                listener,
                store: None,
                pair: None,
                held: VecDeque::new(),
                delay,
            }),
            addr,
        })
    }

    /// The endpoint at which a client reaches the store at `endpoint` through this relay.
    fn front(&self, endpoint: &str) -> String {
        let store = endpoint.trim_start_matches("http://").parse().unwrap();
        self.state.lock().unwrap().store = Some(store);
        format!("http://{}", self.addr)
    }

    /// One idle park's worth of relaying.
    fn turn(&self) {
        let mut state = self.state.lock().unwrap();
        let s = &mut *state;
        if s.pair.is_none() {
            let Some(store) = s.store else { return };
            match s.listener.accept() {
                Ok((client, _)) => {
                    let upstream = std::net::TcpStream::connect(store).unwrap();
                    client.set_nonblocking(true).unwrap();
                    upstream.set_nonblocking(true).unwrap();
                    s.pair = Some((client, upstream));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(e) => panic!("relay accept: {e}"),
            }
        }
        let Some((client, store)) = s.pair.as_mut() else {
            return;
        };
        store.write_all(&drain(client)).unwrap();
        for held in &mut s.held {
            held.0 -= 1;
        }
        while s.held.front().is_some_and(|held| held.0 == 0) {
            let (_, bytes) = s.held.pop_front().unwrap();
            client.write_all(&bytes).unwrap();
        }
        let down = drain(store);
        if !down.is_empty() {
            s.held.push_back((s.delay, down));
        }
    }
}

/// Everything the non-blocking `sock` has to read now.
fn drain(sock: &mut std::net::TcpStream) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match sock.read(&mut buf) {
            Ok(0) => return out,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return out,
            Err(e) => panic!("relay read: {e}"),
        }
    }
}

/// A steady read: the store sends the body of [`STEADY_LEN`] bytes in [`STEADY_PIECES`]
/// pieces, the clock moving [`STEADY_EVERY`] before each and the next piece sent only once the
/// client has taken the last, through `relay` if there is one.
async fn read_steadily(h: Harness, relay: Option<Arc<Relay>>) {
    let store = Store::new(STEADY_LEN, Script::Fed).await;
    let endpoint = match &relay {
        Some(relay) => relay.front(&store.endpoint),
        None => store.endpoint.clone(),
    };
    let mut reading = read(&endpoint, STEADY_LEN);
    let piece = STEADY_LEN / STEADY_PIECES;
    for sent in 1..=STEADY_PIECES {
        tokio::time::advance(STEADY_EVERY).await;
        store.feed(vec![7u8; piece]);
        h.until(&mut reading, || taken().body == sent * piece).await;
    }
    let got = reading.await.unwrap().unwrap();
    assert_eq!(got.as_ref(), vec![7u8; STEADY_LEN].as_slice());
    assert_eq!(
        taken(),
        Taken {
            heads: 1,
            body: STEADY_LEN
        }
    );
    assert_eq!(store.requests(), 1);
}

const STEADY_LEN: usize = 1000;
const STEADY_PIECES: usize = 20;
/// Under [`LIMIT`]; the pieces take eight times [`LIMIT`] in all.
const STEADY_EVERY: Duration = Duration::from_secs(4);

/// Catches: a read cut by a limit on the whole request: a body that arrives steadily, for
/// several times `request_timeout` in all (20 pieces, 4 s apart, against a limit of 10 s),
/// must come back whole, from one request.
#[test]
fn a_ranged_read_that_keeps_progressing_outlasts_the_request_timeout() {
    Harness::run(None, |h| read_steadily(h, None));
}

/// Catches: an outcome that depends on when the store's bytes reach the client. The steady
/// read above with every write of the store held for fifty idle parks of the runtime before
/// the client can read it, the order of events of the run on macOS that failed. While the
/// clock was left to auto-advance after the store had the request, this read failed every
/// time as a stall.
#[test]
fn a_steady_body_that_reaches_the_client_fifty_idle_turns_late_comes_back_whole() {
    let relay = Relay::new(50);
    Harness::run(Some(Arc::clone(&relay)), |h| read_steadily(h, Some(relay)));
}

/// Catches: a stalled body that is never cut, or cut but reported as a plain transport
/// failure and sent again: it fails as the client's stall, after one request. The clock is
/// let go once the client has taken the half of the body the store sends.
#[test]
fn a_ranged_read_whose_body_stops_fails_as_a_timeout_and_is_not_resent() {
    Harness::run(None, |mut h| async move {
        let store = Store::new(1000, Script::StallsInBody).await;
        let mut reading = read(&store.endpoint, 1000);
        h.until(&mut reading, || taken().body == 500).await;
        h.release();
        let err = reading.await.unwrap().unwrap_err();
        assert!(is_stall(&err), "{err:?}");
        assert!(!is_retryable(&err));
        assert!(err.to_string().contains("timed out"), "{err}");
        assert_eq!(
            taken(),
            Taken {
                heads: 1,
                body: 500
            }
        );
        assert_eq!(store.requests(), 1);
    });
}

/// Catches: the wait for the answer's headers left unbounded once the whole-request limit is
/// gone. The clock is let go once the store has the request.
#[test]
fn a_ranged_read_with_no_answer_fails_as_a_timeout() {
    Harness::run(None, |mut h| async move {
        let store = Store::new(10, Script::Silent).await;
        let mut reading = read(&store.endpoint, 10);
        h.until(&mut reading, || store.requests() == 1).await;
        h.release();
        let err = reading.await.unwrap().unwrap_err();
        assert!(is_stall(&err), "{err:?}");
        assert!(err.to_string().contains("timed out"), "{err}");
        assert_eq!(taken(), Taken::default());
        assert_eq!(store.requests(), 1);
    });
}

/// Catches: an error answer whose body never arrives holding the read forever. The clock is
/// let go once the client has the answer's headers.
#[test]
fn an_error_body_that_stops_fails_as_a_timeout() {
    Harness::run(None, |mut h| async move {
        let store = Store::new(10, Script::StallsInError).await;
        let mut reading = read(&store.endpoint, 10);
        h.until(&mut reading, || taken().heads == 1).await;
        h.release();
        let err = reading.await.unwrap().unwrap_err();
        assert!(is_stall(&err), "{err:?}");
        assert!(err.to_string().contains("timed out"), "{err}");
        assert_eq!(taken(), Taken { heads: 1, body: 0 });
        assert_eq!(store.requests(), 1);
    });
}
