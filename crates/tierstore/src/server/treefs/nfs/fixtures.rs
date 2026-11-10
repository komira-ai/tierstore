//! What the tests of the NFS frontend share: a rig with the sample tree attached, a file system
//! over its core, and credentials.

use std::collections::HashSet;
use std::io::Cursor;
use std::sync::Arc;

use nfsserve::auth_unix;
use nfsserve::xdr::XDR as _;

use super::config::NfsSettings;
use super::fs::{Patience, TreeNfs};
use crate::server::treefs::testing::{Rig, sample};

/// The run the sample tree is attached for.
pub(super) const RUN: &str = "run-1";

/// The user id the tests serve.
pub(super) const UID: u32 = 4242;

/// The epoch of the mount the tests serve.
pub(super) const EPOCH: u32 = 0x5eed_0001;

/// The bytes of the handle of `ino` in the mount of [`EPOCH`].
pub(super) fn handle_of(ino: u64) -> Vec<u8> {
    [ino.to_le_bytes().as_slice(), EPOCH.to_le_bytes().as_slice()].concat()
}

/// A patience that never runs out: no read is answered `NFS3ERR_JUKEBOX`.
pub(super) fn endless() -> Patience {
    Arc::new(|| Box::pin(std::future::pending()))
}

/// A patience that is out at once.
pub(super) fn none() -> Patience {
    Arc::new(|| Box::pin(std::future::ready(())))
}

/// A rig whose core counts no lookups, with the sample tree attached; the tree's digest.
pub(super) fn attached(name: &str) -> (Rig, String) {
    let rig = Rig::uncounted(name, sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = rig.core.attach(&root, RUN).unwrap();
    (rig, root)
}

/// A file system over the core of `rig`, for the user [`UID`].
pub(super) fn fs_of(rig: &Rig, settings: &NfsSettings, patience: Patience) -> TreeNfs {
    TreeNfs::new(
        Arc::clone(&rig.core),
        settings,
        HashSet::from([UID]),
        patience,
        EPOCH,
    )
}

/// The credential of a call by `uid` and `gid`, and the groups `gids`, as the client sends it.
pub(super) fn auth(uid: u32, gid: u32, gids: &[u32]) -> auth_unix {
    let mut bytes = Vec::new();
    // The stamp, an empty machine name, the user, the group and the other groups.
    for word in [0, 0, uid, gid, u32::try_from(gids.len()).unwrap()] {
        bytes.extend_from_slice(&word.to_be_bytes());
    }
    for gid in gids {
        bytes.extend_from_slice(&gid.to_be_bytes());
    }
    let mut credential = auth_unix::default();
    credential.deserialize(&mut Cursor::new(bytes)).unwrap();
    credential
}

/// The code of the status a call failed with.
pub(super) fn failed<T: std::fmt::Debug>(result: Result<T, nfsserve::nfs::nfsstat3>) -> u32 {
    result.expect_err("the call fails") as u32
}

/// A file system over `core`, for the user [`UID`].
pub(super) fn fs_over(core: Arc<crate::server::treefs::core::Core>, patience: Patience) -> TreeNfs {
    TreeNfs::new(
        core,
        &NfsSettings::default(),
        HashSet::from([UID]),
        patience,
        EPOCH,
    )
}

/// The bytes of the blob of `length` bytes that the rig's upstream holds.
pub(super) fn blob_of(rig: &Rig, length: usize) -> Vec<u8> {
    let built = rig.upstream.built.lock().unwrap();
    built
        .blobs
        .values()
        .find(|bytes| bytes.len() == length)
        .cloned()
        .unwrap()
}

/// The names of the entries of a listing.
pub(super) fn names(listing: &nfsserve::vfs::ReadDirResult) -> Vec<String> {
    listing
        .entries
        .iter()
        .map(|entry| String::from_utf8_lossy(entry.name.as_ref()).into_owned())
        .collect()
}

/// Settings over the scratch directories of a rig; the NFS table is `nfs`.
pub(super) fn settings(rig: &Rig, nfs: NfsSettings) -> crate::server::treefs::config::FsSettings {
    crate::server::treefs::config::FsSettings {
        nfs,
        ..rig.settings()
    }
}

/// The NFS settings the tests of the process use: any free loopback port, no mount user.
pub(super) fn loopback() -> NfsSettings {
    NfsSettings {
        listen: "127.0.0.1:0".parse().unwrap(),
        mount_wait: std::time::Duration::from_secs(3),
        mount_retry: std::time::Duration::from_secs(1),
        ..NfsSettings::default()
    }
}
