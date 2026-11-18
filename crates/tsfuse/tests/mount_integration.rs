//! Mounts against the kernel (5.2, item 6; ported from the fork's `fuser-tests` mount and
//! unmount cases): `mount(2)`, or `fusermount3` as a user without `CAP_SYS_ADMIN`; INIT, cloned
//! readers serving a `stat`, an unmount that ends every reader, a stop that leaves the mount for a
//! resumed session, and the failures of the helper, the device, the unmount and the abort.
//!
//! An integration target: it opens `/dev/fuse` and runs `fusermount3` (and `true`). Where either is
//! missing, a test that needs them says why on stderr and does not run; CI installs `fuse3` so they
//! run there. The kernel conformance suite (T-F7) runs the rest on a farm node.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod common;

use std::io::{self, Write as _};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tierstore_testkit::dir::TestDir;
use tsfuse::abi::{Attr, AttrOut, InitConfig};
use tsfuse::mount::{MountError, MountMethod, MountOptions, Mounter};
use tsfuse::{DevFd, Errno, Fs, Handled, ReaderEnd, Replier, Request, Session, SessionError};

/// Why this machine cannot make a FUSE mount as the user of the tests, if it cannot.
fn unavailable() -> Option<&'static str> {
    let device = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
        .is_ok();
    let helper = std::process::Command::new("fusermount3")
        .arg("--version")
        .output()
        .is_ok();
    [
        (device, "/dev/fuse cannot be opened for reading and writing"),
        (helper, "fusermount3 is not installed"),
    ]
    .into_iter()
    .find_map(|(ok, why)| (!ok).then_some(why))
}

/// Runs `test` where a FUSE mount can be made; says on stderr why it does not run elsewhere.
fn with_fuse(test: impl FnOnce()) {
    match unavailable() {
        None => test(),
        Some(why) => {
            let _ = writeln!(io::stderr(), "skipped: {why}");
        }
    }
}

/// A scratch directory and an empty mount point in it.
fn mountpoint(name: &str) -> (TestDir, PathBuf) {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let mp = dir.path().join("mnt");
    std::fs::create_dir(&mp).unwrap();
    (dir, mp)
}

/// A tree mount the user of the tests may make through `fusermount3` (`allow_other` needs
/// `user_allow_other` in `/etc/fuse.conf`).
fn options() -> MountOptions {
    MountOptions {
        allow_other: false,
        ..MountOptions::tree("tsfuse-test")
    }
}

/// An empty root directory: GETATTR of the root answers its attributes, LOOKUP `ENOENT`,
/// everything else `ENOSYS`. Counts the GETATTRs.
#[derive(Default)]
struct EmptyRoot {
    getattrs: AtomicUsize,
}

impl Fs for EmptyRoot {
    fn handle(&self, req: Request<'_>, replier: Replier) -> Handled {
        let _ = match req.header.opcode {
            3 => {
                self.getattrs.fetch_add(1, Ordering::SeqCst);
                let attr = Attr {
                    ino: 1,
                    mode: 0o40_755,
                    nlink: 2,
                    uid: rustix::process::geteuid().as_raw(),
                    gid: rustix::process::getegid().as_raw(),
                    ..Attr::default()
                };
                replier.ok(&[AttrOut::new(Duration::ZERO, attr).as_bytes()])
            }
            1 => replier.error(Errno(2)),
            _ => replier.error(Errno::ENOSYS),
        };
        Handled::Done
    }
}

/// Mounts at `mp`, runs INIT and binds clones up to `readers`.
fn mounted(mp: &Path, readers: usize) -> (Session, tsfuse::mount::Mount) {
    let (device, mount) = Mounter::default().mount(mp, &options()).unwrap();
    assert_eq!(mount.mountpoint(), mp);
    let mut session = Session::init(device, &InitConfig::tree()).unwrap();
    session
        .clone_readers(NonZeroUsize::new(readers).unwrap())
        .unwrap();
    assert_eq!(session.readers(), readers);
    (session, mount)
}

#[test]
fn a_mount_serves_through_cloned_readers_and_its_unmount_ends_every_reader() {
    with_fuse(|| {
        let (_dir, mp) = mountpoint("tsfuse-mount");
        let (session, mount) = mounted(&mp, 3);
        let fs = Arc::new(EmptyRoot::default());
        let serving = session.serve(fs.clone()).unwrap();

        assert!(std::fs::metadata(&mp).unwrap().is_dir());
        let missing = std::fs::metadata(mp.join("missing")).unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
        assert!(fs.getattrs.load(Ordering::SeqCst) >= 1);

        mount.unmount(false).unwrap();
        assert_eq!(serving.join().unwrap(), vec![ReaderEnd::Unmounted; 3]);
    });
}

