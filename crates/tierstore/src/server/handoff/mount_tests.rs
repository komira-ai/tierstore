//! The check of the kind of an offered socket: a listening stream socket, and nothing else.

use std::os::fd::AsFd as _;

use tierstore_testkit::dir::TestDir;

use super::{check_control, check_listening};

/// Asserts that a listening TCP socket and a listening Unix socket pass, and that a connected TCP
/// socket, a datagram socket and a regular file are refused with words that name what was
/// checked. It would catch a taker that serves on a descriptor that is not a listening socket,
/// which accepts nothing, or which is some other file the running instance had open.
#[test]
fn only_a_listening_stream_socket_is_a_listening_socket() {
    let dir = TestDir::under(std::env::temp_dir(), "handoff-kinds");
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    check_listening(tcp.as_fd(), "the s3 socket").unwrap();
    let unix = std::os::unix::net::UnixListener::bind(dir.path().join("control.sock")).unwrap();
    check_listening(unix.as_fd(), "the control socket").unwrap();
    let connected = std::net::TcpStream::connect(tcp.local_addr().unwrap()).unwrap();
    let datagram = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let path = dir.path().join("file");
    std::fs::write(&path, b"").unwrap();
    let file = std::fs::File::open(&path).unwrap();
    for refused in [connected.as_fd(), datagram.as_fd(), file.as_fd()] {
        let error = check_listening(refused, "the s3 socket").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            error.to_string(),
            "the descriptor of the s3 socket is not a listening socket"
        );
    }
}

/// Asserts that an offered control socket must be a listening Unix socket: one passes, and a
/// listening TCP socket, a Unix socket that does not listen and a regular file are refused with
/// words that say so. It would catch a successor that serves its control requests on a socket of
/// another kind.
#[test]
fn only_a_listening_unix_socket_is_a_control_socket() {
    let dir = TestDir::under(std::env::temp_dir(), "handoff-control-kind");
    let unix = std::os::unix::net::UnixListener::bind(dir.path().join("control.sock")).unwrap();
    check_control(unix.as_fd()).unwrap();
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let (one, _) = std::os::unix::net::UnixStream::pair().unwrap();
    let path = dir.path().join("file");
    std::fs::write(&path, b"").unwrap();
    let file = std::fs::File::open(&path).unwrap();
    for refused in [tcp.as_fd(), one.as_fd(), file.as_fd()] {
        let error = check_control(refused).unwrap_err();
        assert_eq!(
            error.to_string(),
            "the descriptor of the control socket is not a listening Unix socket"
        );
    }
}
