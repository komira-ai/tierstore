//! The put ingest as it serves a root: an object committed in the task program's rename sequence
//! becomes the object of its key with the declared digest and its file goes; a truncated, changed
//! or undeclared file is refused and nothing is stored; what a scan finds; and when it runs.

use std::sync::Arc;

use super::store::Refusal;
use super::testing::{Bench, commit, sha};
use super::{GRACE_MS, Report};
use crate::server::Drained;
use crate::server::cache_cli_tests::{Node, serving};
use crate::server::local_cache::Tick;
use crate::server::local_cache::testing::wait_until;
use crate::server::local_cache::{Disk, SystemDisk};
use crate::{Backend, FaultyBackend, MemoryBackend, Op};

/// The milliseconds of the grace.
fn grace() -> u64 {
    GRACE_MS
}

/// Asserts that an object committed as the job's task program commits it (`pack.part`,
/// `pack.sha256-<hex>.part`, `pack`) is stored as `cas/b/<build>/<nonce>/pack` with exactly its
/// bytes, readable through the cache, and that its file and its emptied directory are removed.
#[tokio::test]
async fn a_pack_committed_in_the_rename_sequence_becomes_its_object() {
    let mut bench = Bench::new("ingest-stored");
    let dir = bench.job("b1", "n1");
    bench.start().await;
    let bytes = b"one pass object bytes".repeat(1000);
    commit(&dir, "pack", &bytes, sha(&bytes));
    let report = bench.outcome().await;
    let key = "cas/b/b1/n1/pack".to_owned();
    assert_eq!(
        report,
        Report::Stored {
            key: key.clone(),
            created: true
        }
    );
    assert_eq!(bench.object(&key).await, Some(bytes.clone()));
    let read = bench.cache.get(&key, None).await.unwrap();
    assert_eq!(sha(&read), sha(&bytes));
    assert!(!dir.exists());
    assert!(bench.root.join("cas/b/b1").exists());
    bench.stop().await;
}

/// Asserts that a file whose bytes are shorter than those its declared digest names (a truncated
/// pack), or other bytes of the same size, is refused for its digest, that the upstream holds
/// nothing under its key, and that the file is removed.
#[tokio::test]
async fn a_truncated_or_changed_pack_is_refused_and_not_stored() {
    let mut bench = Bench::new("ingest-mismatch");
    let short = bench.job("b1", "short");
    let other = bench.job("b1", "other");
    bench.start().await;
    let whole = b"the whole pack".to_vec();
    commit(&short, "pack", &whole[..5], sha(&whole));
    let refused = bench.outcome().await;
    assert_eq!(
        refused,
        Report::Refused {
            key: "cas/b/b1/short/pack".to_owned(),
            why: Refusal::NotItsDigest
        }
    );
    commit(&other, "pack", b"the whole pacK", sha(&whole));
    let refused = bench.outcome().await;
    assert_eq!(
        refused,
        Report::Refused {
            key: "cas/b/b1/other/pack".to_owned(),
            why: Refusal::NotItsDigest
        }
    );
    assert_eq!(bench.object("cas/b/b1/short/pack").await, None);
    assert_eq!(bench.object("cas/b/b1/other/pack").await, None);
    assert!(!short.join("pack").exists());
    assert!(!other.join("pack").exists());
    bench.stop().await;
}

/// Asserts that a committed file whose digest was never declared (written straight under its
/// name) is left while the grace runs and is refused, unstored and removed, by the first scan after
/// it.
#[tokio::test]
async fn an_undeclared_pack_is_refused_after_the_grace() {
    let mut bench = Bench::new("ingest-undeclared");
    let dir = bench.job("b1", "n1");
    bench.start().await;
    std::fs::write(dir.join("pack"), b"no digest named").unwrap();
    bench.scan("mark1").await;
    assert!(dir.join("pack").exists());
    bench.clock.advance(grace() - 1);
    bench.scan("mark2").await;
    assert!(dir.join("pack").exists());
    bench.clock.advance(1);
    bench.scan("mark3").await;
    assert!(!dir.join("pack").exists());
    assert_eq!(bench.object("cas/b/b1/n1/pack").await, None);
    bench.stop().await;
}

/// Asserts that the refusal of an undeclared file is reported with its key and reason.
#[tokio::test]
async fn the_refusal_of_an_undeclared_pack_names_it() {
    let mut bench = Bench::new("ingest-undeclared-said");
    let dir = bench.job("b1", "n1");
    std::fs::write(dir.join("pack"), b"no digest named").unwrap();
    bench.start().await;
    bench.clock.advance(grace());
    std::fs::create_dir_all(bench.root.join("cas/b/mark")).unwrap();
    let refused = bench.outcome().await;
    assert_eq!(
        refused,
        Report::Refused {
            key: "cas/b/b1/n1/pack".to_owned(),
            why: Refusal::NoDigest
        }
    );
    bench.stop().await;
}

