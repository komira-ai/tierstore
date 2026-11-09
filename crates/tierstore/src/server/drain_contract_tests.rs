//! The drain of every shape of node: readiness, the settle, the door, the deadline.
//!
//! Each shape is run by its own entry point (a build node by `serve_cache`, a storage node with a
//! cache by `serve_combined`, a storage node by `run_serving`), and stopped in two ways: by its
//! stop future, and by a successor that takes its sockets. Every check counts connections and
//! answers, and none looks at how long anything took.

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tierstore_testkit::dir::TestDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

use super::auth::sign;
use super::cache_cli::serve_cache;
use super::cache_cli_tests::Node as BuildNode;
use super::cli::{Addresses, Cli, run_serving};
use super::combined_cli::serve_combined;
use super::combined_cli_tests::Node as CombinedNode;
use super::config::{PerRun, settings_from_text};
use super::config_testing::oci_interface;
use super::local_cache::testing::pattern;
use super::local_cache::{Disk, SystemDisk};
use super::{Drained, ServerConfig};
use crate::{Backend, FaultyBackend, MemoryBackend, Op, SystemClock};

const KEY_ID: &str = "AKIDCACHE";
const SECRET: &str = "cache-secret-0123456789";

/// A settle that does not end within a test.
const LONG: Duration = Duration::from_secs(3600);

/// A settle that is over at once.
const NONE: Duration = Duration::from_millis(1);

/// A drain deadline that does not pass within a test.
const PATIENT: Duration = Duration::from_secs(600);

/// A drain deadline that passes at once, in real time; the checks that follow it count.
const SHORT: Duration = Duration::from_millis(300);

/// How many tries a wait for a state makes, with [`PAUSE`] between two.
const TRIES: u32 = 2_000;

/// A short pause between two looks at a state that another task changes.
const PAUSE: Duration = Duration::from_millis(5);

/// A bound on a whole scenario, which only a hang reaches.
const SAFETY: Duration = Duration::from_secs(300);

/// How many new connections a check makes to see that they are all answered.
const NEW: usize = 8;

/// How many requests a check holds in flight.
const HELD: usize = 4;

/// How many requests a check completes before it stops the node.
const DONE: usize = 3;

/// The kind of node a scenario runs.
#[derive(Clone, Copy, Debug)]
enum Shape {
    /// A build node: `serve_cache`.
    Build,
    /// A storage node with `[local_cache]`: `serve_combined`.
    Combined,
    /// A storage node: `run_serving`.
    Storage,
}

/// What a node answers: the status, the head in lower case, and the body.
struct Answer {
    status: u16,
    head: String,
    body: String,
}

impl Answer {
    /// Whether the answer says the connection ends with it.
    fn closes(&self) -> bool {
        self.head.contains("\r\nconnection: close\r\n")
    }
}

/// Reads one answer from `connection`: its head and as many bytes of body as it says.
async fn read_answer(connection: &mut TcpStream) -> Answer {
    let mut got = Vec::new();
    let mut piece = [0_u8; 4096];
    let end = loop {
        if let Some(at) = got.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        let read = connection.read(&mut piece).await.unwrap();
        assert_ne!(read, 0, "{:?}", String::from_utf8_lossy(&got));
        got.extend_from_slice(&piece[..read]);
    };
    let head = String::from_utf8_lossy(&got[..end]).to_ascii_lowercase();
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length: "))
        .map_or(0, |value| value.trim().parse::<usize>().unwrap());
    while got.len() < end + length {
        let read = connection.read(&mut piece).await.unwrap();
        assert_ne!(read, 0, "{:?}", String::from_utf8_lossy(&got));
        got.extend_from_slice(&piece[..read]);
    }
    Answer {
        status: head[9..12].parse().unwrap(),
        head,
        body: String::from_utf8_lossy(&got[end..end + length]).into_owned(),
    }
}

/// Asks `path` on `connection`, which stays open if the server lets it, and reads the answer.
async fn ask(connection: &mut TcpStream, path: &str) -> Answer {
    let request = format!("GET {path} HTTP/1.1\r\nhost: tier.test\r\n\r\n");
    connection.write_all(request.as_bytes()).await.unwrap();
    read_answer(connection).await
}

