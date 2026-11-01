//! A build node and the lock of its cache directory: a start beside a process that holds the
//! directory, a start that is alone, and two nodes over one directory as a hand-over runs them.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use objstore_s3::{ByteRange, ObjectKey, ObjectStore};

use super::Drained;
use super::build_info::{COMMIT, VERSION, gauge};
use super::cache_cli_tests::{Node, client, probe, serving};
use super::local_cache::testing::{age, pattern, tree};
use super::local_cache::{Disk, LocalCache, Mode, SystemDisk};
use super::lock::try_take;
use crate::{Backend, Clock, FaultyBackend, MemoryBackend, SystemClock};

/// Asserts that a build node started over a directory whose lock another cache holds serves and
/// writes nothing to the directory: `/readyz` says 200 `ready read-only`, the gauge says it, a
/// hit is read from the directory and a miss comes from the upstream, the start-up probe of the
/// file system is not made (it writes under `.stage/` and `obj/`), nothing the holder left in
/// `.stage/` is removed, and the listing of the whole directory is the same after the node has
/// drained as before it started. It would catch a start that cleans up or probes in a directory
/// that another process writes to.
#[tokio::test]
async fn a_build_node_over_a_held_directory_serves_read_only_and_leaves_the_directory_alone() {
    let node = Node::new("tscache-lock-ro");
    let settings = node.settings("", "");
    let memory = Arc::new(MemoryBackend::new());
    let body = pattern(1000);
    for key in ["cas/hit", "cas/miss"] {
        memory.put(key, Bytes::from(body.clone())).await.unwrap();
    }
    let upstream = Arc::new(FaultyBackend::new(memory.clone()));
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let for_holder: Arc<dyn Backend> = upstream.clone();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let cache = settings.cache.clone();
    let holder = LocalCache::open_locked(cache, for_holder, Arc::clone(&disk), clock).unwrap();
    assert_eq!(holder.mode(), Mode::ReadWrite);
    let _ = holder.get("cas/hit", None).await.unwrap();
    let root = node.cache_dir();
    let stage = root.join(".stage");
    std::fs::write(stage.join("0000000000000009"), b"a fill of the holder").unwrap();
    age(&root);
    let before = tree(&root);

    let work = |addr: SocketAddr| async move {
        let (status, text) = probe(addr, "/readyz").await;
        assert_eq!((status, text.as_str()), (200, "ready read-only"));
        let store = client(addr);
        let all = ByteRange::new(0, 1000).unwrap();
        for key in ["cas/hit", "cas/miss"] {
            let key = ObjectKey::new(key).unwrap();
            assert_eq!(store.get_range(&key, all).await.unwrap(), body);
        }
        let (status, text) = probe(addr, "/metricz").await;
        assert_eq!(status, 200);
        for line in [
            "tierstore_cache_mode{mode=\"read_only\"} 1\n",
            "tierstore_cache_mode{mode=\"read_write\"} 0\n",
            "tierstore_cache_hits_total 1\n",
            "tierstore_cache_misses_total 1\n",
            "tierstore_cache_fills_total 0\n",
        ] {
            assert!(text.contains(line), "{line} in {text}");
        }
    };
    let drained = serving(&settings, upstream, disk, work).await.unwrap();
    assert_eq!(drained, Drained { abandoned: 0 });
    assert_eq!(tree(&root), before);
    assert_eq!(holder.mode(), Mode::ReadWrite);
}

/// Asserts that a build node that is alone holds the lock of its cache directory for as long as
/// it runs, says `ready read-write` from its first answer, and lets the lock go when it exits:
/// a try of the lock from outside finds it held while the node serves and takes it after. It
/// would catch a node that serves read-write without holding anything, which a second node could
/// then start beside.
#[tokio::test]
async fn a_lone_build_node_holds_the_lock_while_it_serves() {
    let node = Node::new("tscache-lock-lone");
    let settings = node.settings("", "");
    let upstream = Arc::new(FaultyBackend::new(Arc::new(MemoryBackend::new())));
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let lock = node.cache_dir().join(".lock");
    let work = |addr: SocketAddr| async move {
        let (status, text) = probe(addr, "/readyz").await;
        assert_eq!((status, text.as_str()), (200, "ready read-write"));
        assert!(try_take(&lock).unwrap().is_none());
    };
    let drained = serving(&settings, upstream, disk, work).await.unwrap();
    assert_eq!(drained, Drained { abandoned: 0 });
    // The tasks of the drain that were cut drop their hold of the cache on the runtime's next
    // turns, so the lock is free after a few yields; a node that never lets it go fails here.
    let lock = node.cache_dir().join(".lock");
    let mut freed = false;
    for _ in 0..100_000 {
        freed = try_take(&lock).unwrap().is_some();
        if freed {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(freed, "the lock was still held after the node exited");
}

/// Reads all of `key` through the build node at `addr`.
async fn read_through(addr: SocketAddr, key: &str) -> Bytes {
    let key = ObjectKey::new(key).unwrap();
    let all = ByteRange::new(0, 1000).unwrap();
    client(addr).get_range(&key, all).await.unwrap()
}

/// The lines of the text of `/metricz` that are samples of `family` (its name and the `{` of its
/// labels).
fn samples<'a>(text: &'a str, family: &str) -> Vec<&'a str> {
    let start = format!("{family}{{");
    text.lines()
        .filter(|line| line.starts_with(&start))
        .collect()
}