#[test]
fn a_stop_leaves_the_mount_and_a_session_resumed_on_the_kept_descriptors_serves_it() {
    with_fuse(|| {
        let (_dir, mp) = mountpoint("tsfuse-resume");
        let (session, mount) = mounted(&mp, 2);
        let negotiated = *session.negotiated();
        let fs = Arc::new(EmptyRoot::default());
        let serving = session.serve(fs.clone()).unwrap();
        assert!(std::fs::metadata(&mp).unwrap().is_dir());

        // What a supervisor holds: every descriptor. The originals close when the readers end.
        let kept = serving.dup_devices().unwrap();
        serving.stopper().stop();
        assert_eq!(serving.join().unwrap(), vec![ReaderEnd::Stopped; 2]);

        let devices = kept.into_iter().map(DevFd::from_fd).collect();
        let resumed = Session::resume(devices, negotiated).unwrap();
        let before = fs.getattrs.load(Ordering::SeqCst);
        let serving = resumed.serve(fs.clone()).unwrap();
        assert!(std::fs::metadata(&mp).unwrap().is_dir());
        assert!(fs.getattrs.load(Ordering::SeqCst) > before);

        mount.unmount(true).unwrap();
        assert_eq!(serving.join().unwrap(), vec![ReaderEnd::Unmounted; 2]);
    });
}

#[test]
fn a_clone_needs_a_device_bound_to_a_connection() {
    with_fuse(|| {
        // A device no mount made: open(/dev/fuse) succeeds and the ioctl refuses it (EPERM on
        // the farm's Linux 7.0, EINVAL on older kernels).
        let unbound = DevFd::open().unwrap();
        let refused = unbound.clone_reader().unwrap_err().raw_os_error();
        assert!(matches!(refused, Some(1 | 22)), "{refused:?}");
        // Through a session whose device is a socket: the error names the reader it would have
        // been, and the session keeps the descriptors it had.
        let (device, kernel) = std::os::unix::net::UnixDatagram::pair().unwrap();
        kernel.send(&common::init_request(7, 45, 0)).unwrap();
        let mut session =
            Session::init(DevFd::from_fd(device.into()), &InitConfig::tree()).unwrap();
        session.clone_readers(NonZeroUsize::MIN).unwrap();
        let err = session
            .clone_readers(NonZeroUsize::new(2).unwrap())
            .unwrap_err();
        // The kernel answers EINVAL; a container's security policy may refuse the ioctl first.
        assert!(
            matches!(&err, SessionError::Clone { index: 1, .. }),
            "{err:?}"
        );
        assert!(
            err.to_string()
                .starts_with("cloning the FUSE device (reader 1) failed: "),
            "{err}"
        );
        assert_eq!(session.readers(), 1);
    });
}

#[test]
fn fusermount3_refusing_the_mount_is_an_error_with_its_stderr() {
    with_fuse(|| {
        let (dir, _mp) = mountpoint("tsfuse-helper");
        let absent = dir.path().join("absent");
        let err = Mounter::default()
            .mount_fusermount(&absent, &options())
            .unwrap_err();
        let MountError::Helper {
            program,
            mountpoint,
            status,
            stderr,
        } = &err
        else {
            panic!("{err:?}");
        };
        assert_eq!(program, Path::new("fusermount3"));
        assert_eq!(mountpoint, &absent);
        assert!(!status.success());
        assert!(stderr.contains("fusermount3"), "{stderr}");
        assert!(
            err.to_string()
                .starts_with(&format!("fusermount3 for {} failed (", absent.display())),
            "{err}"
        );
    });
}

#[test]
fn a_helper_that_succeeds_without_passing_a_descriptor_is_an_error() {
    with_fuse(|| {
        let (_dir, mp) = mountpoint("tsfuse-nofd");
        let mounter = Mounter {
            fusermount: PathBuf::from("true"),
            ..Mounter::default()
        };
        let err = mounter.mount_fusermount(&mp, &options()).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "true mounted {} and passed no FUSE descriptor",
                mp.display()
            )
        );
    });
}

#[test]
fn a_helper_that_cannot_be_started_is_an_error() {
    let (_dir, mp) = mountpoint("tsfuse-nohelper");
    let mounter = Mounter {
        fusermount: PathBuf::from("/nonexistent/tsfuse/fusermount3"),
        ..Mounter::default()
    };
    let err = mounter.mount_fusermount(&mp, &options()).unwrap_err();
    let MountError::Spawn { source, .. } = &err else {
        panic!("{err:?}");
    };
    assert_eq!(source.kind(), io::ErrorKind::NotFound);
    assert!(
        err.to_string()
            .starts_with("starting /nonexistent/tsfuse/fusermount3 failed: "),
        "{err}"
    );
}

