//! The NFSv3 file system over the core: [`TreeNfs`] answers the procedures `nfsserve` decodes from
//! the inode numbers of the core (`docs/design/virtual-build-fs.md` sections 6.3 and 8).
//!
//! Everything is read-only: each procedure that would change a tree is answered `NFS3ERR_ROFS`.
//! Every entry reports one fixed, old time, so that the client's caches never revalidate. A file's
//! bytes are read from the cache file of its blob, which the core fills on the first read; the
//! open cache files are kept in a sharded cache, and a read that waits longer than
//! `fs.nfs.jukebox_after` for a fill is answered `NFS3ERR_JUKEBOX`, which has the client ask
//! again, while the fill goes on.
//!
//! Every call of `nfsserve` into the file system is timed from its start to its end, under its
//! kind (`core/requests.rs`): `lookup`, `getattr`, `access`, `read`, `readdir` (READDIR and
//! READDIRPLUS both list through it), `readlink`, and `other` for FSINFO and the changes refused.
//! The attributes the protocol layer reads for a procedure of another kind are calls of their own
//! (`getattr`). What `nfsserve` answers with no call (NULL, an unknown procedure, a change it
//! refuses itself on a read-only file system) is not timed.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nfsserve::auth_unix;
use nfsserve::nfs::{
    ACCESS3_EXECUTE, ACCESS3_LOOKUP, ACCESS3_READ, FSF_HOMOGENEOUS, FSF_SYMLINK, fattr3, fileid3,
    filename3, fsinfo3, ftype3, nfs_fh3, nfspath3, nfsstat3, nfstime3, post_op_attr, sattr3,
    specdata3,
};
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};
use tokio::sync::watch;

use super::config::NfsSettings;
use super::fdcache::FdCache;
use super::handle;
use crate::server::local_cache::opens::requests::RequestKind;
use crate::server::treefs::core::{Attr, Core, Errno, FIXED_MTIME, FileBytes, Kind, Looked};
use crate::server::treefs::{ino, locked};

/// A future that is ready when a read has waited long enough for a fill; one is made for each read
/// that has to fill. The process makes it a timer, a test makes it ready or never.
pub(super) type Patience = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// The state of a fill: not done, or the open file or why there is none.
type Fill = Option<Result<Arc<FileBytes>, nfsstat3>>;

/// The cookie verifier of every listing: the same for every start, so that a client's cookies
/// outlive a restart.
const SERVER_ID: [u8; 8] = *b"tierstfs";

/// The block a file's `used` bytes are counted in.
const BLOCK: u64 = 512;

/// The NFS status for a failure of the core.
fn status(errno: Errno) -> nfsstat3 {
    match errno {
        Errno::NoEnt => nfsstat3::NFS3ERR_NOENT,
        Errno::Acces => nfsstat3::NFS3ERR_ACCES,
        Errno::NotDir => nfsstat3::NFS3ERR_NOTDIR,
        Errno::IsDir => nfsstat3::NFS3ERR_ISDIR,
        Errno::Inval => nfsstat3::NFS3ERR_INVAL,
        Errno::Io => nfsstat3::NFS3ERR_IO,
        #[cfg(any(target_os = "linux", test))]
        Errno::RoFs => nfsstat3::NFS3ERR_ROFS,
    }
}

/// The NFS status for a failure of the core on an inode that a client had a handle of: an inode
/// the core does not have is a stale handle (a handle of a mount of an earlier boot, or of a tree
/// the table does not know), not a missing name.
fn handled(errno: Errno) -> nfsstat3 {
    match errno {
        Errno::NoEnt => nfsstat3::NFS3ERR_STALE,
        other => status(other),
    }
}

/// The time every entry reports.
fn fixed_time() -> nfstime3 {
    nfstime3 {
        seconds: u32::try_from(FIXED_MTIME).unwrap_or(u32::MAX),
        nseconds: 0,
    }
}

/// The attributes of an inode as NFS reports them.
fn fattr(attr: &Attr) -> fattr3 {
    let (ftype, used) = match attr.kind {
        Kind::Dir => (ftype3::NF3DIR, 0),
        Kind::File => (ftype3::NF3REG, attr.size.div_ceil(BLOCK) * BLOCK),
        Kind::Symlink => (ftype3::NF3LNK, 0),
    };
    let time = fixed_time();
    fattr3 {
        ftype,
        mode: u32::from(shown(attr)),
        nlink: attr.nlink,
        uid: attr.uid,
        gid: attr.gid,
        size: attr.size,
        used,
        rdev: specdata3::default(),
        fsid: 1,
        fileid: attr.ino,
        atime: time,
        mtime: time,
        ctime: time,
    }
}

