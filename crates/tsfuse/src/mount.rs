//! Mounting (5.2, item 6): `mount(2)` with the device's descriptor in the options (`fd=`,
//! `rootmode`, `user_id`, `group_id`) when the process has `CAP_SYS_ADMIN`, otherwise
//! `fusermount3`, which mounts and passes the descriptor back over a socket (`_FUSE_COMMFD`).
//! Then the unmount (`umount2(2)`, or `fusermount3 -u`) and the abort of a connection
//! (`/sys/fs/fuse/connections/<n>/abort`).

use std::ffi::CString;
use std::io::{self, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use rustix::fs::{AtFlags, CWD, StatxFlags};
use rustix::io::FdFlags;
use rustix::mount::{MountFlags, UnmountFlags};
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SocketFlags, SocketType,
};

use crate::dev::{DEV_FUSE, DevFd};

/// The directory of the FUSE control file system's connections.
pub const CONNECTIONS: &str = "/sys/fs/fuse/connections";

/// The mount helper, found on `PATH`.
pub const FUSERMOUNT: &str = "fusermount3";

/// The options of a mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountOptions {
    /// `ro`: the file system is read-only (a tree mount).
    pub read_only: bool,
    /// `nosuid`: set-user-id bits are ignored.
    pub nosuid: bool,
    /// `nodev`: device files are not opened.
    pub nodev: bool,
    /// `default_permissions`: the kernel checks the mode bits, so the file system does not.
    pub default_permissions: bool,
    /// `allow_other`: users other than the mounting one may enter the mount. Through
    /// `fusermount3`, a user other than root needs `user_allow_other` in `/etc/fuse.conf`.
    pub allow_other: bool,
    /// The source the mount table shows (`fsname=`).
    pub fsname: String,
    /// The file system type after `fuse.` (`subtype=`); `None` mounts type `fuse`.
    pub subtype: Option<String>,
}

impl MountOptions {
    /// A tree mount: `ro`, `nosuid`, `nodev`, `default_permissions`, `allow_other`.
    #[must_use]
    pub fn tree(fsname: &str) -> Self {
        Self {
            read_only: true,
            nosuid: true,
            nodev: true,
            default_permissions: true,
            allow_other: true,
            fsname: fsname.to_owned(),
            subtype: Some("tsfuse".to_owned()),
        }
    }

    /// The put directory: as [`Self::tree`], writable.
    #[must_use]
    pub fn put(fsname: &str) -> Self {
        Self {
            read_only: false,
            ..Self::tree(fsname)
        }
    }

    /// Checks that `fsname` and `subtype` can be written into an option string: neither empty
    /// nor holding a `,` (the option separator), a `=`, a NUL or whitespace.
    ///
    /// # Errors
    ///
    /// [`MountError::InvalidOption`] naming the option.
    pub fn check(&self) -> Result<(), MountError> {
        let names = [
            ("fsname", Some(&self.fsname)),
            ("subtype", self.subtype.as_ref()),
        ];
        for (option, value) in names {
            let Some(value) = value else { continue };
            let bad = value.is_empty()
                || value
                    .chars()
                    .any(|c| matches!(c, ',' | '=' | '\0') || c.is_whitespace());
            if bad {
                return Err(invalid(option, value.as_bytes()));
            }
        }
        Ok(())
    }

    /// The `mount(2)` flags: `MS_RDONLY`, `MS_NOSUID`, `MS_NODEV`.
    pub(crate) fn flags(&self) -> MountFlags {
        [
            (self.read_only, MountFlags::RDONLY),
            (self.nosuid, MountFlags::NOSUID),
            (self.nodev, MountFlags::NODEV),
        ]
        .into_iter()
        .filter(|(on, _)| *on)
        .fold(MountFlags::empty(), |all, (_, flag)| all | flag)
    }

    /// The data string of `mount(2)` for the device descriptor `fd`, mounted by `uid`/`gid`:
    /// `fd=<n>,rootmode=40000,user_id=<uid>,group_id=<gid>` then `default_permissions` and
    /// `allow_other` when set.
    #[must_use]
    pub fn kernel_data(&self, fd: i32, uid: u32, gid: u32) -> String {
        let mut data = format!("fd={fd},rootmode=40000,user_id={uid},group_id={gid}");
        for (on, name) in self.data_options() {
            if on {
                data.push(',');
                data.push_str(name);
            }
        }
        data
    }

