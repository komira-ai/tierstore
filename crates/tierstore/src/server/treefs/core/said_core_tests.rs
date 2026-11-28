//! The lines of the lookups that fail, said through `Core::say_failed`: once in ten seconds, with
//! the count of those not said. And the report of the requests the look for idle trees sends
//! (`Core::look_around`, `core/requests.rs`), as the node renders it.

use std::sync::Arc;
use std::time::Duration;

use super::super::{Errno, Opened};
use super::SAY_EVERY_MS;
use crate::server::local_cache::opens::requests::RequestKind;
use crate::server::local_cache::trees::testing::Bench;
use crate::server::treefs::ino;
use crate::server::treefs::live_testing::Live;
use crate::server::treefs::testing::{Logged, Rig, logged, sample, sha, until};

/// How long the core of the first test waits for the write role the cache does not take: short, as
/// the cache never takes it.
const WAIT: Duration = Duration::from_millis(100);

/// The lines said while `run` runs, at warn.
fn said(run: impl FnOnce()) -> Vec<String> {
    let log = Logged::default();
    let guard = logged(&log, tracing::Level::WARN);
    run();
    drop(guard);
    log.text().lines().map(str::to_owned).collect()
}

/// Asserts that a lookup of a placement the node's cache cannot answer (it stays read-only for the
/// whole wait: another process holds the lock of its directory) is `EIO` said whole, that a second
/// within ten seconds is not said, and that the next after them carries the count.
#[tokio::test(flavor = "multi_thread")]
async fn a_placement_lookup_that_fails_is_said_once_in_ten_seconds_with_the_count() {
    let read_only = |bench: &Bench| {
        let _holder = bench.rig.locked();
        bench.rig.locked()
    };
    let live = Live::in_process_with("tfs-said-asked", read_only, WAIT).await;
    let clock = Arc::clone(&live.clock);
    let lines = live
        .on_core(move |core| {
            said(|| {
                for _ in 0..2 {
                    assert_eq!(core.lookup(ino::ROOT, "builds"), Err(Errno::Io));
                }
                clock.advance(Duration::from_millis(SAY_EVERY_MS));
                assert_eq!(core.lookup(ino::ROOT, "builds"), Err(Errno::Io));
            })
        })
        .await;
    let line = "tierstore fs: cannot look up the placement at \"builds\": the cache is read-only: \
                another process holds the lock of its directory";
    assert_eq!(
        lines,
        [
            line.to_owned(),
            format!("{line}; 1 more other failed request since the last line"),
        ]
    );
}

/// Asserts that a lookup of a tree whose root manifest cannot be had now is `EIO` said once in
/// ten seconds, with the count of those not said on the next line.
#[test]
fn a_tree_lookup_that_fails_is_said_once_in_ten_seconds_with_the_count() {
    let rig = Rig::new("tfs-said-refusal", sample());
    let digest = sha(b"a tree that is not built");
    let _ = rig
        .upstream
        .failing
        .lock()
        .unwrap()
        .insert(digest.clone(), "upstream 503".to_owned());
    let lines = said(|| {
        for _ in 0..3 {
            assert_eq!(rig.core.lookup(ino::TREES, &digest), Err(Errno::Io));
        }
        rig.clock.advance(Duration::from_millis(SAY_EVERY_MS));
        assert_eq!(rig.core.lookup(ino::TREES, &digest), Err(Errno::Io));
    });
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines[0].starts_with(&format!(
            "tierstore fs: cannot attach {digest} on a lookup: "
        )),
        "{lines:?}"
    );
    assert_eq!(
        lines[1],
        format!(
            "{}; 2 more other failed requests since the last line",
            lines[0]
        )
    );
}

/// The entry of `bin/tool` in the sample tree (`testing::sample`), numbered 1.
const TOOL: u32 = 4;

/// Asserts that a lookup taking 3 ms and an open taking 40 ms on the core's clock, each timed as
/// a frontend times it, land in their kind of the histogram the node renders from the report of
/// the look for idle trees: one request of 0.003 s under `kind="lookup"`, one of 0.04 s under
/// `kind="open"`, and none under any other kind.
#[test]
fn a_lookup_and_an_open_each_land_in_their_kind_of_the_request_seconds_histogram() {
    let rig = Rig::new("tfs-said-requests", sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = rig.core.attach(&root, "run-1").unwrap();
    let core = &rig.core;
    let arrival = core.request_begins();
    rig.clock.advance(Duration::from_millis(3));
    assert!(core.lookup(ino::of(1, 2), "tool").is_ok());
    core.request_ends(arrival, RequestKind::Lookup);
    let arrival = core.request_begins();
    let mut timing = core.open_begins();
    rig.clock.advance(Duration::from_millis(40));
    let opened = core.open_from(ino::of(1, TOOL), false, &mut timing);
    let ok = opened.is_ok();
    if let Ok(Opened::Cached(fh) | Opened::Adopted { fh, .. }) = opened {
        core.release(fh);
    }
    assert!(ok, "bin/tool opens");
    core.open_ends(timing, ok);
    core.request_ends(arrival, RequestKind::Open);
    core.look_around();
    let lookups = "tierstore_fs_request_seconds_count{kind=\"lookup\"} 1";
    until(|| rig.node.metrics().lines().any(|line| line == lookups));
    let rendered = rig.node.metrics();
    let series: Vec<&str> = rendered
        .lines()
        .filter(|line| {
            line.starts_with("tierstore_fs_request_seconds_")
                && (line.contains("_sum{") || line.contains("_count{"))
        })
        .collect();
    let mut want = Vec::new();
    for kind in [
        "lookup", "getattr", "access", "open", "read", "readdir", "readlink", "other",
    ] {
        let (sum, count) = match kind {
            "lookup" => ("0.003", 1),
            "open" => ("0.04", 1),
            _ => ("0", 0),
        };
        want.push(format!(
            "tierstore_fs_request_seconds_sum{{kind=\"{kind}\"}} {sum}"
        ));
        want.push(format!(
            "tierstore_fs_request_seconds_count{{kind=\"{kind}\"}} {count}"
        ));
    }
    assert_eq!(series, want);
}
