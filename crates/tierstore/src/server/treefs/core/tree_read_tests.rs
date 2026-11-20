//! The bound of one read of a tree's manifests ([`TREE_READ`]): a lookup of `trees/<digest>`
//! whose fills the fill socket does not answer is answered `EIO` by that bound on the core's clock,
//! however many manifests the tree has and however long the client would retry one alone, and a
//! socket that comes back within the bound still serves the read. The clock is the rig's, moved
//! only by the client's pauses, so no test waits for real time.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::super::{Core, Errno, Looked, Settings};
use super::TREE_READ;
use crate::server::local_cache::manifest_path;
use crate::server::treefs::fill_client::FillClient;
use crate::server::treefs::ino;
use crate::server::treefs::table::TreeTable;
use crate::server::treefs::testing::{FakeFill, OWNER, Rig, TREE_IDLE, sample};
use crate::{Clock, Mono};

/// What the production client of an `fs.fetch_timeout` of 60 s (the default) retries a closed
/// socket for: 60 pauses of a second.
const TRIES: u32 = 60;

/// The milliseconds of the bound.
fn bound_ms() -> u64 {
    u64::try_from(TREE_READ.as_millis()).unwrap()
}

/// A core over the rig's cache and clock whose fill client asks the socket at `socket`, which is
/// not there, retrying it as the production client does; each pause moves the rig's clock on by
/// the pause, and `paused` is told how many pauses were taken so far.
fn core_over(rig: &Rig, socket: &Path, paused: impl Fn(u32) + Send + Sync + 'static) -> Arc<Core> {
    let runtime = rig.dir.path().join("run-bound");
    std::fs::create_dir_all(&runtime).unwrap();
    let clock = Arc::clone(&rig.clock);
    let count = Mutex::new(0u32);
    let pause = Arc::new(move |pause: Duration| {
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
        fills,
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

/// Asserts that the manifests below a cached root share the one bound with the socket away: the
/// first takes all of it and the others take none, so the lookup is `EIO` at the bound and not at
/// a bound for each manifest.
#[test]
fn the_manifests_of_one_tree_share_one_bound() {
    let rig = Rig::new("tfs-bound-shared", sample());
    let (root, bytes) = {
        let built = rig.upstream.built.lock().unwrap();
        (built.root.clone(), built.manifests[&built.root].clone())
    };
    assert!(rig.upstream.built.lock().unwrap().manifests.len() >= 3);
    let path = manifest_path(&rig.cache, &root);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    let core = core_over(&rig, &away(&rig), |_| {});
    assert_eq!(core.lookup(ino::TREES, &root), Err(Errno::Io));
    assert_eq!(rig.clock.now(), bound_ms());
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
