//! What the tests of the provider share: the tree store's bench (a cache over an upstream with a
//! pack of three blobs, and a tree store over it), a tree of the pack's files put in the upstream
//! as a CAS service writes one (its version 2 list at `cas/l/<id>` and its L2 sidecar at
//! `cas/l2/<id>`), the provider over them, and a gRPC client of the provider's socket.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::client::conn::http2::{self, SendRequest};
use hyper_util::rt::{TokioExecutor, TokioIo};
use prost::Message;
use tierstore_list::{PackCells, PackInfo, sidecar_from_v2, sidecar_key};
use tierstore_trees_api::provider::v1::{MountMode, Source};
use tokio::net::UnixStream;
use tonic::Status;

use super::sources::Settings;
use super::{Provider, ProviderError};
use crate::server::local_cache::pack_testing::Pack;
use crate::server::local_cache::testing::until;
use crate::server::local_cache::trees::TreesConfig;
use crate::server::local_cache::trees::testing::Bench;
use crate::{
    BlobSource, ListEntry, ListKind, Mode, TreeDigest, TreeList, tree_list_digest, tree_list_key,
};

/// The agent of the tests.
pub(super) const AGENT: &str = "agent-1";

/// The mount of the tests' provider.
pub(super) const MOUNT: &str = "/run/tierstore/fs";

/// Bounds no test reaches.
pub(super) fn settings() -> Settings {
    let long = Duration::from_secs(3600);
    Settings {
        mountpoint: PathBuf::from(MOUNT),
        served: true,
        list_timeout: long,
        fill_timeout: long,
        mount_wait: long,
        ttl: long,
        max_list_bytes: crate::MAX_TREE_LIST_BYTES,
        max_queued_bytes: super::sources::PREFETCH_QUEUE_BYTES,
    }
}

/// The source `index` of the run `run` of [`AGENT`].
pub(in crate::server) fn source(run: &str, index: u32) -> Option<Source> {
    Some(Source {
        agent: AGENT.to_owned(),
        run: run.to_owned(),
        index,
    })
}

/// The mode a job's layer asks for.
pub(in crate::server) const READ_ONLY: i32 = MountMode::ReadOnly as i32;

/// The file `path` of the list, whose bytes are the blob `blob` of `pack`.
pub(super) fn member(pack: &Pack, path: &str, blob: usize, mode: u16) -> ListEntry {
    let (sha256, offset, size) = &pack.blobs[blob];
    ListEntry {
        path: path.to_owned(),
        kind: ListKind::File {
            mode: Mode::new(mode).expect("a mode"),
            size: *size,
            source: Some(BlobSource {
                key: Arc::from(pack.key.as_str()),
                offset: *offset,
            }),
            sha256: Some(TreeDigest::from_hex(sha256).expect("a digest")),
        },
    }
}

/// The tree of the pack's three blobs, an empty directory, a link and an empty file.
pub(in crate::server) fn list_of(pack: &Pack) -> TreeList {
    TreeList {
        entries: vec![
            member(pack, "bin/other", 1, 0o644),
            member(pack, "bin/tool", 0, 0o755),
            ListEntry {
                path: "empty".to_owned(),
                kind: ListKind::Dir {
                    mode: Mode::new(0o1777).expect("a mode"),
                },
            },
            member(pack, "lib/deep/data", 2, 0o644),
            ListEntry {
                path: "link".to_owned(),
                kind: ListKind::Symlink {
                    target: "bin/tool".to_owned(),
                },
            },
            ListEntry {
                path: "zero".to_owned(),
                kind: ListKind::File {
                    mode: Mode::new(0o644).expect("a mode"),
                    size: 0,
                    source: None,
                    sha256: None,
                },
            },
        ],
    }
}

/// A provider over the tree store's bench, with the pack's tree in the upstream: the version 2 list
/// of [`list_of`] and its L2 sidecar.
pub(in crate::server) struct Rig {
    pub bench: Bench,
    /// The key of the version 2 list of [`list_of`].
    pub key: String,
    pub provider: Provider,
}

impl Rig {
    pub(in crate::server) async fn new(name: &str) -> Self {
        Self::with(name, TreesConfig::default(), |_| {}).await
    }

