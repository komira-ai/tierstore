//! An NFS front over a core of a node's own cache and tree store, on a free loopback port: the
//! front a Mac's update hook reads the check's trees from, for the hook's tests
//! (`update_hook/launchd_tests/package_provider.rs`).

use std::net::SocketAddr;
use std::path::PathBuf;

use tierstore_testkit::dir::TestDir;

use super::config::NfsSettings;
use super::fixtures::{EPOCH, endless};
use super::fs::TreeNfs;
use super::wire_client::serve;
use crate::server::local_cache::LocalCache;
use crate::server::local_cache::trees::TreeStore;
use crate::server::treefs::live_testing::core_over;

/// A front that serves until it is dropped.
pub(in crate::server) struct NfsFront {
    /// Where it serves.
    pub address: SocketAddr,
    task: tokio::task::JoinHandle<()>,
    /// Last: the runtime directory of its core goes after it.
    _dir: TestDir,
}

impl NfsFront {
    /// The front of a core over `cache` and `trees`, the cache's directory at `cache_path`, the
    /// core's runtime directory in a scratch directory named after `name`. Call it on a runtime of
    /// more than one thread.
    pub(in crate::server) async fn over(
        name: &str,
        (cache, trees): (&LocalCache, &TreeStore),
        cache_path: PathBuf,
    ) -> NfsFront {
        let dir = TestDir::under(std::env::temp_dir(), &format!("{name}-front"));
        let (runtime, mountpoint) = (dir.path().join("run"), dir.path().join("mnt"));
        for path in [&runtime, &mountpoint] {
            std::fs::create_dir_all(path).unwrap();
        }
        let core = core_over((cache, trees), cache_path, (&runtime, mountpoint));
        let fs = TreeNfs::new(core, &NfsSettings::default(), endless(), EPOCH);
        let (address, task) = serve(fs).await;
        NfsFront {
            address,
            task,
            _dir: dir,
        }
    }
}

impl Drop for NfsFront {
    fn drop(&mut self) {
        self.task.abort();
    }
}
