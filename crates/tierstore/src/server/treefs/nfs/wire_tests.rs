//! The server on a real loopback socket, spoken to by a client written apart from it (`wire_client`):
//! the MOUNT and NFSv3 procedures a mount and a build make, as the bytes of the protocol.

use nfsserve::nfs::{ACCESS3_EXECUTE, ACCESS3_LOOKUP, ACCESS3_READ, FSF_HOMOGENEOUS, FSF_SYMLINK};

use super::config::NfsSettings;
use super::fixtures::{UID, attached, blob_of, endless, fs_of, handle_of};
use super::wire_client::{
    ACCESS, CREATE, Client, Cred, Dec, Enc, FSINFO, GETATTR, LINK, LOOKUP, MKDIR, NFS, READ,
    READDIRPLUS, READLINK, REMOVE, RENAME, RMDIR, SETATTR, SYMLINK, WRITE, serve,
};
use crate::server::treefs::ino;

const ME: Cred = Cred::Sys(UID, 20);

/// A client connected to a server over the sample tree, with the handle of the mount's root.
async fn connected(
    name: &str,
) -> (
    Client,
    Vec<u8>,
    String,
    tokio::task::JoinHandle<()>,
    super::super::testing::Rig,
) {
    let (rig, root) = attached(name);
    let (address, task) = serve(fs_of(&rig, &NfsSettings::default(), endless())).await;
    let mut client = Client::connect(address).await;
    let handle = client.mount("/").await;
    (client, handle, root, task, rig)
}

/// LOOKUP of `name` in `dir`: the handle of the entry.
async fn lookup(client: &mut Client, dir: &[u8], name: &str) -> Vec<u8> {
    let reply = client
        .call(
            NFS,
            LOOKUP,
            ME,
            Enc::default().opaque(dir).opaque(name.as_bytes()),
        )
        .await;
    let mut body = Dec::new(&reply.body);
    assert_eq!(body.u32(), 0, "lookup of {name}");
    body.opaque()
}

/// The status of a call, and the rest of its reply.
async fn status(client: &mut Client, procedure: u32, args: Enc) -> (u32, Vec<u8>) {
    let reply = client.call(NFS, procedure, ME, args).await;
    assert_eq!(reply.accepted, 0);
    let mut body = Dec::new(&reply.body);
    let code = body.u32();
    (code, reply.body[4..].to_vec())
}

/// Asserts a mount and a read, as `mount_nfs` and a build make them: MOUNT gives the handle of
/// the root, which is the twelve bytes of its inode and the epoch; LOOKUPs walk `trees/<digest>/bin/tool`; GETATTR,
/// READLINK, ACCESS and READ answer from the core, a cold blob is filled by the first READ, and the
/// bytes and the end-of-file flag are the file's.
#[tokio::test]
async fn mount_then_lookup_then_read_a_file_over_tcp() {
    let (mut client, root, digest, task, rig) = connected("nfs-wire-read").await;
    assert_eq!(root, handle_of(ino::ROOT));

    let attrs = status(&mut client, GETATTR, Enc::default().opaque(&root)).await;
    assert_eq!(attrs.0, 0);
    let attr = Dec::new(&attrs.1).attr();
    assert_eq!(
        (attr.kind, attr.mode, attr.uid, attr.fileid),
        (2, 0o711, 0, ino::ROOT)
    );
    assert_eq!(attr.mtime, 1_790_000_000);

    let trees = lookup(&mut client, &root, "trees").await;
    let tree = lookup(&mut client, &trees, &digest).await;
    let bin = lookup(&mut client, &tree, "bin").await;
    let tool = lookup(&mut client, &bin, "tool").await;
    let cc = lookup(&mut client, &bin, "cc").await;
    let big = lookup(&mut client, &tree, "big.bin").await;
    let lib = lookup(&mut client, &tree, "lib").await;
    assert_eq!(tool, handle_of(ino::of(1, 4)));

    let missing = status(
        &mut client,
        LOOKUP,
        Enc::default().opaque(&tree).opaque(b"nope"),
    )
    .await;
    assert_eq!(missing.0, 2, "NFS3ERR_NOENT");

    let filled = rig.upstream.asked("fill").len();
    let reply = client
        .call(NFS, READ, ME, Enc::default().opaque(&tool).u64(0).u32(100))
        .await;
    let mut body = Dec::new(&reply.body);
    assert_eq!(body.u32(), 0);
    let attr = body.post_op_attr().unwrap();
    assert_eq!((body.u32(), body.u32()), (10, 1), "ten bytes, end of file");
    assert_eq!(body.opaque(), blob_of(&rig, 10));
    assert!(body.done());
    assert_eq!(attr.size, 10);
    assert_eq!(rig.upstream.asked("fill").len(), filled + 1);

    let reply = client
        .call(
            NFS,
            READ,
            ME,
            Enc::default().opaque(&big).u64(100_000).u32(4096),
        )
        .await;
    let mut body = Dec::new(&reply.body);
    assert_eq!(body.u32(), 0);
    let _ = body.post_op_attr();
    assert_eq!((body.u32(), body.u32()), (4096, 0));
    assert_eq!(body.opaque(), blob_of(&rig, 300_000)[100_000..104_096]);

    let reply = client
        .call(NFS, READLINK, ME, Enc::default().opaque(&lib))
        .await;
    let mut body = Dec::new(&reply.body);
    assert_eq!(body.u32(), 0);
    let _ = body.post_op_attr();
    assert_eq!(body.opaque(), b"bin");

    for (handle, want) in [
        (&cc, ACCESS3_READ | ACCESS3_EXECUTE),
        (&tool, ACCESS3_READ),
        (&bin, ACCESS3_READ | ACCESS3_LOOKUP),
    ] {
        let (code, rest) =
            status(&mut client, ACCESS, Enc::default().opaque(handle).u32(0x3f)).await;
        let mut body = Dec::new(&rest);
        let _ = body.post_op_attr();
        assert_eq!((code, body.u32()), (0, want));
    }

    let (code, rest) = status(&mut client, FSINFO, Enc::default().opaque(&root)).await;
    let mut body = Dec::new(&rest);
    assert_eq!(code, 0);
    let _ = body.post_op_attr();
    body.skip(7 * 4 + 8 + 8);
    assert_eq!(body.u32(), FSF_SYMLINK | FSF_HOMOGENEOUS);
    task.abort();
}