/// Asserts that files committed while no ingest ran are found by its first scan: one whose
/// `.sha256-<hex>.part` name is still there (declared by the scan) is stored when it is renamed,
/// and the `.part` names alone are never stored.
#[tokio::test]
async fn the_first_scan_takes_the_declarations_still_there() {
    let mut bench = Bench::new("ingest-first-scan");
    let dir = bench.job("b1", "n1");
    let bytes = b"declared before the start".to_vec();
    let staged = dir.join(format!("pack.sha256-{}.part", sha(&bytes)));
    std::fs::write(&staged, &bytes).unwrap();
    std::fs::write(dir.join("other.part"), b"never stored").unwrap();
    bench.start().await;
    std::fs::rename(&staged, dir.join("pack")).unwrap();
    let stored = bench.outcome().await;
    assert_eq!(
        stored,
        Report::Stored {
            key: "cas/b/b1/n1/pack".to_owned(),
            created: true
        }
    );
    assert_eq!(bench.object("cas/b/b1/n1/other.part").await, None);
    assert!(dir.join("other.part").exists());
    bench.stop().await;
}

/// Asserts that a pack whose key holds the same bytes already is stored (not created), and one
/// whose key holds other bytes is refused, the object left as it was.
#[tokio::test]
async fn a_key_that_holds_bytes_already_is_kept_or_refused() {
    let mut bench = Bench::new("ingest-exists");
    let same = bench.job("b1", "same");
    let other = bench.job("b1", "other");
    bench.start().await;
    let bytes = b"already there".to_vec();
    bench
        .rig
        .tamper
        .put("cas/b/b1/same/pack", bytes.clone().into())
        .await
        .unwrap();
    bench
        .rig
        .tamper
        .put("cas/b/b1/other/pack", b"first bytes".to_vec().into())
        .await
        .unwrap();
    commit(&same, "pack", &bytes, sha(&bytes));
    assert_eq!(
        bench.outcome().await,
        Report::Stored {
            key: "cas/b/b1/same/pack".to_owned(),
            created: false
        }
    );
    commit(&other, "pack", &bytes, sha(&bytes));
    assert_eq!(
        bench.outcome().await,
        Report::Refused {
            key: "cas/b/b1/other/pack".to_owned(),
            why: Refusal::OtherBytes
        }
    );
    let kept = bench.object("cas/b/b1/other/pack").await;
    assert_eq!(kept, Some(b"first bytes".to_vec()));
    bench.stop().await;
}

/// Asserts that a job's directory made after the start is watched: an object committed in it is
/// stored.
#[tokio::test]
async fn a_job_directory_made_after_the_start_is_watched() {
    let mut bench = Bench::new("ingest-new-dir");
    bench.start().await;
    let dir = bench.job("b2", "n2");
    bench.wait(|report| *report == Report::Scanned).await;
    let bytes = b"late directory".to_vec();
    commit(&dir, "pack", &bytes, sha(&bytes));
    assert_eq!(
        bench.outcome().await,
        Report::Stored {
            key: "cas/b/b2/n2/pack".to_owned(),
            created: true
        }
    );
    bench.stop().await;
}

/// Asserts that a put the upstream fails is left with its file, and is put by the next scan once
/// the upstream takes it.
#[tokio::test]
async fn a_failed_put_is_left_and_tried_again() {
    let mut bench = Bench::new("ingest-again");
    let dir = bench.job("b1", "n1");
    bench.start().await;
    bench.rig.upstream.fail_next(Op::Put, 1);
    let bytes = b"put later".to_vec();
    commit(&dir, "pack", &bytes, sha(&bytes));
    assert_eq!(
        bench.outcome().await,
        Report::Left {
            key: "cas/b/b1/n1/pack".to_owned()
        }
    );
    assert!(dir.join("pack").exists());
    std::fs::create_dir_all(bench.root.join("cas/b/mark")).unwrap();
    assert_eq!(
        bench.outcome().await,
        Report::Stored {
            key: "cas/b/b1/n1/pack".to_owned(),
            created: true
        }
    );
    bench.stop().await;
}

