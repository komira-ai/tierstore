//! The fake drive on the injected clock: service times, EIO, hangs, crashes, requests in flight
//! (section 14: "Group commit (fake NVMe, injected clock)", "Drive scheduler (fake drive,
//! injected clock)", "Hung drive", "I/O errors"). Nothing here sleeps: a test waits for the
//! drive's `"start"` event (the request is then a sleeper of the clock) and moves the clock by
//! hand.

use tierstore_fetch_api::clock::{Clock, ManualClock};
use tierstore_harness::disk::{DiskError, DriveStep, FakeDisk, FakeDrive, OpKind};

fn drive_with_file() -> (FakeDrive, ManualClock) {
    let disk = FakeDisk::new();
    disk.create("f")
        .unwrap_or_else(|e| panic!("setup of the starting disk failed: {e}"));
    let clock = ManualClock::new();
    (FakeDrive::new(disk, clock.clone()), clock)
}

/// A request completes only when the clock reaches its service time; until then it is a sleeper
/// of the clock and in flight, and its effect is not yet on the disk.
///
/// Catches: a drive that applies the op at the start of the request, or completes without the
/// clock moving.
#[tokio::test]
async fn a_request_completes_only_when_the_clock_reaches_its_service_time() {
    let (drive, clock) = drive_with_file();
    drive.set_default_service_ms(5);
    let d = drive.clone();
    let h = tokio::spawn(async move { d.write_at("f", 0, b"abc").await });
    drive.progress().reached("start", 1).await;
    assert_eq!(clock.sleepers(), 1);
    assert_eq!(drive.in_flight(), 1);
    assert_eq!(drive.disk().read_all("f").unwrap(), b"");
    clock.advance(4);
    assert!(!h.is_finished());
    assert_eq!(drive.completed(OpKind::WriteAt), 0);
    clock.advance(1);
    h.await.unwrap().unwrap();
    assert_eq!(drive.completed(OpKind::WriteAt), 1);
    assert_eq!(drive.in_flight(), 0);
    assert_eq!(clock.sleepers(), 0);
    assert_eq!(drive.disk().read_all("f").unwrap(), b"abc");
    assert_eq!(drive.progress().count("done"), 1);
}

/// Scripted steps are taken in order per op kind, each with its own service time, and the
/// default step applies once the queue is empty; other op kinds are unaffected by the script.
#[tokio::test]
async fn scripted_service_times_are_taken_in_order_per_kind() {
    let (drive, clock) = drive_with_file();
    drive.script(OpKind::WriteAt, [DriveStep::ok(10), DriveStep::ok(3)]);
    let d = drive.clone();
    let slow = tokio::spawn(async move { d.write_at("f", 0, b"slow").await });
    drive.progress().reached("start", 1).await;
    clock.advance(2);
    let d = drive.clone();
    let fast = tokio::spawn(async move { d.write_at("f", 4, b"fast").await });
    drive.progress().reached("start", 2).await;
    assert_eq!(clock.sleepers(), 2);
    assert_eq!(drive.oldest_in_flight_since(), Some(0));
    clock.advance(3);
    fast.await.unwrap().unwrap();
    assert!(!slow.is_finished());
    assert_eq!(drive.in_flight(), 1);
    assert_eq!(drive.oldest_in_flight_since(), Some(0));
    clock.advance(5);
    slow.await.unwrap().unwrap();
    assert_eq!(drive.oldest_in_flight_since(), None);
    // The script is used up: a third write takes the default 0 ms and needs no clock movement.
    drive.write_at("f", 0, b"last").await.unwrap();
    assert_eq!(clock.now_ms(), 10);
    assert_eq!(drive.disk().read_all("f").unwrap(), b"lastfast");
}

