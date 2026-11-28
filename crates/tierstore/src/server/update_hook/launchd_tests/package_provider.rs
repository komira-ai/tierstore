//! The mount provider of the Mac of `package.rs`: the node's real provider (`Provider` over a tree
//! store and a cache in a scratch directory), served by the release's own listener on the socket
//! the Mac's file names (`packaging/macos/tierstore.toml`), under the Mac's root, with the mount
//! point that file names; asked by the hook's real client (`ProviderSocket`), as `system.rs`
//! builds it for a step, through the root.
//!
//! What stands in is the volume alone: tierstore's file system, which a Mac serves over NFS from
//! the job's own process, cannot be mounted here. So once the provider answers a `Mount` with a
//! directory, the stand-in lays out at that directory, under the root, the tree the provider
//! placed there: the tree of the placement the store has at the source's path, read from the image
//! the provider wrote (the version 2 list it placed), every file's bytes read through the node's
//! cache from the pack the list names, as the volume serves them; a `Release` takes it away. Then
//! the hook's real read walks it. Nothing of the check's own trees is put there: what the hook
//! reads is what the provider made of the objects.
//!
//! The check's objects are in the store as a build service stores them (the pack, both lists, the
//! one sidecar): the node's own `CheckUpstream` is proved by the node test of `serve_tests.rs`.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::runtime::Runtime;
use tokio::sync::Notify;

use crate::server::local_cache::trees::image_path;
use crate::server::provider::check::{WITH_SIDECAR, WITHOUT_SIDECAR, pack, pack_key};
use crate::server::provider::serve;
use crate::server::provider::testing::Rig;
use crate::server::update_hook::machine::{ProviderSocket, ProviderTimes};
use crate::server::update_hook::{MountProvider, ProviderFault, ReadFault, TreeNode};
use crate::{Backend, ByteRange, RecordChild, TreeDigest, TreeLimits, TreeListV2, TreeRecord};

/// The provider's socket in the Mac's file.
pub(super) const SOCKET: &str = "/usr/local/tierstore/run/provider.sock";

/// The node's provider, its socket under the Mac's root, and the hook's client of it.
pub(super) struct PackageProvider {
    runtime: Runtime,
    rig: Rig,
    halt: Arc<Notify>,
    root: PathBuf,
    mount: PathBuf,
    client: ProviderSocket,
}

impl PackageProvider {
    /// The provider of the Mac under `root`, whose mount point is `mount`; with `refuse_plain`,
    /// the list of the tree without a sidecar is left out of the store, so the provider refuses
    /// that tree (`no tree list at cas/l/<id>`).
    pub(super) fn new(name: &str, root: &Path, mount: &str, refuse_plain: bool) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let mount_path = PathBuf::from(mount);
        let rig = runtime.block_on(async {
            let rig = Rig::mounted_under(name, &mount_path).await;
            let up = &rig.bench.rig;
            up.put(&pack_key(), &pack()).await;
            up.put(&WITH_SIDECAR.key(), WITH_SIDECAR.list).await;
            up.put(&WITH_SIDECAR.sidecar_key(), WITH_SIDECAR.sidecar.unwrap())
                .await;
            if !refuse_plain {
                up.put(&WITHOUT_SIDECAR.key(), WITHOUT_SIDECAR.list).await;
            }
            rig
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
            runtime,
            rig,
            halt,
            root: root.to_path_buf(),
            mount: mount_path,
            client: ProviderSocket::new(root.to_path_buf(), ProviderTimes::default()),
        }
    }

    /// Whether the store holds a placement.
    pub(super) fn placements(&self) -> usize {
        self.rig.bench.store.sizes().0
    }

    /// `dir` (absolute on the Mac) under the root.
    fn at(&self, dir: &Path) -> PathBuf {
        self.root.join(dir.strip_prefix("/").unwrap_or(dir))
    }

