//! The fill socket of a storage node with a cache in front of its proxy, as the start serves it:
//! bound beside the S3 front, answered from the node's cache with the proxy as the upstream.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixStream;

use super::{Node, QUICK, serving};
use crate::Backend;
use crate::server::Drained;
use crate::server::cache_cli_tests::probe;
use crate::server::config::settings_from_text;
use crate::server::local_cache::pack_testing::Pack;
use crate::server::local_cache::testing::until;
use crate::server::local_cache::{Disk, SystemDisk};

/// Asserts that a combined node with `local_cache.fill_socket` binds the socket while it serves;
/// that a fill over it of a blob inside a pack that only the cold tier holds is answered `ok`
/// (the proxy is the cache's upstream) and is counted on `/metricz`; that a blob that no tier
/// holds is answered with a refusal, not `ok`; and that the socket is gone when the node has
/// drained.
#[tokio::test]
async fn a_combined_node_answers_a_fill_of_a_blob_only_the_cold_tier_holds() {
    let node = Node::new("tcfs-cold");
    node.activate().await;
    let socket = node.dir.path().join("fill.sock");
    let text = node.text_with("", "", "");
    let at = format!("path   = \"{}\"", node.cache_dir().display());
    let text = text.replace(
        &at,
        &format!("{at}\nfill_socket = \"{}\"", socket.display()),
    );
    let settings = settings_from_text(&text, QUICK).unwrap();
    let settings = settings.into_combined().unwrap();
    assert_eq!(settings.fill_socket.as_deref(), Some(socket.as_path()));
    let pack = Pack::new("packs/one", &[1500, 2500]);
    let cold = Node::tier(node.cold());
    cold.put("packs/one", Bytes::copy_from_slice(pack.bytes()))
        .await
        .unwrap();
    assert!(Node::tier(node.hot()).head("packs/one").await.is_err());
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
        let (other, _, small) = pack.blobs[0].clone();
        let lost = format!("fill {other} {small} from packs/absent @ 0\n");
        write.write_all(lost.as_bytes()).await.unwrap();
        let mut answer = String::new();
        read.read_line(&mut answer).await.unwrap();
        assert_ne!(answer, "ok\n", "{answer}");
        let (status, text) = probe(addr, "/metricz").await;
        assert_eq!(status, 200);
        assert!(text.contains("tierstore_cache_fills_total 1\n"), "{text}");
    };
    let drained = serving(&settings, disk, work).await.unwrap();
    assert_eq!(drained, Drained { abandoned: 0 });
    assert!(!socket.exists());
}

/// Asserts that a combined node whose file names no fill socket, with no state directory, makes
/// none.
#[tokio::test]
async fn a_combined_node_without_a_place_for_it_has_no_fill_socket() {
    let node = Node::new("tcfs-none");
    let settings = node.settings("");
    assert_eq!(settings.fill_socket, None);
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let dir = node.dir.path().to_path_buf();
    let work = |_addr: SocketAddr| async move {
        let names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(names.iter().all(|name| name != "fill.sock"), "{names:?}");
    };
    serving(&settings, disk, work).await.unwrap();
}