    /// The `-o` string of `fusermount3`: `ro`, `nosuid`, `nodev`, `default_permissions`,
    /// `allow_other` when set, then `fsname=` and `subtype=`.
    #[must_use]
    pub fn fusermount_options(&self) -> String {
        let flags = [
            (self.read_only, "ro"),
            (self.nosuid, "nosuid"),
            (self.nodev, "nodev"),
        ];
        let mut parts: Vec<String> = flags
            .into_iter()
            .chain(self.data_options())
            .filter(|(on, _)| *on)
            .map(|(_, name)| name.to_owned())
            .collect();
        parts.push(format!("fsname={}", self.fsname));
        parts.extend(self.subtype.iter().map(|s| format!("subtype={s}")));
        parts.join(",")
    }

    /// The `mount(2)` file system type: `fuse.<subtype>` or `fuse`.
    #[must_use]
    pub fn fstype(&self) -> String {
        self.subtype
            .as_ref()
            .map_or_else(|| "fuse".to_owned(), |s| format!("fuse.{s}"))
    }

    fn data_options(&self) -> [(bool, &'static str); 2] {
        [
            (self.default_permissions, "default_permissions"),
            (self.allow_other, "allow_other"),
        ]
    }
}

/// An option value that cannot be written into an option string.
fn invalid(option: &'static str, value: &[u8]) -> MountError {
    let value = String::from_utf8_lossy(value).into_owned();
    MountError::InvalidOption { option, value }
}

/// How a mount was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MountMethod {
    /// `mount(2)`: the process has `CAP_SYS_ADMIN`.
    Syscall,
    /// `fusermount3`.
    Fusermount,
}

/// The number of a FUSE connection: its directory under `/sys/fs/fuse/connections`, the kernel's
/// device number of the mount's super block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionId(pub u32);

/// Why a mount, an unmount or an abort failed.
#[derive(Debug, thiserror::Error)]
pub enum MountError {
    /// An option cannot be written into an option string.
    #[error("mount option {option}={value:?} is empty or holds a comma, '=', NUL or whitespace")]
    InvalidOption {
        /// The option.
        option: &'static str,
        /// Its value.
        value: String,
    },
    /// The FUSE device could not be opened.
    #[error("opening {path} failed: {source}")]
    Open {
        /// The device's path.
        path: PathBuf,
        /// The error.
        #[source]
        source: io::Error,
    },
    /// `mount(2)` failed (`EPERM` without `CAP_SYS_ADMIN`).
    #[error("mount(2) of FUSE at {mountpoint} failed: {source}")]
    Syscall {
        /// The mount point.
        mountpoint: PathBuf,
        /// The error.
        #[source]
        source: io::Error,
    },
    /// The helper could not be started.
    #[error("starting {program} failed: {source}")]
    Spawn {
        /// The helper.
        program: PathBuf,
        /// The error.
        #[source]
        source: io::Error,
    },
    /// The helper ran and failed.
    #[error("{program} for {mountpoint} failed ({status}): {stderr}")]
    Helper {
        /// The helper.
        program: PathBuf,
        /// The mount point.
        mountpoint: PathBuf,
        /// Its exit status.
        status: ExitStatus,
        /// What it wrote on stderr, trimmed.
        stderr: String,
    },
    /// The helper exited 0 and passed no descriptor.
    #[error("{program} mounted {mountpoint} and passed no FUSE descriptor")]
    NoDescriptor {
        /// The helper.
        program: PathBuf,
        /// The mount point.
        mountpoint: PathBuf,
    },
    /// The socket the helper passes the descriptor on failed.
    #[error("the socket of {program} failed: {source}")]
    Socket {
        /// The helper.
        program: PathBuf,
        /// The error.
        #[source]
        source: io::Error,
    },
    /// `umount2(2)` failed.
    #[error("unmounting {mountpoint} failed: {source}")]
    Unmount {
        /// The mount point.
        mountpoint: PathBuf,
        /// The error.
        #[source]
        source: io::Error,
    },
    /// The mount point's device number could not be read.
    #[error("reading the connection of {mountpoint} failed: {source}")]
    Connection {
        /// The mount point.
        mountpoint: PathBuf,
        /// The error.
        #[source]
        source: io::Error,
    },
    /// Writing the connection's `abort` file failed.
    #[error("aborting the FUSE connection through {path} failed: {source}")]
    Abort {
        /// The `abort` file.
        path: PathBuf,
        /// The error.
        #[source]
        source: io::Error,
    },
}

impl MountError {
    /// Whether this is `mount(2)` refused with `EPERM`: the process lacks `CAP_SYS_ADMIN`.
    fn is_eperm(&self) -> bool {
        let errno = match self {
            Self::Syscall { source, .. } => source.raw_os_error(),
            _ => None,
        };
        errno == Some(rustix::io::Errno::PERM.raw_os_error())
    }
}

