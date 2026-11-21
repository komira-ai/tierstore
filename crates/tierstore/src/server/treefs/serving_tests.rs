//! The readers of a connection: stopped, started again, and the end of a mount.

use std::num::NonZeroUsize;

use super::{Serving, attach, readers};
use crate::server::treefs::fakedev;
use crate::server::treefs::ino;
use crate::server::treefs::testing::{Rig, sample, with_fuse};

/// Asserts that stopping the readers twice is the same as once, that a request that comes while they
/// are stopped waits, and that starting them again answers it from the same descriptor.
#[test]
fn stopped_readers_are_stopped_once_and_start_again_where_they_were() {
    let rig = Rig::new("tfs-serving-restart", sample());
    let settings = rig.settings();
    let (device, kernel) = fakedev::pair();
    let outcome = fakedev::outcome(false);
    let mut serving = Serving::adopt(&rig.core, &settings, device, outcome, (None, false)).unwrap();
    kernel.getattr(1, ino::ROOT);
    assert_eq!(kernel.reply().unique, 1);
    serving.stop_reading();
    serving.stop_reading();
    kernel.getattr(2, ino::ROOT);
    kernel.assert_silent();
    serving.restart(&rig.core, &settings).unwrap();
    assert_eq!(kernel.reply().unique, 2);
    assert_eq!(serving.outcome(), outcome);
    // A connection that was set up without resend is not asked to send anything again.
    assert!(!serving.resend_failed());
    kernel.assert_silent();
}

/// Asserts that ending a serving that did not make its mount stops its readers rather than
/// waiting for an unmount that will not come.
#[test]
fn ending_a_serving_that_made_no_mount_stops_its_readers() {
    let rig = Rig::new("tfs-serving-unmount", sample());
    let (device, kernel) = fakedev::pair();
    let serving = Serving::adopt(
        &rig.core,
        &rig.settings(),
        device,
        fakedev::outcome(false),
        (None, false),
    )
    .unwrap();
    kernel.getattr(1, ino::ROOT);
    assert_eq!(kernel.reply().unique, 1);
    serving.unmount().unwrap();
}

/// Asserts that a serving that made the mount unmounts it, and that its descriptor is a copy of the
/// connection's.
#[test]
fn a_serving_that_made_the_mount_unmounts_it() {
    with_fuse(mounts_and_unmounts);
}

fn mounts_and_unmounts() {
    let rig = Rig::new("tfs-serving-mount", sample());
    let serving = Serving::mount(&rig.core, &rig.settings()).unwrap();
    assert!(serving.outcome().max_write > 0);
    let copy = serving.device().unwrap();
    drop(copy);
    let table = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    assert!(table.contains(rig.mountpoint.to_str().unwrap()));
    serving.unmount().unwrap();
    let table = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    assert!(!table.contains(rig.mountpoint.to_str().unwrap()));
}

/// Asserts that a serving whose readers are stopped has nothing left to do when it is ended, and
/// that ending a serving that made no mount does not wait for the kernel.
#[test]
fn a_serving_ended_after_its_readers_stopped_has_nothing_left_to_do() {
    let rig = Rig::new("tfs-serving-stopped", sample());
    let (device, _kernel) = fakedev::pair();
    let mut serving = Serving::adopt(
        &rig.core,
        &rig.settings(),
        device,
        fakedev::outcome(false),
        (None, false),
    )
    .unwrap();
    serving.stop_reading();
    serving.unmount().unwrap();
}

/// Asserts that a mount that cannot be made is an error of the system, which names what failed,
/// and leaves nothing mounted: the mount point is not there.
#[test]
fn a_mount_that_cannot_be_made_is_an_error() {
    let rig = Rig::new("tfs-serving-nomount", sample());
    let settings = crate::server::treefs::config::FsSettings {
        mountpoint: rig.dir.path().join("absent").join("mnt"),
        ..rig.settings()
    };
    let error = Serving::mount(&rig.core, &settings).err().unwrap();
    assert!(!error.to_string().is_empty());
    assert!(!settings.mountpoint.exists());
}

/// Asserts that a serving that made the mount and whose readers were stopped leaves the mount
/// when it is ended: nothing would answer the kernel's `DESTROY` of an unmount.
#[test]
fn a_mount_whose_readers_were_stopped_is_left_mounted_when_ended() {
    with_fuse(stops_and_leaves);
}

fn stops_and_leaves() {
    let rig = Rig::new("tfs-serving-leave", sample());
    let mut serving = Serving::mount(&rig.core, &rig.settings()).unwrap();
    serving.stop_reading();
    serving.unmount().unwrap();
    let mounted = || {
        std::fs::read_to_string("/proc/self/mountinfo")
            .unwrap()
            .contains(rig.mountpoint.to_str().unwrap())
    };
    assert!(mounted());
    let status = std::process::Command::new("fusermount3")
        .args(["-u", "-z"])
        .arg(&rig.mountpoint)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(!mounted());
}

/// Asserts that a mount whose readers cannot be started is not left behind: past the most
/// descriptors a session holds, the start fails after the mount was made, and the mount is gone.
#[test]
fn a_mount_whose_readers_cannot_start_is_unmounted() {
    with_fuse(unmounts_on_failure);
}

fn unmounts_on_failure() {
    let rig = Rig::new("tfs-serving-toomany", sample());
    let settings = crate::server::treefs::config::FsSettings {
        threads: tsfuse::MAX_DEVICES + 1,
        ..rig.settings()
    };
    assert!(Serving::mount(&rig.core, &settings).is_err());
    let table = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    assert!(!table.contains(rig.mountpoint.to_str().unwrap()));
}

/// Asserts that the readers of a connection are `fs.threads` copies of its one descriptor, at
/// least one: a request sent to the descriptor is read and answered once, by one of them.
#[test]
fn the_readers_are_as_many_copies_of_the_descriptor_as_the_settings_say() {
    let rig = Rig::new("tfs-serving-readers", sample());
    let three = crate::server::treefs::config::FsSettings {
        threads: 3,
        ..rig.settings()
    };
    assert_eq!(readers(&three), NonZeroUsize::new(3).unwrap());
    let none = crate::server::treefs::config::FsSettings {
        threads: 0,
        ..rig.settings()
    };
    assert_eq!(readers(&none), NonZeroUsize::MIN);
    let (device, kernel) = fakedev::pair();
    let outcome = fakedev::outcome(false);
    let session = attach(&device, outcome.negotiated(), readers(&three)).unwrap();
    assert_eq!(session.readers(), 3);
    let serving = session
        .serve(std::sync::Arc::new(
            super::NodeFs::new(rig.core.clone(), std::num::NonZeroUsize::MIN).unwrap(),
        ))
        .unwrap();
    kernel.getattr(1, ino::ROOT);
    assert_eq!(kernel.reply().unique, 1);
    kernel.assert_silent();
    let _ = serving.hand_over().unwrap();
}