    /// Lays out at `dir` the tree the store has placed there, as the volume serves it.
    fn serve_volume(&self, dir: &Path) {
        let place = dir.strip_prefix(&self.mount).expect("below the mount");
        let located = self
            .rig
            .bench
            .store
            .locate_fs(&place.to_string_lossy())
            .expect("a placement at the source's path");
        let image = fs::read(image_path(&self.rig.bench.rig.config.path, &located.tree))
            .expect("the image the provider wrote");
        let list = TreeListV2::open(&image).expect("a version 2 list");
        let built = list
            .build(&TreeLimits::default(), |_| {})
            .expect("the tree");
        let records: BTreeMap<[u8; 32], TreeRecord> = built
            .records
            .into_iter()
            .map(|(digest, record)| (*digest.as_bytes(), record))
            .collect();
        let sources: BTreeMap<String, (String, u64)> = built
            .files
            .into_iter()
            .filter_map(|file| {
                let source = file.source?;
                Some((file.path, (source.key.to_string(), source.offset)))
            })
            .collect();
        let at = self.at(dir);
        fs::create_dir_all(&at).expect("the source's directory");
        self.lay_out(&records, &sources, &built.root, &at, "");
    }

    /// Lays out the directory whose record is `digest` at `at`, its paths below `prefix`.
    fn lay_out(
        &self,
        records: &BTreeMap<[u8; 32], TreeRecord>,
        sources: &BTreeMap<String, (String, u64)>,
        digest: &TreeDigest,
        at: &Path,
        prefix: &str,
    ) {
        for child in records[digest.as_bytes()].children() {
            let path = format!("{prefix}{}", child.name());
            let to = at.join(child.name());
            match child {
                RecordChild::Dir { digest, .. } => {
                    fs::create_dir(&to).expect("a directory");
                    self.lay_out(records, sources, digest, &to, &format!("{path}/"));
                }
                RecordChild::Symlink { target, .. } => {
                    std::os::unix::fs::symlink(target, &to).expect("a link");
                }
                RecordChild::File {
                    mode, size, sha256, ..
                } => {
                    let bytes = match sources.get(&path) {
                        Some((key, offset)) => {
                            let cache = self.rig.bench.rig.cache.clone();
                            let range = Some(ByteRange::new(*offset, *size));
                            let key = key.clone();
                            self.runtime
                                .block_on(async move { cache.get(&key, range).await })
                                .expect("the file's bytes through the cache")
                                .to_vec()
                        }
                        None => Vec::new(),
                    };
                    assert_eq!(TreeDigest::of(&bytes), *sha256, "{path}");
                    fs::write(&to, &bytes).expect("a file");
                    fs::set_permissions(&to, fs::Permissions::from_mode(u32::from(mode.bits())))
                        .expect("its mode");
                }
            }
        }
    }

    /// The directory the source `index` is placed at, as the provider names it.
    fn dir_of(&self, index: u32) -> PathBuf {
        let place = format!(
            "mount-provider/{}/{}/{index}",
            hex::encode(crate::server::provider::check::AGENT),
            hex::encode(crate::server::provider::check::RUN)
        );
        self.mount.join(place)
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
        let dir = self.client.mount(socket, index)?;
        // The volume shows a placed tree only once: a second `Mount` of a source finds it there.
        if !self.at(&dir).exists() {
            self.serve_volume(&dir);
        }
        Ok(dir)
    }

    fn release(&self, socket: &Path, index: u32) -> Result<(), ProviderFault> {
        self.client.release(socket, index)?;
        // The volume no longer shows the source's tree.
        let at = self.at(&self.dir_of(index));
        if at.exists() {
            fs::remove_dir_all(&at).expect("the released tree");
        }
        Ok(())
    }

    fn read_tree(&self, dir: &Path) -> Result<BTreeMap<String, TreeNode>, ReadFault> {
        self.client.read_tree(dir)
    }
}