/// Where the mount's tools and files are: the device, the helper, the connections directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mounter {
    /// The FUSE device (`/dev/fuse`).
    pub device: PathBuf,
    /// The helper (`fusermount3`, on `PATH`).
    pub fusermount: PathBuf,
    /// The connections of the FUSE control file system (`/sys/fs/fuse/connections`).
    pub connections: PathBuf,
}

impl Default for Mounter {
    fn default() -> Self {
        Self {
            device: PathBuf::from(DEV_FUSE),
            fusermount: PathBuf::from(FUSERMOUNT),
            connections: PathBuf::from(CONNECTIONS),
        }
    }
}

impl Mounter {
    /// Mounts at `mountpoint` with `options`: `mount(2)`, and `fusermount3` when that is refused
    /// with `EPERM`. Returns the connection's descriptor (in blocking mode; the kernel's INIT
    /// is waiting on it) and the mount.
    ///
    /// # Errors
    ///
    /// [`MountError::InvalidOption`], [`MountError::Open`], [`MountError::Syscall`] for an error
    /// other than `EPERM`, or the errors of the helper ([`MountError::Spawn`],
    /// [`MountError::Helper`], [`MountError::NoDescriptor`], [`MountError::Socket`]).
    pub fn mount(
        &self,
        mountpoint: &Path,
        options: &MountOptions,
    ) -> Result<(DevFd, Mount), MountError> {
        match self.mount_syscall(mountpoint, options) {
            Err(e) if e.is_eperm() => self.mount_fusermount(mountpoint, options),
            made => made,
        }
    }

    /// Mounts with `mount(2)`, which needs `CAP_SYS_ADMIN`.
    ///
    /// # Errors
    ///
    /// [`MountError::InvalidOption`], [`MountError::Open`] or [`MountError::Syscall`].
    pub fn mount_syscall(
        &self,
        mountpoint: &Path,
        options: &MountOptions,
    ) -> Result<(DevFd, Mount), MountError> {
        options.check()?;
        let device = DevFd::open_at(&self.device).map_err(|source| MountError::Open {
            path: self.device.clone(),
            source,
        })?;
        let data = options.kernel_data(
            device.as_raw_fd(),
            rustix::process::geteuid().as_raw(),
            rustix::process::getegid().as_raw(),
        );
        // `check` refused a NUL in every value the string is made of, so this never fails.
        let data = CString::new(data).map_err(|e| invalid("data", &e.into_vec()))?;
        rustix::mount::mount(
            options.fsname.as_str(),
            mountpoint,
            options.fstype().as_str(),
            options.flags(),
            data.as_c_str(),
        )
        .map_err(|e| MountError::Syscall {
            mountpoint: mountpoint.to_owned(),
            source: e.into(),
        })?;
        Ok((device, self.made(mountpoint, MountMethod::Syscall)))
    }

    /// Mounts with `fusermount3 -o <options> -- <mountpoint>`, which passes the descriptor back
    /// over a socket pair whose end it finds in `_FUSE_COMMFD`.
    ///
    /// # Errors
    ///
    /// [`MountError::InvalidOption`], [`MountError::Socket`], [`MountError::Spawn`],
    /// [`MountError::Helper`] or [`MountError::NoDescriptor`].
    pub fn mount_fusermount(
        &self,
        mountpoint: &Path,
        options: &MountOptions,
    ) -> Result<(DevFd, Mount), MountError> {
        options.check()?;
        let (ours, theirs) = socket_pair().map_err(|e| self.socket_error(e))?;
        let mut command = Command::new(&self.fusermount);
        command
            .arg("-o")
            .arg(options.fusermount_options())
            .arg("--")
            .arg(mountpoint)
            .env("_FUSE_COMMFD", theirs.as_raw_fd().to_string());
        let output = run(&self.fusermount, &mut command);
        drop(theirs);
        self.helper_succeeded(mountpoint, output?)?;
        let fd = self
            .receive_fd(&ours)?
            .ok_or_else(|| MountError::NoDescriptor {
                program: self.fusermount.clone(),
                mountpoint: mountpoint.to_owned(),
            })?;
        Ok((
            DevFd::from_fd(fd),
            self.made(mountpoint, MountMethod::Fusermount),
        ))
    }

