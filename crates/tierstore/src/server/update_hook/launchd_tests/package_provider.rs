//! The mount provider of the Mac of `package.rs`: the node's real provider (`Provider` over a tree
//! store and a cache in a scratch directory), served by the release's own listener on the socket
//! the Mac's file names (`packaging/macos/tierstore.toml`), under the Mac's root, with the mount
//! point that file names; asked by the hook's real client (`ProviderSocket`), as `system.rs`
//! builds it for a step, through the root.
//!
//! The trees it places are read as a Mac's check reads them: from the NFS front of the node's
//! `fs` interface, by the hook's real reader over loopback TCP. The front is real too: the NFS
//! front of the release, over a core of the file system that calls the provider's own cache and
//! tree store in the process, as the node's `fs` interface does, so what the hook reads is what
//! the provider placed, found by the front at the source's path. It serves on a free loopback
//! port, not on the port of the Mac's file (`127.0.0.1:12049`): a read the hook asks of the
//! file's front is checked to name that front and then asked of this one.
//!
//! The volume itself is the one thing not read: the hook's process may not read it on a Mac
//! (macOS answers `Operation not permitted` to a process that its privacy grant of network
//! volumes does not cover), and a read of it here is answered so.
//!
//! The check's objects are in the store as a build service stores them (the pack, both lists, the
//! one sidecar): the node's own `CheckUpstream` is proved by the node test of `serve_tests.rs`.

use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::runtime::Runtime;
use tokio::sync::Notify;

use crate::server::provider::check::{WITH_SIDECAR, WITHOUT_SIDECAR, pack, pack_key};
use crate::server::provider::serve;
use crate::server::provider::testing::Rig;
use crate::server::treefs::NfsFront;
use crate::server::update_hook::machine::{ProviderSocket, ProviderTimes};
use crate::server::update_hook::{MountProvider, ProviderFault, ReadFault, TreeAt, TreeNode};

/// The provider's socket in the Mac's file.
pub(super) const SOCKET: &str = "/usr/local/tierstore/run/provider.sock";

/// The NFS front of the Mac's file (`fs.nfs.listen`).
pub(super) const FILE_FRONT: &str = "127.0.0.1:12049";

/// What macOS answers the hook's process for a read of the volume.
pub(super) const NOT_PERMITTED: &str =
    "the directory could not be listed: Operation not permitted (os error 1)";

/// How the check's objects and placements stand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Arranged {
    /// As a build service stores the objects, and the placements as the provider makes them.
    Built,
    /// The list of the tree without a sidecar left out of the store, so that the provider
    /// refuses that tree (`no tree list at cas/l/<id>`).
    NoPlainList,
    /// The placements let go of in the tree store just before the hook reads them, so that the
    /// front finds no tree at the sources' paths.
    GoneBeforeRead,
}

/// The node's provider, its socket under the Mac's root, its NFS front, and the hook's client.
pub(super) struct PackageProvider {
    /// The runtime the provider, the front and the cache run on, kept while they serve.
    _runtime: Runtime,
    rig: Rig,
    halt: Arc<Notify>,
    front: NfsFront,
    arranged: Arranged,
    client: ProviderSocket,
}

impl PackageProvider {
    /// The provider of the Mac under `root`, whose mount point is `mount`, arranged as
    /// `arranged` says.
    pub(super) fn new(name: &str, root: &Path, mount: &str, arranged: Arranged) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let mount_path = PathBuf::from(mount);
        let (rig, front) = runtime.block_on(async {
            let rig = Rig::mounted_under(name, &mount_path).await;
            let up = &rig.bench.rig;
            up.put(&pack_key(), &pack()).await;
            up.put(&WITH_SIDECAR.key(), WITH_SIDECAR.list).await;
            up.put(&WITH_SIDECAR.sidecar_key(), WITH_SIDECAR.sidecar.unwrap())
                .await;
            if arranged != Arranged::NoPlainList {
                up.put(&WITHOUT_SIDECAR.key(), WITHOUT_SIDECAR.list).await;
            }
            let trees = (&rig.bench.rig.cache, &rig.bench.store);
            let front = NfsFront::over(name, trees, rig.bench.rig.config.path.clone()).await;
            (rig, front)
        });
        let socket = root.join(SOCKET.trim_start_matches('/'));
        fs::create_dir_all(socket.parent().expect("the socket's directory")).expect("run");
        let halt = Arc::new(Notify::new());
        let (provider, cache) = (rig.provider.clone(), rig.bench.rig.cache.clone());
        let (path, notify) = (socket.clone(), Arc::clone(&halt));
        runtime.spawn(async move { serve(provider, &cache, &path, notify).await });
        runtime.block_on(crate::server::local_cache::testing::until(
            "the socket is bound",
            || socket.exists(),
        ));
        Self {
            _runtime: runtime,
            rig,
            halt,
            front,
            arranged,
            client: ProviderSocket::new(root.to_path_buf(), ProviderTimes::default()),
        }
    }

    /// Whether the store holds a placement.
    pub(super) fn placements(&self) -> usize {
        self.rig.bench.store.sizes().0
    }

    /// Where the front this provider's trees are read from serves.
    pub(super) fn front(&self) -> SocketAddr {
        self.front.address
    }

    /// Lets go of every placement of the check's sources in the tree store.
    fn let_go(&self) {
        let store = &self.rig.bench.store;
        for index in 0..2 {
            let place = format!(
                "mount-provider/{}/{}/{index}",
                hex::encode(crate::server::provider::check::AGENT),
                hex::encode(crate::server::provider::check::RUN)
            );
            if let Some(located) = store.locate_fs(&place) {
                store.release_now(&located.placement).expect("let go");
            }
        }
    }
}

impl Drop for PackageProvider {
    fn drop(&mut self) {
        self.halt.notify_one();
    }
}

impl MountProvider for PackageProvider {
    fn prepare(&self, socket: &Path, index: u32, reference: &str) -> Result<(), ProviderFault> {
        self.client.prepare(socket, index, reference)
    }

    fn mount(&self, socket: &Path, index: u32) -> Result<PathBuf, ProviderFault> {
        self.client.mount(socket, index)
    }

    fn release(&self, socket: &Path, index: u32) -> Result<(), ProviderFault> {
        self.client.release(socket, index)
    }

    fn read_tree(&self, at: TreeAt<'_>) -> Result<BTreeMap<String, TreeNode>, ReadFault> {
        match at {
            TreeAt::Mount(_) => Err(ReadFault::Failed(NOT_PERMITTED.to_owned())),
            TreeAt::Front { front, below } => {
                assert_eq!(front.to_string(), FILE_FRONT, "the front of the Mac's file");
                if self.arranged == Arranged::GoneBeforeRead {
                    self.let_go();
                }
                let front = self.front.address;
                self.client.read_tree(TreeAt::Front { front, below })
            }
        }
    }
}
