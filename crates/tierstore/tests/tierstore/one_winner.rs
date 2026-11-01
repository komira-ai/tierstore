//! One winner among concurrent create-only puts of one key.
//!
//! Eight writers with eight different bodies are all inside the hot tier's create (or, where the
//! hot tier is full, the cold tier's) before any of them goes on, so the store's own create-only
//! write is the only thing that decides. Each test asserts what was answered and what was stored:
//! never two winners, and the stored bytes are the winner's bytes whole.
//!
//! The testkit has no model of a set of storage nodes, and these tests do not pretend to have
//! one. "Part of the set is down" is modelled as the hot tier's create failing for some of the
//! writers (`Error::Unavailable`, which the front answers 503): 0, 1 and 2 of the 8. That shows
//! that the proxy neither turns a failed create into a second winner nor leaves a partial object;
//! it does not show that a real distributed store keeps `If-None-Match: *` atomic with part of
//! its members down, which only a run against the store can.
//!
//! The memory rounds only show that the proxy adds no second path to the create: the memory
//! backend's create is a check and insert under one mutex, so it is atomic by construction. The
//! directory rounds are the real primitive (a hard link that fails where the name is taken).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::{StreamExt, stream};
use hyper::StatusCode;
use tierstore::{
    Backend, DirBackend, Error, FaultyBackend, JournalEntry, JournalOp, MemoryBackend, Op,
    PartBody, Proxy, ProxyConfig, TestClock, decode_journal,
};
use tierstore_testkit::dir::TestDir;
use tokio::sync::Notify;

use crate::hold::{Gate, Stall};
use crate::recheck::{answered, put_request, server_over};
use crate::support::{assert_absent, body, chunks, read};

const WRITERS: usize = 8;

/// The hot tier's published usage when it is at its budget of 1000 bytes.
const HOT_AT_BUDGET: &str = r#"{"hot_bytes":1000,"copy_lag":0,"time":0}"#;

/// Two tiers: in memory, or in directories that are removed with the value.
struct Tiers {
    hot: Arc<dyn Backend>,
    cold: Arc<dyn Backend>,
    hot_dir: Option<PathBuf>,
    _dirs: Vec<TestDir>,
}

impl Tiers {
    fn memory() -> Self {
        Self {
            hot: Arc::new(MemoryBackend::new()),
            cold: Arc::new(MemoryBackend::new()),
            hot_dir: None,
            _dirs: Vec::new(),
        }
    }

    fn directory() -> Self {
        let hot_dir = TestDir::under(std::env::temp_dir(), "tierstore-winner-hot");
        let cold_dir = TestDir::under(std::env::temp_dir(), "tierstore-winner-cold");
        Self {
            hot: Arc::new(DirBackend::open(hot_dir.path()).unwrap()),
            cold: Arc::new(DirBackend::open(cold_dir.path()).unwrap()),
            hot_dir: Some(hot_dir.path().to_path_buf()),
            _dirs: vec![hot_dir, cold_dir],
        }
    }

    /// Fresh tiers of the kind named, for one run.
    fn of(kind: &str) -> Self {
        if kind == "memory" {
            Self::memory()
        } else {
            Self::directory()
        }
    }
}

/// The kinds of tier the tests run over.
const KINDS: [&str; 2] = ["memory", "directory"];

/// The tiers, a fault injector on the hot tier, and the two nodes (proxies `p1` and `p2`) that
/// share them. The writers of `key` wait for each other at the hot tier and at the cold tier.
struct Setup {
    tiers: Tiers,
    faults: Arc<FaultyBackend>,
    hot: Arc<dyn Backend>,
    cold: Arc<dyn Backend>,
}

impl Setup {
    async fn new(tiers: Tiers, key: &str, hot_full: bool) -> Self {
        if hot_full {
            tiers
                .hot
                .put("_tiering/usage", body(HOT_AT_BUDGET))
                .await
                .unwrap();
        }
        let faults = Arc::new(FaultyBackend::new(tiers.hot.clone()));
        let hot: Arc<dyn Backend> = Arc::new(Gate::new(faults.clone(), key, WRITERS));
        let cold: Arc<dyn Backend> = Arc::new(Gate::new(tiers.cold.clone(), key, WRITERS));
        Self {
            tiers,
            faults,
            hot,
            cold,
        }
    }

