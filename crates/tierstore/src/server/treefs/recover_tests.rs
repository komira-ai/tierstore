//! The bound on a restart where the kernel cannot send requests again.

use std::path::Path;
use std::time::Duration;

use tierstore_testkit::dir::TestDir;

use super::{Connection, SAMPLE, Verdict, connection_of, guard, looks_in, watch};

/// The lines of `/proc/self/mountinfo` of a node with two FUSE mounts and a mount of another kind
/// at the path of ours.
const MOUNTINFO: &str = "\
24 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw
36 24 0:45 / /run/tierstore/fs rw,nosuid,nodev,relatime shared:19 - fuse.tierstore-fs tierstore-fs rw,user_id=0
40 24 0:51 / /mnt/other rw,relatime - fuse.sshfs user@host: rw
41 24 0:52 / /run/tierstore/nfs rw,relatime - nfs4 server:/ rw
";

/// Asserts that the connection number is the minor number of the device of the FUSE mount at the
/// path, that the last mount at a path counts, and that a path with no FUSE mount, or one of
/// another kind, has none.
#[test]
fn the_connection_of_a_mount_is_the_minor_number_of_its_device() {
    let at = |path: &str| connection_of(MOUNTINFO, Path::new(path));
    assert_eq!(at("/run/tierstore/fs"), Some(45));
    assert_eq!(at("/mnt/other"), Some(51));
    assert_eq!(at("/run/tierstore/nfs"), None);
    assert_eq!(at("/nowhere"), None);
    assert_eq!(at("/"), None);
    let stacked = format!("{MOUNTINFO}50 24 0:99 / /run/tierstore/fs rw - fuse x rw\n");
    assert_eq!(
        connection_of(&stacked, Path::new("/run/tierstore/fs")),
        Some(99)
    );
    assert_eq!(connection_of("short\nnot - enough\n", Path::new("/")), None);
    assert_eq!(
        connection_of("1 2 nodevice / /x - fuse a b", Path::new("/x")),
        None
    );
}

/// Asserts the reading of the kernel's files: the number of waiting requests, none when the
/// file is not there or not a number, and that an abort writes to the file the kernel watches.
#[test]
fn the_waiting_requests_are_read_and_the_abort_is_written() {
    let root = TestDir::under(std::env::temp_dir(), "tfs-recover-conn");
    let connection = Connection::new(root.path(), 45);
    assert_eq!(connection.waiting(), None);
    std::fs::create_dir(root.path().join("45")).unwrap();
    assert_eq!(connection.waiting(), None);
    std::fs::write(root.path().join("45/waiting"), "3\n").unwrap();
    assert_eq!(connection.waiting(), Some(3));
    std::fs::write(root.path().join("45/waiting"), "many").unwrap();
    assert_eq!(connection.waiting(), None);
    connection.abort().unwrap();
    assert_eq!(
        std::fs::read_to_string(root.path().join("45/abort")).unwrap(),
        "1"
    );
    assert!(Connection::new(root.path(), 46).abort().is_err());
}

/// Asserts the three verdicts, and that the watch pauses between looks and stops at the first
/// that finds none waiting.
#[test]
fn the_watch_ends_at_the_first_look_that_finds_nothing_waiting() {
    let mut pauses = 0;
    let mut seen = vec![Some(2), Some(1), Some(0), Some(5)].into_iter();
    let verdict = watch(|| seen.next().unwrap(), || pauses += 1, 10);
    assert_eq!((verdict, pauses), (Verdict::Answered, 2));
    let mut pauses = 0;
    assert_eq!(watch(|| Some(1), || pauses += 1, 4), Verdict::Hung);
    assert_eq!(pauses, 4);
    assert_eq!(watch(|| None, || (), 4), Verdict::Unwatched);
    assert_eq!(watch(|| Some(1), || (), 0), Verdict::Hung);
}

/// Asserts how many looks a wait is: one per sample, and at least one.
#[test]
fn a_wait_is_a_number_of_looks() {
    assert_eq!(looks_in(Duration::from_secs(30)), 300);
    assert_eq!(looks_in(SAMPLE), 1);
    assert_eq!(looks_in(Duration::from_millis(1)), 1);
    assert_eq!(looks_in(Duration::MAX), u32::MAX);
}

/// What a guard calls on a hang: it sends on `hung`.
fn hang(hung: &std::sync::mpsc::Sender<()>) -> impl FnOnce() + Send + 'static + use<> {
    let hung = hung.clone();
    move || hung.send(()).unwrap()
}

/// Asserts the guard: a connection that is found idle is left alone; one found hung is aborted and
/// the hang is sent; one that cannot be read is left alone and said.
#[test]
fn a_guard_aborts_a_hung_connection_and_leaves_an_idle_one() {
    let root = TestDir::under(std::env::temp_dir(), "tfs-recover-guard");
    let (hung, heard) = std::sync::mpsc::channel();
    let dir = |number: &str, waiting: Option<&str>| {
        std::fs::create_dir_all(root.path().join(number)).unwrap();
        if let Some(waiting) = waiting {
            std::fs::write(root.path().join(number).join("waiting"), waiting).unwrap();
        }
    };
    let wait = Duration::from_millis(100);

    dir("1", Some("0"));
    let verdict = guard(Connection::new(root.path(), 1), wait, hang(&hung))
        .join()
        .unwrap();
    assert_eq!(verdict, Verdict::Answered);
    assert!(!root.path().join("1/abort").exists());

    dir("2", None);
    let verdict = guard(Connection::new(root.path(), 2), wait, hang(&hung))
        .join()
        .unwrap();
    assert_eq!(verdict, Verdict::Unwatched);
    assert!(!root.path().join("2/abort").exists());
    assert!(heard.try_recv().is_err());

    dir("3", Some("2"));
    let verdict = guard(Connection::new(root.path(), 3), wait, hang(&hung))
        .join()
        .unwrap();
    assert_eq!(verdict, Verdict::Hung);
    assert_eq!(
        std::fs::read_to_string(root.path().join("3/abort")).unwrap(),
        "1"
    );
    heard.try_recv().unwrap();

    // An abort that cannot be written is said, and the hang is sent all the same.
    dir("4", Some("2"));
    std::fs::create_dir(root.path().join("4/abort")).unwrap();
    let verdict = guard(Connection::new(root.path(), 4), wait, hang(&hung))
        .join()
        .unwrap();
    assert_eq!(verdict, Verdict::Hung);
    heard.try_recv().unwrap();
}
