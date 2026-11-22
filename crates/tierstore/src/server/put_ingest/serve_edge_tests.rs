//! The edges of the put ingest's serve loop: a root that is made after the start, names the
//! ingest passes over, a link, an unreadable file and a file that goes before its put, a put that
//! is running at a scan and at the halt, a directory that cannot be removed, the loss of the lock,
//! and a declaration that arrives after the committed file.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use tierstore_testkit::dir::TestDir;
use tokio::sync::Notify;

use super::store::Refusal;
use super::testing::{Bench, commit, sha};
use super::{ABANDONED_MS, PutIngest, Report};
use crate::Clock;
use crate::server::local_cache::Tick;
use crate::server::local_cache::testing::wait_until;

/// Lets the tasks of the runtime run until all of them wait.
async fn idle() {
    for _ in 0..300 {
        tokio::task::yield_now().await;
    }
}

/// Whether this test runs as a user that the modes of files do not bind; says so when it does.
fn modes_do_not_bind(name: &str, why: &str) -> bool {
    let scratch = TestDir::under(std::env::temp_dir(), name);
    let probe = scratch.path().join("probe");
    std::fs::write(&probe, b"x").unwrap();
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o000)).unwrap();
    let readable = std::fs::File::open(&probe).is_ok();
    if readable {
        tracing::warn!("skipped: this user can read a file of mode 000, so {why}");
    }
    readable
}

/// The milliseconds a clock must show for the directory `dir` to have been unchanged for a day.
fn a_day_after(dir: &Path) -> u64 {
    let changed = std::fs::metadata(dir).unwrap().modified().unwrap();
    let changed = changed.duration_since(std::time::UNIX_EPOCH).unwrap();
    let day = ABANDONED_MS;
    u64::try_from(changed.as_millis()).unwrap() + day + 1000
}

/// Asserts that a root that does not exist at the start is reported as taken (no watcher, a
/// warning), and that when it is made the next scan makes the watcher, so that an object committed
/// afterwards, whose declaration only an event could show, is stored.
#[tokio::test(start_paused = true)]
async fn a_root_made_after_the_start_is_watched_from_the_next_scan() {
    let mut bench = Bench::new("ingest-edge-late-root");
    std::fs::remove_dir_all(&bench.root).unwrap();
    bench.start().await;
    let dir = bench.job("b1", "n1");
    bench.wait(|report| *report == Report::Scanned).await;
    let bytes = b"watched since the tick".to_vec();
    commit(&dir, "pack", &bytes, sha(&bytes));
    assert_eq!(
        bench.outcome().await,
        Report::Stored {
            key: "cas/b/b1/n1/pack".to_owned(),
            created: true
        }
    );
    assert_eq!(bench.object("cas/b/b1/n1/pack").await, Some(bytes));
    bench.stop().await;
}

/// Asserts that a directory named as a committed file is left alone, and that a plain file in a
/// place that is no job's directory is passed over: an object committed after them is stored.
#[tokio::test]
async fn directories_and_strays_are_passed_over() {
    let mut bench = Bench::new("ingest-edge-strays");
    let dir = bench.job("b1", "n1");
    bench.start().await;
    std::fs::write(bench.root.join("stray"), b"at the root").unwrap();
    std::fs::write(bench.root.join("cas/b/stray"), b"above the jobs").unwrap();
    std::fs::create_dir(dir.join("pack")).unwrap();
    let bytes = b"after the directory".to_vec();
    commit(&dir, "next", &bytes, sha(&bytes));
    assert_eq!(
        bench.outcome().await,
        Report::Stored {
            key: "cas/b/b1/n1/next".to_owned(),
            created: true
        }
    );
    assert!(dir.join("pack").is_dir());
    assert!(bench.quiet());
    bench.stop().await;
}