async fn connect(at: SocketAddr) -> TcpStream {
    TcpStream::connect(at).await.unwrap()
}

/// The head of a request signed for a node, as a client writes it, for a body of `body`'s length
/// (the body is not part of the text).
fn signed(method: &str, path: &str, extra: &[(&str, &str)], body: &[u8]) -> String {
    let config = ServerConfig::new("tier-bucket", KEY_ID, SECRET, Arc::new(SystemClock));
    let (parts, _) = sign(&config, method, path, extra, body).into_parts();
    let mut text = format!("{method} {path} HTTP/1.1\r\n");
    for (name, value) in &parts.headers {
        text.push_str(&format!("{name}: {}\r\n", value.to_str().unwrap()));
    }
    text.push_str(&format!("content-length: {}\r\n\r\n", body.len()));
    text
}

/// Asks `/readyz` on `connection` until the answer is not 200, and gives it. Every answer before
/// it is a 200 that keeps the connection.
async fn until_draining(connection: &mut TcpStream) -> Answer {
    for _ in 0..TRIES {
        let answer = ask(connection, "/readyz").await;
        if answer.status != 200 {
            return answer;
        }
        assert!(!answer.closes(), "{}", answer.head);
        tokio::time::sleep(PAUSE).await;
    }
    panic!("/readyz was 200 on the first node's connection for {TRIES} looks");
}

/// The scratch files of the nodes of a scenario, and what a build node reads from.
struct Rig {
    shape: Shape,
    build: BuildNode,
    combined: CombinedNode,
    _control: TestDir,
    socket: PathBuf,
    memory: Arc<MemoryBackend>,
    upstream: Arc<FaultyBackend>,
}

/// A node that is not started yet: how it ends, and what it is bound to.
struct Launched {
    run: Pin<Box<dyn Future<Output = Result<Drained, String>>>>,
    bound: oneshot::Receiver<Addresses>,
}

impl Rig {
    async fn new(shape: Shape) -> Self {
        let combined = CombinedNode::new("tsdrain-combined");
        combined.activate().await;
        let control = TestDir::under(std::env::temp_dir(), "tsdrain-control");
        let socket = control.path().join("control.sock");
        let memory = Arc::new(MemoryBackend::new());
        let upstream = Arc::new(FaultyBackend::new(memory.clone()));
        Self {
            shape,
            build: BuildNode::new("tsdrain-build"),
            combined,
            _control: control,
            socket,
            memory,
            upstream,
        }
    }

    /// The file of a node with an S3 and an OCI front, handing off its sockets over the control
    /// socket if `handoff`.
    fn text(&self, handoff: bool) -> String {
        let server = if handoff {
            format!(
                "handoff_socket = {:?}\nhandoff_wait = \"60s\"",
                self.socket.display().to_string()
            )
        } else {
            String::new()
        };
        let oci = oci_interface("127.0.0.1:0", "");
        let policy = "\n[[policy]]\nprefix = \"oci/\"\ncache = true\n";
        match self.shape {
            Shape::Build => format!("{}{oci}{policy}", self.build.text(&server, "")),
            Shape::Combined => self
                .combined
                .text_with(&server, "", &format!("{oci}{policy}")),
            Shape::Storage => format!(
                "[[interface]]\ntype = \"s3\"\nparams = {{ addresses = [\"127.0.0.1:0\"] }}\n\
                 {oci}\n[server]\nproxy_id = \"p1\"\nbucket = \"tier-bucket\"\n\
                 access_key_file = \"{}\"\n{server}\n\n[hot_store]\nkind = \"memory\"\n\
                 budget = 1000000\n\n[cold_store]\nkind = \"memory\"\n",
                self.combined.key.display()
            ),
        }
    }

