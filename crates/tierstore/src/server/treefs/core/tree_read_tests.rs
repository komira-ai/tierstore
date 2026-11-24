//! The bound of one read of a tree's manifests ([`TREE_READ`]): a lookup of `trees/<digest>`
//! whose fills the fill socket does not answer is answered `EIO` by that bound on the core's clock,
//! however many manifests the tree has and however long the client would retry one alone, and a
//! socket that comes back within the bound still serves the read. The clock is the rig's, moved
//! only by the client's pauses and the slow socket's answers, so no test waits for real time.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::super::{Core, Errno, Looked, Settings};
use super::TREE_READ;
use crate::Clock;
use crate::server::local_cache::manifest_path;
use crate::server::treefs::fill_client::FillClient;
use crate::server::treefs::ino;
use crate::server::treefs::table::TreeTable;
use crate::server::treefs::testing::{Built, FakeFill, OWNER, Rig, TREE_IDLE, dir, file, sample};

/// What the production client of an `fs.fetch_timeout` of 60 s (the default) retries a closed
/// socket for: 60 pauses of a second.
const TRIES: u32 = 60;

/// The milliseconds of the bound.
fn bound_ms() -> u64 {
    u64::try_from(TREE_READ.as_millis()).unwrap()
}

/// The name of the client's thread that sends the reports (`fetched` after a read), which pauses
/// with the same function as the requests of a read.
const REPORTER: &str = "fs-reports";

/// A core over the rig's cache and clock whose fill client asks the socket at `socket`, retrying
/// it as the production client does. Each pause of a request moves the rig's clock on by the
/// pause, and `paused` is told how many such pauses were taken so far; the pauses of the thread
/// that sends the reports move nothing, so the clock is the read's alone.
fn core_over(rig: &Rig, socket: &Path, paused: impl Fn(u32) + Send + Sync + 'static) -> Arc<Core> {
    let runtime = rig.dir.path().join("run-bound");
    std::fs::create_dir_all(&runtime).unwrap();
    let clock = Arc::clone(&rig.clock);
    let count = Mutex::new(0u32);
    let pause = Arc::new(move |pause: Duration| {
        if std::thread::current().name() == Some(REPORTER) {
            return;
        }
        clock.advance(pause);
        let mut count = count.lock().unwrap();
        *count += 1;
        paused(*count);
    });
    let fills = FillClient::pausing(socket.to_owned(), TRIES, Duration::from_secs(60), pause);
    Arc::new(Core::new(
        Settings {
            owner: OWNER,
            cache_path: rig.cache.clone(),
            mountpoint: rig.mountpoint.clone(),
            mount_id: "mount-bound".to_owned(),
            generation: 1,
            count_lookups: true,
            tree_idle: TREE_IDLE,
            clock: Arc::clone(&rig.clock) as Arc<dyn Clock>,
        },
        Arc::new(fills),
        TreeTable::create(&runtime).unwrap(),
        &runtime,
    ))
}

/// A socket path in the rig's directory that nothing listens on.
fn away(rig: &Rig) -> PathBuf {
    rig.dir.path().join("away.sock")
}

