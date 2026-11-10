//! The fill socket of a build node as the start serves it: bound beside the S3 front, answered
//! from the same cache, gone when the node has drained, and out of reach of an S3 client.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use objstore_s3::{ByteRange, ObjectKey, ObjectStore};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixStream;

use super::cache_cli_tests::{Node, client, probe, serving};
use super::local_cache::pack_testing::Pack;
use super::local_cache::testing::until;
use super::local_cache::{Disk, SystemDisk};
use super::{Backend, Drained};
use crate::{FaultyBackend, MemoryBackend};

/// Asserts that a node with `local_cache.fill_socket` binds the socket while it serves; that a fill
/// over it is answered `ok` and puts the blob in the cache the S3 front reads (the fill is counted
/// on `/metricz`); that an S3 client cannot read the blob by its reserved key; and that the socket
/// is gone when the node has drained.
#[tokio::test]
async fn a_build_node_binds_its_fill_socket_beside_the_s3_front() {
    let node = Node::new("tfsn-serve");
    let socket = node.cache_dir().with_file_name("fill.sock");
    let settings = node.settings("", &format!("fill_socket = \"{}\"", socket.display()));
    let memory = Arc::new(MemoryBackend::new());
    let pack = Pack::new("packs/one", &[1500, 2500]);
    memory
        .put("packs/one", Bytes::copy_from_slice(pack.bytes()))
        .await
        .unwrap();
    let upstream: Arc<dyn Backend> = Arc::new(FaultyBackend::new(memory));
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let (sha, offset, size) = pack.blobs[1].clone();
    let path = socket.clone();
    let work = |addr: SocketAddr| async move {
        until("the socket is bound", || path.exists()).await;
        let stream = UnixStream::connect(&path).await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);
        let line = format!("fill {sha} {size} from packs/one @ {offset}\n");
        write.write_all(line.as_bytes()).await.unwrap();
        let mut answer = String::new();
        read.read_line(&mut answer).await.unwrap();
        assert_eq!(answer, "ok\n");
        let store = client(addr);
        let reserved = ObjectKey::new(format!("_tiering/fill/{sha}")).unwrap();
        let first = ByteRange::new(0, 10).unwrap();
        assert!(store.get_range(&reserved, first).await.is_err());
        let (status, text) = probe(addr, "/metricz").await;
        assert_eq!(status, 200);
        assert!(text.contains("tierstore_cache_fills_total 1\n"), "{text}");
    };
    let drained = serving(&settings, upstream, disk, work).await.unwrap();
    assert_eq!(drained, Drained { abandoned: 0 });
    assert!(!socket.exists());
}

/// Asserts that a node whose file names no fill socket, and that has no state directory, makes
/// none.
#[tokio::test]
async fn a_node_without_a_place_for_it_has_no_fill_socket() {
    let node = Node::new("tfsn-none");
    let settings = node.settings("", "");
    assert_eq!(settings.fill_socket, None);
    let upstream: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let dir = node.cache_dir();
    let work = |_addr: SocketAddr| async move {
        let entries = std::fs::read_dir(dir.parent().unwrap()).unwrap();
        let names: Vec<_> = entries.map(|e| e.unwrap().file_name()).collect();
        assert!(names.iter().all(|name| name != "fill.sock"), "{names:?}");
    };
    serving(&settings, upstream, disk, work).await.unwrap();
}