/// Asserts that a symbolic link committed under a name whose digest was declared is refused as not
/// a plain file, removed, and that what it pointed to is not stored.
#[tokio::test]
async fn a_declared_symbolic_link_is_refused_as_not_plain() {
    let mut bench = Bench::new("ingest-edge-link");
    let dir = bench.job("b1", "n1");
    bench.start().await;
    let bytes = b"behind the link".to_vec();
    let target = bench.root.join("target");
    std::fs::write(&target, &bytes).unwrap();
    let declaration = format!("pack.sha256-{}.part", sha(&bytes));
    std::fs::write(dir.join(declaration), b"declaration").unwrap();
    std::os::unix::fs::symlink(&target, dir.join("pack")).unwrap();
    assert_eq!(
        bench.outcome().await,
        Report::Refused {
            key: "cas/b/b1/n1/pack".to_owned(),
            why: Refusal::NotPlain("a symbolic link")
        }
    );
    assert!(dir.join("pack").symlink_metadata().is_err());
    assert!(target.exists());
    assert_eq!(bench.object("cas/b/b1/n1/pack").await, None);
    bench.stop().await;
}

/// Asserts that a declared file the ingest cannot read is reported as left and kept, and that it
/// is stored by a later scan once it can be read.
#[tokio::test]
async fn an_unreadable_file_is_left_and_stored_once_it_can_be_read() {
    if modes_do_not_bind(
        "ingest-edge-probe-read",
        "an unreadable file cannot be made",
    ) {
        return;
    }
    let mut bench = Bench::new("ingest-edge-unreadable");
    let dir = bench.job("b1", "n1");
    bench.start().await;
    let bytes = b"closed at first".to_vec();
    let part = dir.join("pack.part");
    std::fs::write(&part, &bytes).unwrap();
    std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o000)).unwrap();
    let staged = dir.join(format!("pack.sha256-{}.part", sha(&bytes)));
    std::fs::rename(&part, &staged).unwrap();
    std::fs::rename(&staged, dir.join("pack")).unwrap();
    assert_eq!(
        bench.outcome().await,
        Report::Left {
            key: "cas/b/b1/n1/pack".to_owned()
        }
    );
    assert!(dir.join("pack").exists());
    std::fs::set_permissions(dir.join("pack"), std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::create_dir_all(bench.root.join("cas/b/mark")).unwrap();
    assert_eq!(
        bench.outcome().await,
        Report::Stored {
            key: "cas/b/b1/n1/pack".to_owned(),
            created: true
        }
    );
    assert_eq!(bench.object("cas/b/b1/n1/pack").await, Some(bytes));
    bench.stop().await;
}

/// A clock that removes one file the first time it is read after the file is named.
struct Vanishing {
    victim: Mutex<Option<PathBuf>>,
}

impl Clock for Vanishing {
    fn now(&self) -> u64 {
        let mut victim = self.victim.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(path) = victim.take() {
            std::fs::remove_file(path).unwrap();
        }
        0
    }
}

/// Asserts that a committed file that is gone when its put opens it ends without a report, and
/// without a refusal or a stored object: the next object committed is the next thing reported.
#[tokio::test]
async fn a_file_gone_before_its_put_ends_without_a_report() {
    let mut bench = Bench::new("ingest-edge-gone");
    let dir = bench.job("b1", "n1");
    let clock = Arc::new(Vanishing {
        victim: Mutex::new(None),
    });
    bench.spawn_clocked(bench.cache.clone(), clock.clone());
    bench.wait(|report| *report == Report::Watching).await;
    *clock.victim.lock().unwrap() = Some(dir.join("pack"));
    let bytes = b"gone at its put".to_vec();
    commit(&dir, "pack", &bytes, sha(&bytes));
    let next = b"the next one".to_vec();
    commit(&dir, "next", &next, sha(&next));
    assert_eq!(
        bench.outcome().await,
        Report::Stored {
            key: "cas/b/b1/n1/next".to_owned(),
            created: true
        }
    );
    assert!(bench.quiet());
    assert_eq!(bench.object("cas/b/b1/n1/pack").await, None);
    bench.stop().await;
}

