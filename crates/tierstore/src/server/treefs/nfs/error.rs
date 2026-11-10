//! Why the NFS frontend did not start, or did not stop cleanly.

use std::net::SocketAddr;
use std::path::PathBuf;

use crate::server::treefs::start::StartError;

/// A failure of the NFS frontend.
#[derive(Debug, thiserror::Error)]
pub(in crate::server) enum NfsError {
    /// What a start does before a frontend serves failed.
    #[error("{0}")]
    Start(#[from] StartError),
    /// The address could not be listened on.
    #[error("fs.nfs.listen {address}: {source}")]
    Listen {
        /// The address.
        address: SocketAddr,
        /// What the system said.
        source: std::io::Error,
    },
    /// A command could not be run at all.
    #[error("running {command}: {source}")]
    Run {
        /// The program.
        command: String,
        /// What the system said.
        source: std::io::Error,
    },
    /// The mount user is not a user of this machine.
    #[error("fs.nfs.mount_user {user}: {said}")]
    User {
        /// The name.
        user: String,
        /// What was found.
        said: String,
    },
    /// The mount point belongs to someone else than the mount user, who could not mount on it.
    #[error(
        "fs.mountpoint {path} is owned by uid {owner}, not by the mount user (uid {user}); \
         give it to that user"
    )]
    MountPointOwner {
        /// The mount point.
        path: PathBuf,
        /// Its owner.
        owner: u32,
        /// The mount user.
        user: u32,
    },
    /// The process is not the mount user and is not root, who could run the mount as that user.
    #[error(
        "this process is uid {euid} and fs.nfs.mount_user {user} is uid {uid}: only root runs a \
         mount as another user"
    )]
    NotThatUser {
        /// The user of the process.
        euid: u32,
        /// The mount user's name.
        user: String,
        /// The mount user's uid.
        uid: u32,
    },
    /// The mount did not succeed in the time it is tried for.
    #[error("mounting {path} did not succeed in {tries} tries: {said}")]
    Mount {
        /// The mount point.
        path: PathBuf,
        /// How many times it was tried.
        tries: u32,
        /// What the last try said.
        said: String,
    },
    /// The unmount did not succeed.
    #[error("unmounting {path}: {said}")]
    Unmount {
        /// The mount point.
        path: PathBuf,
        /// What it said.
        said: String,
    },
}

impl NfsError {
    /// The exit status of a serve that this ended: that of the start that failed, else 1.
    pub(in crate::server) fn status(&self) -> u8 {
        match self {
            Self::Start(why) => why.status(),
            _ => 1,
        }
    }
}