/// The mode of a tree entry as NFS presents it: the permission bits with the owner's read and
/// execute also set for the group and for everyone else, and no setuid, setgid or sticky bit; the
/// write bits are never added. The mount is read-only and refuses every write; a tree is readable
/// by anyone who knows its digest; the server knows no job user (a job's user changes from job to
/// job), so an allow list or a per-user owner would put users into the server; and a setuid file
/// owned by the service account must not run as it.
fn presented(perm: u16) -> u16 {
    let owner_rx = (perm >> 6) & 0o5;
    (perm & 0o777) | (owner_rx << 3) | owner_rx
}

/// The mode `attr` is served with: the namespace's directories (the mount's root, `trees/` and the
/// directories above the fs locations of the placements) as they are, so that they do not look
/// listable, and every entry of a tree as [`presented`].
fn shown(attr: &Attr) -> u16 {
    if namespace(attr.ino) {
        attr.perm
    } else {
        presented(attr.perm)
    }
}

/// The permission bits of `attr`, as [`shown`], for a caller who says it is `auth`: the owner's, the
/// group's, or everyone else's, as a file system picks them.
fn class_bits(attr: &Attr, auth: &auth_unix) -> u16 {
    let perm = shown(attr);
    let in_group = auth.gid() == attr.gid || auth.gids().contains(&attr.gid);
    if auth.uid() == attr.uid {
        (perm >> 6) & 0o7
    } else if in_group {
        (perm >> 3) & 0o7
    } else {
        perm & 0o7
    }
}

/// The bits of `requested` the user of `auth` is granted on the inode `id` of `core`: the answer
/// to ACCESS.
fn granted(
    core: &Core,
    id: fileid3,
    auth: Option<&auth_unix>,
    requested: u32,
) -> Result<u32, nfsstat3> {
    let auth = auth.ok_or(nfsstat3::NFS3ERR_ACCES)?;
    let attr = core.getattr(id).map_err(handled)?.attr;
    let bits = class_bits(&attr, auth);
    let mut granted = 0;
    // Nobody lists the namespace, whatever its mode says (the core's rule).
    if bits & 0o4 != 0 && !namespace(id) {
        granted |= ACCESS3_READ;
    }
    if bits & 0o1 != 0 {
        granted |= match attr.kind {
            Kind::Dir => ACCESS3_LOOKUP,
            _ => ACCESS3_EXECUTE,
        };
    }
    // Modify, extend and delete are never granted: the tree is read-only.
    Ok(granted & requested)
}

/// The NFS file system of the core.
pub(super) struct TreeNfs {
    core: Arc<Core>,
    fds: Arc<FdCache>,
    patience: Patience,
    /// The epoch of the mount, which its handles carry.
    epoch: u32,
    /// The fills that are on, by inode.
    filling: Arc<Mutex<HashMap<fileid3, watch::Receiver<Fill>>>>,
}

impl TreeNfs {
    /// The file system of `core`, the mount of `epoch`.
    pub(super) fn new(
        core: Arc<Core>,
        settings: &NfsSettings,
        patience: Patience,
        epoch: u32,
    ) -> Self {
        Self {
            core,
            fds: Arc::new(FdCache::new(
                settings.fd_cache_shards,
                settings.fd_cache_per_shard,
            )),
            patience,
            epoch,
            filling: Arc::default(),
        }
    }

    /// What `answer` gives, timed from now to its end under `kind` (`core/requests.rs`).
    fn timed<T>(&self, kind: RequestKind, answer: impl FnOnce() -> T) -> T {
        let arrival = self.core.request_begins();
        let answered = answer();
        self.core.request_ends(arrival, kind);
        answered
    }

    /// A change of a tree, refused (`NFS3ERR_ROFS`) and timed as `other`.
    fn refused<T>(&self) -> Result<T, nfsstat3> {
        self.timed(RequestKind::Other, || -> Result<T, nfsstat3> {
            Err(nfsstat3::NFS3ERR_ROFS)
        })
    }

    /// The core's answer to a lookup of `name` in `dirid`. A lookup in the namespace may read a
    /// tree that no run has attached through the cache (`core/implicit.rs`), or ask it where
    /// a path is among the placements (`core/placed.rs`), so it runs on a blocking thread, as a
    /// fill for a read does, and holds up no other request.
    async fn looked(&self, dirid: fileid3, name: &str) -> Result<Looked, Errno> {
        if !namespace(dirid) {
            return self.core.lookup(dirid, name);
        }
        let (core, name) = (Arc::clone(&self.core), name.to_owned());
        tokio::task::spawn_blocking(move || core.lookup(dirid, &name))
            .await
            .unwrap_or(Err(Errno::Io))
    }