#[test]
fn a_device_that_cannot_be_opened_fails_the_mount_without_the_helper() {
    let (_dir, mp) = mountpoint("tsfuse-nodev");
    let mounter = Mounter {
        device: PathBuf::from("/nonexistent/tsfuse/fuse"),
        fusermount: PathBuf::from("/nonexistent/tsfuse/fusermount3"),
        ..Mounter::default()
    };
    // An open error is not EPERM: `mount` does not fall back to the helper.
    let err = mounter.mount(&mp, &options()).unwrap_err();
    let MountError::Open { path, source } = &err else {
        panic!("{err:?}");
    };
    assert_eq!(path, Path::new("/nonexistent/tsfuse/fuse"));
    assert_eq!(source.kind(), io::ErrorKind::NotFound);
}

#[test]
fn invalid_options_are_refused_before_any_mount() {
    let (_dir, mp) = mountpoint("tsfuse-badopt");
    let bad = MountOptions {
        fsname: "a,b".to_owned(),
        ..options()
    };
    let mounter = Mounter {
        device: PathBuf::from("/nonexistent/tsfuse/fuse"),
        fusermount: PathBuf::from("/nonexistent/tsfuse/fusermount3"),
        ..Mounter::default()
    };
    for err in [
        mounter.mount_syscall(&mp, &bad).unwrap_err(),
        mounter.mount_fusermount(&mp, &bad).unwrap_err(),
    ] {
        assert!(
            matches!(
                err,
                MountError::InvalidOption {
                    option: "fsname",
                    ..
                }
            ),
            "{err:?}"
        );
    }
}

#[test]
fn unmounting_a_directory_that_is_not_mounted_fails_by_either_method() {
    with_fuse(|| {
        let (_dir, mp) = mountpoint("tsfuse-notmounted");
        let mounter = Mounter::default();
        let helper = tsfuse::mount::Mount::adopt(&mp, MountMethod::Fusermount, &mounter);
        assert_eq!(helper.method(), MountMethod::Fusermount);
        let err = helper.unmount(true).unwrap_err();
        assert!(matches!(err, MountError::Helper { .. }), "{err:?}");
        // EPERM for a user without CAP_SYS_ADMIN, EINVAL for root: not a mount point. Lazy or
        // not.
        let lazy = tsfuse::mount::Mount::adopt(&mp, MountMethod::Syscall, &mounter);
        assert!(matches!(
            lazy.unmount(true).unwrap_err(),
            MountError::Unmount { .. }
        ));
        let syscall = tsfuse::mount::Mount::adopt(&mp, MountMethod::Syscall, &mounter);
        let err = syscall.unmount(false).unwrap_err();
        let MountError::Unmount { mountpoint, .. } = &err else {
            panic!("{err:?}");
        };
        assert_eq!(mountpoint, &mp);
        assert!(
            err.to_string()
                .starts_with(&format!("unmounting {} failed: ", mp.display())),
            "{err}"
        );
    });
}

#[test]
fn abort_writes_the_abort_file_of_the_mounts_connection() {
    let (dir, mp) = mountpoint("tsfuse-abort");
    let connections = dir.path().join("connections");
    let mounter = Mounter {
        connections: connections.clone(),
        ..Mounter::default()
    };
    let mount = tsfuse::mount::Mount::adopt(&mp, MountMethod::Syscall, &mounter);
    let id = mount.connection().unwrap();
    let stat = rustix::fs::stat(&mp).unwrap();
    assert_eq!(
        id.0,
        (rustix::fs::major(stat.st_dev) << 20) | rustix::fs::minor(stat.st_dev),
        "the kernel's device number of the mount point"
    );
    let abort = connections.join(id.0.to_string()).join("abort");

    // No such connection: the error names the file.
    let err = mount.abort().unwrap_err();
    assert!(
        matches!(&err, MountError::Abort { path, source } if path == &abort && source.kind() == io::ErrorKind::NotFound),
        "{err:?}"
    );

    std::fs::create_dir_all(abort.parent().unwrap()).unwrap();
    std::fs::write(&abort, b"").unwrap();
    mount.abort().unwrap();
    assert_eq!(std::fs::read(&abort).unwrap(), b"1");
}

#[test]
fn the_connection_of_a_missing_mount_point_is_an_error() {
    let (dir, _mp) = mountpoint("tsfuse-noconn");
    let absent = dir.path().join("absent");
    let mount = tsfuse::mount::Mount::adopt(&absent, MountMethod::Fusermount, &Mounter::default());
    let err = mount.connection().unwrap_err();
    assert!(
        matches!(&err, MountError::Connection { source, .. } if source.kind() == io::ErrorKind::NotFound),
        "{err:?}"
    );
    // Abort reads the connection first.
    assert!(matches!(
        mount.abort().unwrap_err(),
        MountError::Connection { .. }
    ));
}