    /// A node whose hot tier has a budget of 1000 bytes.
    async fn proxy(&self, id: &str) -> Proxy {
        let mut config = ProxyConfig::new(id, 1000, Arc::new(TestClock::new()));
        config.linger = Duration::from_millis(1);
        config.usage_refresh = Duration::ZERO;
        Proxy::open(self.hot.clone(), self.cold.clone(), config)
            .await
            .unwrap()
    }

    /// Makes the hot tier's create of `key` fail for the next `down` writers that reach it.
    fn down(&self, key: &str, down: usize) {
        for _ in 0..down {
            let error = Error::Unavailable("a member of the set is down".to_owned());
            self.faults.fail_next_on(Op::Put, key, error);
        }
    }

    /// Every journal entry about `key`, over the journals of both nodes.
    async fn entries(&self, key: &str) -> Vec<JournalEntry> {
        let mut found = Vec::new();
        for object in self.tiers.hot.list("_journal/").await.unwrap() {
            let bytes = self.tiers.hot.get(&object.key, None).await.unwrap();
            found.extend(
                decode_journal(&bytes)
                    .unwrap()
                    .into_iter()
                    .filter(|entry| entry.key == key),
            );
        }
        found
    }

    /// Asserts that a `.tmp-` file of a put in flight is not left in the hot directory.
    fn assert_no_temporary_file(&self) {
        if let Some(dir) = &self.tiers.hot_dir {
            assert_eq!(temporary_files(dir), Vec::<String>::new());
        }
    }
}

fn temporary_files(dir: &Path) -> Vec<String> {
    let names = std::fs::read_dir(dir).unwrap();
    let names = names.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned());
    names.filter(|name| name.starts_with(".tmp-")).collect()
}

/// The three ways a node takes a put: a body held whole, a streamed body, and a streamed body
/// that was presigned (it is written to the hot tier or not at all).
#[derive(Clone, Copy, Debug)]
enum Mode {
    Held,
    Streamed,
    HotOnly,
}

const MODES: [Mode; 3] = [Mode::Held, Mode::Streamed, Mode::HotOnly];

/// The body writer number `writer` sends: different in content and in length from every other.
fn body_of(writer: usize) -> Bytes {
    Bytes::from(format!("writer {writer} wrote this body. ").repeat(writer + 1))
}

async fn put_as(proxy: &Proxy, mode: Mode, key: &str, data: &Bytes) -> Result<(), Error> {
    let pieces: Vec<Result<Bytes, Error>> = data
        .chunks(7)
        .map(|piece| Ok(Bytes::copy_from_slice(piece)))
        .collect();
    let streamed: PartBody<'_> = stream::iter(pieces).boxed();
    let len = Some(data.len() as u64);
    match mode {
        Mode::Held => proxy.put(key, data.clone()).await,
        Mode::Streamed => proxy.put_stream(key, len, streamed).await,
        Mode::HotOnly => proxy.put_stream_hot_only(key, len, streamed).await,
    }
}

/// Eight writers of `key`, four through each of two nodes, on threads of their own. Gives each
/// writer's body and what its put answered.
async fn race(setup: &Setup, mode: Mode, key: &str) -> Vec<(Bytes, Result<(), Error>)> {
    let nodes = [
        Arc::new(setup.proxy("p1").await),
        Arc::new(setup.proxy("p2").await),
    ];
    let mut tasks = Vec::new();
    for writer in 0..WRITERS {
        let node = nodes[writer % nodes.len()].clone();
        let (key, data) = (key.to_owned(), body_of(writer));
        tasks.push(tokio::spawn(async move {
            let answer = put_as(&node, mode, &key, &data).await;
            (data, answer)
        }));
    }
    let mut done = Vec::new();
    for task in tasks {
        done.push(task.await.unwrap());
    }
    done
}

fn count(done: &[(Bytes, Result<(), Error>)], wanted: impl Fn(&Error) -> bool) -> usize {
    let errors = done.iter().filter_map(|(_, answer)| answer.as_ref().err());
    errors.filter(|error| wanted(error)).count()
}

fn winners(done: &[(Bytes, Result<(), Error>)]) -> Vec<Bytes> {
    let won = done.iter().filter(|(_, answer)| answer.is_ok());
    won.map(|(data, _)| data.clone()).collect()
}

