//! Frames and their descriptors on the connection: what ends a connection, and that descriptors
//! are matched to the frame they were sent with however the bytes are split or joined.
//!
//! Proves: a frame whose descriptors are not its kind's number (too few, an unexpected one, or more
//! than a frame carries) ends the connection with no reply; so do a bad magic, a length over the
//! limit (refused before the body is read), a length shorter than the header, a stream that ends
//! inside a frame, and a server's message sent by a mount, each counted as a protocol error; a
//! clean close and a reset are not protocol errors; a handshake split over two writes, and a
//! handshake and an `open-pack` in one write, are each answered; replies leave in the order they
//! complete and a peer that stopped reading ends the writer, not the server.

use std::os::fd::AsFd;

use crate::common::{Fixture, MountProc, send_raw};
use rustix::net::{Shutdown, shutdown};
use tierstore_fetch_api::ids::{ByteRange, JobId};
use tierstore_fetch_api::wire::{Epoch, FRAME_HEADER_LEN, Frame, MAX_BODY, Message, PackHandle};

fn frame(id: u64, message: Message) -> Vec<u8> {
    Frame {
        id,
        epoch: Epoch(0),
        message,
    }
    .encode()
    .unwrap()
}

/// Sends `bytes` with `fds` and checks the server ends the connection without a reply, counting a
/// protocol error.
async fn ends_connection(bytes: &[u8], fds: &[std::os::fd::BorrowedFd<'_>]) {
    let fx = Fixture::new();
    let conn = fx.connect();
    send_raw(&conn.stream, bytes, fds).await.unwrap();
    assert!(conn.closed().await);
    let stats = fx.server.wait_for(|s| s.connections_closed == 1).await;
    assert_eq!((stats.protocol_errors, stats.replies), (1, 0));
}

#[tokio::test]
async fn a_frame_with_the_wrong_descriptors_ends_the_connection() {
    let mount = MountProc::new(1, 4096);
    // A handshake with one descriptor.
    ends_connection(&frame(1, mount.handshake()), &[mount.counters.as_fd()]).await;
    // An `open-pack`, which carries none, with one.
    ends_connection(&frame(1, mount.open(0, 0)), &[mount.counters.as_fd()]).await;
    // A handshake with sixteen: more than the server's buffer holds, truncated by the kernel.
    let [c, l] = mount.fds();
    ends_connection(&frame(1, mount.handshake()), &[c, l].repeat(8)).await;
}

#[tokio::test]
async fn bytes_that_are_not_a_frame_end_the_connection() {
    let mount = MountProc::new(1, 4096);
    let mut bad_magic = frame(1, Message::HandshakeOk);
    bad_magic[4] ^= 0xff;
    ends_connection(&bad_magic, &[]).await;
    let too_long = ((FRAME_HEADER_LEN + MAX_BODY + 1) as u32).to_le_bytes();
    ends_connection(&too_long, &[]).await;
    let too_short = ((FRAME_HEADER_LEN - 1) as u32).to_le_bytes();
    ends_connection(&too_short, &[]).await;
    // A server's message, from a mount.
    ends_connection(&frame(1, Message::Released), &[]).await;
    // A stream that ends inside the length, and inside the body.
    let open = frame(1, mount.open(0, 0));
    for cut in [2, open.len() - 1] {
        let fx = Fixture::new();
        let conn = fx.connect();
        send_raw(&conn.stream, &open[..cut], &[]).await.unwrap();
        shutdown(&conn.stream, Shutdown::Write).unwrap();
        assert!(conn.closed().await);
        let stats = fx.server.wait_for(|s| s.connections_closed == 1).await;
        assert_eq!(stats.protocol_errors, 1, "cut at {cut}");
    }
}

#[tokio::test]
async fn a_close_between_frames_and_a_reset_are_not_protocol_errors() {
    let fx = Fixture::new();
    let mount = MountProc::new(1, 4096);
    let mut clean = fx.connect();
    assert_eq!(clean.handshake(&mount).await, Message::HandshakeOk);
    drop(clean);
    fx.server.wait_for(|s| s.connections_closed == 1).await;

    // A mount that closes with a reply unread in its queue resets the connection.
    let mut reset = fx.connect();
    reset.send(mount.handshake(), &mount.fds()).await;
    reset.stream.readable().await.unwrap();
    drop(reset);
    let stats = fx.server.wait_for(|s| s.connections_closed == 2).await;
    assert_eq!(stats.protocol_errors, 0);
    // Neither ended the holder.
    assert_eq!(stats.holders_live(), 1);
}

#[tokio::test]
async fn descriptors_follow_their_frame_however_the_bytes_are_split_or_joined() {
    let fx = Fixture::new();
    let mount = MountProc::new(1, 4096);
    // A handshake in two writes, its descriptors on the first.
    let conn = fx.connect();
    let hs = frame(1, mount.handshake());
    send_raw(&conn.stream, &hs[..3], &mount.fds())
        .await
        .unwrap();
    send_raw(&conn.stream, &hs[3..], &[]).await.unwrap();
    let (reply, _) = conn.recv().await;
    assert_eq!((reply.id, reply.message), (1, Message::HandshakeOk));

    // A handshake and an open-pack in one write (the open-pack stamped with the server's epoch).
    let other = MountProc::new(2, 4096);
    let conn = fx.connect();
    let mut bytes = frame(1, other.handshake());
    bytes.extend(
        Frame {
            id: 2,
            epoch: fx.server.epoch(),
            message: other.open(0, 0),
        }
        .encode()
        .unwrap(),
    );
    send_raw(&conn.stream, &bytes, &other.fds()).await.unwrap();
    let (first, none) = conn.recv().await;
    assert_eq!(
        (first.id, first.message, none.len()),
        (1, Message::HandshakeOk, 0)
    );
    let (second, fds) = conn.recv().await;
    assert_eq!(second.id, 2);
    assert!(
        matches!(second.message, Message::PackOpened { .. }),
        "{second:?}"
    );
    assert_eq!(fds.len(), 2);
    assert_eq!(fx.server.stats().protocol_errors, 0);
}

#[tokio::test]
async fn a_peer_that_stopped_reading_ends_only_the_writer() {
    let fx = Fixture::new();
    let mut conn = fx.connect();
    shutdown(&conn.stream, Shutdown::Read).unwrap();
    let wait = Message::Wait {
        pack: PackHandle(1),
        range: ByteRange { offset: 0, len: 1 },
        job: JobId(0),
    };
    // Refused, and the refusal cannot be written: the writer stops.
    conn.send(wait.clone(), &[]).await;
    fx.server.wait_for(|s| s.replies_lost == 1).await;
    // A later reply is dropped with the writer, and the connection still reads requests.
    conn.send(wait, &[]).await;
    shutdown(&conn.stream, Shutdown::Write).unwrap();
    let stats = fx.server.wait_for(|s| s.connections_closed == 1).await;
    assert_eq!(
        (stats.replies, stats.replies_lost, stats.protocol_errors),
        (0, 1, 0)
    );
    // The server serves the next connection.
    let mount = MountProc::new(1, 4096);
    let mut next = fx.connect();
    assert_eq!(next.handshake(&mount).await, Message::HandshakeOk);
}

#[tokio::test]
async fn a_socket_that_passes_credentials_is_served() {
    let fx = Fixture::new();
    let mount = MountProc::new(1, 4096);
    let mut conn = crate::common::Conn::passing_credentials(&fx.server);
    assert_eq!(conn.handshake(&mount).await, Message::HandshakeOk);
    let (reply, fds) = conn.call(mount.open(0, 0), &[]).await;
    assert!(
        matches!(reply.message, Message::PackOpened { .. }),
        "{reply:?}"
    );
    assert_eq!(fds.len(), 2);
    assert_eq!(fx.server.stats().protocol_errors, 0);
}
