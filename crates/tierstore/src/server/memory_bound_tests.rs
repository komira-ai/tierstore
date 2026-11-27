//! The memory bound: the look at `/proc/self/status`, the mark and the bound on a series of looks,
//! the count of exits in the state directory, the stop, the exit status, `/memoryz` and the series
//! of `/metricz`. Every look is handed in, so no test reads the memory of the test process or
//! waits.

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Request, StatusCode};
use tierstore_testkit::dir::TestDir;

use super::*;
use crate::server::config::settings_from_text;
use crate::server::config_testing::{DEFAULTS, MINIMAL};
use crate::server::testing::Rig;

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

/// A look of `anon` bytes of anonymous memory and nothing else.
fn anon(anon: u64) -> Option<Sample> {
    Some(Sample {
        anon,
        file: 0,
        shmem: 0,
    })
}

/// The steps of the looks `sizes`, in order, on `watch`.
fn steps(watch: &MemoryWatch, sizes: &[u64]) -> Vec<Step> {
    let mut after = None;
    sizes
        .iter()
        .map(|size| watch.observe(anon(*size), &mut after))
        .collect()
}

/// Asserts that the three sizes are read from a status file as Linux writes it, in KiB, into
/// bytes. Catches a look that takes the resident size (`VmRSS`, which counts mapped files) for
/// the anonymous one, or forgets the unit.
#[test]
fn the_status_file_gives_the_anonymous_file_and_shared_sizes_in_bytes() {
    let status = "Name:\ttierstore\nVmRSS:\t 2101248 kB\nRssAnon:\t 1887436 kB\n\
                  RssFile:\t  204800 kB\nRssShmem:\t    9012 kB\nThreads:\t96\n";
    let sample = sample_of_status(status).unwrap();
    assert_eq!(
        sample,
        Sample {
            anon: 1_887_436 * 1024,
            file: 204_800 * 1024,
            shmem: 9012 * 1024,
        }
    );
}

/// Asserts that a status with no `RssAnon` that reads has no sample, whatever is wrong with it,
/// and that one whose `RssFile` or `RssShmem` is missing or does not read has 0 for that size.
/// Catches a bound held to a size that was not read.
#[test]
fn a_status_without_a_readable_anonymous_size_has_no_sample() {
    for status in [
        "",
        "VmRSS:\t 2101248 kB\n",
        "RssAnon:\t 1887436\n",
        "RssAnon:\t 1887436 MB\n",
        "RssAnon:\t many kB\n",
        "RssAnon:\t 18446744073709551615 kB\n",
    ] {
        assert_eq!(sample_of_status(status), None, "{status:?}");
    }
    let partial = "RssAnon:\t 4 kB\nRssFile:\t x kB\n";
    assert_eq!(
        sample_of_status(partial),
        Some(Sample {
            anon: 4096,
            file: 0,
            shmem: 0,
        })
    );
}

/// Asserts that a status file that cannot be read has no sample and one that can has its sizes.
#[test]
fn a_status_file_that_cannot_be_read_has_no_sample() {
    let dir = TestDir::under(std::env::temp_dir(), "memory-status");
    let path = dir.path().join("status");
    assert_eq!(sample_at(&path), None);
    std::fs::write(&path, "RssAnon:\t 8 kB\n").unwrap();
    assert_eq!(sample_at(&path), anon(8192));
}

/// The normal load of a build node as looks of its anonymous memory: the resident sizes the farm
/// measured 10 to 12 minutes after a start (1.2 to 2.0 GiB), then the most its configured
/// budgets allow at once (about 10 GiB: `docs/limits.md`, "Memory"), and back.
const NORMAL_LOAD: [u64; 12] = [
    GIB + GIB / 5,
    GIB + GIB / 2,
    2 * GIB,
    GIB + 4 * GIB / 5,
    3 * GIB,
    5 * GIB,
    8 * GIB,
    10 * GIB,
    10 * GIB,
    6 * GIB,
    2 * GIB,
    GIB + GIB / 2,
];

