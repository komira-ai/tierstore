//! The Linux build node on the file the build farm runs (`tests/live_configs/linux-build-node.toml`,
//! moved onto a scratch directory by `live_configs.rs`): started as the process starts on it, over
//! a real FUSE mount where one can be made, and asked for trees on the mount provider's socket as a
//! job agent asks. The file's own settings are all that is served from: the test changes nothing in
//! it but the mount's `allow_other`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use tierstore_testkit::dir::TestDir;
use tierstore_trees_api::provider::v1::{
    MountRequest, MountResponse, PrepareRequest, PrepareResponse, ReleaseRequest, ReleaseResponse,
};
use tokio::sync::oneshot;

use super::{Detached, Service, Started, platform, stop};
use crate::server::Drained;
use crate::server::cache_cli::{Imaging, OnSystem, serve_cache_on};
use crate::server::cache_cli_tests::{QUICK, probe};
use crate::server::cli::Addresses;
use crate::server::config::{PerRun, settings_from_text};
use crate::server::config_cache::CacheSettings;
use crate::server::handoff::testing::within;
use crate::server::live_configs::{HOST, LINUX_KEYS, linux_on, write_keys};
use crate::server::local_cache::Disk;
use crate::server::local_cache::pack_testing::Pack;
use crate::server::local_cache::testing::FakeDisk;
use crate::server::provider::testing::{Client, READ_ONLY, Rig, source, tree_objects};
use crate::server::treefs::launch::Platform;
use crate::server::treefs::run::Kernel;
use crate::server::treefs::testing::{FakeManager, with_fuse};
use crate::{Backend, MemoryBackend};

/// The bytes of the file system the node's cache and put directory are on: more than the file's
/// 185 GiB budget and the put directory's 20 GiB of free space it keeps.
const DISK: u64 = 1 << 40;

/// The Linux node's settings from the farm's file moved onto `dir`, loaded as the process loads it
/// on a host whose accounts are the test's own.
fn settings_on(dir: &Path) -> CacheSettings {
    write_keys(dir, &LINUX_KEYS);
    // A scoped key file is JSON, not the two lines of the others.
    let scoped = dir.join("etc/kbf-cold.key");
    std::fs::write(
        &scoped,
        "{\"access_key_id\": \"AKIDSCOPED\", \"secret_access_key\": \"live-scoped-secret-0000\", \"read_prefixes\": [\"cas/\"], \"write_prefixes\": [], \"delete_prefixes\": []}\n",
    )
    .unwrap();
    // The service manager makes the state directory of the unit before the process starts.
    std::fs::create_dir_all(dir.join("state")).unwrap();
    let per_run = PerRun {
        host: HOST,
        ..QUICK
    };
    let mut settings = settings_from_text(&linux_on(dir), per_run)
        .unwrap()
        .into_cache()
        .unwrap();
    // A mount the test makes is its user's alone: `allow_other` needs the system's consent.
    if let Some(fs) = &mut settings.fs {
        fs.allow_other = false;
    }
    settings
}

/// The kernel's files as a node that mounts afresh sees them: a table of mounts that shows nothing
/// and no connections.
fn kernel_showing_nothing(dir: &Path) -> Kernel {
    let table = dir.join("mountinfo");
    std::fs::write(&table, "").unwrap();
    let connections = dir.join("connections");
    std::fs::create_dir_all(&connections).unwrap();
    Kernel {
        mountinfo: table,
        connections,
        device: |_| Ok(()),
    }
}

/// Starts the node of `settings` over `upstream` and a disk that has room, on `platform`.
fn start_node(settings: CacheSettings, upstream: Arc<dyn Backend>, platform: Platform) -> Started {
    let (stop, stopped) = oneshot::channel::<()>();
    let (told, bound) = oneshot::channel::<Addresses>();
    let disk = FakeDisk::new();
    disk.set_space(DISK, DISK);
    let disk: Arc<dyn Disk> = Arc::new(disk);
    let system = OnSystem {
        service: Some(Service::on(settings.fs.clone(), platform)),
        nfs: None,
    };
    let ended = tokio::spawn(async move {
        let stopped = async move {
            let _ = stopped.await;
        };
        let told = move |at| drop(told.send(at));
        let imaging = Imaging::default();
        serve_cache_on(&settings, (upstream, disk), system, stopped, told, imaging).await
    });
    Started { bound, stop, ended }
}

/// Puts `pack` in `upstream`, and the tree of its files as the build service writes it: with its L2
/// sidecar beside its list when `sidecar`, and with the list alone otherwise; the list's key.
async fn put_tree(upstream: &MemoryBackend, pack: &Pack, sidecar: bool) -> String {
    upstream
        .put(&pack.key, Bytes::copy_from_slice(pack.bytes()))
        .await
        .unwrap();
    let [(key, list), (sidecar_key, l2)] = tree_objects(pack);
    upstream.put(&key, Bytes::from(list)).await.unwrap();
    if sidecar {
        upstream.put(&sidecar_key, Bytes::from(l2)).await.unwrap();
    }
    key
}

