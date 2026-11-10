//! The file handle: the eight bytes of the core's inode number, and nothing else. A handle carries
//! no start time and no generation, so that one issued before a restart names the same entry after
//! it (invariant I1, `docs/design/virtual-build-fs.md` section 6.3).

use nfsserve::nfs::{fileid3, nfs_fh3, nfsstat3};

/// The length of a handle.
const LENGTH: usize = 8;

/// The handle of the inode `ino`.
pub(super) fn of(ino: fileid3) -> nfs_fh3 {
    nfs_fh3 {
        data: ino.to_le_bytes().to_vec(),
    }
}

/// The inode number a handle holds.
///
/// # Errors
/// `NFS3ERR_BADHANDLE` for a handle that is not eight bytes.
pub(super) fn ino_of(handle: &nfs_fh3) -> Result<fileid3, nfsstat3> {
    let bytes: [u8; LENGTH] = handle
        .data
        .as_slice()
        .try_into()
        .map_err(|_| nfsstat3::NFS3ERR_BADHANDLE)?;
    Ok(u64::from_le_bytes(bytes))
}

#[cfg(test)]
#[path = "handle_tests.rs"]
mod tests;