/// The oldest start in flight is what a watchdog compares to `stall_after`: it is the earliest
/// start among the requests in flight, and moves on when that request ends.
#[tokio::test]
async fn the_oldest_in_flight_start_moves_when_the_oldest_request_ends() {
    let (drive, clock) = drive_with_file();
    drive.script(OpKind::Fdatasync, [DriveStep::ok(4), DriveStep::ok(100)]);
    let d = drive.clone();
    let first = tokio::spawn(async move { d.fdatasync("f").await });
    drive.progress().reached("start", 1).await;
    clock.advance(1);
    let d = drive.clone();
    let second = tokio::spawn(async move { d.fdatasync("f").await });
    drive.progress().reached("start", 2).await;
    assert_eq!(drive.oldest_in_flight_since(), Some(0));
    clock.advance(3);
    first.await.unwrap().unwrap();
    assert_eq!(drive.oldest_in_flight_since(), Some(1));
    clock.advance(100);
    second.await.unwrap().unwrap();
    assert_eq!(drive.oldest_in_flight_since(), None);
}

/// Concurrent requests are all in flight at once, the peak is kept after they end, and each
/// request marks "start" and "done".
#[tokio::test]
async fn peak_concurrency_is_tracked() {
    let (drive, clock) = drive_with_file();
    drive.set_default_service_ms(5);
    let tasks: Vec<_> = (0..3)
        .map(|i| {
            let d = drive.clone();
            tokio::spawn(async move { d.write_at("f", i, b"x").await })
        })
        .collect();
    drive.progress().reached("start", 3).await;
    assert_eq!(clock.sleepers(), 3);
    assert_eq!(drive.in_flight(), 3);
    assert_eq!(drive.peak_in_flight(), 3);
    clock.advance(5);
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    assert_eq!(drive.in_flight(), 0);
    assert_eq!(drive.peak_in_flight(), 3);
    assert_eq!(drive.progress().count("done"), 3);
    assert_eq!(drive.completed(OpKind::WriteAt), 3);
}

/// A scripted EIO completes after its service time, fails with an EIO naming the op and path,
/// has no effect on the disk, and the next request succeeds.
#[tokio::test]
async fn an_eio_step_fails_the_request_without_effect() {
    let (drive, clock) = drive_with_file();
    drive.disk().write_at("f", 0, b"abc").unwrap();
    drive.script(OpKind::Fdatasync, [DriveStep::eio(2)]);
    let d = drive.clone();
    let h = tokio::spawn(async move { d.fdatasync("f").await });
    drive.progress().reached("start", 1).await;
    clock.advance(1);
    assert!(!h.is_finished());
    clock.advance(1);
    assert_eq!(
        h.await.unwrap(),
        Err(DiskError::Eio {
            op: OpKind::Fdatasync,
            path: "f".into()
        })
    );
    assert_eq!(drive.disk().fdatasyncs("f"), 0);
    assert_eq!(drive.completed(OpKind::Fdatasync), 1);
    drive.fdatasync("f").await.unwrap();
    assert_eq!(drive.disk().fdatasyncs("f"), 1);
}

/// A hung request never completes, however far the clock moves, and is in flight with its start
/// time (so a watchdog sees it stall); `release_hangs` completes it as `Ok` and the drive is idle.
///
/// Catches: a hang that a clock advance releases, or one that is not counted in flight.
#[tokio::test]
async fn a_hung_request_completes_only_when_released() {
    let (drive, clock) = drive_with_file();
    drive.disk().write_at("f", 0, b"abc").unwrap();
    drive.script(OpKind::Fdatasync, [DriveStep::hang()]);
    clock.advance(7);
    let d = drive.clone();
    let h = tokio::spawn(async move { d.fdatasync("f").await });
    drive.progress().reached("start", 1).await;
    assert_eq!(
        clock.sleepers(),
        0,
        "a hang waits on the release, not on the clock"
    );
    clock.advance(1_000_000);
    assert!(!h.is_finished());
    assert_eq!(drive.in_flight(), 1);
    assert_eq!(drive.oldest_in_flight_since(), Some(7));
    assert_eq!(drive.disk().fdatasyncs("f"), 0);
    drive.release_hangs();
    h.await.unwrap().unwrap();
    assert_eq!(drive.in_flight(), 0);
    assert_eq!(drive.disk().fdatasyncs("f"), 1);
    assert_eq!(drive.completed(OpKind::Fdatasync), 1);
}