/// Asserts that a scan that finds a file whose put is running, in a directory unchanged for a
/// day, neither puts it again nor removes the directory, and that the one put stores it.
#[tokio::test]
async fn a_scan_leaves_a_running_put_and_its_directory_alone() {
    let mut bench = Bench::new("ingest-edge-running");
    let dir = bench.job("b1", "n1");
    bench.start().await;
    let held = bench.rig.disk.hold_writes();
    let bytes = b"held in its put".to_vec();
    commit(&dir, "pack", &bytes, sha(&bytes));
    bench.putting("cas/b/b1/n1/pack").await;
    bench.clock.set(a_day_after(&dir));
    bench.scan("mark").await;
    idle().await;
    assert!(dir.join("pack").exists());
    assert!(bench.quiet());
    held.add_permits(1000);
    assert_eq!(
        bench.outcome().await,
        Report::Stored {
            key: "cas/b/b1/n1/pack".to_owned(),
            created: true
        }
    );
    assert!(bench.quiet());
    bench.stop().await;
}

/// Asserts that a halt lets a running put finish and report before the ingest ends, and that the
/// object is stored and its file removed.
#[tokio::test]
async fn a_halt_lets_a_running_put_finish() {
    let mut bench = Bench::new("ingest-edge-halt");
    let dir = bench.job("b1", "n1");
    bench.start().await;
    let held = bench.rig.disk.hold_writes();
    let bytes = b"finished after the halt".to_vec();
    commit(&dir, "pack", &bytes, sha(&bytes));
    bench.putting("cas/b/b1/n1/pack").await;
    bench.halt();
    idle().await;
    assert!(!bench.ended());
    held.add_permits(1000);
    bench.stop().await;
    assert_eq!(
        bench.outcome().await,
        Report::Stored {
            key: "cas/b/b1/n1/pack".to_owned(),
            created: true
        }
    );
    assert!(!dir.exists());
    assert_eq!(bench.object("cas/b/b1/n1/pack").await, Some(bytes));
}

/// Asserts that an instance that finds the lock of the cache directory gone stops ingesting: an
/// object committed afterwards is neither stored nor removed, nothing is reported, and the halt
/// still ends the ingest.
#[tokio::test]
async fn an_instance_that_loses_the_lock_stops_ingesting() {
    let mut bench = Bench::new("ingest-edge-lost");
    let dir = bench.job("b1", "n1");
    bench.start().await;
    std::fs::remove_file(bench.rig.config.path.join(".lock")).unwrap();
    assert_eq!(bench.cache.tick().unwrap(), Tick::Lost);
    idle().await;
    let bytes = b"after the loss".to_vec();
    commit(&dir, "pack", &bytes, sha(&bytes));
    idle().await;
    assert!(dir.join("pack").exists());
    assert!(bench.quiet());
    assert_eq!(bench.object("cas/b/b1/n1/pack").await, None);
    bench.stop().await;
}

/// Asserts that an instance that never held the lock ends at the halt, having ingested nothing.
#[tokio::test]
async fn an_instance_without_the_lock_ends_at_the_halt() {
    let mut bench = Bench::new("ingest-edge-never");
    let waiting = bench.rig.locked();
    bench.spawn_on(waiting);
    idle().await;
    assert!(bench.quiet());
    bench.stop().await;
    assert!(bench.quiet());
}

