//! The file of the provider's socket: a stale file replaced, a newer one kept, mode 0600, a
//! directory in the way, and the pause of a rebind.

use std::os::unix::fs::PermissionsExt as _;
use std::time::Duration;

use tierstore_testkit::dir::TestDir;

use super::{SocketFile, bind_socket, pause_if};

/// Asserts that a file left at the path by an instance that is gone is replaced by the socket,
/// which is mode 0600 and removed when its file is dropped; and that the file of a socket another
/// instance made at the path since is not removed by the one that made the first.
#[tokio::test]
async fn a_stale_file_is_replaced_and_a_newer_one_is_not_removed() {
    let dir = TestDir::under(std::env::temp_dir(), "provider-stale");
    let path = dir.path().join("p.sock");
    std::fs::write(&path, b"left over").unwrap();
    let (listener, file) = bind_socket(&path).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    drop(listener);
    drop(file);
    assert!(!path.exists());

    let first = dir.path().join("a.sock");
    let listener = std::os::unix::net::UnixListener::bind(&first).unwrap();
    let ours = SocketFile {
        path: first.clone(),
        identity: SocketFile::identity_of(&first).unwrap(),
    };
    std::fs::remove_file(&first).unwrap();
    let _newer = std::os::unix::net::UnixListener::bind(&first).unwrap();
    drop(ours);
    assert!(first.exists());
    drop(listener);
}

/// Asserts that a path that is a directory cannot be bound: the file in the way cannot be removed,
/// and the socket is not made.
#[tokio::test]
async fn a_directory_in_the_way_is_not_replaced() {
    let dir = TestDir::under(std::env::temp_dir(), "provider-dir");
    let path = dir.path().join("p.sock");
    std::fs::create_dir(&path).unwrap();
    assert!(bind_socket(&path).is_err());
    assert!(path.is_dir());
}

/// Asserts that a pause that is wanted ends after its time, on a paused clock, and that one that
/// is not wanted never ends.
#[tokio::test(start_paused = true)]
async fn a_pause_ends_only_when_wanted() {
    pause_if(true, Duration::from_secs(1)).await;
    let never = tokio::time::timeout(Duration::from_secs(3600), pause_if(false, Duration::ZERO));
    assert!(never.await.is_err());
}