/// Asserts that the default bound is 32 GiB with its mark at 16 GiB, and that the normal load of a
/// build node is held under the default without the process being degraded or reaching it: every
/// look is steady, `/memoryz` says 200 and the exit status is the drain's. Catches a default
/// that would restart a node in normal work.
#[test]
fn the_normal_load_neither_degrades_nor_reaches_the_default_bound() {
    let watch = MemoryWatch::new(DEFAULT_BOUND, 0);
    assert_eq!((watch.bound(), watch.mark()), (32 * GIB, 16 * GIB));
    assert_eq!(steps(&watch, &NORMAL_LOAD), vec![Step::Steady; 12]);
    assert!(!watch.degraded());
    assert!(!watch.has_reached());
    assert_eq!(watch.exits(), 0);
    assert_eq!(probe(Some(&watch)).0, StatusCode::OK);
    assert_eq!(status_of(Some(&watch), 0), 0);
}

/// Asserts that a runaway at the rate of the leak that started this bound, 7 GiB a minute (120 MiB
/// a look) from 2 GiB, degrades the process at the first look at or over the mark, reaches the
/// bound at the first look at or over it, counts one exit, and is cut on the 30th look after
/// that, every other look steady. Catches a bound that never fires, fires twice, or never cuts a
/// drain that does not end.
#[test]
fn a_runaway_degrades_at_the_mark_reaches_the_bound_and_is_cut_30_looks_later() {
    let watch = MemoryWatch::new(DEFAULT_BOUND, 4);
    let sizes: Vec<u64> = (0..400).map(|look| 2 * GIB + look * 120 * MIB).collect();
    let degrade_at = sizes.iter().position(|size| *size >= 16 * GIB).unwrap();
    let reach_at = sizes.iter().position(|size| *size >= 32 * GIB).unwrap();
    assert_eq!((degrade_at, reach_at), (120, 256));
    let cut_at = reach_at + CUT_AFTER as usize;
    let got = steps(&watch, &sizes[..=cut_at]);
    for (look, step) in got.iter().enumerate() {
        let want = match look {
            _ if look == degrade_at => Step::Degraded,
            _ if look == reach_at => Step::Reached,
            _ if look == cut_at => Step::Cut,
            _ => Step::Steady,
        };
        assert_eq!(*step, want, "look {look}");
    }
    assert!(watch.degraded() && watch.has_reached());
    assert_eq!(watch.exits(), 5);
    assert_eq!(watch.anon(), sizes[cut_at]);
    assert_eq!(status_of(Some(&watch), 0), EXIT_STATUS);
    assert_eq!(status_of(Some(&watch), 1), 75);
}

/// Asserts the edges: a byte under the mark is steady and the mark degrades; a byte under the
/// bound is steady and the bound is reached; the process stays degraded down to seven eighths of
/// the mark and recovers a byte under it, and is degraded again at the mark.
#[test]
fn the_mark_and_the_bound_hold_at_their_size_and_not_a_byte_below() {
    let watch = MemoryWatch::new(1024, 0);
    assert_eq!(watch.mark(), 512);
    let got = steps(&watch, &[511, 512, 1000, 448, 447, 446, 512]);
    assert_eq!(
        got,
        [
            Step::Steady,
            Step::Degraded,
            Step::Steady,
            Step::Steady,
            Step::Recovered,
            Step::Steady,
            Step::Degraded,
        ]
    );
    let watch = MemoryWatch::new(1024, 0);
    let got = steps(&watch, &[512, 1023, 1024]);
    assert_eq!(got, [Step::Degraded, Step::Steady, Step::Reached]);
    // Straight to the bound from under the mark: degraded with it.
    let watch = MemoryWatch::new(1024, 0);
    assert_eq!(steps(&watch, &[100, 4096]), [Step::Steady, Step::Reached]);
    assert!(watch.degraded());
}

/// Asserts that the thread's loop pauses before each look and ends at the cut, 30 looks after
/// the bound, and not before: 2 looks under it, the look at it, 30 more.
#[test]
fn the_loop_pauses_before_each_look_and_ends_at_the_cut() {
    let watch = Arc::new(MemoryWatch::new(1024, 0));
    let dog = Dog::new(Arc::clone(&watch), None);
    let mut sizes = vec![100, 200, 2048].into_iter();
    let (mut looks, mut pauses) = (0u32, 0u32);
    let look = || {
        looks += 1;
        sizes.next().map_or_else(|| anon(2048), anon)
    };
    watch_until_cut(dog, look, || pauses += 1);
    assert_eq!((looks, pauses), (3 + CUT_AFTER, 3 + CUT_AFTER));
    assert!(watch.has_reached());
}

