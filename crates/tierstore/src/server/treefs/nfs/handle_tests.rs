//! The file handle: what it holds, what a handle of another length or another tree is, that one
//! issued by a core is read by another built the same way, that a handle of a release before is
//! still read, and how the sums of the handles' bytes spread.

use std::collections::BTreeMap;

use nfsserve::nfs::{nfs_fh3, nfsstat3};
use nfsserve::vfs::NFSFileSystem as _;

use super::{LENGTH, NAMED, SPREAD, ino_of, of};
use crate::server::treefs::ino;
use crate::server::treefs::nfs::config::NfsSettings;
use crate::server::treefs::nfs::fixtures::{EPOCH, RUN, attached, endless, failed, fs_of, fs_over};

/// The sum of the bytes of `data`: what the macOS NFS client hashes a handle by.
fn byte_sum(data: &[u8]) -> u64 {
    data.iter().map(|byte| u64::from(*byte)).sum()
}

/// Asserts that a handle is the eight bytes of the inode number, the four of the epoch and
/// fifty-two more, sixty-four in all (NFSv3's largest), that the fifty-two are 255s, one byte of
/// the rest and 0s, and that the handle reads back as the inode number.
#[test]
fn handle_is_the_core_ino_the_mount_epoch_and_the_spread() {
    let id = ino::of(3, 77);
    let handle = of(id, EPOCH);
    assert_eq!((LENGTH, NAMED, SPREAD), (64, 12, 52));
    assert_eq!(handle.data.len(), 64);
    assert_eq!(handle.data[..8], id.to_le_bytes());
    assert_eq!(handle.data[8..12], EPOCH.to_le_bytes());
    let tail = &handle.data[12..];
    let full = tail.iter().take_while(|byte| **byte == 255).count();
    assert!(tail[full + 1..].iter().all(|byte| *byte == 0), "{tail:?}");
    assert_eq!(ino_of(&handle, EPOCH).unwrap(), id);
    assert_eq!(of(id, EPOCH).data, handle.data);
}

/// Asserts that a handle of the twelve bytes alone, as a release before this one gave out, names
/// the same inode, and is stale for another epoch as a handle of this release is.
#[test]
fn a_handle_of_the_release_before_names_the_same_inode() {
    let id = ino::of(5, 1234);
    let mut data = id.to_le_bytes().to_vec();
    data.extend_from_slice(&EPOCH.to_le_bytes());
    let before = nfs_fh3 { data };
    assert_eq!(ino_of(&before, EPOCH).unwrap(), id);
    assert_eq!(
        failed(ino_of(&before, EPOCH ^ 1)),
        nfsstat3::NFS3ERR_STALE as u32
    );
    assert_eq!(
        failed(ino_of(&of(id, EPOCH), EPOCH ^ 1)),
        nfsstat3::NFS3ERR_STALE as u32
    );
}

/// Asserts that a handle whose last fifty-two bytes are not those of its inode number and epoch is
/// `NFS3ERR_BADHANDLE`: a byte of the tail changed, and the tail of another inode's handle.
#[test]
fn a_handle_whose_spread_is_not_its_own_is_badhandle() {
    let id = ino::of(3, 77);
    let mut changed = of(id, EPOCH);
    changed.data[63] ^= 1;
    assert_eq!(
        failed(ino_of(&changed, EPOCH)),
        nfsstat3::NFS3ERR_BADHANDLE as u32
    );
    let mut swapped = of(id, EPOCH);
    swapped.data[12..].copy_from_slice(&of(ino::of(3, 78), EPOCH).data[12..]);
    assert_ne!(swapped.data, of(id, EPOCH).data);
    assert_eq!(
        failed(ino_of(&swapped, EPOCH)),
        nfsstat3::NFS3ERR_BADHANDLE as u32
    );
}

/// Asserts that the sums of the bytes of the handles of sixteen trees of 16384 entries each take
/// more than 10000 values with no sum shared by more than 100 handles, where the twelve bytes alone
/// sum to fewer than 1000 values, one of them shared by more than 1000 handles: the spread that
/// keeps the macOS client's chains of nodes short.
#[test]
fn the_sums_of_the_handles_spread_over_thousands_of_values() {
    let (mut spread, mut named) = (BTreeMap::new(), BTreeMap::new());
    for tree in 1..=16 {
        for index in 0..16384 {
            let handle = of(ino::of(tree, index), EPOCH);
            *spread.entry(byte_sum(&handle.data)).or_insert(0u64) += 1;
            *named.entry(byte_sum(&handle.data[..12])).or_insert(0u64) += 1;
        }
    }
    let most = |sums: &BTreeMap<u64, u64>| sums.values().copied().max().unwrap_or(0);
    assert!(spread.len() > 10_000, "{}", spread.len());
    assert!(most(&spread) <= 100, "{}", most(&spread));
    assert!(named.len() < 1000, "{}", named.len());
    assert!(most(&named) > 1000, "{}", most(&named));
}

/// Asserts that a handle of the mount of another epoch (a boot that is gone) is `NFS3ERR_STALE`,
/// for every call that takes one, and that the same inode number of this epoch is served.
#[tokio::test]
async fn a_handle_of_another_mount_id_is_stale() {
    let (rig, _) = attached("nfs-handle-epoch");
    let fs = fs_of(&rig, &NfsSettings::default(), endless());
    let stale = nfsstat3::NFS3ERR_STALE as u32;
    let earlier = of(ino::ROOT, EPOCH ^ 1);
    assert_eq!(failed(fs.fh_to_id(&earlier)), stale);
    assert_eq!(fs.fh_to_id(&of(ino::ROOT, EPOCH)).unwrap(), ino::ROOT);
    assert_eq!(fs.fh_to_id(&fs.id_to_fh(ino::TREES)).unwrap(), ino::TREES);
}

/// Asserts that a handle of any length but sixty-four and twelve is `NFS3ERR_BADHANDLE`.
#[test]
fn handle_with_wrong_length_is_badhandle() {
    for length in [0, 7, 8, 9, 11, 13, 16, 63, 65] {
        let handle = nfs_fh3 {
            data: vec![1; length],
        };
        assert_eq!(
            failed(ino_of(&handle, EPOCH)),
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
