//! Frames of [`tierstore_fetch_api::wire`] over a unix stream socket, with the descriptors that
//! ride on them: the mount's side.
//!
//! A sender puts a frame's descriptors on the `sendmsg` of the frame's first bytes. The kernel
//! detaches them on the first `recvmsg` that copies any byte of that `sendmsg`, and that byte is
//! the first of the frame. The reader asks each `recvmsg` for no byte past the frame it is
//! reading (first the length, then exactly the rest), so every descriptor it receives while it
//! reads a frame is that frame's. The server (T5) frames the same way.

use std::io::{self, IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{BorrowedFd, OwnedFd};

use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
};
use tierstore_fetch_api::wire::{FRAME_HEADER_LEN, Frame, MAX_BODY, WireError};
use tokio::io::Interest;
use tokio::net::UnixStream;

/// The most descriptors one frame carries (`Handshake` and `PackOpened` carry two). The ancillary
/// buffer has room for this many, so more are truncated by the kernel and seen as `MSG_CTRUNC`.
const MOST_FDS: usize = 2;

/// Why a frame could not be read. Each ends the connection: the client reconnects.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ReadError {
    /// The server closed the stream inside a frame.
    #[error("the stream ended inside a frame")]
    Truncated,
    /// The bytes are not a frame.
    #[error(transparent)]
    Wire(#[from] WireError),
    /// The frame came with another number of descriptors than its kind carries.
    #[error("a frame of {expected} descriptors came with {got}")]
    Fds {
        /// What the frame's kind carries.
        expected: usize,
        /// What came.
        got: usize,
    },
    /// More descriptors came than a frame can carry, and the kernel dropped the rest.
    #[error("descriptors were truncated: more came than a frame carries")]
    ControlTruncated,
    /// The system failed the read.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// One frame read, and its descriptors in the order they were sent.
#[derive(Debug)]
pub(crate) struct Incoming {
    pub(crate) frame: Frame,
    pub(crate) fds: Vec<OwnedFd>,
}

/// Reads the next frame; `None` when the server closed the stream between frames.
pub(crate) async fn read_frame(stream: &UnixStream) -> Result<Option<Incoming>, ReadError> {
    let mut fds = Vec::new();
    let mut head = [0u8; 4];
    let got = fill(stream, &mut head, &mut fds).await?;
    // Descriptors come only with bytes, so a read of none is the end of the stream.
    if got == 0 {
        return Ok(None);
    }
    if got < head.len() {
        return Err(ReadError::Truncated);
    }
    let need = frame_len(head)?;
    let mut bytes = vec![0u8; need];
    bytes[..4].copy_from_slice(&head);
    if fill(stream, &mut bytes[4..], &mut fds).await? < need - 4 {
        return Err(ReadError::Truncated);
    }
    let (frame, _) = Frame::decode(&bytes)?;
    let expected = frame.message.fds().len();
    if fds.len() != expected {
        return Err(ReadError::Fds {
            expected,
            got: fds.len(),
        });
    }
    Ok(Some(Incoming { frame, fds }))
}

/// The bytes of the frame whose length field is `head`, the field included; refused before a byte
/// of the body is read, so a length over the limit never makes the reader wait for, or buffer,
/// that many bytes.
fn frame_len(head: [u8; 4]) -> Result<usize, WireError> {
    let len = u32::from_le_bytes(head) as usize;
    if len < FRAME_HEADER_LEN {
        return Err(WireError::Invalid("length shorter than the frame header"));
    }
    if len - FRAME_HEADER_LEN > MAX_BODY {
        return Err(WireError::TooLong(len - FRAME_HEADER_LEN));
    }
    Ok(4 + len)
}

/// Reads into `buf` until it is full or the server closes; the bytes read. Descriptors that come
/// are appended to `fds`.
async fn fill(
    stream: &UnixStream,
    buf: &mut [u8],
    fds: &mut Vec<OwnedFd>,
) -> Result<usize, ReadError> {
    let mut got = 0;
    while got < buf.len() {
        let chunk = &mut buf[got..];
        let (read, truncated) = stream
            .async_io(Interest::READABLE, || -> io::Result<(usize, bool)> {
                // Room for the credentials a socket with `SO_PASSCRED` adds, so they never
                // truncate the descriptors.
                let mut space = [MaybeUninit::uninit();
                    rustix::cmsg_space!(ScmRights(MOST_FDS), ScmCredentials(1))];
                let mut control = RecvAncillaryBuffer::new(&mut space);
                let mut iov = [IoSliceMut::new(chunk)];
                let msg = recvmsg(stream, &mut iov, &mut control, RecvFlags::CMSG_CLOEXEC)?;
                for message in control.drain() {
                    // Other kinds (credentials, on a socket that passes them) are not the
                    // protocol's and are dropped.
                    if let RecvAncillaryMessage::ScmRights(rights) = message {
                        fds.extend(rights);
                    }
                }
                Ok((msg.bytes, msg.flags.contains(ReturnFlags::CTRUNC)))
            })
            .await?;
        if truncated {
            return Err(ReadError::ControlTruncated);
        }
        if read == 0 {
            break;
        }
        got += read;
    }
    Ok(got)
}

/// Writes `bytes` (one encoded frame) with `fds` on its first bytes.
pub(crate) async fn send_frame(
    stream: &UnixStream,
    bytes: &[u8],
    fds: &[BorrowedFd<'_>],
) -> io::Result<()> {
    // The descriptors ride on the first `sendmsg` only.
    let mut rights = fds;
    let mut sent = 0;
    while sent < bytes.len() {
        let rights = std::mem::take(&mut rights);
        let chunk = &bytes[sent..];
        sent += stream
            .async_io(Interest::WRITABLE, || -> io::Result<usize> {
                let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MOST_FDS))];
                let mut control = SendAncillaryBuffer::new(&mut space);
                // A request carries at most MOST_FDS descriptors, which the space has room for.
                if !rights.is_empty() {
                    let _ = control.push(SendAncillaryMessage::ScmRights(rights));
                }
                let iov = [IoSlice::new(chunk)];
                Ok(sendmsg(stream, &iov, &mut control, SendFlags::NOSIGNAL)?)
            })
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_length_is_checked_before_the_body_is_read() {
        let len = |n: usize| (n as u32).to_le_bytes();
        assert_eq!(frame_len(len(FRAME_HEADER_LEN)), Ok(4 + FRAME_HEADER_LEN));
        assert_eq!(
            frame_len(len(FRAME_HEADER_LEN + MAX_BODY)),
            Ok(4 + FRAME_HEADER_LEN + MAX_BODY)
        );
        assert_eq!(
            frame_len(len(FRAME_HEADER_LEN - 1)),
            Err(WireError::Invalid("length shorter than the frame header"))
        );
        assert_eq!(
            frame_len(len(FRAME_HEADER_LEN + MAX_BODY + 1)),
            Err(WireError::TooLong(MAX_BODY + 1))
        );
    }
}