    /// The same, over a tree store of `config`, with the settings changed by `change`.
    pub(super) async fn with(
        name: &str,
        config: TreesConfig,
        change: impl FnOnce(&mut Settings),
    ) -> Self {
        let bench = Bench::with(name, config).await;
        let mut settings = settings();
        change(&mut settings);
        let provider = Provider::with(bench.store.clone(), bench.rig.cache.clone(), settings);
        let key = put_pack_list(&bench).await;
        Self {
            bench,
            key,
            provider,
        }
    }

    /// Puts the version 1 bytes of `list` in the upstream under its key, and no sidecar (none is
    /// made of a version 1 list); the key.
    pub(super) async fn put_list(&self, list: &TreeList) -> String {
        put(&self.bench, &list.encode()).await
    }

    /// Puts the version 2 bytes of `list` in the upstream under its key, and its L2 sidecar
    /// beside it; the key.
    pub(super) async fn put_list_v2(&self, list: &TreeList) -> String {
        put_tree(&self.bench, &list.encode_v2().expect("a list to encode")).await
    }

    /// Puts raw `bytes` in the upstream under their own tree list key, and no sidecar; the key.
    pub(super) async fn put_bytes(&self, bytes: &[u8]) -> String {
        put(&self.bench, bytes).await
    }

    /// Puts the version 2 list `bytes` in the upstream under its key, and the L2 sidecar
    /// [`sidecar_of`] makes of them beside it; the key.
    pub(super) async fn put_tree_bytes(&self, bytes: &[u8]) -> String {
        put_tree(&self.bench, bytes).await
    }

    /// Puts `bytes` in the upstream as the L2 sidecar of the tree list `key`.
    pub(super) async fn put_sidecar(&self, key: &str, bytes: &[u8]) {
        self.bench.rig.put(&sidecar_key_of(key), bytes).await;
    }

    /// The directory the provider answers for the source `index` of `run`.
    pub(super) fn path_of(run: &str, index: u32) -> PathBuf {
        Path::new(MOUNT).join(Self::place_of(run, index))
    }

    /// The path of that source under the mount, as the tree store places it.
    pub(in crate::server) fn place_of(run: &str, index: u32) -> String {
        format!(
            "mount-provider/{}/{}/{index}",
            hex::encode(AGENT),
            hex::encode(run)
        )
    }

    /// Whether the cache holds the span of the blob `blob` of the pack.
    pub(super) fn holds(&self, blob: usize) -> bool {
        let (_, offset, size) = &self.bench.pack.blobs[blob];
        self.bench
            .rig
            .cache
            .span_of(&self.bench.pack.key, *offset, *size)
            .is_some()
    }

    /// Prepares the source `index` of `run` with the pack's list, and mounts it.
    pub(super) async fn mounted(&self, run: &str, index: u32) -> PathBuf {
        self.provider
            .prepare(source(run, index), &self.key, READ_ONLY)
            .expect("prepared");
        self.provider
            .mount(source(run, index))
            .await
            .expect("mounted")
    }

    /// Waits until every blob of the pack is in the cache.
    pub(super) async fn filled(&self) {
        until("every file was read", || {
            (0..3).all(|blob| self.holds(blob))
        })
        .await;
    }
}

/// Puts the tree of [`list_of`] of `bench`'s pack in its upstream, its version 2 list under its
/// key and its L2 sidecar beside it; the key.
pub(super) async fn put_pack_list(bench: &Bench) -> String {
    put_tree(
        bench,
        &list_of(&bench.pack).encode_v2().expect("a list to encode"),
    )
    .await
}

/// Puts `bytes` under their tree list key; the key.
async fn put(bench: &Bench, bytes: &[u8]) -> String {
    let key = tree_list_key(bytes);
    bench.rig.put(&key, bytes).await;
    key
}

/// Puts the version 2 list `v2` under its key and its L2 sidecar beside it; the key.
async fn put_tree(bench: &Bench, v2: &[u8]) -> String {
    let key = put(bench, v2).await;
    bench
        .rig
        .put(&sidecar_key_of(&key), &sidecar_of(&bench.pack, v2))
        .await;
    key
}

/// The key of the L2 sidecar of the tree list `key`: `cas/l2/` and the same id.
pub(super) fn sidecar_key_of(key: &str) -> String {
    let digest = tree_list_digest(key).expect("a tree list key");
    sidecar_key(digest.as_bytes())
}

