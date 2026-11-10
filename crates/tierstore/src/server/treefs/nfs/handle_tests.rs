//! The file handle: what it holds, what a handle of another length or another tree is, and that
//! one issued by a core is read by another built the same way.

use nfsserve::nfs::{nfs_fh3, nfsstat3};
use nfsserve::vfs::NFSFileSystem as _;

use super::{ino_of, of};
use crate::server::treefs::ino;
use crate::server::treefs::nfs::config::NfsSettings;
use crate::server::treefs::nfs::fixtures::{RUN, attached, endless, failed, fs_of, fs_over};

/// Asserts that a handle is the eight bytes of the inode number, and reads back as it.
#[test]
fn handle_is_eight_bytes_of_the_core_ino() {
    let id = ino::of(3, 77);
    let handle = of(id);
    assert_eq!(handle.data, id.to_le_bytes());
    assert_eq!(handle.data.len(), 8);
    assert_eq!(ino_of(&handle).unwrap(), id);
}

/// Asserts that a handle of any other length is `NFS3ERR_BADHANDLE`.
#[test]
fn handle_with_wrong_length_is_badhandle() {
    for length in [0, 7, 9, 16] {
        let handle = nfs_fh3 {
            data: vec![1; length],
        };
        assert_eq!(
            failed(ino_of(&handle)),
            nfsstat3::NFS3ERR_BADHANDLE as u32,
            "{length}"
        );
    }
}

/// Asserts that a handle of a tree the table does not know is `NFS3ERR_STALE` for every call that
/// takes one, and not a missing name.
#[tokio::test]
async fn handle_of_an_unknown_tree_is_stale() {
    let (rig, _) = attached("nfs-handle-stale");
    let fs = fs_of(&rig, &NfsSettings::default(), endless());
    let stale = nfsstat3::NFS3ERR_STALE as u32;
    let gone = ino::of(9, 1);
    assert_eq!(failed(fs.getattr(gone).await), stale);
    assert_eq!(failed(fs.read(gone, 0, 10).await), stale);
    assert_eq!(failed(fs.readlink(gone).await), stale);
    assert_eq!(failed(fs.lookup(gone, &b"x"[..].into()).await), stale);
    assert_eq!(failed(fs.lookup(gone, &b".."[..].into()).await), stale);
    assert_eq!(failed(fs.readdir(gone, 0, 10).await), stale);
}

/// Asserts that a handle issued by one core names the same entry in a core that is built over the
/// same cache and has the same tree attached first (so the table numbers it alike), as a restart
/// makes it.
#[tokio::test]
async fn handle_of_an_attached_tree_entry_round_trips_to_the_same_entry_after_a_fresh_core_is_built_from_the_same_table()
 {
    let (rig, root) = attached("nfs-handle-round");
    let first = fs_of(&rig, &NfsSettings::default(), endless());
    let path = format!("/trees/{root}/bin/tool");
    let tool = first.path_to_id(path.as_bytes()).await.unwrap();
    let handle = first.id_to_fh(tool);
    let before = first
        .getattr(first.fh_to_id(&handle).unwrap())
        .await
        .unwrap();

    let core = rig.second_core();
    let _ = core.attach(&root, RUN).unwrap();
    let second = fs_over(core, endless());
    let after = second
        .getattr(second.fh_to_id(&handle).unwrap())
        .await
        .unwrap();
    assert_eq!(
        (after.fileid, after.size, after.mode, after.uid),
        (before.fileid, before.size, before.mode, before.uid)
    );
    assert_eq!(before.size, 10);
    let (bytes, end) = second.read(tool, 0, 100).await.unwrap();
    assert_eq!((bytes.as_slice(), end), (&b"tool bytes"[..], true));
}
