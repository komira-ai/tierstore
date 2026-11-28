//! The live rig over stand-ins: this crate's `fake-s3` as the upstream, as the rig starts it,
//! and again as the node (a process that prints a start line, has a memory and takes signals,
//! but serves no provider), with a provider served on the node's socket by the test. Its name
//! ends in `_integration`: the rig reads the node's `/proc/<pid>/status`.

#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use objstore_s3::s3::fake::Serving;
use support::provider::FakeProvider;
use tierstore_blackbox::live::{Live, NodeCommand, Setup};
use tierstore_blackbox::metrics::Gauges;
use tierstore_blackbox::provider::source;
use tierstore_blackbox::rig::Rig as _;
use tierstore_blackbox::tree::{Tree, TreeFault};
use tierstore_testkit::dir::TestDir;

fn scratch(name: &str) -> (TestDir, PathBuf) {
    let temp = std::env::temp_dir();
    TestDir::for_socket(
        &[temp.as_path(), Path::new(support::SCRATCH)],
        name,
        "provider.sock",
    )
}

/// The stand-in node: `fake-s3` on a control socket of its own at each start, but at the starts
/// in `failing`, which get a flag it refuses.
fn stand_in(dir: &Path, failing: &'static [usize]) -> NodeCommand {
    let starts = Arc::new(AtomicUsize::new(0));
    let dir = dir.to_owned();
    Box::new(move |_file: &Path| {
        let n = starts.fetch_add(1, Ordering::SeqCst);
        let mut command = Command::new(support::FAKE_S3);
        if failing.contains(&n) {
            command.arg("--no-such-flag");
        } else {
            command
                .arg("--control")
                .arg(dir.join(format!("node-{n}.sock")));
        }
        command
    })
}

fn setup(dir: &Path, small_disk: Option<PathBuf>) -> Setup {
    Setup {
        dir: dir.to_owned(),
        fake_s3: PathBuf::from(support::FAKE_S3),
        small_disk,
        budget_mib: 64,
        max_placements: Some(8),
        socket_wait: Duration::from_secs(30),
    }
}

/// Asserts what the rig does from outside: the node file it writes, the tree it puts into the
/// upstream, the agent's calls on the node's socket, a read of an empty mount, the placements
/// in the cache directory, the node's memory, `/metricz` of its front, the upstream's modes and
/// counts, a restart, the clock, and a stop of both processes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_live_rig_drives_its_node_and_upstream_from_outside() {
    let (dir, socket) = scratch("live-rig");
    let provider = FakeProvider::serve(&socket, &dir.path().join("trees"));
    let nowhere = dir.path().join("nowhere");
    let mut live = Live::start(
        setup(dir.path(), Some(nowhere.clone())),
        stand_in(dir.path(), &[]),
    )
    .await
    .unwrap();
    let file = std::fs::read_to_string(dir.path().join("node.toml")).unwrap();
    assert!(
        file.contains(&format!("path   = \"{}\"", nowhere.join("cache").display())),
        "{file}"
    );
    assert!(file.ends_with("[trees]\nmax_placements = 8\n"), "{file}");
    assert!(
        file.contains("endpoint         = \"http://127.0.0.1:"),
        "{file}"
    );

    let tree = Tree::new("live", 2, 16).unwrap();
    live.put(&tree).await.unwrap();
    let one = source("blackbox-agent", "js.live-0", 0);
    live.prepare(&one, tree.key()).await.unwrap();
    let at = live.mount(&one).await.unwrap();
    assert_eq!(at, dir.path().join("trees/js.live-0/0"));
    let empty = live.read(&at, &tree).await.unwrap_err();
    assert!(matches!(empty, TreeFault::Unreadable { .. }), "{empty}");
    live.release(&one).await.unwrap();
    assert!(provider.prepared().is_empty());

    assert_eq!(live.placements().unwrap(), 0);
    assert!(live.resident().unwrap() > 0);
    match live.gauges().await {
        Ok(gauges) => assert_eq!(gauges, Gauges::default()),
        Err(why) => assert!(why.contains("/metricz answered "), "{why}"),
    }
    live.serve_as(Serving::Failing {
        prefix: String::new(),
        status: 503,
    })
    .await
    .unwrap();
    let refused = live.put(&tree).await.unwrap_err();
    assert!(
        refused.starts_with("PUT /cas/cas/p/") && refused.ends_with(" to fake-s3 answered 503"),
        "{refused}"
    );
    let refused = live
        .serve_as(Serving::Failing {
            prefix: String::new(),
            status: 200,
        })
        .await
        .unwrap_err();
    assert_eq!(
        refused,
        "fake-s3 answered \"fail 200 \" with \"refused: \\\"200\\\" is not a status of 400 to 599\""
    );
    for serving in [
        Serving::Stalled {
            prefix: "cas/p/".to_owned(),
        },
        Serving::Failing {
            prefix: String::new(),
            status: 503,
        },
        Serving::All,
    ] {
        live.serve_as(serving).await.unwrap();
    }
    assert_eq!(live.stalled().await.unwrap(), 0);

    assert!(live.alive());
    live.restart().await.unwrap();
    assert!(live.alive());
    let refused = live.fill_disk().unwrap_err();
    assert!(
        refused.starts_with(&format!("filling {}: ", nowhere.join("ballast").display())),
        "{refused}"
    );
    let refused = live.free_disk().unwrap_err();
    assert!(
        refused.starts_with(&format!("{}: ", nowhere.join("ballast").display())),
        "{refused}"
    );
    let before = live.now();
    live.pause(Duration::from_millis(1)).await;
    // The rig's clock moves on with a pause: the cases' waits rest on it.
    assert!(live.now() > before);
    std::fs::remove_file(dir.path().join("upstream.sock")).unwrap();
    let refused = live.stalled().await.unwrap_err();
    assert!(
        refused.starts_with("fake-s3's control socket, \"stats\": "),
        "{refused}"
    );
    live.stop().await.unwrap();
}

