//! The Mac build node of the farm, started from the file the farm commits for it
//! (`tests/live_configs/mac-build-node.toml`, moved onto a scratch directory by
//! `server/live_configs.rs`): the node serves the mount provider's answers over its NFS front, as a
//! job agent of the farm meets them. Over the fake mount command of `run_tests.rs` (no mount is
//! made) and real loopback and Unix sockets; no assertion is about time.

use std::collections::BTreeSet;
use std::sync::Arc;

use bytes::Bytes;
use tierstore_testkit::dir::TestDir;
use tierstore_trees_api::provider::v1::{
    MountRequest, MountResponse, PrepareRequest, PrepareResponse, ReleaseRequest, ReleaseResponse,
};

use super::super::super::wire_client::READDIRPLUS;
use super::{
    Client, Dec, Enc, LISTING, MOUNT_NFS, NFS, NfsNode, READ, Recorder, UMOUNT, handle, lookup,
    loopback, me, programs, start_over,
};
use crate::server::cache_cli_tests::{Node, QUICK};
use crate::server::config::{PerRun, settings_from_text};
use crate::server::handoff::testing::within;
use crate::server::live_configs::{self, HOST, MAC_KEYS};
use crate::server::local_cache::Disk;
use crate::server::local_cache::pack_testing::Pack;
use crate::server::local_cache::testing::{FakeDisk, until};
use crate::server::provider::testing::{
    Client as ProviderClient, READ_ONLY, Rig, list_of, source, tree_objects,
};
use crate::{Backend, MemoryBackend, tree_list_key};

/// The names of the entries a directory of `list` has at its top: the first component of each
/// path, once.
fn top_of(list: &crate::TreeList) -> BTreeSet<String> {
    list.entries
        .iter()
        .map(|entry| entry.path.split('/').next().unwrap_or_default().to_owned())
        .collect()
}

/// The names READDIRPLUS of the directory `dir` lists, from its first entry to its end: every name
/// the server returns, `.` and `..` included if it returns them.
async fn names_in(client: &mut Client, dir: &[u8]) -> Vec<String> {
    let args = Enc::default()
        .opaque(dir)
        .u64(0)
        .fixed(&[0; 8])
        .u32(8192)
        .u32(65_536);
    let reply = client.call(NFS, READDIRPLUS, me(), args).await;
    let mut body = Dec::new(&reply.body);
    assert_eq!(body.u32(), 0, "READDIRPLUS is answered");
    let _ = body.post_op_attr();
    body.skip(8);
    let (entries, end) = body.entries();
    assert!(end, "the listing reaches its end in one page");
    entries.into_iter().map(|entry| entry.name).collect()
}

/// The bytes a READ of `handle` returns from its start, asking for at most `count`, and whether
/// the reply says the file ended.
async fn bytes_of(client: &mut Client, handle: &[u8], count: u32) -> (Vec<u8>, bool) {
    let args = Enc::default().opaque(handle).u64(0).u32(count);
    let reply = client.call(NFS, READ, me(), args).await;
    let mut body = Dec::new(&reply.body);
    assert_eq!(body.u32(), 0, "READ is answered");
    let _ = body.post_op_attr();
    let _count = body.u32();
    let ended = body.u32() == 1;
    (body.opaque(), ended)
}

/// The handle of the directory `place` below the root of the export, a component at a time.
async fn walk(client: &mut Client, place: &str) -> Vec<u8> {
    let mut handle = client.mount("/").await;
    for part in place.split('/') {
        handle = lookup(client, &handle, part).await;
    }
    handle
}

/// Prepares the source `run` of `key` on the provider's socket and mounts it: the directory the
/// provider answers.
async fn mounted(client: &mut ProviderClient, run: &str, key: &str) -> String {
    let prepare = PrepareRequest {
        source: source(run, 0),
        r#ref: key.to_owned(),
        mode: READ_ONLY,
        host: String::new(),
    };
    let _: PrepareResponse = client
        .try_call("Prepare", &prepare)
        .await
        .expect("an answer")
        .expect("prepared");
    let mount = MountRequest {
        source: source(run, 0),
    };
    let mounted: MountResponse = client
        .try_call("Mount", &mount)
        .await
        .expect("an answer")
        .expect("mounted");
    mounted.path
}