    /// A node of this shape with this settle and deadline, not yet polled. It stops when `stop`
    /// resolves, or when a successor takes its sockets.
    fn launch(&self, spec: &Spec, stop: impl Future<Output = ()> + Send + 'static) -> Launched {
        let text = self.text(spec.handoff);
        let knobs = PerRun {
            drain_lb_wait: Some(spec.lb_wait),
            drain_deadline: Some(spec.deadline),
            state_dir: None,
        };
        let (told, bound) = oneshot::channel::<Addresses>();
        let ready = move |at: Addresses| drop(told.send(at));
        let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
        let run: Pin<Box<dyn Future<Output = Result<Drained, String>>>> = match self.shape {
            Shape::Build => {
                let settings = settings_from_text(&text, knobs)
                    .unwrap()
                    .into_cache()
                    .unwrap();
                let upstream: Arc<dyn Backend> = self.upstream.clone();
                Box::pin(async move {
                    serve_cache(&settings, upstream, disk, stop, ready)
                        .await
                        .map_err(|why| why.to_string())
                })
            }
            Shape::Combined => {
                let settings = settings_from_text(&text, knobs)
                    .unwrap()
                    .into_combined()
                    .unwrap();
                Box::pin(async move {
                    serve_combined(&settings, disk, stop, ready)
                        .await
                        .map_err(|why| why.to_string())
                })
            }
            Shape::Storage => {
                let mut cli = Cli::from_config_text(&text).unwrap();
                cli.drain_lb_wait = spec.lb_wait;
                cli.drain_deadline = spec.deadline;
                Box::pin(async move {
                    run_serving(&cli, stop, ready)
                        .await
                        .map_err(|why| why.to_string())
                })
            }
        };
        Launched { run, bound }
    }

    /// Completes [`DONE`] requests at `at`, each answered 200.
    async fn complete(&self, at: SocketAddr) {
        if matches!(self.shape, Shape::Build) {
            for i in 0..DONE {
                let key = format!("cas/done{i}");
                self.memory
                    .put(&key, Bytes::from(pattern(100)))
                    .await
                    .unwrap();
                let mut connection = connect(at).await;
                let head = signed("GET", &format!("/tier-bucket/{key}"), &[], b"");
                connection.write_all(head.as_bytes()).await.unwrap();
                assert_eq!(read_answer(&mut connection).await.status, 200);
            }
        } else {
            for i in 0..DONE {
                let mut connection = connect(at).await;
                let body = pattern(10);
                let path = format!("/tier-bucket/cas/done{i}");
                let head = signed("PUT", &path, &[("if-none-match", "*")], &body);
                connection.write_all(head.as_bytes()).await.unwrap();
                connection.write_all(&body).await.unwrap();
                assert_eq!(read_answer(&mut connection).await.status, 200);
            }
        }
    }

    /// Holds [`HELD`] requests in flight at `at`, each on a connection of its own, and gives the
    /// connections once the node is working on every one. A build node's are gets of objects its
    /// upstream is slow to give; the others' are puts that announce a body and have not sent it.
    async fn hold(&self, at: SocketAddr) -> Vec<TcpStream> {
        let mut held = Vec::new();
        if matches!(self.shape, Shape::Build) {
            let seen = self.upstream.calls(Op::Get);
            self.upstream.set_delay(Op::Get, Some(LONG));
            for i in 0..HELD {
                let key = format!("cas/held{i}");
                self.memory
                    .put(&key, Bytes::from(pattern(100)))
                    .await
                    .unwrap();
                let mut connection = connect(at).await;
                let head = signed("GET", &format!("/tier-bucket/{key}"), &[], b"");
                connection.write_all(head.as_bytes()).await.unwrap();
                held.push(connection);
            }
            for _ in 0..TRIES {
                if self.upstream.calls(Op::Get) >= seen + HELD as u64 {
                    return held;
                }
                tokio::time::sleep(PAUSE).await;
            }
            panic!("the upstream was not asked for every held object");
        }
        for i in 0..HELD {
            let mut connection = connect(at).await;
            let path = format!("/tier-bucket/cas/held{i}");
            let extra = [("if-none-match", "*"), ("expect", "100-continue")];
            let head = signed("PUT", &path, &extra, &pattern(10));
            connection.write_all(head.as_bytes()).await.unwrap();
            let mut got = Vec::new();
            while !got.ends_with(b"\r\n\r\n") {
                let mut byte = [0_u8; 1];
                assert_eq!(connection.read(&mut byte).await.unwrap(), 1);
                got.push(byte[0]);
            }
            let text = String::from_utf8_lossy(&got).into_owned();
            assert!(text.starts_with("HTTP/1.1 100"), "{text}");
            held.push(connection);
        }
        held
    }
}