/// Asserts that a node that does not come back from a restart leaves a rig that says so: no
/// memory to read, not alive, no restart; that such a rig still stops its upstream; and that a
/// case without a disk of its own cannot fill one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_that_does_not_come_back_leaves_a_rig_that_says_so() {
    let (dir, socket) = scratch("live-gone");
    let _provider = FakeProvider::serve(&socket, &dir.path().join("trees"));
    let mut live = Live::start(setup(dir.path(), None), stand_in(dir.path(), &[1]))
        .await
        .unwrap();
    let none = "this case's cache has no disk of its own";
    assert_eq!(live.fill_disk().unwrap_err(), none);
    assert_eq!(live.free_disk().unwrap_err(), none);
    let refused = live.restart().await.unwrap_err();
    assert_eq!(
        refused,
        "tierstore ended before it printed its start line (exit status: 2); see tierstore.err"
    );
    assert!(!live.alive());
    assert_eq!(live.resident().unwrap_err(), "the node is not running");
    assert_eq!(live.restart().await.unwrap_err(), "the node is not running");
    live.stop().await.unwrap();
}

/// Asserts that a rig that cannot start says why: an upstream program that is not there, a node
/// whose provider socket takes no connection in time, and a case directory that is a file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rig_that_cannot_start_says_why() {
    let (dir, _) = scratch("live-unstarted");
    let mut missing = setup(dir.path(), None);
    missing.fake_s3 = dir.path().join("no-fake");
    let refused = Live::start(missing, stand_in(dir.path(), &[]))
        .await
        .err()
        .unwrap();
    assert!(
        refused.starts_with("fake-s3 could not be started: "),
        "{refused}"
    );

    let mut quick = setup(dir.path(), None);
    quick.dir = dir.path().join("quick");
    quick.socket_wait = Duration::ZERO;
    let refused = Live::start(quick, stand_in(dir.path(), &[]))
        .await
        .err()
        .unwrap();
    assert_eq!(
        refused,
        format!(
            "the node's provider socket {} took no connection within 0ns",
            dir.path().join("quick/provider.sock").display()
        )
    );

    let file = dir.path().join("a-file");
    std::fs::write(&file, b"x").unwrap();
    let mut filed = setup(dir.path(), None);
    filed.dir = file.clone();
    let refused = Live::start(filed, stand_in(dir.path(), &[]))
        .await
        .err()
        .unwrap();
    assert!(
        refused.starts_with(&format!("{}: ", file.join("logs").display())),
        "{refused}"
    );
}

/// Asserts that a start waits, looking again, for a provider socket that takes a connection
/// only some time after the node's start line: here, once the stand-in node is running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_waits_for_a_socket_bound_after_the_start_line() {
    let (dir, socket) = scratch("live-late");
    let root = dir.path().to_owned();
    let late = tokio::spawn(async move {
        while !root.join("node-0.sock").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        FakeProvider::serve(&socket, &root.join("trees"))
    });
    let live = Live::start(setup(dir.path(), None), stand_in(dir.path(), &[]))
        .await
        .unwrap();
    let _provider = late.await.unwrap();
    live.stop().await.unwrap();
}