    /// The answer to LOOKUP of `filename` in `dirid`.
    async fn looked_up(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let Ok(name) = std::str::from_utf8(filename.as_ref()) else {
            return Err(nfsstat3::NFS3ERR_NOENT);
        };
        match name {
            "." => self
                .core
                .getattr(dirid)
                .map(|entry| entry.attr.ino)
                .map_err(handled),
            ".." => self.core.parent(dirid).map_err(handled),
            _ => match self.looked(dirid, name).await {
                Ok(Looked::Found(entry)) => Ok(entry.attr.ino),
                Ok(Looked::Absent(_)) => Err(nfsstat3::NFS3ERR_NOENT),
                // A name the namespace lacks is a missing name; a directory the core lacks is a
                // stale handle.
                Err(Errno::NoEnt) if namespace(dirid) => Err(nfsstat3::NFS3ERR_NOENT),
                Err(errno) => Err(handled(errno)),
            },
        }
    }

    /// The answer to READ of `count` bytes of `id` from `offset`.
    async fn read_bytes(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        let attr = self.core.getattr(id).map_err(handled)?.attr;
        match attr.kind {
            Kind::Dir => return Err(nfsstat3::NFS3ERR_ISDIR),
            Kind::Symlink => return Err(nfsstat3::NFS3ERR_INVAL),
            Kind::File => {}
        }
        let wanted = attr.size.saturating_sub(offset).min(u64::from(count));
        if wanted == 0 {
            return Ok((Vec::new(), offset >= attr.size));
        }
        let file = self.file(id).await?;
        // `wanted` is at most `count`, a `u32`.
        let data = file
            .read(offset, wanted as usize)
            .map_err(|error| status(self.core.unreadable(id, offset, &error.to_string())))?;
        let end = offset + data.len() as u64 >= attr.size;
        Ok((data, end))
    }

    /// The open cache file of the file `id`: the cached descriptor, else the core's, which fills
    /// the blob first. The fill goes on in a thread of its own, so that a read that waits longer
    /// than the patience is answered `NFS3ERR_JUKEBOX` and finds the descriptor cached when the
    /// client asks again.
    async fn file(&self, id: fileid3) -> Result<Arc<FileBytes>, nfsstat3> {
        if let Some(file) = self.fds.get(id) {
            return Ok(file);
        }
        let mut done = self.fill_of(id);
        tokio::select! {
            // A fill whose task died leaves no answer: that is an I/O error.
            result = done.wait_for(Option::is_some) => result
                .ok()
                .and_then(|answer| answer.clone())
                .unwrap_or(Err(nfsstat3::NFS3ERR_IO)),
            () = (self.patience)() => Err(nfsstat3::NFS3ERR_JUKEBOX),
        }
    }

    /// The receiver of the fill of `id`: one blocking task fills a blob however many reads ask for
    /// it, so a client that is told JUKEBOX and asks again, or eight threads that read one cold
    /// header, wait for the fill that is on and do not start another. That fill is the open of the
    /// file, timed from the read that started it to the descriptor or its failure, and waiting
    /// meanwhile (`core/opens.rs`).
    fn fill_of(&self, id: fileid3) -> watch::Receiver<Fill> {
        let mut filling = locked(&self.filling);
        if let Some(done) = filling.get(&id) {
            return done.clone();
        }
        let (sender, receiver) = watch::channel(None);
        filling.insert(id, receiver.clone());
        let (core, fds, table) = (
            Arc::clone(&self.core),
            Arc::clone(&self.fds),
            Arc::clone(&self.filling),
        );
        let timing = core.open_begins();
        // The blocking task starts now, not when the watcher is first polled.
        let blocking = tokio::task::spawn_blocking(move || {
            let mut timing = timing;
            let got = core.blob_file(id, &mut timing);
            let got = got.and_then(|file| file.ok_or(Errno::Io));
            core.open_ends(timing, got.is_ok());
            let file = Arc::new(got?.0);
            fds.insert(id, Arc::clone(&file));
            Ok::<_, Errno>(file)
        });
        tokio::spawn(async move {
            let result = blocking.await.unwrap_or(Err(Errno::Io)).map_err(status);
            // Out of the table first, so that a read that finds no descriptor after a failure
            // starts a fill of its own.
            let _ = locked(&table).remove(&id);
            sender.send_replace(Some(result));
        });
        receiver
    }
}

