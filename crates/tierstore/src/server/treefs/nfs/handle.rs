//! The file handle: the eight bytes of the core's inode number and the four bytes of the epoch of
//! the mount, and nothing else. A handle carries no start time and no generation, so that one
//! issued before a restart names the same entry after it (invariant I1,
//! `docs/design/virtual-build-fs.md` sections 6.3 and 7.5); one of another boot's mount is stale.

use nfsserve::nfs::{fileid3, nfs_fh3, nfsstat3};

/// The length of the inode number in a handle.
const INO: usize = 8;

/// The length of a handle.
const LENGTH: usize = INO + 4;

/// The handle of the inode `ino` of the mount of `epoch`.
pub(super) fn of(ino: fileid3, epoch: u32) -> nfs_fh3 {
    let mut data = ino.to_le_bytes().to_vec();
    data.extend_from_slice(&epoch.to_le_bytes());
    nfs_fh3 { data }
}

/// The inode number a handle of the mount of `epoch` holds.
///
/// # Errors
/// `NFS3ERR_BADHANDLE` for a handle that is not twelve bytes; `NFS3ERR_STALE` for one of another
/// epoch.
pub(super) fn ino_of(handle: &nfs_fh3, epoch: u32) -> Result<fileid3, nfsstat3> {
    let bytes: [u8; LENGTH] = handle
        .data
        .as_slice()
        .try_into()
        .map_err(|_| nfsstat3::NFS3ERR_BADHANDLE)?;
    let (ino, issued) = bytes.split_at(INO);
    if issued != epoch.to_le_bytes() {
        return Err(nfsstat3::NFS3ERR_STALE);
    }
    let mut number = [0u8; INO];
    number.copy_from_slice(ino);
    Ok(u64::from_le_bytes(number))
}

#[cfg(test)]
#[path = "handle_tests.rs"]
mod tests;