/// Asserts that a look that cannot be read changes nothing and is said once, and that once the
/// bound is reached such a look still counts toward the cut.
#[test]
fn a_look_that_cannot_be_read_is_skipped_but_counts_after_the_bound() {
    let watch = Arc::new(MemoryWatch::new(1024, 0));
    let mut dog = Dog::new(Arc::clone(&watch), None);
    assert_eq!(dog.tick(anon(100)), Tick::Again);
    for _ in 0..3 {
        assert_eq!(dog.tick(None), Tick::Again);
        assert!(dog.said_blind);
    }
    assert_eq!(watch.anon(), 100);
    assert!(!watch.degraded());
    assert_eq!(dog.tick(anon(2048)), Tick::Again);
    for _ in 1..CUT_AFTER {
        assert_eq!(dog.tick(None), Tick::Again);
    }
    assert_eq!(dog.tick(None), Tick::Exit);
    assert_eq!(watch.anon(), 2048);
}

/// Asserts that the dog says each change and goes on looking, and that the mark and its recovery
/// leave the bound unreached.
#[test]
fn the_dog_goes_on_through_the_mark_and_its_recovery() {
    let watch = Arc::new(MemoryWatch::new(1024, 0));
    let mut dog = Dog::new(Arc::clone(&watch), None);
    for size in [600, 700, 400, 100] {
        assert_eq!(dog.tick(anon(size)), Tick::Again, "{size}");
    }
    assert!(!watch.degraded() && !watch.has_reached());
}

/// Asserts that each exit at the bound raises the count in the state directory, which the next
/// start reads, so that the second exit of the same node writes 2, and leaves no other file; that
/// an exit adds to the count the file has when it exits (one the other instance raised); and that
/// a count that does not read, or no count, is 0.
#[test]
fn the_exits_at_the_bound_are_counted_across_starts_in_the_state_directory() {
    let dir = TestDir::under(std::env::temp_dir(), "memory-count");
    assert_eq!(read_count(dir.path()), 0);
    for start in 1..=2u64 {
        let watch = Arc::new(MemoryWatch::new(1024, read_count(dir.path())));
        let mut dog = Dog::new(Arc::clone(&watch), Some(dir.path().to_owned()));
        assert_eq!(dog.tick(anon(4096)), Tick::Again);
        assert_eq!(watch.exits(), start);
        let text = std::fs::read_to_string(dir.path().join(COUNT_FILE)).unwrap();
        assert_eq!(text, format!("{start}\n"));
    }
    let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
    assert_eq!(left.len(), 1, "only the count file: {left:?}");
    // The other instance raised the count since this one started: the exit adds to the file.
    let watch = Arc::new(MemoryWatch::new(1024, 0));
    std::fs::write(dir.path().join(COUNT_FILE), "7\n").unwrap();
    let mut dog = Dog::new(Arc::clone(&watch), Some(dir.path().to_owned()));
    assert_eq!(dog.tick(anon(4096)), Tick::Again);
    let text = std::fs::read_to_string(dir.path().join(COUNT_FILE)).unwrap();
    assert_eq!(text, "8\n");
    std::fs::write(dir.path().join(COUNT_FILE), "two\n").unwrap();
    assert_eq!(read_count(dir.path()), 0);
}

/// Asserts that an exit that cannot be counted (the state directory is gone) is still an exit:
/// the dog goes on to the drain and the watch counts it.
#[test]
fn an_exit_that_cannot_be_counted_still_drains() {
    let dir = TestDir::under(std::env::temp_dir(), "memory-count-gone");
    let gone = dir.path().join("gone");
    assert!(write_count(&gone, 1).is_err());
    let watch = Arc::new(MemoryWatch::new(1024, 0));
    let mut dog = Dog::new(Arc::clone(&watch), Some(gone.clone()));
    assert_eq!(dog.tick(anon(1024)), Tick::Again);
    assert!(watch.has_reached());
    assert_eq!(watch.exits(), 1);
    assert!(!gone.exists());
}