/// Asserts that the Mac build node of the farm, started from the farm's own file, serves what its
/// mount provider answers: the node says it serves once it has listed the mounts and made the
/// export's (the `fs` interface the file declares is served over NFS, and a file's 200 GiB budget
/// is the file system's to judge); a job agent's Prepare and Mount of two trees on the provider's
/// socket, one whose list has its L2 sidecar upstream and one whose list is alone, are each
/// answered with the directory its run has under the file's mount point; and through the NFS front
/// that mount reaches, the directory of each answer lists exactly the entries of its tree, an
/// empty directory of it lists nothing, and a file of it reads as the bytes its pack holds, to the
/// end; a Release of each is answered and the stop unmounts. Catches a release that loads the
/// farm's Mac file and answers its jobs' Mounts with a directory nothing serves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_mac_build_nodes_file_serves_its_mount_provider_over_nfs() {
    // The longest socket of the file is the control socket's, below its runtime directory.
    let (dir, control) = TestDir::for_socket(
        &[&std::env::temp_dir()],
        "nfs-live-mac",
        "run/fs/control.sock",
    );
    std::fs::write(dir.path().join("boot_id"), "boot-1\n").unwrap();
    live_configs::write_keys(dir.path(), &MAC_KEYS);
    let provider = dir.path().join("run/provider.sock");
    let text = live_configs::mac_on(dir.path());
    let host = PerRun {
        host: HOST,
        ..QUICK
    };
    let settings = settings_from_text(&text, host)
        .unwrap()
        .into_cache()
        .unwrap();
    assert_eq!(settings.fs.as_ref().unwrap().control_socket, control);
    let node = NfsNode {
        node: Node::new("nfs-live-mac"),
        dir,
        control,
    };
    assert_eq!(settings.fs.as_ref().unwrap().mountpoint, node.mountpoint());

    // The upstream holds two trees: one with its list and its L2 sidecar, and one with its list
    // alone, of a pack of its own.
    let upstream = MemoryBackend::new();
    let (one, two) = (
        Pack::new("cas/pack/one", &[1000, 2000, 3000]),
        Pack::new("cas/pack/two", &[1500, 2500, 3500]),
    );
    for pack in [&one, &two] {
        let bytes = Bytes::copy_from_slice(pack.bytes());
        upstream.put(&pack.key, bytes).await.unwrap();
    }
    let [(first, list), (sidecar, l2)] = tree_objects(&one);
    upstream.put(&first, Bytes::from(list)).await.unwrap();
    upstream.put(&sidecar, Bytes::from(l2)).await.unwrap();
    let alone = list_of(&two).encode();
    let second = tree_list_key(&alone);
    upstream.put(&second, Bytes::from(alone)).await.unwrap();

    // The 200 GiB budget of the file fits the file system the node is told it has.
    let disk = FakeDisk::new();
    disk.set_space(1 << 40, 1 << 40);
    let disk: Arc<dyn Disk> = Arc::new(disk);
    let runner = Recorder::new(true);
    let system = node.service(&settings, &runner, handle);
    let upstream: Arc<dyn Backend> = Arc::new(upstream);
    let mut started = start_over(
        settings,
        (upstream, disk),
        (system, Arc::clone(&runner)),
        (None, None),
        None,
    );
    let (_, ran) = within("the node serves", &mut started.bound).await.unwrap();
    assert_eq!(ran, [LISTING, MOUNT_NFS]);

    until("the provider binds its socket", || provider.exists()).await;
    let mut agent = ProviderClient::try_connect(&provider)
        .await
        .expect("the provider listens");
    let mountpoint = node.mountpoint();
    let mut client = Client::connect(loopback(runner.mounted_port().unwrap())).await;
    for (run, key, pack) in [("js.1", &first, &one), ("js.2", &second, &two)] {
        let answered = within(
            "the provider mounts the source",
            mounted(&mut agent, run, key),
        )
        .await;
        let place = Rig::place_of(run, 0);
        assert_eq!(answered, mountpoint.join(&place).to_string_lossy(), "{run}");

        let tree = walk(&mut client, &place).await;
        let mut names = names_in(&mut client, &tree).await;
        names.sort();
        let wanted: Vec<String> = top_of(&list_of(pack)).into_iter().collect();
        assert_eq!(names, wanted, "{run}");
        let empty = lookup(&mut client, &tree, "empty").await;
        assert_eq!(names_in(&mut client, &empty).await, Vec::<String>::new());
        let bin = lookup(&mut client, &tree, "bin").await;
        let tool = lookup(&mut client, &bin, "tool").await;
        let (_, offset, size) = pack.blobs[0].clone();
        let (offset, size) = (offset as usize, size as usize);
        let held = &pack.bytes()[offset..offset + size];
        let count = u32::try_from(size + 100).unwrap();
        assert_eq!(
            bytes_of(&mut client, &tool, count).await,
            (held.to_vec(), true)
        );
    }
    drop(client);
    for run in ["js.1", "js.2"] {
        let release = ReleaseRequest {
            source: source(run, 0),
        };
        let _: ReleaseResponse = agent
            .try_call("Release", &release)
            .await
            .expect("an answer")
            .expect("released");
    }
    drop(agent);

    started.stop.send(()).unwrap();
    within("the node ends", started.ended)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(programs(&runner), [LISTING, MOUNT_NFS, LISTING, UMOUNT]);
}
