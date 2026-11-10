//! The procedures of the file system, called as `nfsserve` calls them, over a core with a fake fill
//! socket.

use nfsserve::nfs::{
    ACCESS3_DELETE, ACCESS3_EXECUTE, ACCESS3_EXTEND, ACCESS3_LOOKUP, ACCESS3_MODIFY, ACCESS3_READ,
    ftype3, nfsstat3, post_op_attr, sattr3,
};
use nfsserve::vfs::{NFSFileSystem as _, VFSCapabilities};

use super::{TreeNfs, class_bits, fattr, handled, status};
use crate::server::treefs::core::{Attr, Errno, Kind};
use crate::server::treefs::ino;
use crate::server::treefs::locked;
use crate::server::treefs::nfs::config::NfsSettings;
use crate::server::treefs::nfs::fixtures::{
    UID, attached, auth, blob_of, endless, failed, fs_of, names, none,
};
use crate::server::treefs::testing::{OWNER, until};

const BIG: u64 = 1;
const BIN: u64 = 2;
const CC: u64 = 3;
const TOOL: u64 = 4;
const EMPTY: u64 = 5;
const INCLUDE: u64 = 6;
const LIB: u64 = 8;

fn id(index: u64) -> u64 {
    ino::of(1, u32::try_from(index).unwrap())
}

fn name(text: &str) -> nfsserve::nfs::filename3 {
    text.as_bytes().into()
}

fn code(status: nfsstat3) -> u32 {
    status as u32
}

/// How many times the core was asked for a blob's file.
fn blob_files(fs: &TreeNfs) -> usize {
    locked(&fs.core.served)
        .iter()
        .filter(|what| **what == "blob_file")
        .count()
}

/// Asserts that a name is found in a directory of a tree, in the root of a tree, and in the
/// namespace; and that a name that is not there, and one that is not text, are `NFS3ERR_NOENT`.
#[tokio::test]
async fn lookup_finds_a_name_in_a_tree_and_misses_with_noent() {
    let (rig, root) = attached("nfs-fs-lookup");
    let fs = fs_of(&rig, &NfsSettings::default(), endless());
    assert_eq!(
        fs.lookup(ino::ROOT, &name("trees")).await.unwrap(),
        ino::TREES
    );
    let tree = fs.lookup(ino::TREES, &name(&root)).await.unwrap();
    assert_eq!(tree, ino::of(1, 0));
    assert_eq!(fs.lookup(tree, &name("bin")).await.unwrap(), id(BIN));
    assert_eq!(fs.lookup(id(BIN), &name("tool")).await.unwrap(), id(TOOL));
    for (dir, missing) in [
        (tree, "nothing"),
        (ino::ROOT, "nothing"),
        (ino::TREES, "0".repeat(64).as_str()),
    ] {
        let status = failed(fs.lookup(dir, &name(missing)).await);
        assert_eq!(status, code(nfsstat3::NFS3ERR_NOENT), "{missing}");
    }
    let not_text = nfsserve::nfs::filename3::from(vec![0xff, 0xfe]);
    assert_eq!(
        failed(fs.lookup(tree, &not_text).await),
        code(nfsstat3::NFS3ERR_NOENT)
    );
    assert_eq!(
        failed(fs.lookup(id(BIG), &name("x")).await),
        code(nfsstat3::NFS3ERR_NOTDIR)
    );
}

/// Asserts that `.` is the directory itself and `..` its parent: the root of a tree has `trees/`
/// for its parent, a directory below it the directory above, `trees/` the mount's root, and the
/// root of the mount itself.
#[tokio::test]
async fn lookup_dotdot_of_a_tree_root_returns_to_trees_dir() {
    let (rig, _) = attached("nfs-fs-dotdot");
    let fs = fs_of(&rig, &NfsSettings::default(), endless());
    let up = |dir: u64| {
        let fs = &fs;
        async move { fs.lookup(dir, &name("..")).await.unwrap() }
    };
    assert_eq!(up(ino::of(1, 0)).await, ino::TREES);
    assert_eq!(up(id(BIN)).await, ino::of(1, 0));
    assert_eq!(up(id(CC)).await, id(BIN));
    assert_eq!(up(id(INCLUDE)).await, ino::of(1, 0));
    assert_eq!(up(ino::TREES).await, ino::ROOT);
    assert_eq!(up(ino::ROOT).await, ino::ROOT);
    assert_eq!(fs.lookup(id(BIN), &name(".")).await.unwrap(), id(BIN));
    assert_eq!(
        failed(fs.lookup(ino::of(1, 99), &name(".")).await),
        code(nfsstat3::NFS3ERR_STALE)
    );
}