/// Asserts that a pack found committed with no declaration, which waits, is stored at once when
/// the `.sha256-<hex>.part` name that declares it appears next to it.
#[tokio::test]
async fn a_declaration_that_comes_after_the_pack_stores_it() {
    let mut bench = Bench::new("ingest-edge-late-declaration");
    let dir = bench.job("b1", "n1");
    bench.start().await;
    let bytes = b"declared late".to_vec();
    std::fs::write(dir.join("pack"), &bytes).unwrap();
    bench.scan("mark").await;
    assert!(dir.join("pack").exists());
    let declaration = format!("pack.sha256-{}.part", sha(&bytes));
    std::fs::write(dir.join(declaration), b"declaration").unwrap();
    assert_eq!(
        bench.outcome().await,
        Report::Stored {
            key: "cas/b/b1/n1/pack".to_owned(),
            created: true
        }
    );
    assert_eq!(bench.object("cas/b/b1/n1/pack").await, Some(bytes));
    bench.stop().await;
}

/// Asserts that a build, a nonce and a file name as long as the file system allows (the longest key
/// there can be) are stored under their whole key.
#[tokio::test]
async fn the_longest_names_are_stored_under_their_whole_key() {
    let mut bench = Bench::new("ingest-edge-long");
    let (build, job_name) = ("b".repeat(255), "n".repeat(255));
    let dir = bench.job(&build, &job_name);
    bench.start().await;
    let name = "p".repeat(178);
    let bytes = b"a long way down".to_vec();
    commit(&dir, &name, &bytes, sha(&bytes));
    let key = format!("cas/b/{build}/{job_name}/{name}");
    assert_eq!(
        bench.outcome().await,
        Report::Stored {
            key: key.clone(),
            created: true
        }
    );
    assert_eq!(bench.object(&key).await, Some(bytes));
    bench.stop().await;
}

/// Asserts that an abandoned directory that cannot be removed is kept and not reported as
/// removed, and that the ingest goes on.
#[tokio::test]
async fn an_abandoned_directory_that_cannot_be_removed_is_kept() {
    if modes_do_not_bind(
        "ingest-edge-probe-dir",
        "a directory that cannot be removed cannot be made",
    ) {
        return;
    }
    let mut bench = Bench::new("ingest-edge-stuck");
    let dir = bench.job("b1", "n1");
    std::fs::write(dir.join("pack.part"), b"never committed").unwrap();
    let build = bench.root.join("cas/b/b1");
    bench.clock.set(a_day_after(&dir));
    std::fs::set_permissions(&build, std::fs::Permissions::from_mode(0o555)).unwrap();
    bench.spawn_on(bench.cache.clone());
    loop {
        let report = bench.next().await;
        assert!(!matches!(report, Report::Abandoned { .. }));
        if report == Report::Watching {
            break;
        }
    }
    assert!(dir.exists());
    std::fs::set_permissions(&build, std::fs::Permissions::from_mode(0o755)).unwrap();
    bench.stop().await;
}

/// Asserts that the ingest makes a scan at the tick with its watcher in place, with nothing
/// changed in the root.
#[tokio::test(start_paused = true)]
async fn a_scan_runs_at_the_tick() {
    let mut bench = Bench::new("ingest-edge-tick");
    bench.start().await;
    bench.wait(|report| *report == Report::Scanned).await;
    bench.stop().await;
}

/// Asserts that a file refused while another file of its directory is being put leaves the
/// directory in place, and that the file being put is then stored and its file removed.
#[tokio::test]
async fn a_refusal_beside_a_running_put_leaves_it_to_finish() {
    let mut bench = Bench::new("ingest-edge-busy-dir");
    let dir = bench.job("b1", "n1");
    bench.start().await;
    let held = bench.rig.disk.hold_writes();
    let bytes = b"put while the other is refused".to_vec();
    commit(&dir, "pack", &bytes, sha(&bytes));
    bench.putting("cas/b/b1/n1/pack").await;
    let declaration = format!("link.sha256-{}.part", sha(&bytes));
    std::fs::write(dir.join(declaration), b"declaration").unwrap();
    std::os::unix::fs::symlink(bench.root.join("nowhere"), dir.join("link")).unwrap();
    assert_eq!(
        bench.outcome().await,
        Report::Refused {
            key: "cas/b/b1/n1/link".to_owned(),
            why: Refusal::NotPlain("a symbolic link")
        }
    );
    assert!(dir.join("pack").exists());
    held.add_permits(1000);
    assert_eq!(
        bench.outcome().await,
        Report::Stored {
            key: "cas/b/b1/n1/pack".to_owned(),
            created: true
        }
    );
    assert!(!dir.join("pack").exists());
    bench.stop().await;
}