/// Asserts that eight concurrent create-only puts of one key through two nodes, with 0, 1 and 2
/// of the hot tier's creates failing as if a member were down, have exactly one winner whose
/// bytes are what is stored whole in the hot tier, in memory and in a directory, for a held
/// body, a streamed body and a presigned (hot-only) body. The writers whose create failed are
/// answered `Unavailable`, the others `AlreadyExists`; the cold tier holds nothing; the journals
/// hold exactly one entry for the key, a put of the winner's size; no temporary file is left.
/// Defects it catches: a create that does not refuse the second writer (more than one winner),
/// a proxy that retries a failed create into the cold tier or writes it twice (the counts), a
/// body mixed from two writers (the bytes), a winner that is not journaled (the entries).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eight_writers_of_one_key_have_one_winner_with_part_of_the_hot_tier_failing() {
    for round in 0..3 {
        for kind in KINDS {
            for mode in MODES {
                for down in 0..=2 {
                    let key = "contended";
                    let setup = Setup::new(Tiers::of(kind), key, false).await;
                    setup.down(key, down);
                    let done = race(&setup, mode, key).await;
                    let label = format!("{kind} {mode:?} down={down} round={round}");

                    let won = winners(&done);
                    assert_eq!(won.len(), 1, "{label}: {done:?}");
                    assert_eq!(count(&done, |e| matches!(e, Error::Unavailable(_))), down);
                    let exists = count(&done, |e| matches!(e, Error::AlreadyExists(_)));
                    assert_eq!(exists, WRITERS - 1 - down, "{label}: {done:?}");

                    assert_eq!(read(&*setup.tiers.hot, key).await, won[0], "{label}");
                    assert_absent(&*setup.tiers.cold, key).await;
                    let entries = setup.entries(key).await;
                    assert_eq!(entries.len(), 1, "{label}");
                    assert_eq!(entries[0].op, JournalOp::Put, "{label}");
                    assert_eq!(entries[0].size, won[0].len() as u64, "{label}");
                    setup.assert_no_temporary_file();
                }
            }
        }
    }
}

/// Asserts that when the create fails for every writer there is no winner and no object: the
/// key is in neither tier, nothing is journaled, no temporary file is left, and a put of the key
/// afterwards is stored whole. Defect it catches: a failed create that leaves a partial or an
/// unjournaled object, or a key that stays taken after every writer was refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn when_every_create_fails_nothing_is_stored_and_a_retry_wins() {
    for kind in KINDS {
        for mode in MODES {
            let key = "contended";
            let setup = Setup::new(Tiers::of(kind), key, false).await;
            setup.down(key, WRITERS);
            let done = race(&setup, mode, key).await;
            let label = format!("{kind} {mode:?}");

            assert!(winners(&done).is_empty(), "{label}: {done:?}");
            assert_eq!(
                count(&done, |e| matches!(e, Error::Unavailable(_))),
                WRITERS
            );
            assert_absent(&*setup.tiers.hot, key).await;
            assert_absent(&*setup.tiers.cold, key).await;
            assert_eq!(setup.entries(key).await, Vec::<JournalEntry>::new());
            setup.assert_no_temporary_file();

            let retry = body_of(3);
            let node = setup.proxy("p1").await;
            put_as(&node, mode, key, &retry).await.unwrap();
            assert_eq!(read(&*setup.tiers.hot, key).await, retry, "{label}");
        }
    }
}

/// The two nodes of a setup as servers that take presigned puts.
async fn presigned_servers(setup: &Setup) -> [Arc<tierstore::server::Server>; 2] {
    [
        server_over(setup.proxy("p1").await, 1 << 20),
        server_over(setup.proxy("p2").await, 1 << 20),
    ]
}

/// Eight presigned `PUT`s of `key`, four to each server, on threads of their own: each answer's
/// status and text, with the body that was sent.
async fn presigned_race(
    servers: &[Arc<tierstore::server::Server>; 2],
    key: &str,
) -> Vec<(Bytes, StatusCode, String)> {
    let mut tasks = Vec::new();
    for writer in 0..WRITERS {
        let server = servers[writer % servers.len()].clone();
        let (key, data) = (key.to_owned(), body_of(writer));
        tasks.push(tokio::spawn(async move {
            let request = put_request(&key, &[], data.to_vec());
            let (status, _, text) = answered(&server, request).await;
            (data, status, text)
        }));
    }
    let mut done = Vec::new();
    for task in tasks {
        done.push(task.await.unwrap());
    }
    done
}

