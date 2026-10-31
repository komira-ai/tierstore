//! A writer that is killed, and what it leaves behind.
//!
//! The testkit can stop a task and drop a connection's body, and it can plant what a crashed
//! process leaves on disk; it cannot kill a process, and no test here does. A killed writer is a
//! task that is aborted at a point the test names (the body is stuck after its first piece, or
//! the journal write is), which drops its futures exactly as a client that went away or a
//! server task that was cancelled does. A crash that leaves a file behind is planted by hand.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream;
use tierstore::{
    Backend, DirBackend, Error, JournalEntry, JournalOp, Op, PartBody, PassReport, Proxy,
    ProxyConfig, Reconciler, ReconcilerConfig, TestClock, decode_journal,
};
use tierstore_testkit::dir::TestDir;
use tokio::sync::Notify;

use crate::hold::Stall;
use crate::support::{Rig, assert_absent, body, chunks, journal_keys, keys, read, text};

/// Two directories as the tiers of a proxy.
struct Dirs {
    hot_dir: TestDir,
    cold_dir: TestDir,
    hot: Arc<DirBackend>,
    cold: Arc<DirBackend>,
}

impl Dirs {
    fn new() -> Self {
        let hot_dir = TestDir::under(std::env::temp_dir(), "tierstore-kill-hot");
        let cold_dir = TestDir::under(std::env::temp_dir(), "tierstore-kill-cold");
        let hot = Arc::new(DirBackend::open(hot_dir.path()).unwrap());
        let cold = Arc::new(DirBackend::open(cold_dir.path()).unwrap());
        Self {
            hot_dir,
            cold_dir,
            hot,
            cold,
        }
    }

    fn config(id: &str) -> ProxyConfig {
        let mut config = ProxyConfig::new(id, 1 << 30, Arc::new(TestClock::new()));
        config.linger = Duration::from_millis(1);
        config.usage_refresh = Duration::ZERO;
        config
    }

    /// A proxy over the directories, as a process that has just started would open it.
    async fn proxy(&self, id: &str) -> Arc<Proxy> {
        let (hot, cold) = (self.hot.clone(), self.cold.clone());
        Arc::new(Proxy::open(hot, cold, Self::config(id)).await.unwrap())
    }
}

/// The `.tmp-` files of puts that are not over, in a directory.
fn temporary_files(dir: &Path) -> Vec<String> {
    let names = std::fs::read_dir(dir).unwrap();
    let names = names.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned());
    names.filter(|name| name.starts_with(".tmp-")).collect()
}