#[async_trait]
impl NFSFileSystem for TreeNfs {
    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadOnly
    }

    fn root_dir(&self) -> fileid3 {
        ino::ROOT
    }

    /// A call that names a user (`AUTH_SYS`) is served, whichever user it names: the service
    /// listens on loopback only and serves a read-only tree, so any local user may read it, as any
    /// client that holds a key may read through the S3 front. A call that names no user is
    /// `NFS3ERR_ACCES`: `ACCESS` and the client's own checks of the mode bits need a user.
    fn authorize(&self, auth: Option<&auth_unix>) -> Result<(), nfsstat3> {
        auth.map(|_| ()).ok_or(nfsstat3::NFS3ERR_ACCES)
    }

    async fn access(
        &self,
        id: fileid3,
        auth: Option<&auth_unix>,
        requested: u32,
    ) -> Result<u32, nfsstat3> {
        self.timed(RequestKind::Access, || {
            granted(&self.core, id, auth, requested)
        })
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let arrival = self.core.request_begins();
        let looked = self.looked_up(dirid, filename).await;
        self.core.request_ends(arrival, RequestKind::Lookup);
        looked
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        self.timed(RequestKind::Getattr, || {
            self.core
                .getattr(id)
                .map(|entry| fattr(&entry.attr))
                .map_err(handled)
        })
    }

    async fn setattr(&self, _id: fileid3, _setattr: sattr3) -> Result<fattr3, nfsstat3> {
        self.refused()
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        let arrival = self.core.request_begins();
        let read = self.read_bytes(id, offset, count).await;
        self.core.request_ends(arrival, RequestKind::Read);
        read
    }
    async fn write(&self, _id: fileid3, _offset: u64, _data: &[u8]) -> Result<fattr3, nfsstat3> {
        self.refused()
    }

    async fn create(
        &self,
        _dirid: fileid3,
        _filename: &filename3,
        _attr: sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        self.refused()
    }

    async fn create_exclusive(
        &self,
        _dirid: fileid3,
        _filename: &filename3,
    ) -> Result<fileid3, nfsstat3> {
        self.refused()
    }

    async fn mkdir(
        &self,
        _dirid: fileid3,
        _dirname: &filename3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        self.refused()
    }

    async fn remove(&self, _dirid: fileid3, _filename: &filename3) -> Result<(), nfsstat3> {
        self.refused()
    }

    async fn rename(
        &self,
        _from_dirid: fileid3,
        _from_filename: &filename3,
        _to_dirid: fileid3,
        _to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        self.refused()
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        self.timed(
            RequestKind::Readdir,
            || -> Result<ReadDirResult, nfsstat3> {
                let mut entries = Vec::new();
                let mut end = true;
                self.core
                    .readdir_after(dirid, start_after, &mut |item| {
                        if entries.len() >= max_entries.max(1) {
                            end = false;
                            return false;
                        }
                        entries.push(DirEntry {
                            fileid: item.entry.attr.ino,
                            name: item.name.into_bytes().into(),
                            attr: fattr(&item.entry.attr),
                        });
                        true
                    })
                    .map_err(handled)?;
                Ok(ReadDirResult { entries, end })
            },
        )
    }

    async fn symlink(
        &self,
        _dirid: fileid3,
        _linkname: &filename3,
        _symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        self.refused()
    }

    async fn readlink(&self, id: fileid3) -> Result<nfspath3, nfsstat3> {
        self.timed(RequestKind::Readlink, || -> Result<nfspath3, nfsstat3> {
            self.core
                .readlink(id)
                .map(|target| target.as_bytes().into())
                .map_err(handled)
        })
    }

    async fn fsinfo(&self, root_fileid: fileid3) -> Result<fsinfo3, nfsstat3> {
        self.timed(RequestKind::Other, || -> Result<fsinfo3, nfsstat3> {
            let root = self
                .core
                .getattr(root_fileid)
                .map_or(post_op_attr::Void, |entry| {
                    post_op_attr::attributes(fattr(&entry.attr))
                });
            Ok(fsinfo3 {
                obj_attributes: root,
                rtmax: 1024 * 1024,
                rtpref: 1024 * 1024,
                rtmult: 4096,
                wtmax: 0,
                wtpref: 0,
                wtmult: 0,
                dtpref: 1024 * 1024,
                maxfilesize: u64::MAX >> 1,
                time_delta: nfstime3 {
                    seconds: 1,
                    nseconds: 0,
                },
                properties: FSF_SYMLINK | FSF_HOMOGENEOUS,
            })
        })
    }

    fn id_to_fh(&self, id: fileid3) -> nfs_fh3 {
        handle::of(id, self.epoch)
    }

    fn fh_to_id(&self, fh: &nfs_fh3) -> Result<fileid3, nfsstat3> {
        handle::ino_of(fh, self.epoch)
    }

    fn serverid(&self) -> nfsserve::nfs::cookieverf3 {
        SERVER_ID
    }
}

/// Whether `id` is a directory of the namespace: the mount's root, `trees/`, or a directory above
/// the fs locations of the placements.
fn namespace(id: fileid3) -> bool {
    ino::split(id).0 == 0 || ino::is_placed_dir(id)
}

#[cfg(test)]
#[path = "fs_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "opens_tests.rs"]
mod opens_tests;

#[cfg(test)]
#[path = "requests_tests.rs"]
mod requests_tests;
