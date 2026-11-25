//! Whether the mount of the `fs` interface reaches the host: a mount made in a mount namespace of
//! this process's own is seen by this process alone, the job runtime finds an empty directory at
//! the mount point, and the next instance cannot take it
//! (`docs/design/one-service.md` section 2.2). The unit sets no option that gives the service a
//! mount namespace, but a drop-in of the host's can (`ReadWritePaths=`, `ProtectSystem=`,
//! `PrivateTmp=` and the like), and nothing else would say so: the process serves the mount, and
//! the manager is told `READY=1`.
//!
//! The check runs once the start has the mount (taken, adopted or made), before anything is bound
//! or the manager is told. Two signals are read:
//!
//! 1. The namespaces: `/proc/self/ns/mnt` and `/proc/1/ns/mnt`, links that read `mnt:[INODE]`. The
//!    same namespace means the mount is the host's. Reading the link of another user's process
//!    needs `CAP_SYS_PTRACE` (`ptrace_may_access`), which the unit's account does not have: the
//!    packaged unit runs as `User=tierstore` with `CAP_SYS_ADMIN` as its only capability. So the
//!    link of process 1, which belongs to root, is unreadable to the process in the case that
//!    matters, and the comparison alone cannot decide.
//! 2. The tables of mounts: `/proc/1/mountinfo` is readable by every account (the kernel checks
//!    nothing but that the process exists and is visible), and shows the mounts of process 1's
//!    namespace. A mount made in a namespace of this process's own is not shown there, whatever
//!    the namespaces' names say, so the table answers the question asked: is the mount at the mount
//!    point, with the device number of this process's mount, in the host's table? It decides when
//!    the namespaces differ or cannot both be read.
//!
//! The mount is refused only on evidence: the host's table is read and does not show the mount.
//! When neither signal can be read (a `/proc` that hides process 1, `hidepid=2` for another
//! account), the process cannot tell, and a node that works is not stopped for it: it is said and
//! the start goes on.

use std::path::{Path, PathBuf};

use super::start::StartError;

/// Where this process looks to see what the host sees.
#[derive(Clone, Debug)]
pub(super) struct HostView {
    /// This process's mount namespace, `/proc/self/ns/mnt`.
    pub own_ns: PathBuf,
    /// Process 1's mount namespace, `/proc/1/ns/mnt`.
    pub host_ns: PathBuf,
    /// Process 1's table of mounts, `/proc/1/mountinfo`.
    pub host_mountinfo: PathBuf,
}

impl HostView {
    /// The files of the machine.
    pub(super) fn real() -> Self {
        Self {
            own_ns: PathBuf::from("/proc/self/ns/mnt"),
            host_ns: PathBuf::from("/proc/1/ns/mnt"),
            host_mountinfo: PathBuf::from("/proc/1/mountinfo"),
        }
    }

    /// Checks that the mount at `mountpoint`, which `own_mountinfo` (this process's table of
    /// mounts) shows, is in the host's mount namespace.
    ///
    /// # Errors
    /// [`StartError::HostNamespace`]: the host's table of mounts does not show the mount, and the
    /// namespaces are not known to be one.
    pub(super) fn check(&self, mountpoint: &Path, own_mountinfo: &Path) -> Result<(), StartError> {
        let own = std::fs::read_link(&self.own_ns);
        let host = std::fs::read_link(&self.host_ns);
        if let (Ok(own), Ok(host)) = (&own, &host)
            && own == host
        {
            return Ok(());
        }
        let name = |link: &std::io::Result<PathBuf>| match link {
            Ok(name) => name.display().to_string(),
            Err(why) => format!("unreadable ({})", why.kind()),
        };
        let refused = || StartError::HostNamespace {
            mountpoint: mountpoint.display().to_string(),
            own: name(&own),
            host: name(&host),
        };
        let Ok(table) = std::fs::read_to_string(&self.host_mountinfo) else {
            if own.is_ok() && host.is_ok() {
                // Two namespaces that differ, and no table to show the mount in either.
                return Err(refused());
            }
            tracing::warn!(
                "tierstore fs: cannot tell whether the mount at {} is in the host's mount namespace: \
                 neither the namespace nor the table of mounts of process 1 can be read",
                mountpoint.display()
            );
            return Ok(());
        };
        let ours = std::fs::read_to_string(own_mountinfo).unwrap_or_default();
        let device = device_at(&ours, mountpoint);
        match device_at(&table, mountpoint) {
            Some(seen) if device.is_none_or(|ours| ours == seen) => Ok(()),
            _ => Err(refused()),
        }
    }
}

/// The device number (`major:minor`) of the FUSE mount at `mountpoint` in the text of a table of
/// mounts; the last mount at the path counts.
fn device_at<'a>(mountinfo: &'a str, mountpoint: &Path) -> Option<&'a str> {
    mountinfo
        .lines()
        .filter_map(|line| {
            let (before, after) = line.split_once(" - ")?;
            let mut fields = before.split(' ');
            let device = fields.nth(2)?;
            let at = fields.nth(1)?;
            let fstype = after.split(' ').next()?;
            let fuse = fstype == "fuse" || fstype.starts_with("fuse.");
            (fuse && Path::new(at) == mountpoint).then_some(device)
        })
        .next_back()
}

#[cfg(test)]
#[path = "host_view_tests.rs"]
mod tests;