/// A body of one piece, after which the writer is stuck for good: `reached` is notified when the
/// proxy asks for the second piece, so the test knows the first was taken.
fn stuck_after(first: &'static str, reached: Arc<Notify>) -> PartBody<'static> {
    let piece = stream::once(async move { Ok::<Bytes, Error>(body(first)) });
    let stuck = stream::once(async move {
        reached.notify_one();
        std::future::pending::<Result<Bytes, Error>>().await
    });
    piece.chain(stuck).boxed()
}

/// Every journal entry about `key` that proxy `p1` wrote.
async fn entries_about(hot: &dyn Backend, key: &str) -> Vec<JournalEntry> {
    let mut found = Vec::new();
    for journal in journal_keys(hot, "p1").await {
        let bytes = read(hot, &journal).await;
        found.extend(
            decode_journal(&bytes)
                .unwrap()
                .into_iter()
                .filter(|entry| entry.key == key),
        );
    }
    found
}

/// Asserts that a streamed put whose writer is killed with the first piece of its body taken (as
/// a presigned put and as a put that is not) leaves nothing a reader can see, in a directory
/// tier, which is where a partial file could be: the object is in neither tier, the proxy's
/// listing holds only the objects there were, no journal entry names it, no temporary file is
/// left, an object stored before is whole, and a put of the same key afterwards is stored whole.
/// Before the kill the partial body is on disk as a temporary file, so the test looks at the
/// moment it claims to. Defect it catches: a directory backend that links the name before the
/// last piece, or that leaves the temporary file when the call is dropped; a proxy that journals
/// a put before its body has ended.
#[tokio::test(start_paused = true)]
async fn a_put_killed_with_part_of_its_body_taken_leaves_nothing_visible() {
    let dirs = Dirs::new();
    let proxy = dirs.proxy("p1").await;
    proxy.put("old", body("previous bytes")).await.unwrap();
    let mut stored = vec!["old".to_owned()];

    for (key, hot_only) in [("killed-plain", false), ("killed-presigned", true)] {
        let reached = Arc::new(Notify::new());
        let writer = {
            let (proxy, reached) = (proxy.clone(), reached.clone());
            tokio::spawn(async move {
                let stuck = stuck_after("half of an obj", reached);
                if hot_only {
                    proxy.put_stream_hot_only(key, None, stuck).await
                } else {
                    proxy.put_stream(key, None, stuck).await
                }
            })
        };
        reached.notified().await;
        assert_eq!(temporary_files(dirs.hot_dir.path()).len(), 1, "{key}");
        writer.abort();
        assert!(writer.await.unwrap_err().is_cancelled());

        assert_eq!(temporary_files(dirs.hot_dir.path()), Vec::<String>::new());
        assert_eq!(temporary_files(dirs.cold_dir.path()), Vec::<String>::new());
        assert_absent(&*dirs.hot, key).await;
        assert_absent(&*dirs.cold, key).await;
        assert_absent(&*proxy, key).await;
        assert_eq!(keys(&*proxy).await, stored);
        assert_eq!(
            entries_about(&*dirs.hot, key).await,
            Vec::<JournalEntry>::new()
        );
        assert_eq!(read(&*proxy, "old").await, body("previous bytes"));

        let again = chunks(&["half of ", "an object"]);
        proxy.put_stream(key, None, again).await.unwrap();
        assert_eq!(read(&*proxy, key).await, body("half of an object"));
        assert_eq!(entries_about(&*dirs.hot, key).await.len(), 1);
        stored.push(key.to_owned());
        stored.sort();
    }
}

/// Asserts that what a crashed process leaves in a directory tier, a temporary file with part of
/// a body in it, is not an object: it is not listed, not read, not found by the proxy, a restart
/// over the directory neither counts it nor stumbles on it, and a put of any key afterwards is
/// stored whole. The file is planted by hand, because the test cannot kill a process. Defect it
/// catches: a listing or a read that treats a `.tmp-` file as an object.
#[tokio::test(start_paused = true)]
async fn a_temporary_file_left_by_a_crashed_process_is_not_an_object() {
    let dirs = Dirs::new();
    std::fs::write(dirs.hot_dir.path().join(".tmp-dead-0"), b"half of an obj").unwrap();

    let proxy = dirs.proxy("p1").await;
    assert_eq!(keys(&*dirs.hot).await, Vec::<String>::new());
    assert_eq!(keys(&*proxy).await, Vec::<String>::new());
    assert_absent(&*proxy, ".tmp-dead-0").await;
    proxy.put("k", body("whole")).await.unwrap();
    assert_eq!(keys(&*proxy).await, ["k"]);
    assert_eq!(read(&*proxy, "k").await, body("whole"));
}

/// Asserts what the proxy guarantees when the writer is killed after the object was created in
/// the hot tier and before its journal object was written: the object is whole, never a part of
/// the body; it was never acknowledged (the task did not return); nothing journals it, so a
/// reconciler never copies it; and a put of the key after a restart is refused as existing. This
/// PINS A DOCUMENTED LIMIT, the first of the `Proxy` known limits: an unacknowledged object is
/// left in the hot tier with no entry. If the limit is closed this test must change to assert
/// that the object is gone. Defect it catches the other way: an object that is partial at this
/// point, or a journal entry written for an unacknowledged put.
#[tokio::test(start_paused = true)]
async fn a_put_killed_before_its_journal_entry_leaves_a_whole_object_that_is_never_copied() {
    let dirs = Dirs::new();
    let entered = Arc::new(Notify::new());
    let hot: Arc<dyn Backend> = Arc::new(Stall::new(
        dirs.hot.clone(),
        "_journal/",
        entered.clone(),
        None,
    ));
    let proxy = Proxy::open(hot, dirs.cold.clone(), Dirs::config("p1"))
        .await
        .unwrap();
    let proxy = Arc::new(proxy);
    let writer = {
        let proxy = proxy.clone();
        tokio::spawn(async move {
            let pieces = chunks(&["whole ", "object"]);
            proxy.put_stream("k", None, pieces).await
        })
    };
    entered.notified().await;
    writer.abort();
    assert!(writer.await.unwrap_err().is_cancelled());

    assert_eq!(read(&*dirs.hot, "k").await, body("whole object"));
    assert_eq!(journal_keys(&*dirs.hot, "p1").await, Vec::<String>::new());
    assert_eq!(temporary_files(dirs.hot_dir.path()), Vec::<String>::new());

    let restarted = dirs.proxy("p1").await;
    let again = restarted.put("k", body("again")).await;
    assert_eq!(again, Err(Error::AlreadyExists("k".to_owned())));
    let clock = Arc::new(TestClock::new());
    let config = ReconcilerConfig::new("r1", 1 << 30, clock);
    let mut reconciler = Reconciler::new(dirs.hot.clone(), dirs.cold.clone(), config);
    let report = reconciler.pass().await;
    assert!(report.held);
    assert_eq!(report.copied, 0);
    assert_absent(&*dirs.cold, "k").await;
}

/// Asserts that every put that was acknowledged is in a journal object in the hot tier, once, as
/// a put of its size, whatever the batch it shared; and that after the proxy is gone a new
/// reconciler copies every one of them to the cold tier byte for byte. Ten streamed puts at once
/// in batches of four. Defect it catches: an acknowledgement sent before the journal object was
/// written (an entry missing), an entry lost in a full batch, an entry written twice.
#[tokio::test(start_paused = true)]
async fn every_acknowledged_put_is_in_a_journal_and_a_restart_copies_it() {
    let rig = Rig::new();
    let mut config = rig.config("p1", 1 << 20);
    config.batch_entries = 4;
    let proxy = Arc::new(rig.open(config).await.unwrap());
    let names: Vec<String> = (0..10).map(|n| format!("object-{n}")).collect();
    let puts = names.iter().map(|name| {
        let proxy = proxy.clone();
        async move { proxy.put_stream(name, None, chunks(&["abc", "def"])).await }
    });
    for acknowledged in futures::future::join_all(puts).await {
        acknowledged.unwrap();
    }
    drop(proxy);

    for name in &names {
        let entries = entries_about(&*rig.hot_mem, name).await;
        assert_eq!(entries.len(), 1, "{name}");
        assert_eq!(entries[0].op, JournalOp::Put, "{name}");
        assert_eq!(entries[0].size, 6, "{name}");
    }
    let report = rig.reconciler("r1", 1 << 20).pass().await;
    assert!(report.held);
    assert_eq!(report.copied, names.len());
    assert_eq!(report.failed, Vec::<Error>::new());
    for name in &names {
        assert_eq!(read(&*rig.cold_mem, name).await, body("abcdef"), "{name}");
    }
}

/// Asserts that a reconciler killed in the middle of a pass (the copy of one object never ends)
/// leaves the lease with nothing that stops a replacement but its age: a second reconciler does
/// nothing before the ttl, takes the lease at the ttl, and its pass leaves the cold tier equal to
/// the hot tier for every acknowledged object, whatever the killed pass had done; and when the
/// killed process comes back (it was paused, not gone) its next pass is stopped at the renewal:
/// it makes no put or delete in the hot tier, no change in the cold tier, and leaves the new
/// holder's lease alone. Defect it catches: a checkpoint that moves before the copy is done (an
/// object that is never copied), a lease that is taken before its ttl, a renewal that does not
/// check the content tag (the old holder holds again).
#[tokio::test(start_paused = true)]
async fn a_reconciler_killed_mid_pass_is_replaced_and_the_cold_tier_converges() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1 << 20).await;
    let names = ["k1", "k2", "k3", "k4"];
    for name in names {
        proxy.put(name, body(name)).await.unwrap();
    }
    let entered = Arc::new(Notify::new());
    let stuck: Arc<dyn Backend> =
        Arc::new(Stall::new(rig.cold.clone(), "k3", entered.clone(), None));
    let config = rig.reconciler_config("r1", 1 << 20);
    let mut one = Reconciler::new(rig.hot.clone(), stuck, config);
    let stuck_there = tokio::select! {
        _ = one.pass() => false,
        () = entered.notified() => true,
    };
    assert!(stuck_there, "the pass was to be stuck at the copy of k3");
    assert_absent(&*rig.cold_mem, "k3").await;

    let mut two = rig.reconciler("r2", 1 << 20);
    assert_eq!(two.pass().await, PassReport::default());
    rig.clock.set(30_000);
    let report = two.pass().await;
    assert!(report.held);
    assert_eq!(report.copied, names.len());
    assert_eq!(report.failed, Vec::<Error>::new());
    for name in names {
        assert_eq!(read(&*rig.cold_mem, name).await, body(name), "{name}");
        assert_eq!(read(&*rig.hot_mem, name).await, body(name), "{name}");
    }

    let puts = rig.hot.calls(Op::Put);
    let deletes = rig.hot.calls(Op::Delete);
    let cold_before = keys(&*rig.cold_mem).await;
    assert_eq!(one.pass().await, PassReport::default());
    assert_eq!(rig.hot.calls(Op::Put), puts);
    assert_eq!(rig.hot.calls(Op::Delete), deletes);
    assert_eq!(keys(&*rig.cold_mem).await, cold_before);
    assert!(
        text(&*rig.hot_mem, "_tiering/lease")
            .await
            .starts_with("r2\n")
    );
    assert!(two.pass().await.held);
}