/// Asserts the attributes of the entries: the fixed time for all three times, the mode the manifest
/// gives (0644, 0755 for an executable and for a directory), the owner `fs.owner`, the kind, the
/// size, the link count, and the inode number as the file id; the namespace's are root's and 0711.
#[tokio::test]
async fn getattr_reports_the_fixed_mtime_and_manifest_mode_and_owner() {
    let (rig, _) = attached("nfs-fs-getattr");
    let fs = fs_of(&rig, &NfsSettings::default(), endless());
    let cases = [
        (BIG, ftype3::NF3REG, 0o644, 300_000),
        (CC, ftype3::NF3REG, 0o755, 3000),
        (BIN, ftype3::NF3DIR, 0o755, 0),
        (LIB, ftype3::NF3LNK, 0o777, 3),
    ];
    for (index, kind, mode, size) in cases {
        let attr = fs.getattr(id(index)).await.unwrap();
        assert_eq!(
            (attr.ftype as u32, attr.mode, attr.size),
            (kind as u32, mode, size),
            "{index}"
        );
        assert_eq!((attr.uid, attr.gid), OWNER, "{index}");
        assert_eq!(attr.fileid, id(index));
        for time in [attr.atime, attr.mtime, attr.ctime] {
            assert_eq!((time.seconds, time.nseconds), (1_790_000_000, 0));
        }
    }
    assert_eq!(fs.getattr(id(BIG)).await.unwrap().used, 300_032);
    assert_eq!(fs.getattr(id(BIN)).await.unwrap().nlink, 2);
    let namespace = fs.getattr(ino::ROOT).await.unwrap();
    assert_eq!(
        (namespace.mode, namespace.uid, namespace.gid),
        (0o711, 0, 0)
    );
    assert_eq!(
        failed(fs.getattr(ino::of(0, 9)).await),
        code(nfsstat3::NFS3ERR_STALE)
    );
    assert_eq!(
        failed(fs.getattr(ino::of(1, 99)).await),
        code(nfsstat3::NFS3ERR_STALE)
    );
}

/// Asserts how a caller is put in a class of the mode bits: the owner, then the group (as the
/// primary group or one of the others), then everyone else.
#[test]
fn a_caller_gets_the_bits_of_the_first_class_it_is_in() {
    let attr = Attr {
        ino: 9,
        kind: Kind::File,
        size: 1,
        perm: 0o640,
        nlink: 1,
        uid: 10,
        gid: 20,
    };
    assert_eq!(class_bits(&attr, &auth(10, 99, &[])), 0o6);
    assert_eq!(class_bits(&attr, &auth(11, 20, &[])), 0o4);
    assert_eq!(class_bits(&attr, &auth(11, 99, &[5, 20])), 0o4);
    assert_eq!(class_bits(&attr, &auth(11, 99, &[5])), 0);
    assert_eq!(fattr(&attr).mode, 0o640);
}