/// Asserts that an ingest that tells nobody what it does stores a declared pack that its first
/// scan finds, removes its file, and ends at the halt.
#[tokio::test]
async fn an_ingest_that_reports_to_nobody_still_stores() {
    let bench = Bench::new("ingest-edge-silent");
    let dir = bench.job("b1", "n1");
    let bytes = b"stored in silence".to_vec();
    let declaration = format!("pack.sha256-{}.part", sha(&bytes));
    std::fs::write(dir.join(declaration), b"declaration").unwrap();
    std::fs::write(dir.join("pack"), &bytes).unwrap();
    let halt = Arc::new(Notify::new());
    let ingest = PutIngest::new(bench.cache.clone(), bench.root.clone());
    let task = tokio::spawn(ingest.serve(Arc::clone(&halt)));
    wait_until("the pack is stored and its file removed", || {
        !dir.join("pack").exists()
    })
    .await;
    halt.notify_one();
    task.await.unwrap();
    assert_eq!(bench.object("cas/b/b1/n1/pack").await, Some(bytes));
}

/// Asserts that a file the job put at the path while the put of the file before it ran is not
/// removed with it: the stored file's own inode is what goes, and the new file is taken by the
/// next scan (refused here, since the key holds the first bytes).
#[tokio::test]
async fn a_file_put_at_the_path_during_its_put_is_not_removed_with_it() {
    let mut bench = Bench::new("ingest-edge-replaced");
    let dir = bench.job("b1", "n1");
    bench.start().await;
    let held = bench.rig.disk.hold_writes();
    let first = b"the first pack".to_vec();
    commit(&dir, "pack", &first, sha(&first));
    bench.putting("cas/b/b1/n1/pack").await;
    let second = b"the second pack".to_vec();
    let staged = dir.join("pack.new");
    std::fs::write(&staged, &second).unwrap();
    std::fs::rename(&staged, dir.join("pack")).unwrap();
    held.add_permits(1000);
    assert_eq!(
        bench.outcome().await,
        Report::Stored {
            key: "cas/b/b1/n1/pack".to_owned(),
            created: true
        }
    );
    assert_eq!(std::fs::read(dir.join("pack")).unwrap(), second);
    assert_eq!(bench.object("cas/b/b1/n1/pack").await, Some(first));
    bench.stop().await;
}

/// Asserts that an instance that loses the lock while a put runs, and takes it again, keeps the
/// declaration it saw: the file left by the dropped put is stored by the scan of the new run, not
/// refused for want of a digest.
#[tokio::test]
async fn a_declaration_outlives_the_loss_and_return_of_the_lock() {
    let mut bench = Bench::new("ingest-edge-flip");
    let dir = bench.job("b1", "n1");
    bench.start().await;
    let held = bench.rig.disk.hold_writes();
    let bytes = b"put across a flip".to_vec();
    commit(&dir, "pack", &bytes, sha(&bytes));
    bench.putting("cas/b/b1/n1/pack").await;
    std::fs::remove_file(bench.rig.config.path.join(".lock")).unwrap();
    assert_eq!(bench.cache.tick().unwrap(), Tick::Lost);
    idle().await;
    held.add_permits(1000);
    assert_eq!(bench.cache.tick().unwrap(), Tick::Took);
    bench.wait(|report| *report == Report::Watching).await;
    assert_eq!(
        bench.outcome().await,
        Report::Stored {
            key: "cas/b/b1/n1/pack".to_owned(),
            created: true
        }
    );
    bench.stop().await;
}