/// How a scenario runs its nodes.
struct Spec {
    lb_wait: Duration,
    deadline: Duration,
    /// Whether the first is replaced by a second that takes its sockets, rather than stopped.
    handoff: bool,
}

/// The ends of the second node of a hand-off.
struct Take {
    go: Option<oneshot::Sender<()>>,
    /// What the second serves, once it has taken the sockets.
    bound: oneshot::Receiver<Addresses>,
    /// Held until the scenario's work is over: the second stops when it is dropped.
    _stop: oneshot::Sender<()>,
}

/// What the work of a scenario is given.
struct Players {
    /// What the first node serves.
    old: Addresses,
    stop: Option<oneshot::Sender<()>>,
    take: Option<Take>,
    /// Resolves when the first node has ended.
    old_ended: oneshot::Receiver<Result<Drained, String>>,
}

impl Players {
    /// Begins the end of the first node: stops it, or starts the second.
    fn begin(&mut self) {
        match self.take.as_mut() {
            Some(take) => take.go.take().expect("once").send(()).unwrap(),
            None => self.stop.take().expect("once").send(()).unwrap(),
        }
    }
}

/// How a scenario ended.
struct Played<T> {
    old: Option<Result<Drained, String>>,
    out: T,
}

/// Runs the first node of `rig` (and, under a hand-off, a second that starts when the work says)
/// with `work`. With `finish`, it returns when both nodes have ended, after the work, which must
/// end them; without it, it returns when the work does, and drops the nodes.
async fn play<F, Fut, T>(rig: &Rig, spec: Spec, finish: bool, work: F) -> Played<T>
where
    F: FnOnce(Players) -> Fut,
    Fut: Future<Output = T>,
{
    let (stop_old, stopped_old) = oneshot::channel::<()>();
    let old = rig.launch(&spec, async move {
        let _ = stopped_old.await;
    });
    let (ended_tx, old_ended) = oneshot::channel();
    let first = async move {
        let ended = old.run.await;
        let _ = ended_tx.send(ended.clone());
        ended
    };
    let (go, going) = oneshot::channel::<()>();
    let (stop_new, stopped_new) = oneshot::channel::<()>();
    let (new_run, new_bound) = if spec.handoff {
        let new = rig.launch(&spec, async move {
            let _ = stopped_new.await;
        });
        (Some(new.run), Some(new.bound))
    } else {
        (None, None)
    };
    let second = async move {
        match new_run {
            Some(run) if going.await.is_ok() => Some(run.await),
            _ => None,
        }
    };
    let take = new_bound.map(|bound| Take {
        go: Some(go),
        bound,
        _stop: stop_new,
    });
    let bound = old.bound;
    let working = async move {
        let at = bound.await.expect("the first node started");
        work(Players {
            old: at,
            stop: Some(stop_old),
            take,
            old_ended,
        })
        .await
    };
    let scenario = async {
        if finish {
            let (old, _new, out) = tokio::join!(first, second, working);
            Played {
                old: Some(old),
                out,
            }
        } else {
            let both = async { tokio::join!(first, second) };
            tokio::pin!(both);
            tokio::select! {
                ended = &mut both => panic!("the nodes ended before the work: {ended:?}"),
                out = working => Played { old: None, out },
            }
        }
    };
    tokio::time::timeout(SAFETY, scenario)
        .await
        .expect("the scenario hung")
}

/// Opens the connections a drain is watched on: two to the S3 front and one to the OCI front,
/// each having had one answer that keeps it open.
async fn kept(players: &Players) -> [TcpStream; 3] {
    let oci = players.old.oci.expect("an OCI front");
    let mut s3 = [connect(players.old.s3).await, connect(players.old.s3).await];
    let mut registry = connect(oci).await;
    for connection in &mut s3 {
        let answer = ask(connection, "/healthz").await;
        assert_eq!(answer.status, 200);
        assert!(!answer.closes(), "{}", answer.head);
    }
    let answer = ask(&mut registry, "/v2/").await;
    assert_eq!(answer.status, 200);
    assert!(!answer.closes(), "{}", answer.head);
    let [first, second] = s3;
    [first, second, registry]
}