/// One page of a READDIRPLUS: its entries, whether it is the last, and the length of its results.
async fn page(client: &mut Client, args: Enc) -> (Vec<super::wire_client::Entry>, bool, usize) {
    let reply = client.call(NFS, READDIRPLUS, ME, args).await;
    let mut body = Dec::new(&reply.body);
    assert_eq!(body.u32(), 0);
    let _ = body.post_op_attr();
    body.skip(8);
    let (entries, end) = body.entries();
    assert!(body.done());
    (entries, end, reply.body.len())
}

/// Asserts READDIRPLUS: all the entries of a directory with their attributes and handles, the
/// handle being the twelve bytes of the file id and the epoch; the end flag; and that pages bounded by `maxcount`
/// (the whole reply fits in it) and by `dircount` resume at the cookie of the last entry without
/// repeating or skipping any.
#[tokio::test]
async fn readdirplus_respects_dircount_and_maxcount_limits() {
    let (mut client, root, digest, task, _rig) = connected("nfs-wire-plus").await;
    let trees = lookup(&mut client, &root, "trees").await;
    let tree = lookup(&mut client, &trees, &digest).await;
    let plus = |cookie: u64, dircount: u32, maxcount: u32| {
        Enc::default()
            .opaque(&tree)
            .u64(cookie)
            .fixed(&[0; 8])
            .u32(dircount)
            .u32(maxcount)
    };
    let (all, end, _) = page(&mut client, plus(0, 8192, 65_536)).await;
    let names: Vec<&str> = all.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(names, ["big.bin", "bin", "empty.h", "include", "lib"]);
    assert!(end);
    for entry in &all {
        assert_eq!(entry.cookie, entry.fileid);
        assert_eq!(
            entry.handle.as_deref(),
            Some(handle_of(entry.fileid).as_slice())
        );
        let attr = entry.attr.as_ref().unwrap();
        assert_eq!((attr.fileid, attr.uid), (entry.fileid, 1234));
    }
    assert_eq!(all[1].attr.as_ref().unwrap().kind, 2);
    assert_eq!(all[0].attr.as_ref().unwrap().size, 300_000);

    for (dircount, maxcount) in [(8192, 512), (40, 65_536)] {
        let (mut seen, mut cookie, mut pages) = (Vec::new(), 0, 0);
        loop {
            let (entries, end, length) = page(&mut client, plus(cookie, dircount, maxcount)).await;
            assert!(
                !entries.is_empty() && entries.len() <= 2,
                "{dircount} {maxcount}"
            );
            // The reply header is 24 bytes, and the limit counts it.
            assert!(length + 24 <= maxcount as usize, "{length} of {maxcount}");
            cookie = entries.last().unwrap().cookie;
            seen.extend(entries.into_iter().map(|entry| entry.name));
            pages += 1;
            if end {
                break;
            }
        }
        assert!(pages > 1, "{dircount} {maxcount}");
        assert_eq!(seen, names, "{dircount} {maxcount}");
    }
    task.abort();
}

/// Asserts that every procedure that would change a tree is answered `NFS3ERR_ROFS` over the wire,
/// and that LINK, which the server does not implement, is a procedure that is not available.
#[tokio::test]
async fn every_write_procedure_returns_rofs_over_the_wire() {
    let (mut client, root, digest, task, _rig) = connected("nfs-wire-rofs").await;
    let trees = lookup(&mut client, &root, "trees").await;
    let tree = lookup(&mut client, &trees, &digest).await;
    let file = lookup(&mut client, &tree, "big.bin").await;
    let void_attrs = || Enc::default().u32(0).u32(0).u32(0).u32(0).u32(0).u32(0);
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
        (
            CREATE,
            Enc::default()
                .opaque(&tree)
                .opaque(b"n")
                .u32(0)
                .fixed(&void_attrs().0),
        ),
        (
            MKDIR,
            Enc::default()
                .opaque(&tree)
                .opaque(b"n")
                .fixed(&void_attrs().0),
        ),
        (REMOVE, Enc::default().opaque(&tree).opaque(b"big.bin")),
        (RMDIR, Enc::default().opaque(&tree).opaque(b"bin")),
        (
            RENAME,
            Enc::default()
                .opaque(&tree)
                .opaque(b"lib")
                .opaque(&tree)
                .opaque(b"lib2"),
        ),
        (
            SYMLINK,
            Enc::default()
                .opaque(&tree)
                .opaque(b"l")
                .fixed(&void_attrs().0)
                .opaque(b"t"),
        ),
    ];
    for (procedure, args) in calls {
        let (code, _) = status(&mut client, procedure, args).await;
        assert_eq!(code, 30, "procedure {procedure}: NFS3ERR_ROFS");
    }
    let link = client
        .call(
            NFS,
            LINK,
            ME,
            Enc::default().opaque(&file).opaque(&tree).opaque(b"h"),
        )
        .await;
    assert_eq!(link.accepted, 3, "PROC_UNAVAIL");
    task.abort();
}