/// Asserts that execute follows the mode bits: granted on an executable file and denied on a plain
/// one, for the owner of the entries and for another user alike (the bit is set for all three
/// classes), looked-up on a directory instead of executed, and that read is not granted on the
/// namespace, whose directories nobody lists.
#[tokio::test]
async fn access_grants_execute_only_where_a_mode_bit_has_it_for_the_caller_class() {
    let (rig, _) = attached("nfs-fs-access");
    let fs = fs_of(&rig, &NfsSettings::default(), endless());
    let everything = 0x3f;
    let owner = auth(OWNER.0, OWNER.1, &[]);
    let group = auth(UID, OWNER.1, &[]);
    let other = auth(UID, 99, &[]);
    for caller in [&owner, &group, &other] {
        let got = |index: u64| {
            let fs = &fs;
            async move {
                fs.access(id(index), Some(caller), everything)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(got(CC).await, ACCESS3_READ | ACCESS3_EXECUTE);
        assert_eq!(got(BIG).await, ACCESS3_READ);
        assert_eq!(got(BIN).await, ACCESS3_READ | ACCESS3_LOOKUP);
        assert_eq!(got(LIB).await, ACCESS3_READ | ACCESS3_EXECUTE);
        for namespace in [ino::ROOT, ino::TREES] {
            let granted = fs
                .access(namespace, Some(caller), everything)
                .await
                .unwrap();
            assert_eq!(granted, ACCESS3_LOOKUP, "{namespace}");
        }
    }
    assert_eq!(
        fs.access(id(CC), Some(&other), ACCESS3_EXECUTE)
            .await
            .unwrap(),
        ACCESS3_EXECUTE
    );
    assert_eq!(
        fs.access(id(CC), Some(&other), ACCESS3_LOOKUP)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        failed(fs.access(id(CC), None, everything).await),
        code(nfsstat3::NFS3ERR_ACCES)
    );
    assert_eq!(
        failed(fs.access(ino::of(1, 99), Some(&other), everything).await),
        code(nfsstat3::NFS3ERR_STALE)
    );
}

/// Asserts that modify, extend and delete are never granted, on any kind of entry, to any caller.
#[tokio::test]
async fn access_never_grants_modify_extend_delete() {
    let (rig, _) = attached("nfs-fs-access-write");
    let fs = fs_of(&rig, &NfsSettings::default(), endless());
    let write = ACCESS3_MODIFY | ACCESS3_EXTEND | ACCESS3_DELETE;
    for caller in [
        auth(OWNER.0, OWNER.1, &[]),
        auth(0, 0, &[]),
        auth(UID, 99, &[]),
    ] {
        for target in [id(BIG), id(CC), id(BIN), id(LIB), ino::ROOT, ino::TREES] {
            let granted = fs.access(target, Some(&caller), 0x3f).await.unwrap();
            assert_eq!(granted & write, 0, "{target}");
        }
    }
}

/// Asserts that a listing pages by the file id of the last entry: each page resumes after it
/// without repeating or skipping an entry, whatever the page size; a cookie that is not an entry
/// resumes at the next entry above it; the end is flagged with the last page and no sooner.
#[tokio::test]
async fn readdir_pages_by_cookie_and_resumes_without_repeating_or_skipping_entries() {
    let (rig, _) = attached("nfs-fs-readdir");
    let fs = fs_of(&rig, &NfsSettings::default(), endless());
    let root = ino::of(1, 0);
    let all = fs.readdir(root, 0, 100).await.unwrap();
    assert_eq!(names(&all), ["big.bin", "bin", "empty.h", "include", "lib"]);
    assert!(all.end);
    for page in 1..=6 {
        let (mut seen, mut after) = (Vec::new(), 0);
        loop {
            let listing = fs.readdir(root, after, page).await.unwrap();
            assert!(listing.entries.len() <= page);
            seen.extend(names(&listing));
            after = listing.entries.last().map_or(after, |entry| entry.fileid);
            if listing.end {
                break;
            }
            assert!(!listing.entries.is_empty());
        }
        assert_eq!(seen, names(&all), "pages of {page}");
    }
    let first = fs.readdir(root, 0, 2).await.unwrap();
    assert_eq!(
        (names(&first), first.end),
        (vec!["big.bin".to_owned(), "bin".to_owned()], false)
    );
    let exact = fs.readdir(root, 0, 5).await.unwrap();
    assert!(exact.end);
    let middle = fs.readdir(root, id(CC), 100).await.unwrap();
    assert_eq!(names(&middle), ["empty.h", "include", "lib"]);
    let past = fs.readdir(root, id(LIB), 100).await.unwrap();
    assert!(past.entries.is_empty() && past.end);
    let zero = fs.readdir(root, 0, 0).await.unwrap();
    assert_eq!(names(&zero), ["big.bin"]);
}

/// Asserts that every entry of a listing carries its attributes, the file id that is its inode
/// number and the handle the file id makes, so that READDIRPLUS needs no lookup after it.
#[tokio::test]
async fn readdirplus_carries_attributes_and_handles_for_every_entry() {
    let (rig, _) = attached("nfs-fs-plus");
    let fs = fs_of(&rig, &NfsSettings::default(), endless());
    let listing = fs.readdir(ino::of(1, 0), 0, 100).await.unwrap();
    assert_eq!(listing.entries.len(), 5);
    for entry in &listing.entries {
        let attr = fs.getattr(entry.fileid).await.unwrap();
        assert_eq!(entry.attr.fileid, entry.fileid);
        assert_eq!(
            (
                entry.attr.size,
                entry.attr.mode,
                entry.attr.nlink,
                entry.attr.ftype as u32
            ),
            (attr.size, attr.mode, attr.nlink, attr.ftype as u32)
        );
        assert_eq!(
            fs.fh_to_id(&fs.id_to_fh(entry.fileid)).unwrap(),
            entry.fileid
        );
    }
}

/// Asserts that the namespace's directories are not listed, by anyone, and that a file is not
/// listed either.
#[tokio::test]
async fn namespace_root_listing_is_refused_for_other_users() {
    let (rig, _) = attached("nfs-fs-namespace");
    let fs = fs_of(&rig, &NfsSettings::default(), endless());
    for dir in [ino::ROOT, ino::TREES] {
        assert_eq!(
            failed(fs.readdir(dir, 0, 10).await),
            code(nfsstat3::NFS3ERR_ACCES),
            "{dir}"
        );
    }
    assert_eq!(
        failed(fs.readdir(id(BIG), 0, 10).await),
        code(nfsstat3::NFS3ERR_NOTDIR)
    );
}

/// Asserts that a link reads as its target, and that a file and a directory do not.
#[tokio::test]
async fn readlink_returns_the_symlink_target() {
    let (rig, _) = attached("nfs-fs-readlink");
    let fs = fs_of(&rig, &NfsSettings::default(), endless());
    assert_eq!(fs.readlink(id(LIB)).await.unwrap().as_ref(), b"bin");
    assert_eq!(
        failed(fs.readlink(id(BIG)).await),
        code(nfsstat3::NFS3ERR_INVAL)
    );
    assert_eq!(
        failed(fs.readlink(id(BIN)).await),
        code(nfsstat3::NFS3ERR_INVAL)
    );
}

/// Asserts the bytes a read gives at any offset and count, the end-of-file flag (set when the read
/// reaches the end, and for one that starts at or past it), a count of zero, an empty file, and the
/// status for a directory and a link.
#[tokio::test]
async fn read_returns_bytes_at_offset_and_eof_flag() {
    let (rig, _) = attached("nfs-fs-read");
    let fs = fs_of(&rig, &NfsSettings::default(), endless());
    let big = blob_of(&rig, 300_000);
    let read = |offset: u64, count: u32| {
        let fs = &fs;
        async move { fs.read(id(BIG), offset, count).await.unwrap() }
    };
    assert_eq!(read(0, 100).await, (big[..100].to_vec(), false));
    assert_eq!(read(1000, 5).await, (big[1000..1005].to_vec(), false));
    assert_eq!(read(299_950, 100).await, (big[299_950..].to_vec(), true));
    assert_eq!(read(299_995, 5).await, (big[299_995..].to_vec(), true));
    assert_eq!(read(0, 300_000).await, (big.clone(), true));
    assert_eq!(read(0, u32::MAX).await, (big, true));
    assert_eq!(read(300_000, 10).await, (Vec::new(), true));
    assert_eq!(read(u64::MAX, 10).await, (Vec::new(), true));
    assert_eq!(read(5, 0).await, (Vec::new(), false));
    assert_eq!(fs.read(id(EMPTY), 0, 10).await.unwrap(), (Vec::new(), true));
    assert_eq!(
        failed(fs.read(id(BIN), 0, 10).await),
        code(nfsstat3::NFS3ERR_ISDIR)
    );
    assert_eq!(
        failed(fs.read(id(LIB), 0, 10).await),
        code(nfsstat3::NFS3ERR_INVAL)
    );
}

/// Asserts that a blob the cache lacks is filled once, through the fill socket, by the first read,
/// and that later reads, at other offsets, ask for nothing more.
#[tokio::test]
async fn read_of_a_cold_blob_fills_through_the_fill_socket_once() {
    let (rig, _) = attached("nfs-fs-cold");
    let fs = fs_of(&rig, &NfsSettings::default(), endless());
    let tool = blob_of(&rig, 10);
    let filled = rig.upstream.asked("fill").len();
    assert_eq!(
        fs.read(id(TOOL), 0, 100).await.unwrap(),
        (tool.clone(), true)
    );
    assert_eq!(rig.upstream.asked("fill").len(), filled + 1);
    assert_eq!(
        fs.read(id(TOOL), 2, 3).await.unwrap(),
        (tool[2..5].to_vec(), false)
    );
    assert_eq!(
        fs.read(id(TOOL), 9, 3).await.unwrap(),
        (tool[9..].to_vec(), true)
    );
    assert_eq!(rig.upstream.asked("fill").len(), filled + 1);
}

/// Asserts that a fill that fails is `NFS3ERR_IO`, and that nothing is kept for the file: the next
/// read asks again and succeeds once the upstream has the blob.
#[tokio::test]
async fn read_fill_failure_is_nfs3err_io() {
    let (rig, _) = attached("nfs-fs-fail");
    let fs = fs_of(&rig, &NfsSettings::default(), endless());
    let cc = {
        let built = rig.upstream.built.lock().unwrap();
        built
            .blobs
            .iter()
            .find(|(_, bytes)| bytes.len() == 3000)
            .map(|(sha, _)| sha.clone())
            .unwrap()
    };
    let _ = rig
        .upstream
        .failing
        .lock()
        .unwrap()
        .insert(cc.clone(), "no space".to_owned());
    assert_eq!(
        failed(fs.read(id(CC), 0, 10).await),
        code(nfsstat3::NFS3ERR_IO)
    );
    assert_eq!(fs.fds.len(), 0);
    let _ = rig.upstream.failing.lock().unwrap().remove(&cc);
    assert_eq!(fs.read(id(CC), 0, 4).await.unwrap().0.len(), 4);
    assert_eq!(fs.fds.len(), 1);
}

/// Asserts that a read that has waited out the patience for a fill is `NFS3ERR_JUKEBOX`, that the
/// fill goes on, and that the next read of the file is answered from the descriptor the fill left.
/// The patience is a future the test makes ready, and the fill waits on the fake socket's gate, so
/// no clock is read.
#[tokio::test]
async fn read_slower_than_the_jukebox_threshold_returns_jukebox() {
    let (rig, _) = attached("nfs-fs-jukebox");
    let fs = fs_of(&rig, &NfsSettings::default(), none());
    let tool = blob_of(&rig, 10);
    rig.upstream.hold_fills();
    let status = failed(fs.read(id(TOOL), 0, 100).await);
    rig.upstream.release_fills();
    assert_eq!(status, code(nfsstat3::NFS3ERR_JUKEBOX));
    until(|| fs.fds.len() == 1);
    assert_eq!(fs.read(id(TOOL), 0, 100).await.unwrap(), (tool, true));
    assert_eq!(blob_files(&fs), 1);
}

/// Asserts that every procedure that would change a tree is `NFS3ERR_ROFS`, on a file, a directory
/// and the namespace, and that none of them changes what a read gives.
#[tokio::test]
async fn every_write_procedure_returns_rofs() {
    let (rig, _) = attached("nfs-fs-rofs");
    let fs = fs_of(&rig, &NfsSettings::default(), endless());
    let rofs = code(nfsstat3::NFS3ERR_ROFS);
    assert!(matches!(fs.capabilities(), VFSCapabilities::ReadOnly));
    for target in [id(BIG), id(BIN), ino::ROOT] {
        assert_eq!(failed(fs.setattr(target, sattr3::default()).await), rofs);
        assert_eq!(failed(fs.write(target, 0, b"x").await), rofs);
        assert_eq!(
            failed(fs.create(target, &name("n"), sattr3::default()).await),
            rofs
        );
        assert_eq!(failed(fs.create_exclusive(target, &name("n")).await), rofs);
        assert_eq!(failed(fs.mkdir(target, &name("n")).await), rofs);
        assert_eq!(failed(fs.remove(target, &name("n")).await), rofs);
        assert_eq!(
            failed(fs.rename(target, &name("a"), id(BIN), &name("b")).await),
            rofs
        );
        let link = fs
            .symlink(target, &name("l"), &name("t"), &sattr3::default())
            .await;
        assert_eq!(failed(link), rofs);
    }
    assert_eq!(
        fs.read(id(BIG), 0, 3).await.unwrap().0,
        blob_of(&rig, 300_000)[..3]
    );
}

/// Asserts the constants of the file system: the root, the properties FSINFO reports (links to
/// files are not there; times are not set), and a cookie verifier that is the same for every start.
#[tokio::test]
async fn the_file_system_reports_constants() {
    let (rig, _) = attached("nfs-fs-constants");
    let fs = fs_of(&rig, &NfsSettings::default(), endless());
    assert_eq!(fs.root_dir(), ino::ROOT);
    assert_eq!(fs.serverid(), *b"tierstfs");
    let info = fs.fsinfo(ino::ROOT).await.unwrap();
    assert_eq!(
        info.properties,
        nfsserve::nfs::FSF_SYMLINK | nfsserve::nfs::FSF_HOMOGENEOUS
    );
    assert!(matches!(info.obj_attributes, post_op_attr::attributes(_)));
    let missing = fs.fsinfo(ino::of(9, 9)).await.unwrap();
    assert!(matches!(missing.obj_attributes, post_op_attr::Void));
}

/// Asserts that a read of a file already read is answered from the cached descriptor: the core is
/// asked for the blob's file once, for any number of reads.
#[tokio::test]
async fn fd_cache_reuses_a_descriptor_for_repeated_reads_of_one_blob() {
    let (rig, _) = attached("nfs-fs-fd-reuse");
    let fs = fs_of(&rig, &NfsSettings::default(), endless());
    for offset in [0, 10, 20, 30] {
        let _ = fs.read(id(BIG), offset, 10).await.unwrap();
    }
    assert_eq!(blob_files(&fs), 1);
    assert_eq!(fs.fds.len(), 1);
}

/// Asserts that a shard that is full closes its oldest descriptor, and that a read of that file
/// opens it again and gives the same bytes.
#[tokio::test]
async fn fd_cache_evicts_within_a_shard_and_reopens_correctly() {
    let (rig, _) = attached("nfs-fs-fd-evict");
    let settings = NfsSettings {
        fd_cache_shards: 1,
        fd_cache_per_shard: 2,
        ..NfsSettings::default()
    };
    let fs = fs_of(&rig, &settings, endless());
    let big = blob_of(&rig, 300_000);
    for index in [BIG, CC, TOOL] {
        let _ = fs.read(id(index), 0, 10).await.unwrap();
    }
    assert_eq!((blob_files(&fs), fs.fds.len()), (3, 2));
    assert_eq!(fs.read(id(TOOL), 0, 10).await.unwrap().0, b"tool bytes");
    assert_eq!(blob_files(&fs), 3);
    assert_eq!(fs.read(id(BIG), 100, 10).await.unwrap().0, big[100..110]);
    assert_eq!((blob_files(&fs), fs.fds.len()), (4, 2));
}

/// Asserts the status each failure of the core is answered with, and that a handle the core does
/// not know is stale where a name it does not know is missing.
#[test]
fn each_failure_of_the_core_has_its_nfs_status() {
    let cases = [
        (
            Errno::NoEnt,
            nfsstat3::NFS3ERR_NOENT,
            nfsstat3::NFS3ERR_STALE,
        ),
        (
            Errno::Acces,
            nfsstat3::NFS3ERR_ACCES,
            nfsstat3::NFS3ERR_ACCES,
        ),
        (
            Errno::NotDir,
            nfsstat3::NFS3ERR_NOTDIR,
            nfsstat3::NFS3ERR_NOTDIR,
        ),
        (
            Errno::IsDir,
            nfsstat3::NFS3ERR_ISDIR,
            nfsstat3::NFS3ERR_ISDIR,
        ),
        (
            Errno::Inval,
            nfsstat3::NFS3ERR_INVAL,
            nfsstat3::NFS3ERR_INVAL,
        ),
        (Errno::Io, nfsstat3::NFS3ERR_IO, nfsstat3::NFS3ERR_IO),
        (Errno::RoFs, nfsstat3::NFS3ERR_ROFS, nfsstat3::NFS3ERR_ROFS),
    ];
    for (errno, by_name, by_handle) in cases {
        assert_eq!(code(status(errno)), code(by_name), "{errno:?}");
        assert_eq!(code(handled(errno)), code(by_handle), "{errno:?}");
    }
}
