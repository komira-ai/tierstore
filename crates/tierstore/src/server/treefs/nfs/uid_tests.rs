//! Who is served, over the wire: a call that names any user (`AUTH_SYS`) is served, whichever user
//! it names, and no user changes a tree; a call that names no user is refused.

use super::config::NfsSettings;
use super::fixtures::{UID, attached, blob_of, endless, fs_of};
use super::wire_client::{
    Client, Cred, Dec, Enc, GETATTR, LOOKUP, MKNOD, NFS, READ, REMOVE, RENAME, SETATTR, WRITE,
    serve,
};
use crate::server::treefs::ino;

const ACCES: u32 = 13;
const ROFS: u32 = 30;

/// The users the tests call as, none of whom any setting names: the user the other tests serve and
/// the one after it, root, a Mac's first user, `nobody`, and the largest id.
const USERS: [(u32, u32); 6] = [
    (UID, 20),
    (UID + 1, 20),
    (0, 0),
    (501, 20),
    (65_534, 65_534),
    (u32::MAX, 0),
];

async fn connected(
    name: &str,
) -> (
    Client,
    Vec<u8>,
    String,
    tokio::task::JoinHandle<()>,
    super::super::testing::Rig,
) {
    let (rig, digest) = attached(name);
    let (address, task) = serve(fs_of(&rig, &NfsSettings::default(), endless())).await;
    let mut client = Client::connect(address).await;
    let root = client.mount("/").await;
    (client, root, digest, task, rig)
}

/// LOOKUP of `name` in `dir` as `cred`: the handle of the entry.
async fn lookup(client: &mut Client, cred: Cred, dir: &[u8], name: &str) -> Vec<u8> {
    let args = Enc::default().opaque(dir).opaque(name.as_bytes());
    let reply = client.call(NFS, LOOKUP, cred, args).await;
    let mut body = Dec::new(&reply.body);
    assert_eq!(body.u32(), 0, "lookup of {name}");
    body.opaque()
}

/// Asserts that a call from each of [`USERS`] is served, whatever user and group it names: GETATTR
/// of the root is its attributes, and a walk to a file and a READ of it give the file's bytes (the
/// first READ fills the cold blob, once). Catches a server that serves only some users.
#[tokio::test]
async fn rpc_from_any_uid_is_served() {
    let (mut client, root, digest, task, rig) = connected("nfs-uid-any").await;
    let fills = rig.upstream.asked("fill").len();
    for (uid, gid) in USERS {
        let cred = Cred::Sys(uid, gid);
        let reply = client
            .call(NFS, GETATTR, cred, Enc::default().opaque(&root))
            .await;
        let mut body = Dec::new(&reply.body);
        assert_eq!(body.u32(), 0, "user {uid}");
        assert_eq!(body.attr().fileid, ino::ROOT, "user {uid}");
        assert!(body.done(), "user {uid}");

        let trees = lookup(&mut client, cred, &root, "trees").await;
        let tree = lookup(&mut client, cred, &trees, &digest).await;
        let bin = lookup(&mut client, cred, &tree, "bin").await;
        let tool = lookup(&mut client, cred, &bin, "tool").await;
        let reply = client
            .call(
                NFS,
                READ,
                cred,
                Enc::default().opaque(&tool).u64(0).u32(100),
            )
            .await;
        let mut body = Dec::new(&reply.body);
        assert_eq!(body.u32(), 0, "user {uid}");
        let _ = body.post_op_attr();
        assert_eq!((body.u32(), body.u32()), (10, 1), "user {uid}");
        assert_eq!(body.opaque(), blob_of(&rig, 10), "user {uid}");
    }
    assert_eq!(rig.upstream.asked("fill").len(), fills + 1);
    task.abort();
}

/// Asserts that no user changes a tree, root included: SETATTR, WRITE, REMOVE and RENAME from a
/// user other than the one the tests serve, and from root, are `NFS3ERR_ROFS`, MKNOD is a procedure
/// that is not available, and the file is there afterwards with its bytes. The tree is read-only
/// for every user, which is what is left to fence the service once it serves every user.
#[tokio::test]
async fn no_uid_changes_a_tree() {
    let (mut client, root, digest, task, rig) = connected("nfs-uid-rofs").await;
    let me = Cred::Sys(UID, 20);
    let trees = lookup(&mut client, me, &root, "trees").await;
    let tree = lookup(&mut client, me, &trees, &digest).await;
    let file = lookup(&mut client, me, &tree, "big.bin").await;
    let void_attrs = || Enc::default().u32(0).u32(0).u32(0).u32(0).u32(0).u32(0);
    for cred in [Cred::Sys(UID + 1, 20), Cred::Sys(0, 0)] {
        let calls = [
            (
                SETATTR,
                Enc::default().opaque(&file).fixed(&void_attrs().0).u32(0),
            ),
            (
                WRITE,
                Enc::default()
                    .opaque(&file)
                    .u64(0)
                    .u32(1)
                    .u32(0)
                    .opaque(b"x"),
            ),
            (REMOVE, Enc::default().opaque(&tree).opaque(b"big.bin")),
            (
                RENAME,
                Enc::default()
                    .opaque(&tree)
                    .opaque(b"big.bin")
                    .opaque(&tree)
                    .opaque(b"moved"),
            ),
        ];
        for (procedure, args) in calls {
            let reply = client.call(NFS, procedure, cred, args).await;
            assert_eq!(reply.accepted, 0, "procedure {procedure}");
            assert_eq!(
                Dec::new(&reply.body).u32(),
                ROFS,
                "procedure {procedure}: NFS3ERR_ROFS"
            );
        }
        let mknod = client
            .call(
                NFS,
                MKNOD,
                cred,
                Enc::default().opaque(&tree).opaque(b"x").u32(1),
            )
            .await;
        assert_eq!(mknod.accepted, 3, "MKNOD: PROC_UNAVAIL");
    }
    let after = lookup(&mut client, me, &tree, "big.bin").await;
    assert_eq!(after, file);
    let reply = client
        .call(NFS, READ, me, Enc::default().opaque(&after).u64(0).u32(16))
        .await;
    let mut body = Dec::new(&reply.body);
    assert_eq!(body.u32(), 0);
    let _ = body.post_op_attr();
    assert_eq!((body.u32(), body.u32()), (16, 0));
    assert_eq!(body.opaque(), blob_of(&rig, 300_000)[..16]);
    task.abort();
}

/// Asserts that a call with `AUTH_NONE`, which names no user, is `NFS3ERR_ACCES` with no body: the
/// mode bits that `ACCESS` and the client check need a user to be read against.
#[tokio::test]
async fn rpc_with_auth_none_is_denied() {
    let (mut client, root, _digest, task, _rig) = connected("nfs-uid-none").await;
    let reply = client
        .call(NFS, GETATTR, Cred::None, Enc::default().opaque(&root))
        .await;
    assert_eq!(reply.body, ACCES.to_be_bytes());
    task.abort();
}