/// Asserts the one-winner claim on the presigned path, over HTTP: eight presigned `PUT`s of one
/// key with different bodies, four to each of two servers over the same tiers, with 0, 1 and 2
/// of the hot tier's creates failing. Exactly one is answered 200; as many as failed are 503
/// `ServiceUnavailable`; the rest are 412 `PreconditionFailed`; the object stored is the 200's
/// body whole. Defect it catches: a front that answers 200 to a writer whose create was refused,
/// or that maps the refusal to anything but 412.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eight_presigned_puts_from_two_nodes_have_one_winner_and_it_is_what_is_stored() {
    for kind in KINDS {
        for down in 0..=2 {
            let key = "cas/b/contended";
            let setup = Setup::new(Tiers::of(kind), key, false).await;
            setup.down(key, down);
            let servers = presigned_servers(&setup).await;
            let done = presigned_race(&servers, key).await;
            let label = format!("{kind} down={down}");

            let status = |code: StatusCode| done.iter().filter(|(_, s, _)| *s == code).count();
            assert_eq!(status(StatusCode::OK), 1, "{label}: {done:?}");
            assert_eq!(status(StatusCode::SERVICE_UNAVAILABLE), down, "{label}");
            let refused = status(StatusCode::PRECONDITION_FAILED);
            assert_eq!(refused, WRITERS - 1 - down, "{label}: {done:?}");
            for (_, code, text) in &done {
                if *code == StatusCode::PRECONDITION_FAILED {
                    assert!(text.contains("<Code>PreconditionFailed</Code>"), "{text}");
                }
            }

            let (winner, _, _) = done.iter().find(|(_, s, _)| *s == StatusCode::OK).unwrap();
            assert_eq!(read(&*setup.tiers.hot, key).await, *winner, "{label}");
            assert_absent(&*setup.tiers.cold, key).await;
            assert_eq!(setup.entries(key).await.len(), 1, "{label}");
            setup.assert_no_temporary_file();
        }
    }
}

/// Asserts that with the hot tier at its budget a presigned `PUT` is never written to the cold
/// tier: all eight are answered 503 `SlowDown`, nothing is stored in either tier, nothing is
/// journaled. This is why the presigned path has one winner when the hot tier is full: it has no
/// winner. Defect it catches: a presigned put that falls back to the cold tier, where it could
/// race a put that went to the hot tier.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_the_hot_tier_full_no_presigned_put_is_written_anywhere() {
    for kind in KINDS {
        let key = "cas/b/contended";
        let setup = Setup::new(Tiers::of(kind), key, true).await;
        let servers = presigned_servers(&setup).await;
        let done = presigned_race(&servers, key).await;

        for (_, code, text) in &done {
            assert_eq!(*code, StatusCode::SERVICE_UNAVAILABLE, "{kind}: {text}");
            assert!(text.contains("<Code>SlowDown</Code>"), "{kind}: {text}");
        }
        assert_absent(&*setup.tiers.hot, key).await;
        assert_absent(&*setup.tiers.cold, key).await;
        assert_eq!(setup.entries(key).await, Vec::<JournalEntry>::new());
    }
}

/// Asserts that with the hot tier at its budget, eight writers that are not presigned (a held
/// body and a streamed body) all go to the cold tier, where its create-only write decides: one
/// winner, the others `AlreadyExists`, the cold object is the winner's bytes whole, the hot tier
/// has no object and no journal entry (a put that went to the cold tier is not journaled).
/// Defect it catches: a cold create that does not refuse a second writer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_the_hot_tier_full_eight_unsigned_puts_have_one_winner_in_the_cold_tier() {
    for kind in KINDS {
        for mode in [Mode::Held, Mode::Streamed] {
            let key = "contended";
            let setup = Setup::new(Tiers::of(kind), key, true).await;
            let done = race(&setup, mode, key).await;
            let label = format!("{kind} {mode:?}");

            let won = winners(&done);
            assert_eq!(won.len(), 1, "{label}: {done:?}");
            let exists = count(&done, |e| matches!(e, Error::AlreadyExists(_)));
            assert_eq!(exists, WRITERS - 1, "{label}: {done:?}");
            assert_eq!(read(&*setup.tiers.cold, key).await, won[0], "{label}");
            assert_absent(&*setup.tiers.hot, key).await;
            assert_eq!(setup.entries(key).await, Vec::<JournalEntry>::new());
        }
    }
}

