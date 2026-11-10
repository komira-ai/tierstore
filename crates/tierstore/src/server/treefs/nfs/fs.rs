//! The NFSv3 file system over the core: [`TreeNfs`] answers the procedures `nfsserve` decodes from
//! the inode numbers of the core (`docs/design/virtual-build-fs.md` sections 6.3 and 8).
//!
//! Everything is read-only: each procedure that would change a tree is answered `NFS3ERR_ROFS`.
//! Every entry reports one fixed, old time, so that the client's caches never revalidate. A file's
//! bytes are read from the cache file of its blob, which the core fills on the first read; the
//! open cache files are kept in a sharded cache, and a read that waits longer than
//! `fs.nfs.jukebox_after` for a fill is answered `NFS3ERR_JUKEBOX`, which has the client ask
//! again, while the fill goes on.

use std::collections::{HashMap, HashSet};
use std::fs::File;
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
use crate::server::treefs::core::{Attr, Core, Errno, FIXED_MTIME, Kind, Looked, read_file};
use crate::server::treefs::{ino, locked};

/// A future that is ready when a read has waited long enough for a fill; one is made for each read
/// that has to fill. The process makes it a timer, a test makes it ready or never.
pub(super) type Patience = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// The state of a fill: not done, or the open file or why there is none.
type Fill = Option<Result<Arc<File>, nfsstat3>>;

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
        mode: u32::from(attr.perm),
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

/// The permission bits of `attr` for a caller who says it is `auth`: the owner's, the group's, or
/// everyone else's, as a file system picks them.
fn class_bits(attr: &Attr, auth: &auth_unix) -> u16 {
    let in_group = auth.gid() == attr.gid || auth.gids().contains(&attr.gid);
    if auth.uid() == attr.uid {
        (attr.perm >> 6) & 0o7
    } else if in_group {
        (attr.perm >> 3) & 0o7
    } else {
        attr.perm & 0o7
    }
}

/// The NFS file system of the core.
pub(super) struct TreeNfs {
    core: Arc<Core>,
    fds: Arc<FdCache>,
    allowed: HashSet<u32>,
    patience: Patience,
    /// The epoch of the mount, which its handles carry.
    epoch: u32,
    /// The fills that are on, by inode.
    filling: Arc<Mutex<HashMap<fileid3, watch::Receiver<Fill>>>>,
}

impl TreeNfs {
    /// The file system of `core`, the mount of `epoch`, which serves the users `allowed`.
    pub(super) fn new(
        core: Arc<Core>,
        settings: &NfsSettings,
        allowed: HashSet<u32>,
        patience: Patience,
        epoch: u32,
    ) -> Self {
        Self {
            core,
            fds: Arc::new(FdCache::new(
                settings.fd_cache_shards,
                settings.fd_cache_per_shard,
            )),
            allowed,
            patience,
            epoch,
            filling: Arc::default(),
        }
    }

    /// The open cache file of the file `id`: the cached descriptor, else the core's, which fills
    /// the blob first. The fill goes on in a thread of its own, so that a read that waits longer
    /// than the patience is answered `NFS3ERR_JUKEBOX` and finds the descriptor cached when the
    /// client asks again.
    async fn file(&self, id: fileid3) -> Result<Arc<File>, nfsstat3> {
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
    /// header, wait for the fill that is on and do not start another.
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
        // The blocking task starts now, not when the watcher is first polled.
        let blocking = tokio::task::spawn_blocking(move || {
            let (file, _) = core.blob_file(id)?.ok_or(Errno::Io)?;
            let file = Arc::new(file);
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

    fn authorize(&self, auth: Option<&auth_unix>) -> Result<(), nfsstat3> {
        match auth {
            Some(auth) if self.allowed.contains(&auth.uid()) => Ok(()),
            _ => Err(nfsstat3::NFS3ERR_ACCES),
        }
    }

    async fn access(
        &self,
        id: fileid3,
        auth: Option<&auth_unix>,
        requested: u32,
    ) -> Result<u32, nfsstat3> {
        let auth = auth.ok_or(nfsstat3::NFS3ERR_ACCES)?;
        let attr = self.core.getattr(id).map_err(handled)?.attr;
        let bits = class_bits(&attr, auth);
        let namespace = ino::split(id).0 == 0;
        let mut granted = 0;
        // Nobody lists the namespace, whatever its mode says (the core's rule).
        if bits & 0o4 != 0 && !namespace {
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

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
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
            _ => match self.core.lookup(dirid, name) {
                Ok(Looked::Found(entry)) => Ok(entry.attr.ino),
                Ok(Looked::Absent(_)) => Err(nfsstat3::NFS3ERR_NOENT),
                // A name the namespace lacks is a missing name; a directory the core lacks is a
                // stale handle.
                Err(Errno::NoEnt) if ino::split(dirid).0 == 0 => Err(nfsstat3::NFS3ERR_NOENT),
                Err(errno) => Err(handled(errno)),
            },
        }
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        self.core
            .getattr(id)
            .map(|entry| fattr(&entry.attr))
            .map_err(handled)
    }

    async fn setattr(&self, _id: fileid3, _setattr: sattr3) -> Result<fattr3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn read(
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
        let data = read_file(&file, offset, wanted as usize).map_err(status)?;
        let end = offset + data.len() as u64 >= attr.size;
        Ok((data, end))
    }

    async fn write(&self, _id: fileid3, _offset: u64, _data: &[u8]) -> Result<fattr3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn create(
        &self,
        _dirid: fileid3,
        _filename: &filename3,
        _attr: sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn create_exclusive(
        &self,
        _dirid: fileid3,
        _filename: &filename3,
    ) -> Result<fileid3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn mkdir(
        &self,
        _dirid: fileid3,
        _dirname: &filename3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn remove(&self, _dirid: fileid3, _filename: &filename3) -> Result<(), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn rename(
        &self,
        _from_dirid: fileid3,
        _from_filename: &filename3,
        _to_dirid: fileid3,
        _to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
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
    }

    async fn symlink(
        &self,
        _dirid: fileid3,
        _linkname: &filename3,
        _symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn readlink(&self, id: fileid3) -> Result<nfspath3, nfsstat3> {
        self.core
            .readlink(id)
            .map(|target| target.as_bytes().into())
            .map_err(handled)
    }

    async fn fsinfo(&self, root_fileid: fileid3) -> Result<fsinfo3, nfsstat3> {
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

#[cfg(test)]
#[path = "fs_tests.rs"]
mod tests;