/// Asserts that a job's directory unchanged for a day is removed with what it holds, and a build's
/// directory left empty for as long with it, while a younger one is kept.
#[tokio::test]
async fn an_abandoned_directory_is_removed() {
    let mut bench = Bench::new("ingest-abandoned");
    let dir = bench.job("b1", "n1");
    std::fs::write(dir.join("pack.part"), b"never committed").unwrap();
    let changed = std::fs::metadata(&dir).unwrap().modified().unwrap();
    let changed = changed.duration_since(std::time::UNIX_EPOCH).unwrap();
    let day = 24 * 60 * 60 * 1000;
    bench
        .clock
        .set(u64::try_from(changed.as_millis()).unwrap() + day + 1000);
    bench.start().await;
    assert!(!dir.exists());
    assert!(!bench.root.join("cas/b/b1").exists());
    bench.clock.set(0);
    let young = bench.job("b2", "n2");
    bench.wait(|report| *report == Report::Scanned).await;
    assert!(young.exists());
    bench.stop().await;
}

/// Asserts that more files than run at once are all stored: those over the limit wait for a put
/// to come free.
#[tokio::test]
async fn more_packs_than_run_at_once_are_all_stored() {
    let mut bench = Bench::new("ingest-many");
    let count = super::MOST_PUTS * 2 + 1;
    let dirs: Vec<_> = (0..count)
        .map(|n| bench.job("b1", &format!("n{n}")))
        .collect();
    bench.start().await;
    for (n, dir) in dirs.iter().enumerate() {
        let bytes = format!("pack {n}").into_bytes();
        commit(dir, "pack", &bytes, sha(&bytes));
    }
    let mut stored = 0;
    while stored < count {
        if let Report::Stored { .. } = bench.outcome().await {
            stored += 1;
        }
    }
    for n in 0..count {
        let key = format!("cas/b/b1/n{n}/pack");
        assert_eq!(
            bench.object(&key).await,
            Some(format!("pack {n}").into_bytes())
        );
    }
    bench.stop().await;
}

/// Asserts that an instance that does not hold the lock of the cache directory ingests nothing,
/// and takes the root when it takes the lock: its first scan finds the declaration left, and the
/// commit after it is stored.
#[tokio::test]
async fn only_the_holder_of_the_lock_ingests() {
    let mut bench = Bench::new("ingest-lock");
    let dir = bench.job("b1", "n1");
    let bytes = b"after the lock".to_vec();
    let staged = dir.join(format!("pack.sha256-{}.part", sha(&bytes)));
    std::fs::write(&staged, &bytes).unwrap();
    let waiting = bench.rig.locked();
    bench.spawn_on(waiting.clone());
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert!(bench.quiet());
    // The holder goes: the waiting instance takes the lock at its next tick.
    bench.cache = waiting.clone();
    assert_eq!(waiting.tick().unwrap(), Tick::Took);
    bench.wait(|report| *report == Report::Watching).await;
    std::fs::rename(&staged, dir.join("pack")).unwrap();
    assert_eq!(
        bench.outcome().await,
        Report::Stored {
            key: "cas/b/b1/n1/pack".to_owned(),
            created: true
        }
    );
    bench.stop().await;
}

/// Asserts that a build node whose file names `local_cache.put_ingest` ingests while it serves: a
/// pack committed into a job's directory under the root is stored in the node's upstream with its
/// bytes, and its file is removed. The file is read, the node started and stopped, as the binary
/// does.
#[tokio::test]
async fn a_build_node_with_the_key_ingests_while_it_serves() {
    let node = Node::new("ingest-node");
    let root = node.cache_dir().with_file_name("put");
    let dir = root.join("cas/b/b1/n1");
    std::fs::create_dir_all(&dir).unwrap();
    let local = format!("write_through = true\nput_ingest = \"{}\"", root.display());
    let settings = node.settings("", &local);
    let memory = Arc::new(MemoryBackend::new());
    let upstream: Arc<dyn Backend> = Arc::new(FaultyBackend::new(memory.clone()));
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let bytes = b"committed while the node serves".to_vec();
    let staged = dir.join(format!("pack.sha256-{}.part", sha(&bytes)));
    std::fs::write(&staged, &bytes).unwrap();
    let pack = dir.join("pack");
    // A job's directory last changed at the epoch: the ingest's first scan removes it, which says
    // the watcher is in place.
    let old = root.join("cas/b/old/n0");
    std::fs::create_dir_all(&old).unwrap();
    let epoch = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1);
    std::fs::File::open(&old)
        .unwrap()
        .set_modified(epoch)
        .unwrap();
    let work = |_| async {
        wait_until("the first scan removes the old directory", || !old.exists()).await;
        std::fs::rename(&staged, &pack).unwrap();
        wait_until("the pack is stored and its file removed", || !pack.exists()).await;
    };
    let drained = serving(&settings, upstream, disk, work).await.unwrap();
    assert_eq!(drained, Drained { abandoned: 0 });
    let got = memory.get("cas/b/b1/n1/pack", None).await.unwrap();
    assert_eq!(got.to_vec(), bytes);
}