/// Asserts that the stop ends at the bound when the signal has not come, at the signal when the
/// bound has not been reached, and at the signal alone without a watch. Catches a bound that does
/// not stop the process, and a watch that stops it before its bound.
#[tokio::test]
async fn the_stop_ends_at_the_bound_or_at_the_signal() {
    let watch = Arc::new(MemoryWatch::new(1024, 0));
    let stop = or_reached_of(std::future::pending(), Some(Arc::clone(&watch)));
    tokio::pin!(stop);
    assert!(futures::poll!(&mut stop).is_pending());
    // Under the mark, and over it: the stop waits for the bound either way.
    assert_eq!(steps(&watch, &[300, 1000]), [Step::Steady, Step::Degraded]);
    assert!(futures::poll!(&mut stop).is_pending());
    assert_eq!(steps(&watch, &[1024]), [Step::Reached]);
    stop.await;

    let unreached = Arc::new(MemoryWatch::new(1024, 0));
    or_reached_of(std::future::ready(()), Some(unreached)).await;
    or_reached_of(std::future::ready(()), None).await;
}

/// Asserts that the exit status is the drain's unless the bound was reached, and then 75 whatever
/// the drain gave.
#[test]
fn the_exit_status_is_75_once_the_bound_is_reached() {
    assert_eq!(status_of(None, 0), 0);
    assert_eq!(status_of(None, 1), 1);
    let watch = MemoryWatch::new(1024, 0);
    assert_eq!(status_of(Some(&watch), 3), 3);
    assert_eq!(steps(&watch, &[2048]), [Step::Reached]);
    assert_eq!(status_of(Some(&watch), 0), 75);
    assert_eq!(status_of(Some(&watch), 3), 75);
}

/// Asserts the four answers of `/memoryz`, by exact equality: not watched, under the mark, over
/// it, and at the bound.
#[test]
fn memoryz_says_the_sizes_and_is_503_over_the_mark_and_at_the_bound() {
    assert_eq!(
        probe(None),
        (StatusCode::OK, "memory not watched".to_owned())
    );
    let watch = MemoryWatch::new(1024, 0);
    steps(&watch, &[100]);
    assert_eq!(
        probe(Some(&watch)),
        (
            StatusCode::OK,
            "anonymous memory 100 bytes, mark 512 bytes, bound 1024 bytes".to_owned()
        )
    );
    steps(&watch, &[600]);
    assert_eq!(
        probe(Some(&watch)),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "anonymous memory is above the mark: 600 bytes, mark 512 bytes, bound 1024 bytes"
                .to_owned()
        )
    );
    steps(&watch, &[1500]);
    assert_eq!(
        probe(Some(&watch)),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "anonymous memory reached the bound: 1500 bytes, bound 1024 bytes; draining".to_owned()
        )
    );
}

/// Asserts the series of the bound, by exact equality: all 0 without a watch, and the last look,
/// the bound, the degraded state and the exits counted with one.
#[test]
fn the_series_of_the_bound_are_0_without_a_watch_and_its_state_with_one() {
    let zero = "# TYPE tierstore_memory_anon_bytes gauge\ntierstore_memory_anon_bytes 0\n\
                # TYPE tierstore_memory_bound_bytes gauge\ntierstore_memory_bound_bytes 0\n\
                # TYPE tierstore_memory_degraded gauge\ntierstore_memory_degraded 0\n\
                # TYPE tierstore_memory_bound_exits_total counter\n\
                tierstore_memory_bound_exits_total 0\n";
    assert_eq!(family(None), zero);
    let watch = MemoryWatch::new(1024, 3);
    steps(&watch, &[700]);
    let degraded = "# TYPE tierstore_memory_anon_bytes gauge\ntierstore_memory_anon_bytes 700\n\
                    # TYPE tierstore_memory_bound_bytes gauge\ntierstore_memory_bound_bytes 1024\n\
                    # TYPE tierstore_memory_degraded gauge\ntierstore_memory_degraded 1\n\
                    # TYPE tierstore_memory_bound_exits_total counter\n\
                    tierstore_memory_bound_exits_total 3\n";
    assert_eq!(family(Some(&watch)), degraded);
}

async fn get(rig: &Rig, path: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .method("GET")
        .uri(path)
        .body(Full::new(Bytes::new()))
        .unwrap();
    let reply = rig.send_raw(request).await;
    (reply.status, reply.body)
}

