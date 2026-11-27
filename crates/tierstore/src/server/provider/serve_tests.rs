//! The provider as the start of a build node serves it: bound beside the S3 front from the node's
//! `mount-provider` interface, a source prepared, mounted and released over the socket from a tree
//! list in the node's upstream, and the socket gone when the node has drained.

use std::sync::Arc;

use bytes::Bytes;
use tierstore_trees_api::provider::v1::{
    MountRequest, MountResponse, PrepareRequest, PrepareResponse, ReleaseRequest, ReleaseResponse,
};

use super::super::Drained;
use super::super::cache_cli_tests::{Node, serving};
use super::super::local_cache::pack_testing::Pack;
use super::super::local_cache::{Disk, SystemDisk};
use super::check::TREES;
use super::testing::{Client, READ_ONLY, Rig, source, tree_objects};
use crate::server::local_cache::testing::until;
use crate::{Backend, FaultyBackend, MemoryBackend};

/// Asserts that a build node whose file has a `mount-provider` interface serves the provider on
/// its socket while it serves: a prepare of a tree list in the node's upstream, a mount that
/// answers the source's directory under the interface's mount, and a release; and that the socket
/// is gone when the node has drained.
#[tokio::test]
async fn a_build_node_serves_its_mount_provider_beside_the_s3_front() {
    let node = Node::new("prov-node");
    let socket = node.cache_dir().with_file_name("provider.sock");
    let interface = format!(
        "\n[[interface]]\ntype   = \"mount-provider\"\nparams = {{ socket = \"{}\", mountpoint = \"/run/tierstore/fs\" }}\n",
        socket.display()
    );
    let settings = node.settings("", &interface);
    let memory = Arc::new(MemoryBackend::new());
    let pack = Pack::new("cas/pack/one", &[1000, 2000, 3000]);
    memory
        .put(&pack.key, Bytes::copy_from_slice(pack.bytes()))
        .await
        .unwrap();
    let [(key, list), (sidecar, l2)] = tree_objects(&pack);
    memory.put(&key, Bytes::from(list)).await.unwrap();
    memory.put(&sidecar, Bytes::from(l2)).await.unwrap();
    let upstream: Arc<dyn Backend> = Arc::new(FaultyBackend::new(memory));
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let path = socket.clone();
    let work = |_| async move {
        let mut client = Client::connect(&path).await;
        let prepare = PrepareRequest {
            source: source("js.1", 0),
            r#ref: key,
            mode: READ_ONLY,
            host: String::new(),
        };
        let _: PrepareResponse = client.call("Prepare", &prepare).await.unwrap();
        let mount = MountRequest {
            source: source("js.1", 0),
        };
        let mounted: MountResponse = client.call("Mount", &mount).await.unwrap();
        assert_eq!(
            mounted.path,
            Rig::path_of("js.1", 0).to_string_lossy().into_owned()
        );
        let release = ReleaseRequest {
            source: source("js.1", 0),
        };
        let _: ReleaseResponse = client.call("Release", &release).await.unwrap();
    };
    let drained = serving(&settings, upstream, disk, work).await.unwrap();
    assert_eq!(drained, Drained { abandoned: 0 });
    assert!(!socket.exists());
}

/// Asserts that a build node with a mount provider has the update check's objects in front of its
/// upstream: with nothing of the check in the upstream, both trees of the check are prepared and
/// mounted over the socket (one from its sidecar, one from its list alone), read from no store, and
/// released, and nothing is written to the upstream. Catches a node whose update check would need
/// the objects in the store, or the network.
#[tokio::test]
async fn a_build_node_answers_the_check_trees_without_its_upstream() {
    use crate::server::update_hook::MountProvider as _;
    use crate::server::update_hook::machine::{ProviderSocket, ProviderTimes};

    let node = Node::new("prov-node-check");
    let socket = node.cache_dir().with_file_name("provider.sock");
    let interface = format!(
        "\n[[interface]]\ntype   = \"mount-provider\"\nparams = {{ socket = \"{}\" }}\n",
        socket.display()
    );
    let settings = node.settings("", &interface);
    let memory = Arc::new(MemoryBackend::new());
    let upstream: Arc<dyn Backend> = Arc::new(FaultyBackend::new(memory.clone()));
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let path = socket.clone();
    let work = |_| async move {
        until("the socket is bound", || path.exists()).await;
        let answers = tokio::task::spawn_blocking(move || {
            let client = ProviderSocket::new("/".into(), ProviderTimes::default());
            let mut answers = Vec::new();
            for tree in TREES {
                answers.push(
                    client
                        .prepare(&path, tree.index, &tree.key())
                        .map(|()| None),
                );
                answers.push(client.mount(&path, tree.index).map(Some));
                answers.push(client.release(&path, tree.index).map(|()| None));
            }
            answers
        })
        .await
        .unwrap();
        for answer in answers {
            assert!(answer.is_ok(), "{answer:?}");
        }
    };
    let drained = serving(&settings, upstream, disk, work).await.unwrap();
    assert_eq!(drained, Drained { abandoned: 0 });
    assert!(memory.list("").await.unwrap().is_empty());
}