/// Asserts what `/metricz` says of two build nodes over one cache directory, slots 9301 and 9302,
/// as a hand-over runs them: each one's build gauge carries its own slot, so a scrape says which
/// instance answered; the first, which holds the lock, says `read_write` and counts the fill of
/// its miss; the second says `read_only` (and `/readyz` says it), serves a hit and a miss, and its
/// fill and eviction families, the staging files and the files discarded at start are 0. It would
/// catch two instances that a scrape cannot tell apart, and a read-only node that fills, evicts
/// or cleans up.
#[tokio::test]
async fn two_build_nodes_over_one_directory_say_their_slot_and_their_mode() {
    let node = Node::new("tscache-lock-two");
    let first = node.settings("slot = 9301", "");
    let second = node.settings("slot = 9302", "");
    let memory = Arc::new(MemoryBackend::new());
    let body = pattern(1000);
    for key in ["cas/hit", "cas/miss"] {
        memory.put(key, Bytes::from(body.clone())).await.unwrap();
    }
    let upstream = Arc::new(FaultyBackend::new(memory.clone()));
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let for_first: Arc<dyn Backend> = upstream.clone();
    let for_second: Arc<dyn Backend> = upstream;
    let second_disk = Arc::clone(&disk);
    let second = &second;
    let work = |at_first: SocketAddr| async move {
        assert_eq!(read_through(at_first, "cas/hit").await, body);
        let beside = |at_second: SocketAddr| async move {
            let (status, text) = probe(at_second, "/readyz").await;
            assert_eq!((status, text.as_str()), (200, "ready read-only"));
            let (status, text) = probe(at_first, "/readyz").await;
            assert_eq!((status, text.as_str()), (200, "ready read-write"));
            assert_eq!(read_through(at_second, "cas/hit").await, body);
            assert_eq!(read_through(at_second, "cas/miss").await, body);

            let (status, first_text) = probe(at_first, "/metricz").await;
            assert_eq!(status, 200);
            let (status, text) = probe(at_second, "/metricz").await;
            assert_eq!(status, 200);
            let first_build = gauge(VERSION, COMMIT, Some(9301));
            let second_build = gauge(VERSION, COMMIT, Some(9302));
            assert!(
                first_text.contains(&first_build),
                "{first_build} in {first_text}"
            );
            assert!(!first_text.contains(&second_build), "{first_text}");
            assert!(text.contains(&second_build), "{second_build} in {text}");
            assert!(!text.contains(&first_build), "{text}");
            for line in [
                "tierstore_cache_mode{mode=\"read_write\"} 1\n",
                "tierstore_cache_mode{mode=\"read_only\"} 0\n",
                "tierstore_cache_fills_total 1\n",
            ] {
                assert!(first_text.contains(line), "{line} in {first_text}");
            }
            for line in [
                "tierstore_cache_mode{mode=\"read_only\"} 1\n",
                "tierstore_cache_mode{mode=\"read_write\"} 0\n",
                "tierstore_cache_hits_total 1\n",
                "tierstore_cache_misses_total 1\n",
                "tierstore_cache_fills_total 0\n",
                "tierstore_cache_fills_skipped_total 0\n",
                "tierstore_cache_fills_skipped_busy_total 0\n",
                "tierstore_cache_pinned_skips_total 0\n",
                "tierstore_cache_staging_files 0\n",
                "tierstore_cache_discarded_at_start_total 0\n",
            ] {
                assert!(text.contains(line), "{line} in {text}");
            }
            for family in [
                "tierstore_cache_fill_failures_total",
                "tierstore_cache_evictions_total",
            ] {
                let lines = samples(&text, family);
                assert!(!lines.is_empty(), "{family} in {text}");
                for line in lines {
                    assert!(line.ends_with(" 0"), "{line}");
                }
            }
        };
        let drained = serving(second, for_second, second_disk, beside).await;
        assert_eq!(drained.unwrap(), Drained { abandoned: 0 });
    };
    let drained = serving(&first, for_first, disk, work).await.unwrap();
    assert_eq!(drained, Drained { abandoned: 0 });
}
