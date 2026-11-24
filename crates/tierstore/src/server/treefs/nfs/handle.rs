//! The file handle: the eight bytes of the core's inode number, the four bytes of the epoch of the
//! mount, and fifty-two bytes made from those twelve alone ([`SPREAD`]). A handle carries no start
//! time and no generation, so that one issued before a restart names the same entry after it
//! (invariant I1, `docs/design/virtual-build-fs.md` sections 6.3 and 7.5); one of another boot's
//! mount is stale.
//!
//! The fifty-two bytes are there for the macOS NFS client, which finds the node of a handle in a
//! hash table keyed by the sum of the handle's bytes (`nfs_hash` in Apple's `nfs_node.c`). The
//! twelve bytes alone sum to a few hundred values for all the entries of a tree, and the trees of
//! the mount to overlapping ranges of them, so the client's nodes fall into a few hundred buckets
//! and each lookup of an entry it does not have walks a chain of hundreds of nodes. The extra bytes
//! add to the sum a number from 0 to `52 * 255` taken from a hash of the twelve, so the sums of a
//! mount's handles spread evenly over thousands of buckets. They are a function of the inode
//! number and the epoch, so a handle is still the same for the same entry in every process of the
//! boot.
//!
//! A handle of the twelve bytes alone, as a release before this one gave out, is still read: a
//! client that holds one across an update names the same entry with it. The other way does not
//! hold: a release before this one answers `NFS3ERR_BADHANDLE` to the sixty-four bytes, so a
//! process of it must never take over a mount this release served. On a Mac an update (and so a
//! rollback) unmounts before it starts the other release, whose mount has a new epoch, and no Mac
//! hands its socket over (`fs.nfs.handoff_socket`); a hand-over back to a release before this one
//! is the one way to meet it. [`SPREAD`] and [`mix`] are part of the handle: a change to either
//! makes every handle a client holds a bad one.

use nfsserve::nfs::{fileid3, nfs_fh3, nfsstat3};

/// The length of the inode number in a handle.
const INO: usize = 8;

/// The length of the inode number and the epoch: a whole handle of a release before this one.
const NAMED: usize = INO + 4;

/// The bytes after the inode number and the epoch that spread the sums of the handles.
const SPREAD: usize = 52;

/// The length of a handle: NFSv3's largest (`NFS3_FHSIZE`).
const LENGTH: usize = NAMED + SPREAD;

/// The handle of the inode `ino` of the mount of `epoch`.
pub(super) fn of(ino: fileid3, epoch: u32) -> nfs_fh3 {
    let mut data = Vec::with_capacity(LENGTH);
    data.extend_from_slice(&ino.to_le_bytes());
    data.extend_from_slice(&epoch.to_le_bytes());
    data.extend_from_slice(&spread(ino, epoch));
    nfs_fh3 { data }
}

/// The inode number a handle of the mount of `epoch` holds.
///
/// # Errors
/// `NFS3ERR_BADHANDLE` for a handle that is neither sixty-four bytes nor the twelve of a release
/// before, or whose last fifty-two bytes are not those of its inode number and epoch;
/// `NFS3ERR_STALE` for one of another epoch.
pub(super) fn ino_of(handle: &nfs_fh3, epoch: u32) -> Result<fileid3, nfsstat3> {
    let data = handle.data.as_slice();
    if data.len() != LENGTH && data.len() != NAMED {
        return Err(nfsstat3::NFS3ERR_BADHANDLE);
    }
    let (ino, rest) = data.split_at(INO);
    let (issued, tail) = rest.split_at(NAMED - INO);
    if issued != epoch.to_le_bytes() {
        return Err(nfsstat3::NFS3ERR_STALE);
    }
    let mut number = [0u8; INO];
    number.copy_from_slice(ino);
    let ino = u64::from_le_bytes(number);
    if !tail.is_empty() && tail != spread(ino, epoch) {
        return Err(nfsstat3::NFS3ERR_BADHANDLE);
    }
    Ok(ino)
}

/// The fifty-two bytes of the handle of `ino` of the mount of `epoch`: as many 255s as fit in a
/// number from 0 to `52 * 255` taken from a hash of the two, then the rest of the number, then 0s,
/// so that the bytes sum to that number.
fn spread(ino: fileid3, epoch: u32) -> [u8; SPREAD] {
    let most = (SPREAD * 255) as u64;
    let mut left = mix(ino ^ (u64::from(epoch) << 32)) % (most + 1);
    let mut bytes = [0u8; SPREAD];
    for byte in &mut bytes {
        let here = left.min(255);
        // At most 255.
        *byte = here as u8;
        left -= here;
    }
    bytes
}

/// SplitMix64's finalizer: every bit of `x` moves every bit of the answer.
fn mix(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

#[cfg(test)]
#[path = "handle_tests.rs"]
mod tests;