/// Prepares and mounts the source `run` of the list `key` on the provider's socket at `socket`
/// until a Mount answers it, as a job agent asks again when it finds no socket or the call fails:
/// the directory the Mount names.
async fn mount(socket: &Path, run: &str, key: &str) -> PathBuf {
    loop {
        if let Some(path) = mount_once(socket, run, key).await {
            return PathBuf::from(path);
        }
        tokio::task::yield_now().await;
    }
}

/// One Prepare and Mount of `run` on a connection of its own: the directory, or none when either
/// is not answered or not done.
async fn mount_once(socket: &Path, run: &str, key: &str) -> Option<String> {
    let mut client = Client::try_connect(socket).await?;
    let prepare = PrepareRequest {
        source: source(run, 0),
        r#ref: key.to_owned(),
        mode: READ_ONLY,
        host: String::new(),
    };
    let _: PrepareResponse = client.try_call("Prepare", &prepare).await?.ok()?;
    let mount = MountRequest {
        source: source(run, 0),
    };
    let mounted: MountResponse = client.try_call("Mount", &mount).await?.ok()?;
    Some(mounted.path)
}

/// Releases the source `run` on the provider's socket at `socket`, which is listening.
async fn release(socket: &Path, run: &str) {
    let mut client = Client::try_connect(socket).await.unwrap();
    let request = ReleaseRequest {
        source: source(run, 0),
    };
    let _: ReleaseResponse = client.try_call("Release", &request).await.unwrap().unwrap();
}

/// What a directory the provider answered holds, read through the mount.
struct Seen {
    /// The names in its root, sorted.
    names: Vec<String>,
    /// The bytes of `bin/tool`.
    tool: Vec<u8>,
    /// The bytes of `lib/deep/data`.
    deep: Vec<u8>,
    /// The target of `link`.
    link: PathBuf,
}

/// Reads the directory `root` of a tree through the mount. The server of the mount is in this
/// process, so this runs on a thread of its own, off the runtime.
fn look(root: &Path) -> Seen {
    let mut names: Vec<String> = std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    Seen {
        names,
        tool: std::fs::read(root.join("bin/tool")).unwrap(),
        deep: std::fs::read(root.join("lib/deep/data")).unwrap(),
        link: std::fs::read_link(root.join("link")).unwrap(),
    }
}

/// Asserts that the node started on the farm's Linux file serves, over a real FUSE mount, the trees
/// its mount provider is asked for: one whose upstream holds its list and its L2 sidecar, and one
/// whose upstream holds its list alone (the build service writes such trees). The file's `fs` and
/// `mount-provider` interfaces share the mount, which is made afresh; each source's Mount answers
/// its directory under the mount, which lists the tree's names, holds the bytes of its files and a
/// link to its target; `/readyz` says `ready read-write`; and a Release of each source and a stop
/// end the node with no request abandoned and its socket gone. Catches a release that loads the
/// farm's Linux file and serves no tree from it: a refused tree without a sidecar, or a mount
/// provider whose mount nothing serves.
#[test]
fn the_linux_build_nodes_file_serves_its_mount_provider_over_fuse() {
    let (dir, _) = TestDir::for_socket(
        &[&std::env::temp_dir()],
        "tsfs-live-linux",
        "state/handoff.sock",
    );
    let settings = settings_on(dir.path());
    let socket = dir.path().join("state/provider.sock");
    let mountpoint = settings.fs.as_ref().unwrap().mountpoint.clone();
    assert_eq!(mountpoint, dir.path().join("mnt"));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    with_fuse(|| {
        runtime.block_on(async {
            let upstream = MemoryBackend::new();
            let one = Pack::new("cas/pack/one", &[1000, 2000, 3000]);
            let two = Pack::new("cas/pack/two", &[1500, 2500, 3500]);
            let with = put_tree(&upstream, &one, true).await;
            let without = put_tree(&upstream, &two, false).await;
            let manager = FakeManager::start(&dir.path().join("manager.sock"));
            let on = platform(&manager, None, kernel_showing_nothing(dir.path()));
            let _detached = Detached(mountpoint.clone());
            let mut node = start_node(settings, Arc::new(upstream), on);
            let bound = within("the node serves", &mut node.bound).await.unwrap();
            let (status, text) = probe(bound.s3, "/readyz").await;
            assert_eq!((status, text.as_str()), (200, "ready read-write"));

            for (run, key, pack, which) in [
                ("js.1", &with, &one, "the tree with its sidecar"),
                ("js.2", &without, &two, "the tree without its sidecar"),
            ] {
                let mounted = mount(&socket, run, key);
                let answered = within("the provider mounts the source", mounted).await;
                assert_eq!(answered, mountpoint.join(Rig::place_of(run, 0)), "{which}");
                let root = answered.clone();
                let seen = tokio::task::spawn_blocking(move || look(&root))
                    .await
                    .unwrap();
                let names = ["bin", "empty", "lib", "link", "zero"];
                assert_eq!(seen.names, names, "{which}");
                assert_eq!(seen.tool, pack.content(0), "{which}");
                assert_eq!(seen.deep, pack.content(2), "{which}");
                assert_eq!(seen.link, Path::new("bin/tool"), "{which}");
            }

            release(&socket, "js.1").await;
            release(&socket, "js.2").await;
            assert_eq!(stop(node).await.unwrap(), Drained { abandoned: 0 });
            assert!(!socket.exists());
            drop(manager);
        });
    });
}