/// Asserts that a server shows the watch it is given on `/memoryz`, unsigned, and on `/metricz`,
/// and that a server without one says the memory is not watched. Catches a probe that is not
/// routed and a watch that is not the server's.
#[tokio::test]
async fn the_server_shows_its_watch_on_memoryz_and_metricz() {
    let watch = Arc::new(MemoryWatch::new(1024, 0));
    steps(&watch, &[600]);
    let given = Arc::clone(&watch);
    let rig = Rig::with(1 << 20, move |config| config.memory = Some(given)).await;
    let (status, text) = get(&rig, "/memoryz").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        text,
        "anonymous memory is above the mark: 600 bytes, mark 512 bytes, bound 1024 bytes"
    );
    let (_, metrics) = get(&rig, "/metricz").await;
    assert!(
        metrics.contains("\ntierstore_memory_degraded 1\n"),
        "{metrics}"
    );
    assert!(
        metrics.contains("\ntierstore_memory_anon_bytes 600\n"),
        "{metrics}"
    );

    let plain = Rig::new(1 << 20).await;
    assert_eq!(
        get(&plain, "/memoryz").await,
        (StatusCode::OK, "memory not watched".to_owned())
    );
}

/// A node over the cold tier alone (as `config_cold_tests.rs` has it).
const COLD: &str = r#"
[[interface]]
type   = "s3"
params = { addresses = ["127.0.0.1:9300"] }

[server]
proxy_id        = "front-1"
bucket          = "tier-bucket"
access_key_file = "/etc/tierstore/access.key"

[cold_store]
kind             = "s3"
bucket           = "tierstore-cold"
endpoints        = ["http://127.0.0.1:30900"]
credentials_file = "/etc/tierstore/cold.credentials"
"#;

/// A build node (as `config_cache_tests.rs` has it).
const CACHE: &str = r#"
[[interface]]
type   = "s3"
params = { addresses = ["127.0.0.1:0"] }

[server]
proxy_id        = "c1"
bucket          = "tier-bucket"
access_key_file = "key"

[local_cache]
path   = "/c"
budget = 1000

[cold_store]
kind = "memory"

[[policy]]
prefix = "cas/"
cache  = true
"#;

/// A storage node with a local cache (as `config_combined_tests.rs` has it).
const COMBINED: &str = r#"
[[interface]]
type   = "s3"
params = { addresses = ["192.0.2.10:9400"] }

[server]
proxy_id        = "tierstore-1"
bucket          = "tierstore"
access_key_file = "/etc/tierstore/access.key"

[hot_store]
kind   = "dir"
path   = "/srv/hot"
budget = "500GiB"

[cold_store]
kind = "dir"
path = "/srv/cold"

[local_cache]
path   = "/var/lib/tierstore-cache"
budget = "40GiB"

[[policy]]
prefix = "cas/b/"
cache  = true
"#;

/// `text` with `memory_bound = value` in its `[server]` table.
fn with_bound(text: &str, value: &str) -> String {
    assert!(text.contains("[server]\n"));
    text.replacen(
        "[server]\n",
        &format!("[server]\nmemory_bound = {value}\n"),
        1,
    )
}

/// Asserts that `server.memory_bound` is 32 GiB when the file leaves it out, the size the file
/// gives, and 0 (no bound) for `"0B"`, for every shape of node; and that a size that does not read
/// is refused naming the key.
#[test]
fn the_bound_is_the_key_of_the_server_table_in_every_shape() {
    for (shape, text) in [
        ("storage", MINIMAL),
        ("cold", COLD),
        ("cache", CACHE),
        ("combined", COMBINED),
    ] {
        let settings = settings_from_text(text, DEFAULTS).unwrap();
        assert_eq!(bound_of(&settings), DEFAULT_BOUND, "{shape}");
        for (value, bytes) in [("\"1GiB\"", GIB), ("\"0B\"", 0)] {
            let settings = settings_from_text(&with_bound(text, value), DEFAULTS).unwrap();
            assert_eq!(bound_of(&settings), bytes, "{shape} {value}");
        }
    }
    let bad = with_bound(MINIMAL, "\"lots\"");
    let why = settings_from_text(&bad, DEFAULTS).unwrap_err().to_string();
    assert!(why.contains("memory_bound"), "{why}");
}

/// Asserts that a bound of 0 starts no watch: the binary's stop is the signal alone, `/memoryz`
/// says the memory is not watched and the exit status is the drain's. (No test installs a bound
/// above 0: its thread would look at the memory of the test process.)
#[tokio::test]
async fn a_bound_of_zero_installs_no_watch() {
    install(0, None);
    assert!(installed().is_none());
    assert_eq!(status(2), 2);
    or_reached(std::future::ready(())).await;
}