/// A hang started after a release hangs again until the next release.
#[tokio::test]
async fn a_hang_after_a_release_waits_for_the_next_release() {
    let (drive, _clock) = drive_with_file();
    drive.release_hangs();
    drive.script(OpKind::Unlink, [DriveStep::hang()]);
    let d = drive.clone();
    let h = tokio::spawn(async move { d.unlink("f").await });
    drive.progress().reached("start", 1).await;
    assert!(!h.is_finished());
    drive.release_hangs();
    h.await.unwrap().unwrap();
    assert!(!drive.disk().exists("f").unwrap());
}

/// A request dropped while hanging leaves the in-flight list and does not count as completed.
#[tokio::test]
async fn a_dropped_request_leaves_flight() {
    let (drive, _clock) = drive_with_file();
    drive.script(OpKind::WriteAt, [DriveStep::hang()]);
    let d = drive.clone();
    let h = tokio::spawn(async move { d.write_at("f", 0, b"x").await });
    drive.progress().reached("start", 1).await;
    assert_eq!(drive.in_flight(), 1);
    h.abort();
    assert!(h.await.unwrap_err().is_cancelled());
    assert_eq!(drive.in_flight(), 0);
    assert_eq!(drive.completed(OpKind::WriteAt), 0);
    assert_eq!(drive.progress().count("done"), 0);
}

/// A scripted crash crashes the disk at once, without waiting for the clock, and fails the
/// request; later requests fail as crashed until the disk restarts.
#[tokio::test]
async fn a_crash_step_crashes_the_disk() {
    let (drive, clock) = drive_with_file();
    drive.script(OpKind::WriteAt, [DriveStep::crash()]);
    assert_eq!(drive.write_at("f", 0, b"x").await, Err(DiskError::Crashed));
    assert!(drive.disk().is_crashed());
    assert_eq!(clock.now_ms(), 0);
    assert_eq!(drive.completed(OpKind::WriteAt), 1);
    assert_eq!(drive.create("g").await, Err(DiskError::Crashed));
    drive.disk().restart();
    drive.create("g").await.unwrap();
}

/// Every op of the disk is served through the drive with the disk's own results and errors.
#[tokio::test]
async fn every_op_is_served_through_the_drive() {
    let (drive, _clock) = drive_with_file();
    drive.create("d/a").await.unwrap();
    assert_eq!(
        drive.create("d/a").await,
        Err(DiskError::Exists("d/a".into()))
    );
    drive.write_at("d/a", 0, b"abcdef").await.unwrap();
    assert_eq!(drive.read_at("d/a", 1, 3).await.unwrap(), b"bcd");
    drive.punch_hole("d/a", 0, 2).await.unwrap();
    drive.set_len("d/a", 4).await.unwrap();
    assert_eq!(drive.disk().read_all("d/a").unwrap(), b"\0\0cd");
    drive.fdatasync("d/a").await.unwrap();
    drive.rename("d/a", "d/b", true).await.unwrap();
    drive.fsync_dir("d").await.unwrap();
    drive.disk().crash();
    drive.disk().restart();
    assert_eq!(drive.disk().read_all("d/b").unwrap(), b"\0\0cd");
    drive.unlink("d/b").await.unwrap();
    assert_eq!(
        drive.unlink("d/b").await,
        Err(DiskError::NotFound("d/b".into()))
    );
    assert_eq!(drive.completed(OpKind::Unlink), 2);
    assert_eq!(drive.completed(OpKind::FsyncDir), 1);
}
