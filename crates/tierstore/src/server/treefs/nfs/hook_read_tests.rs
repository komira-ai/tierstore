//! The update hook's read of a tree from the NFS front (`update_hook/machine/nfs_read.rs`), as a
//! Mac's mount check makes it, against this server on a real loopback socket: the sample tree read
//! whole, and a path the front does not have.

use std::collections::BTreeMap;
use std::path::PathBuf;

use super::config::NfsSettings;
use super::fixtures::{attached, endless, fs_of};
use super::wire_client::serve;
use crate::server::update_hook::machine::{ProviderSocket, ProviderTimes};
use crate::server::update_hook::{MountProvider as _, ReadFault, TreeAt, TreeNode};

/// What the hook's client reads of `below` from the front at `front`.
async fn read(
    front: std::net::SocketAddr,
    below: String,
) -> Result<BTreeMap<String, TreeNode>, ReadFault> {
    tokio::task::spawn_blocking(move || {
        let client = ProviderSocket::new(PathBuf::from("/"), ProviderTimes::default());
        let below = PathBuf::from(below);
        client.read_tree(TreeAt::Front {
            front,
            below: &below,
        })
    })
    .await
    .unwrap()
}

/// Asserts that the hook's read from the front walks the sample tree attached at
/// `trees/<digest>` (the MOUNT of the export's root, a LOOKUP of each name, a READDIRPLUS of each
/// directory) and finds every path with what it is: each file's bytes whole (one of 300000 bytes,
/// read in several READs, and an empty one), each directory, and the link's target, and nothing
/// more. Catches a read that stops at the first READ of a file, or follows the link.
#[tokio::test(flavor = "multi_thread")]
async fn the_hook_reads_a_tree_whole_from_the_front() {
    let (rig, digest) = attached("nfs-hook-read");
    let (front, task) = serve(fs_of(&rig, &NfsSettings::default(), endless())).await;
    let read = read(front, format!("trees/{digest}")).await.unwrap();
    let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let cc: Vec<u8> = (0..3000u32).map(|i| (i % 241) as u8).collect();
    let want = BTreeMap::from([
        ("big.bin".to_owned(), TreeNode::File(big)),
        ("bin".to_owned(), TreeNode::Dir),
        ("bin/cc".to_owned(), TreeNode::File(cc)),
        (
            "bin/tool".to_owned(),
            TreeNode::File(b"tool bytes".to_vec()),
        ),
        ("empty.h".to_owned(), TreeNode::File(Vec::new())),
        ("include".to_owned(), TreeNode::Dir),
        ("include/a.h".to_owned(), TreeNode::File(b"int a;".to_vec())),
        ("lib".to_owned(), TreeNode::Link("bin".to_owned())),
    ]);
    assert_eq!(read, want);
    task.abort();
}

/// Asserts that a path the front does not have is a failure that names the path read, the front
/// and the status it answered, not slowness. Catches a check that defers, and so waits out
/// hostkeeper's deadline, on a front that will never serve the tree.
#[tokio::test(flavor = "multi_thread")]
async fn a_path_the_front_does_not_have_is_a_failure_naming_it() {
    let (rig, digest) = attached("nfs-hook-missing");
    let (front, task) = serve(fs_of(&rig, &NfsSettings::default(), endless())).await;
    let below = format!("trees/{digest}/missing");
    let read = read(front, below.clone()).await;
    assert_eq!(
        read,
        Err(ReadFault::Failed(format!(
            "{below} could not be looked up: the NFS front at {front} answered NFS3ERR_NOENT (2)"
        )))
    );
    task.abort();
}