/// Pins the documented weaker path, the one a write that is not presigned takes: a put that goes to the cold tier because its node sees the hot tier full, and a put that
/// creates the object in the hot tier, can both succeed, leaving the key in both tiers with
/// different bodies. The node that sees the hot tier full has passed its check that the hot tier
/// lacks the key when the other node's put is made and acknowledged. This test PINS A KNOWN LIMIT
/// (the `Proxy` documentation lists it): if the limit is ever closed it must be changed to
/// assert one winner. Defect it catches the other way: nothing; it shows the claim of one winner
/// does not cover this path.
#[tokio::test(start_paused = true)]
async fn pins_a_known_limit_an_unsigned_cold_put_and_a_hot_put_can_both_succeed() {
    let tiers = Tiers::memory();
    tiers
        .hot
        .put("_tiering/usage", body(HOT_AT_BUDGET))
        .await
        .unwrap();
    let (entered, go) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let cold: Arc<dyn Backend> = Arc::new(Stall::new(
        tiers.cold.clone(),
        "contended",
        entered.clone(),
        Some(go.clone()),
    ));
    let clock = Arc::new(TestClock::new());
    let open = |id: &'static str, budget: u64| {
        let (hot, cold, clock) = (tiers.hot.clone(), cold.clone(), clock.clone());
        async move {
            let mut config = ProxyConfig::new(id, budget, clock);
            config.linger = Duration::from_millis(1);
            config.usage_refresh = Duration::ZERO;
            Proxy::open(hot, cold, config).await.unwrap()
        }
    };
    // The same usage object: at the budget for the node with a budget of 1000, far below it for
    // the node with a large budget.
    let sees_full = Arc::new(open("p1", 1000).await);
    let sees_room = open("p2", 1 << 30).await;

    let late = {
        let node = sees_full.clone();
        tokio::spawn(async move {
            node.put_stream("contended", None, chunks(&["cold bytes"]))
                .await
        })
    };
    entered.notified().await;
    sees_room
        .put_stream("contended", None, chunks(&["hot bytes"]))
        .await
        .unwrap();
    go.notify_one();
    late.await.unwrap().unwrap();

    assert_eq!(read(&*tiers.hot, "contended").await, body("hot bytes"));
    assert_eq!(read(&*tiers.cold, "contended").await, body("cold bytes"));
}

/// Asserts the contrast: a presigned put (hot-only) on the node that sees the hot tier full is
/// refused with `Full` before it touches the cold tier, so the cold tier never holds a rival to
/// the object another node's presigned put created in the hot tier. Defect it catches: a
/// hot-only put that is sent to the cold tier when the node sees the hot tier full.
#[tokio::test(start_paused = true)]
async fn a_presigned_put_on_a_node_that_sees_the_hot_tier_full_is_refused_not_sent_to_cold() {
    let tiers = Tiers::memory();
    tiers
        .hot
        .put("_tiering/usage", body(HOT_AT_BUDGET))
        .await
        .unwrap();
    let clock = Arc::new(TestClock::new());
    let open = |id: &'static str, budget: u64| {
        let (hot, cold, clock) = (tiers.hot.clone(), tiers.cold.clone(), clock.clone());
        async move {
            let mut config = ProxyConfig::new(id, budget, clock);
            config.linger = Duration::from_millis(1);
            config.usage_refresh = Duration::ZERO;
            Proxy::open(hot, cold, config).await.unwrap()
        }
    };
    let sees_full = open("p1", 1000).await;
    let sees_room = open("p2", 1 << 30).await;

    sees_room
        .put_stream_hot_only("contended", None, chunks(&["hot bytes"]))
        .await
        .unwrap();
    let refused = sees_full
        .put_stream_hot_only("contended", None, chunks(&["cold bytes"]))
        .await;
    assert!(matches!(refused, Err(Error::Full(_))), "{refused:?}");
    assert_eq!(read(&*tiers.hot, "contended").await, body("hot bytes"));
    assert_absent(&*tiers.cold, "contended").await;
}
