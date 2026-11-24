//! The frames of the control socket, and the file descriptors that ride on them. The hand-off of
//! the listening sockets uses them, and so does the hand-over of the mount of `tierstore fs`
//! (`treefs/handover.rs`).
//!
//! A frame is text: lines, each ended by a newline, then an empty line. The descriptors of an
//! offer (listening sockets, or the `/dev/fuse` descriptor and a control socket) travel in the
//! same `sendmsg` as the frame's first bytes, as an `SCM_RIGHTS` message, so a frame that names
//! `n` descriptors arrives with `n` of them. A read takes one frame and leaves the next in the
//! socket: a side may send two frames before the other answers (`bye`, then `written`). Nothing
//! here is `unsafe`: `rustix` wraps the system calls.

use std::io::{self, IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{BorrowedFd, OwnedFd};

use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
};
use tokio::io::Interest;
use tokio::net::UnixStream;

/// The most descriptors one message may carry: the kernel's own limit (`SCM_MAX_FD`), so the
/// protocol adds no number of its own.
pub(crate) const MAX_FDS: usize = 253;

/// The longest frame that is read: a line for every descriptor the kernel lets a message carry,
/// at the length of the longest `<front> <address>` line (an IPv6 address with a scope and a
/// port), and the lines of the request.
const MAX_FRAME: usize = MAX_FDS * 128 + 128;

/// A frame read: its lines, and the descriptors that came with it.
#[derive(Debug)]
pub(crate) struct Frame {
    pub(crate) lines: Vec<String>,
    pub(crate) fds: Vec<OwnedFd>,
}

/// An error of the kind `InvalidData` that says what was wrong with the frame.
fn invalid(why: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why)
}

/// Writes a frame of `lines` to `stream`, with `fds` on its first bytes. At most [`MAX_FDS`] of
/// them may be given.
pub(crate) async fn send(
    stream: &UnixStream,
    lines: &[String],
    fds: &[BorrowedFd<'_>],
) -> io::Result<()> {
    let mut bytes = lines.join("\n").into_bytes();
    bytes.extend_from_slice(b"\n\n");
    let mut sent = 0;
    while sent < bytes.len() {
        let rights: &[BorrowedFd<'_>] = if sent == 0 { fds } else { &[] };
        let chunk = &bytes[sent..];
        let wrote = stream
            .async_io(Interest::WRITABLE, || -> io::Result<usize> {
                let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
                let mut control = SendAncillaryBuffer::new(&mut space);
                if !rights.is_empty() {
                    // The offer holds at most MAX_FDS sockets, which the space has room for.
                    let _ = control.push(SendAncillaryMessage::ScmRights(rights));
                }
                let iov = [IoSlice::new(chunk)];
                Ok(sendmsg(stream, &iov, &mut control, SendFlags::empty())?)
            })
            .await?;
        sent += wrote;
    }
    Ok(())
}

/// How many of the `peeked` bytes that follow `read` (what was read of the frame so far) belong
/// to the frame: up to and including its empty line, or all of them if it does not end there.
fn of_frame(read: &[u8], peeked: &[u8]) -> usize {
    // The end may begin in what was read: its last newline and the first peeked one.
    let from = read.len().saturating_sub(1);
    let mut seen = read[from..].to_vec();
    seen.extend_from_slice(peeked);
    let end = seen.windows(2).position(|pair| pair == b"\n\n");
    end.map_or(peeked.len(), |at| at + 2 - (read.len() - from))
}

/// Reads one frame from `stream`, with the descriptors that came with it, and nothing of the
/// frame that follows it.
///
/// # Errors
/// The stream ended inside the frame, the frame is longer than [`MAX_FRAME`] or is not text, or
/// the system failed the read.
pub(crate) async fn recv(stream: &UnixStream) -> io::Result<Frame> {
    let mut bytes: Vec<u8> = Vec::new();
    let mut fds = Vec::new();
    let mut buffer = vec![0_u8; MAX_FRAME];
    while !bytes.ends_with(b"\n\n") {
        let (read, mut received) = stream
            .async_io(
                Interest::READABLE,
                || -> io::Result<(usize, Vec<OwnedFd>)> {
                    // What is there is looked at first, so that the read below stops at the
                    // frame's end. A peek takes no descriptor (it has no room for one).
                    let (peeked, _) = rustix::net::recv(stream, &mut buffer[..], RecvFlags::PEEK)?;
                    let take = of_frame(&bytes, &buffer[..peeked]);
                    let mut space =
                        [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
                    let mut control = RecvAncillaryBuffer::new(&mut space);
                    let mut iov = [IoSliceMut::new(&mut buffer[..take])];
                    let got = recvmsg(stream, &mut iov, &mut control, RecvFlags::empty())?;
                    let mut received = Vec::new();
                    for message in control.drain() {
                        if let RecvAncillaryMessage::ScmRights(rights) = message {
                            received.extend(rights);
                        }
                    }
                    Ok((got.bytes, received))
                },
            )
            .await?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        bytes.extend_from_slice(&buffer[..read]);
        fds.append(&mut received);
        if bytes.len() > MAX_FRAME {
            return Err(invalid("a frame longer than the protocol allows"));
        }
    }
    let text = String::from_utf8(bytes).map_err(|_| invalid("a frame that is not text"))?;
    let lines = text.trim_end_matches('\n').split('\n');
    Ok(Frame {
        lines: lines.map(str::to_owned).collect(),
        fds,
    })
}

#[cfg(test)]
#[path = "wire_tests.rs"]
mod tests;
