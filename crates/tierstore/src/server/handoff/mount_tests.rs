//! The check of the kind of an offered socket: a listening stream socket, and nothing else.

use std::os::fd::AsFd as _;

use tierstore_testkit::dir::TestDir;

use super::{Claim, Holding, check_control, check_listening};

/// Asserts that a listening TCP socket and a listening Unix socket pass, and that a connected TCP
/// socket, a Unix stream socket whose peer is gone, a datagram socket and a regular file are
/// refused with words that name what was checked. It would catch a taker that serves on a
/// descriptor that is not a listening socket, which accepts nothing, or which is some other file
/// the running instance had open; and, on macOS, which asks the peer, a check that takes a socket
/// without one for a listening socket.
#[test]
fn only_a_listening_stream_socket_is_a_listening_socket() {
    let dir = TestDir::under(std::env::temp_dir(), "handoff-kinds");
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    check_listening(tcp.as_fd(), "the s3 socket").unwrap();
    let unix = std::os::unix::net::UnixListener::bind(dir.path().join("control.sock")).unwrap();
    check_listening(unix.as_fd(), "the control socket").unwrap();
    let connected = std::net::TcpStream::connect(tcp.local_addr().unwrap()).unwrap();
    let (left, gone) = std::os::unix::net::UnixStream::pair().unwrap();
    drop(gone);
    let datagram = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let path = dir.path().join("file");
    std::fs::write(&path, b"").unwrap();
    let file = std::fs::File::open(&path).unwrap();
    let all = [
        connected.as_fd(),
        left.as_fd(),
        datagram.as_fd(),
        file.as_fd(),
    ];
    for refused in all {
        let error = check_listening(refused, "the s3 socket").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            error.to_string(),
            "the descriptor of the s3 socket is not a listening socket"
        );
    }
}

/// Asserts that an offered control socket must be a listening Unix socket: one passes, and a
/// listening TCP socket, a Unix socket that does not listen (whose peer is gone, or still there)
/// and a regular file are refused with words that say so. It would catch a successor that serves
/// its control requests on a socket of another kind.
#[test]
fn only_a_listening_unix_socket_is_a_control_socket() {
    let dir = TestDir::under(std::env::temp_dir(), "handoff-control-kind");
    let unix = std::os::unix::net::UnixListener::bind(dir.path().join("control.sock")).unwrap();
    check_control(unix.as_fd()).unwrap();
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let (left, gone) = std::os::unix::net::UnixStream::pair().unwrap();
    drop(gone);
    let (paired, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let path = dir.path().join("file");
    std::fs::write(&path, b"").unwrap();
    let file = std::fs::File::open(&path).unwrap();
    for refused in [tcp.as_fd(), left.as_fd(), paired.as_fd(), file.as_fd()] {
        let error = check_control(refused).unwrap_err();
        assert_eq!(
            error.to_string(),
            "the descriptor of the control socket is not a listening Unix socket"
        );
    }
}

/// Asserts that the release of a claim puts a taken mount back to [`Holding::Here`], and leaves a
/// mount that is not taken as it is: one that no taker claimed stays here, and one that was given up
/// while the conversation ended stays gone, so that it is never offered again.
#[test]
fn release_returns_only_a_taken_mount() {
    let claim = Claim::default();
    claim.release();
    assert_eq!(claim.holding(), Holding::Here);
    assert_eq!(claim.claim(), Holding::Here);
    assert_eq!(claim.holding(), Holding::Taking);
    claim.release();
    assert_eq!(claim.holding(), Holding::Here);
    assert_eq!(claim.claim(), Holding::Here);
    claim.give_up();
    claim.release();
    assert_eq!(claim.holding(), Holding::Gone);
    assert_eq!(claim.claim(), Holding::Gone);
}