/// Asserts the readiness and the `Connection: close` from the stop on, on the connections opened
/// before it and on a new one.
async fn assert_closing(shape: Shape, handoff: bool) {
    let rig = Rig::new(shape).await;
    let spec = Spec {
        lb_wait: LONG,
        deadline: PATIENT,
        handoff,
    };
    play(&rig, spec, false, |mut players| async move {
        let [mut ready, mut health, mut registry] = kept(&players).await;
        let before = ask(&mut ready, "/readyz").await;
        assert_eq!(before.status, 200, "{}", before.body);
        assert!(!before.closes(), "{}", before.head);
        players.begin();
        let draining = until_draining(&mut ready).await;
        assert_eq!((draining.status, draining.body.as_str()), (503, "draining"));
        assert!(draining.closes(), "{}", draining.head);
        let kept_s3 = ask(&mut health, "/healthz").await;
        assert_eq!(kept_s3.status, 200);
        assert!(kept_s3.closes(), "{}", kept_s3.head);
        let kept_oci = ask(&mut registry, "/v2/").await;
        assert_eq!(kept_oci.status, 200);
        assert!(kept_oci.closes(), "{}", kept_oci.head);
        if !handoff {
            // With a successor, a new connection may be taken by it: it is not draining.
            let mut new = connect(players.old.s3).await;
            let answer = ask(&mut new, "/readyz").await;
            assert_eq!((answer.status, answer.body.as_str()), (503, "draining"));
            assert!(answer.closes(), "{}", answer.head);
        }
    })
    .await;
}

/// Asserts that new connections are accepted and answered while the settle lasts: [`NEW`] to each
/// front, all answered 200.
async fn assert_serving_through_the_settle(shape: Shape, handoff: bool) {
    let rig = Rig::new(shape).await;
    let spec = Spec {
        lb_wait: LONG,
        deadline: PATIENT,
        handoff,
    };
    play(&rig, spec, false, |mut players| async move {
        let [mut ready, _health, _registry] = kept(&players).await;
        players.begin();
        let draining = until_draining(&mut ready).await;
        assert_eq!(draining.status, 503);
        let oci = players.old.oci.expect("an OCI front");
        let (mut s3_answered, mut oci_answered) = (0, 0);
        for _ in 0..NEW {
            let mut connection = connect(players.old.s3).await;
            let answer = ask(&mut connection, "/healthz").await;
            assert_eq!(answer.status, 200);
            s3_answered += 1;
            let mut connection = connect(oci).await;
            let answer = ask(&mut connection, "/v2/").await;
            assert_eq!(answer.status, 200);
            oci_answered += 1;
        }
        assert_eq!((s3_answered, oci_answered), (NEW, NEW));
    })
    .await;
}

/// Asserts that once the settle is over and the run has returned, a connect to either front is
/// refused by the kernel ([`NEW`] of them), or, under a hand-off, is answered by the successor.
async fn assert_listener_after_the_settle(shape: Shape, handoff: bool) {
    let rig = Rig::new(shape).await;
    let spec = Spec {
        lb_wait: NONE,
        deadline: PATIENT,
        handoff,
    };
    let played = play(&rig, spec, true, |mut players| async move {
        players.begin();
        let ended = (&mut players.old_ended).await.unwrap();
        if let Some(take) = players.take.as_mut() {
            let new = (&mut take.bound).await.unwrap();
            assert_eq!((new.s3, new.oci), (players.old.s3, players.old.oci));
        }
        let oci = players.old.oci.expect("an OCI front");
        let (mut refused, mut answered) = (0, 0);
        for _ in 0..NEW {
            for (at, path) in [(players.old.s3, "/healthz"), (oci, "/v2/")] {
                match TcpStream::connect(at).await {
                    Err(why) => {
                        assert_eq!(why.kind(), std::io::ErrorKind::ConnectionRefused);
                        refused += 1;
                    }
                    Ok(mut connection) => {
                        let answer = ask(&mut connection, path).await;
                        assert_eq!(answer.status, 200);
                        assert!(!answer.closes(), "{}", answer.head);
                        answered += 1;
                    }
                }
            }
        }
        (ended, refused, answered)
    })
    .await;
    let (ended, refused, answered) = played.out;
    assert_eq!(ended, Ok(Drained { abandoned: 0 }));
    assert_eq!(played.old, Some(ended));
    if handoff {
        assert_eq!((refused, answered), (0, 2 * NEW));
    } else {
        assert_eq!((refused, answered), (2 * NEW, 0));
    }
}

