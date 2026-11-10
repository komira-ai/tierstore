//! The readers of a connection: stopped, started again, and the end of a mount.

use super::Serving;
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
