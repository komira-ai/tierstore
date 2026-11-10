//! The fence of section 6.8: only the users in `fs.nfs.allowed_uids` are served, over the wire.

use super::config::NfsSettings;
use super::fixtures::{UID, attached, endless, fs_of};
use super::wire_client::{
    Client, Cred, Dec, Enc, GETATTR, LINK, LOOKUP, NFS, READ, RENAME, WRITE, serve,
};
use crate::server::treefs::ino;

const ACCES: u32 = 13;

async fn connected(
    name: &str,
) -> (
    Client,
    Vec<u8>,
    tokio::task::JoinHandle<()>,
    super::super::testing::Rig,
) {
    let (rig, _) = attached(name);
    let (address, task) = serve(fs_of(&rig, &NfsSettings::default(), endless())).await;
    let mut client = Client::connect(address).await;
    let root = client.mount("/").await;
    (client, root, task, rig)
}

/// Asserts that a call under `AUTH_SYS` of a user in the list is served, whatever its group.
#[tokio::test]
async fn rpc_from_an_allowed_uid_is_served() {
    let (mut client, root, task, _rig) = connected("nfs-uid-allowed").await;
    for gid in [0, 20, 5000] {
        let reply = client
            .call(
                NFS,
                GETATTR,
                Cred::Sys(UID, gid),
                Enc::default().opaque(&root),
            )
            .await;
        let mut body = Dec::new(&reply.body);
        assert_eq!(body.u32(), 0, "group {gid}");
        assert_eq!(body.attr().fileid, ino::ROOT);
        assert!(body.done());
    }
    task.abort();
}

/// Asserts that a call from a user not in the list is `NFS3ERR_ACCES`, for a procedure of each shape
/// of failure (the reply is the status and the empty failure body the procedure has, as exact
/// bytes), that the procedure did not run (a READ of a cold blob asks for no fill), and that NULL,
/// which carries nothing, is answered, and so is MOUNT, which hands out the root's handle that any
/// process can derive.
#[tokio::test]
async fn rpc_from_another_uid_is_denied_with_acces() {
    let (mut client, root, task, rig) = connected("nfs-uid-denied").await;
    let other = Cred::Sys(UID + 1, 20);
    let words = |count: usize| {
        std::iter::once(ACCES)
            .chain(std::iter::repeat_n(0, count))
            .flat_map(u32::to_be_bytes)
            .collect::<Vec<u8>>()
    };
    let fh = || Enc::default().opaque(&root);
    let denied = [
        (GETATTR, fh(), 0),
        (LOOKUP, fh().opaque(b"trees"), 1),
        (READ, fh().u64(0).u32(10), 1),
        (WRITE, fh().u64(0).u32(1).u32(0).opaque(b"x"), 2),
        (LINK, fh().opaque(&root).opaque(b"x"), 3),
        (RENAME, fh().opaque(b"a").opaque(&root).opaque(b"b"), 4),
    ];
    let fills = rig.upstream.asked("fill").len();
    for (procedure, args, empty_words) in denied {
        let reply = client.call(NFS, procedure, other, args).await;
        assert_eq!(reply.accepted, 0, "procedure {procedure}");
        assert_eq!(reply.body, words(empty_words), "procedure {procedure}");
    }
    assert_eq!(rig.upstream.asked("fill").len(), fills);

    let null = client.call(NFS, 0, other, Enc::default()).await;
    assert_eq!((null.accepted, null.body.len()), (0, 0));
    assert_eq!(client.mount("/").await, root);
    task.abort();
}

/// Asserts that a call with `AUTH_NONE` is `NFS3ERR_ACCES` as well: a client that names no user
/// is no user in the list.
#[tokio::test]
async fn rpc_with_auth_none_is_denied() {
    let (mut client, root, task, _rig) = connected("nfs-uid-none").await;
    let reply = client
        .call(NFS, GETATTR, Cred::None, Enc::default().opaque(&root))
        .await;
    assert_eq!(reply.body, ACCES.to_be_bytes());
    task.abort();
}