/// Asserts that a lookup of a tree none of whose manifests the cache holds, with the fill socket
/// away, is `EIO` exactly when the bound has passed on the core's clock: not after the 60 s the
/// client would retry one manifest for, which is the job agent's whole bound for the lookup. No
/// tree is numbered or attached.
#[test]
fn a_lookup_with_the_fill_socket_away_is_answered_by_the_bound() {
    let rig = Rig::new("tfs-bound-away", sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let core = core_over(&rig, &away(&rig), |_| {});
    assert_eq!(core.lookup(ino::TREES, &root), Err(Errno::Io));
    assert_eq!(rig.clock.now(), bound_ms());
    assert!(bound_ms() < 60_000);
    assert!(core.implicit().is_empty());
}

/// Serves fills at `socket` slowly on the rig's clock: each `fill <sha>` puts the manifest in the
/// cache, moves the clock on by `step`, and answers `ok`; any other request is refused as unknown.
/// The fill lines are recorded.
fn slow_fills(rig: &Rig, socket: &Path, step: Duration) -> Arc<Mutex<Vec<String>>> {
    let listener = UnixListener::bind(socket).unwrap();
    let lines = Arc::new(Mutex::new(Vec::new()));
    let (seen, clock, upstream) = (
        Arc::clone(&lines),
        Arc::clone(&rig.clock),
        Arc::clone(&rig.upstream),
    );
    let cache = rig.cache.clone();
    let _ = std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (seen, clock) = (Arc::clone(&seen), Arc::clone(&clock));
            let (upstream, cache) = (Arc::clone(&upstream), cache.clone());
            let _ = std::thread::spawn(move || {
                let mut reader = BufReader::new(&stream);
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    let text = line.trim_end_matches('\n').to_owned();
                    line.clear();
                    let Some(sha) = text.strip_prefix("fill ") else {
                        (&stream).write_all(b"refused unknown request\n").unwrap();
                        continue;
                    };
                    seen.lock().unwrap().push(text.clone());
                    let bytes = upstream.built.lock().unwrap().manifests[sha].clone();
                    let path = manifest_path(&cache, sha);
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(&path, bytes).unwrap();
                    clock.advance(step);
                    (&stream).write_all(b"ok\n").unwrap();
                }
            });
        }
    });
    lines
}

/// Asserts that the fills of one tree share the one bound: in a tree three levels deep, read a
/// level at a time with one manifest at each, and each fill answered 20 s later on the clock, the
/// fills of the root and of the level below it are answered within 30 s of the start and the third
/// is not asked, so the lookup is `EIO`, where a bound for each fill would have read the whole
/// tree in 60 s.
#[test]
fn the_manifests_of_one_tree_share_one_bound() {
    let deep = Built::of(vec![("a", dir(vec![("b", dir(vec![("c", file(b"x"))]))]))]);
    let rig = Rig::new("tfs-bound-shared", deep);
    let root = rig.upstream.built.lock().unwrap().root.clone();
    assert_eq!(rig.upstream.built.lock().unwrap().manifests.len(), 3);
    let socket = rig.dir.path().join("slow.sock");
    let lines = slow_fills(&rig, &socket, Duration::from_secs(20));
    let core = core_over(&rig, &socket, |_| {});
    assert_eq!(core.lookup(ino::TREES, &root), Err(Errno::Io));
    let lines = lines.lock().unwrap().clone();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], format!("fill {root}"));
    assert_eq!(rig.clock.now(), 40_000);
    assert!(core.implicit().is_empty());
}

/// Asserts that a fill socket that comes back within the bound (the node's tierstore is being
/// updated) serves the read: the lookup resolves the tree, after the pauses taken while it was
/// away, and the bound is not spent.
#[test]
fn a_socket_back_within_the_bound_serves_the_lookup() {
    let rig = Rig::new("tfs-bound-back", sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let socket = away(&rig);
    let upstream = Arc::clone(&rig.upstream);
    let started: Arc<Mutex<Option<FakeFill>>> = Arc::new(Mutex::new(None));
    let serving = Arc::clone(&started);
    let at = socket.clone();
    let core = core_over(&rig, &socket, move |paused| {
        if paused == 10 {
            *serving.lock().unwrap() = Some(FakeFill::start(&at, Arc::clone(&upstream)));
        }
    });
    let Looked::Found(entry) = core.lookup(ino::TREES, &root).unwrap() else {
        panic!("not found");
    };
    assert_eq!(entry.attr.ino, 1 << 24);
    assert_eq!(rig.clock.now(), 10_000);
    assert_eq!(core.implicit(), vec![1]);
    drop(started.lock().unwrap().take());
}