/// Asserts that a drain whose deadline passes with [`HELD`] requests in flight, beside [`DONE`]
/// that completed, ends with exactly [`HELD`] cut.
async fn assert_the_deadline_counts(shape: Shape, handoff: bool) {
    let rig = Rig::new(shape).await;
    let spec = Spec {
        lb_wait: NONE,
        deadline: SHORT,
        handoff,
    };
    let node = &rig;
    let played = play(&rig, spec, true, |mut players| async move {
        node.complete(players.old.s3).await;
        let held = node.hold(players.old.s3).await;
        players.begin();
        let ended = (&mut players.old_ended).await.unwrap();
        drop(held);
        ended
    })
    .await;
    assert_eq!(played.out, Ok(Drained { abandoned: HELD }));
    assert_eq!(played.old, Some(played.out));
}

macro_rules! contract {
    ($module:ident, $shape:ident, $what:literal) => {
        mod $module {
            use super::*;

            #[doc = concat!("Asserts (T31a) that, stopped by its stop future, ", $what, " answers /readyz 503 `draining` from the stop on, and that every S3 answer and every OCI answer, on a connection opened before the stop and on a new one, carries `Connection: close`.")]
            #[tokio::test]
            async fn stop_answers_503_and_close() {
                assert_closing(Shape::$shape, false).await;
            }

            #[doc = concat!("Asserts (T31a) that, replaced by a successor that takes its sockets, ", $what, " answers /readyz 503 `draining` on a connection it holds, and that its S3 and OCI answers on connections opened before the take carry `Connection: close`.")]
            #[tokio::test]
            async fn handoff_answers_503_and_close() {
                assert_closing(Shape::$shape, true).await;
            }

            #[doc = concat!("Asserts (T31b) that, stopped by its stop future with a settle that does not end, ", $what, " accepts new connections and answers them all: ", "eight to the S3 front and eight to the OCI front are answered 200.")]
            #[tokio::test]
            async fn stop_serves_new_connections_through_the_settle() {
                assert_serving_through_the_settle(Shape::$shape, false).await;
            }

            #[doc = concat!("Asserts (T31b) that, replaced by a successor with a settle that does not end, ", $what, " and its successor answer every new connection: eight to the S3 front and eight to the OCI front are answered 200.")]
            #[tokio::test]
            async fn handoff_serves_new_connections_through_the_settle() {
                assert_serving_through_the_settle(Shape::$shape, true).await;
            }

            #[doc = concat!("Asserts (T31c) that, once ", $what, " has been stopped by its stop future and has returned with nothing cut, every connect to its S3 and OCI addresses (sixteen) is refused by the kernel, and none is accepted.")]
            #[tokio::test]
            async fn stop_then_the_kernel_refuses_connects() {
                assert_listener_after_the_settle(Shape::$shape, false).await;
            }

            #[doc = concat!("Asserts (T31c) that, once ", $what, " has handed its sockets to a successor and returned with nothing cut, no connect to its addresses is refused: all sixteen are answered 200 without `Connection: close`, by the successor.")]
            #[tokio::test]
            async fn handoff_then_the_successor_answers_connects() {
                assert_listener_after_the_settle(Shape::$shape, true).await;
            }

            #[doc = concat!("Asserts (T31d) that, stopped by its stop future with four requests in flight and three completed, ", $what, " returns `Drained { abandoned: 4 }` at its deadline.")]
            #[tokio::test]
            async fn stop_counts_the_requests_cut_at_the_deadline() {
                assert_the_deadline_counts(Shape::$shape, false).await;
            }

            #[doc = concat!("Asserts (T31d) that, replaced by a successor with four requests in flight and three completed, ", $what, " returns `Drained { abandoned: 4 }` at its deadline.")]
            #[tokio::test]
            async fn handoff_counts_the_requests_cut_at_the_deadline() {
                assert_the_deadline_counts(Shape::$shape, true).await;
            }
        }
    };
}

contract!(build, Build, "a build node");
contract!(combined, Combined, "a storage node with a local cache");
contract!(storage, Storage, "a storage node");