    /// The first descriptor passed on `socket` (`SCM_RIGHTS`), or `None` when nothing with one is
    /// queued. Any further descriptor, and any other ancillary message (credentials, on a socket
    /// that asks for them), is dropped.
    fn receive_fd(&self, socket: &OwnedFd) -> Result<Option<OwnedFd>, MountError> {
        let mut byte = [0u8; 1];
        let mut space =
            [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1), ScmCredentials(1))];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let mut iov = [IoSliceMut::new(&mut byte)];
        // Never waits: the helper has exited, so what it sent is queued, and nothing more comes
        // even if a child spawned meanwhile by another thread inherited its end of the pair.
        let flags = RecvFlags::CMSG_CLOEXEC | RecvFlags::DONTWAIT;
        let _ = match rustix::net::recvmsg(socket, &mut iov, &mut control, flags) {
            Err(rustix::io::Errno::AGAIN) => return Ok(None),
            got => got.map_err(|e| self.socket_error(e.into()))?,
        };
        let mut fds = control.drain().filter_map(|m| match m {
            RecvAncillaryMessage::ScmRights(rights) => Some(rights),
            _ => None,
        });
        Ok(fds.next().and_then(|mut rights| rights.next()))
    }

    fn socket_error(&self, source: io::Error) -> MountError {
        let program = self.fusermount.clone();
        MountError::Socket { program, source }
    }

    fn made(&self, mountpoint: &Path, method: MountMethod) -> Mount {
        Mount {
            mountpoint: mountpoint.to_owned(),
            method,
            mounter: self.clone(),
        }
    }

    fn helper_succeeded(
        &self,
        mountpoint: &Path,
        output: std::process::Output,
    ) -> Result<(), MountError> {
        match output.status.success() {
            true => Ok(()),
            false => Err(MountError::Helper {
                program: self.fusermount.clone(),
                mountpoint: mountpoint.to_owned(),
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            }),
        }
    }
}

/// A socket pair for the helper: ours close-on-exec, the helper's (the second) open across its
/// exec.
fn socket_pair() -> io::Result<(OwnedFd, OwnedFd)> {
    let flags = SocketFlags::CLOEXEC;
    let (ours, theirs) =
        rustix::net::socketpair(AddressFamily::UNIX, SocketType::STREAM, flags, None)?;
    rustix::io::fcntl_setfd(&theirs, FdFlags::empty())?;
    Ok((ours, theirs))
}

/// Runs `command` with no stdin or stdout and its stderr captured.
fn run(program: &Path, command: &mut Command) -> Result<std::process::Output, MountError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|source| MountError::Spawn {
            program: program.to_owned(),
            source,
        })
}

/// A FUSE mount this process made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    mountpoint: PathBuf,
    method: MountMethod,
    mounter: Mounter,
}

impl Mount {
    /// The mount of `mountpoint` that an earlier process made by `method` with the tools of
    /// `mounter`: what a process that took the mount over (T-F6) unmounts or aborts.
    #[must_use]
    pub fn adopt(mountpoint: &Path, method: MountMethod, mounter: &Mounter) -> Self {
        mounter.made(mountpoint, method)
    }

    /// The mount point.
    #[must_use]
    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    /// How the mount was made.
    #[must_use]
    pub fn method(&self) -> MountMethod {
        self.method
    }

    /// The mount's connection: the device number of the mount point, read with
    /// `AT_STATX_DONT_SYNC` so that no request is sent to the file system (it may not be served).
    ///
    /// # Errors
    ///
    /// [`MountError::Connection`].
    pub fn connection(&self) -> Result<ConnectionId, MountError> {
        let stat = rustix::fs::statx(
            CWD,
            &self.mountpoint,
            AtFlags::STATX_DONT_SYNC,
            StatxFlags::empty(),
        )
        .map_err(|e| MountError::Connection {
            mountpoint: self.mountpoint.clone(),
            source: e.into(),
        })?;
        // The kernel's `MKDEV`: 12 bits of major above 20 bits of minor.
        Ok(ConnectionId(
            (stat.stx_dev_major << 20) | (stat.stx_dev_minor & 0xf_ffff),
        ))
    }

    /// Aborts the connection: every request waiting on it fails (`ECONNABORTED` for the
    /// readers), and the mount stays until it is unmounted. Writing the `abort` file needs root.
    ///
    /// # Errors
    ///
    /// [`MountError::Connection`] or [`MountError::Abort`].
    pub fn abort(&self) -> Result<(), MountError> {
        let id = self.connection()?;
        let path = self
            .mounter
            .connections
            .join(id.0.to_string())
            .join("abort");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .and_then(|mut f| std::io::Write::write_all(&mut f, b"1"))
            .map_err(|source| MountError::Abort { path, source })
    }