/// The L2 sidecar of the version 2 list `v2`, as a CAS service writes it: the cells of `pack` for
/// its key, and for any other key the list names (a pack the tests leave out of the upstream) the
/// cells of 1 MiB of zeros, which hold every file the tests put in one.
pub(in crate::server) fn sidecar_of(pack: &Pack, v2: &[u8]) -> Vec<u8> {
    let zeros = PackCells::of(&vec![0u8; 1 << 20]);
    sidecar_from_v2(v2, |key| {
        let cells = if key == pack.key {
            PackCells::of(pack.bytes())
        } else {
            zeros.clone()
        };
        Some(PackInfo {
            cells,
            ..PackInfo::default()
        })
    })
    .expect("a sidecar")
}

/// The objects of the tree of [`list_of`] of `pack`, as a CAS service stores them: its version 2
/// list under its key, first, then its L2 sidecar under the sidecar's key.
pub(in crate::server) fn tree_objects(pack: &Pack) -> [(String, Vec<u8>); 2] {
    let v2 = list_of(pack).encode_v2().expect("a list to encode");
    let key = tree_list_key(&v2);
    let sidecar = (sidecar_key_of(&key), sidecar_of(pack, &v2));
    [(key, v2), sidecar]
}

/// The refusal `error` is, as text.
pub(super) fn refused(error: ProviderError) -> String {
    match error {
        ProviderError::Refused(why) => why,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// The unavailable answer `error` is, as text.
pub(super) fn unavailable(error: ProviderError) -> String {
    match error {
        ProviderError::Unavailable(why) => why,
        other => panic!("expected unavailable, got {other:?}"),
    }
}

/// A gRPC client of the provider's socket.
pub(in crate::server) struct Client {
    sender: SendRequest<Full<Bytes>>,
}

impl Client {
    /// Connects to the socket at `path`, waiting for it to be bound.
    pub(super) async fn connect(path: &Path) -> Self {
        until("the socket is bound", || path.exists()).await;
        let stream = UnixStream::connect(path).await.expect("a connection");
        let (sender, connection) = http2::handshake(TokioExecutor::new(), TokioIo::new(stream))
            .await
            .expect("an HTTP/2 handshake");
        tokio::spawn(async move {
            let _ = connection.await;
        });
        Self { sender }
    }

    /// Connects to the socket at `path` once: none when no socket is there or none listens on it
    /// (between two instances of a hand-over, the file of the one that went is still there).
    pub(in crate::server) async fn try_connect(path: &Path) -> Option<Self> {
        let stream = UnixStream::connect(path).await.ok()?;
        let (sender, connection) = http2::handshake(TokioExecutor::new(), TokioIo::new(stream))
            .await
            .ok()?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        Some(Self { sender })
    }

    /// Calls `method` with `message`: the answer, or the status of a refusal.
    pub(super) async fn call<A: Message + Default>(
        &mut self,
        method: &str,
        message: &impl Message,
    ) -> Result<A, Status> {
        self.try_call(method, message).await.expect("an answer")
    }

    /// [`call`](Self::call), or none when the connection ends before the answer: the instance
    /// that served it closed its socket.
    pub(in crate::server) async fn try_call<A: Message + Default>(
        &mut self,
        method: &str,
        message: &impl Message,
    ) -> Option<Result<A, Status>> {
        let bytes = message.encode_to_vec();
        let mut frame = vec![0];
        frame.extend_from_slice(&u32::try_from(bytes.len()).expect("small").to_be_bytes());
        frame.extend_from_slice(&bytes);
        let path = tierstore_trees_api::provider::path(method);
        let request = hyper::Request::post(format!("http://provider{path}"))
            .header("content-type", "application/grpc")
            .header("te", "trailers")
            .body(Full::new(Bytes::from(frame)))
            .expect("a request");
        let response = self.sender.send_request(request).await.ok()?;
        let (parts, body) = response.into_parts();
        let collected = body.collect().await.ok()?;
        let trailers = collected.trailers().cloned();
        let status = Status::from_header_map(&parts.headers)
            .or_else(|| trailers.as_ref().and_then(Status::from_header_map))
            .expect("a grpc-status");
        if status.code() != tonic::Code::Ok {
            return Some(Err(status));
        }
        let data = collected.to_bytes();
        Some(Ok(A::decode(&data[5..]).expect("an answer message")))
    }
}
