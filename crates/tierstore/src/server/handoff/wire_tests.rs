//! The frames of the control socket, over a pair of connected sockets.

use std::io::ErrorKind;
use std::net::TcpListener;
use std::os::fd::AsFd;

use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

use super::{MAX_FRAME, recv, send};
use crate::server::handoff::testing::listening;

/// Asserts that a frame arrives with its lines in order and with its descriptors, and that a
/// descriptor that arrives is the same socket: the address of the listener made of it is the one
/// the sender's listener has.
#[tokio::test]
async fn a_frame_carries_its_lines_and_its_descriptors() {
    let (near, far) = UnixStream::pair().unwrap();
    let ((first, first_addr), (second, second_addr)) = (listening(), listening());
    let lines = vec!["offer".to_owned(), "s3 a".to_owned(), "oci b".to_owned()];
    send(&near, &lines, &[first.as_fd(), second.as_fd()])
        .await
        .unwrap();
    let frame = recv(&far).await.unwrap();
    assert_eq!(frame.lines, lines);
    let addrs: Vec<_> = frame
        .fds
        .into_iter()
        .map(|fd| TcpListener::from(fd).local_addr().unwrap())
        .collect();
    assert_eq!(addrs, vec![first_addr, second_addr]);
}

/// Asserts that a frame that arrives in pieces is read whole: its lines are joined across reads,
/// and the frame ends at the empty line, not before.
#[tokio::test]
async fn a_frame_is_read_whole_across_several_reads() {
    let (mut near, far) = UnixStream::pair().unwrap();
    near.write_all(b"one\nt").await.unwrap();
    let writing = async {
        near.write_all(b"wo\n\n").await.unwrap();
    };
    let (frame, ()) = tokio::join!(recv(&far), writing);
    assert_eq!(
        frame.unwrap().lines,
        vec!["one".to_owned(), "two".to_owned()]
    );
}

/// Asserts that a stream that ends inside a frame is an error of the kind `UnexpectedEof`, not a
/// shorter frame.
#[tokio::test]
async fn a_stream_that_ends_inside_a_frame_is_an_error() {
    let (mut near, far) = UnixStream::pair().unwrap();
    near.write_all(b"half a frame\n").await.unwrap();
    drop(near);
    let error = recv(&far).await.expect_err("an error");
    assert_eq!(error.kind(), ErrorKind::UnexpectedEof);
}

/// Asserts that a peer that never ends its frame cannot make the reader hold more than the
/// protocol allows: the frame is refused as invalid data.
#[tokio::test]
async fn a_frame_longer_than_the_protocol_allows_is_refused() {
    let (mut near, far) = UnixStream::pair().unwrap();
    near.write_all(&vec![b'x'; MAX_FRAME + 1]).await.unwrap();
    let error = recv(&far).await.expect_err("an error");
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    assert_eq!(error.to_string(), "a frame longer than the protocol allows");
}

/// Asserts that a frame that is not UTF-8 is refused as invalid data, with its message.
#[tokio::test]
async fn a_frame_that_is_not_text_is_refused() {
    let (mut near, far) = UnixStream::pair().unwrap();
    near.write_all(&[0xff, 0xfe, b'\n', b'\n']).await.unwrap();
    let error = recv(&far).await.expect_err("an error");
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    assert_eq!(error.to_string(), "a frame that is not text");
}