    /// Unmounts: `umount2(2)` (`MNT_DETACH` when `lazy`) for a mount made by `mount(2)`, or
    /// `fusermount3 -u` (`-z` when `lazy`). A mount that is busy and not `lazy` stays.
    ///
    /// # Errors
    ///
    /// [`MountError::Unmount`], or the helper's [`MountError::Spawn`] or
    /// [`MountError::Helper`].
    pub fn unmount(self, lazy: bool) -> Result<(), MountError> {
        match self.method {
            MountMethod::Syscall => {
                let flags = match lazy {
                    true => UnmountFlags::DETACH,
                    false => UnmountFlags::empty(),
                };
                rustix::mount::unmount(&self.mountpoint, flags).map_err(|e| MountError::Unmount {
                    mountpoint: self.mountpoint.clone(),
                    source: e.into(),
                })
            }
            MountMethod::Fusermount => {
                let program = &self.mounter.fusermount;
                let mut command = Command::new(program);
                command.arg("-u").arg("-q");
                command.args(lazy.then_some("-z"));
                command.arg("--").arg(&self.mountpoint);
                let output = run(program, &mut command)?;
                self.mounter.helper_succeeded(&self.mountpoint, output)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_socket_failure_names_the_helper() {
        let err = Mounter::default().socket_error(io::Error::from_raw_os_error(24));
        assert!(
            err.to_string()
                .starts_with("the socket of fusermount3 failed: "),
            "{err}"
        );
    }

    #[test]
    fn an_invalid_value_is_shown_as_written() {
        let err = invalid("data", b"fd=3,\0x");
        assert_eq!(
            err.to_string(),
            "mount option data=\"fd=3,\\0x\" is empty or holds a comma, '=', NUL or whitespace"
        );
    }

    #[test]
    fn only_mount_2_refused_with_eperm_falls_back_to_the_helper() {
        let syscall = |errno: i32| MountError::Syscall {
            mountpoint: PathBuf::from("/m"),
            source: io::Error::from_raw_os_error(errno),
        };
        assert!(syscall(1).is_eperm());
        assert!(!syscall(22).is_eperm());
        let open = MountError::Open {
            path: PathBuf::from("/dev/fuse"),
            source: io::Error::from_raw_os_error(1),
        };
        assert!(!open.is_eperm());
    }

    #[test]
    fn a_message_with_credentials_and_no_descriptor_passes_none() {
        use rustix::net::{SendAncillaryBuffer, SendFlags, sendmsg, sockopt};
        let (ours, theirs) = socket_pair().unwrap();
        // Asking for credentials makes the kernel attach them to every message.
        sockopt::set_socket_passcred(&ours, true).unwrap();
        let byte = [0u8; 1];
        sendmsg(
            &theirs,
            &[io::IoSlice::new(&byte)],
            &mut SendAncillaryBuffer::default(),
            SendFlags::empty(),
        )
        .unwrap();
        let mounter = Mounter::default();
        assert!(mounter.receive_fd(&ours).unwrap().is_none());
        // And one that passes a descriptor: it is the first.
        let (a, _b) = socket_pair().unwrap();
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        let fds = [std::os::fd::AsFd::as_fd(&a)];
        assert!(control.push(rustix::net::SendAncillaryMessage::ScmRights(&fds)));
        sendmsg(
            &theirs,
            &[io::IoSlice::new(&byte)],
            &mut control,
            SendFlags::empty(),
        )
        .unwrap();
        assert!(mounter.receive_fd(&ours).unwrap().is_some());
        // Nothing queued and the peer's end still open (a child inherited it): no wait.
        let (open, _peer) = socket_pair().unwrap();
        assert!(mounter.receive_fd(&open).unwrap().is_none());
        // A descriptor that is not a socket: the error names the helper.
        let dir = std::fs::File::open(".").unwrap();
        let err = mounter.receive_fd(&OwnedFd::from(dir)).unwrap_err();
        assert!(matches!(err, MountError::Socket { .. }), "{err:?}");
    }

    #[test]
    fn the_mount_flags_are_those_set() {
        let tree = MountOptions::tree("t");
        assert_eq!(
            tree.flags(),
            MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NODEV
        );
        let none = MountOptions {
            read_only: false,
            nosuid: false,
            nodev: false,
            ..tree
        };
        assert_eq!(none.flags(), MountFlags::empty());
        assert_eq!(
            MountOptions::put("p").flags(),
            MountFlags::NOSUID | MountFlags::NODEV
        );
    }
}
