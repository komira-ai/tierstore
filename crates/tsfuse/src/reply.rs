//! Replies, bound to the device that read their request.

use std::fmt;
use std::sync::Arc;

use crate::tokens::{CancelToken, Slot};

/// The length of `fuse_out_header`: `len u32`, `error i32`, `unique u64`.
pub const OUT_HEADER_LEN: usize = 16;

/// An errno a reply carries (positive; the header holds its negation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Errno(pub i32);

impl Errno {
    /// `EIO`: a cell is `bad`, or its fill is lost.
    pub const EIO: Self = Self(5);
    /// `EINTR`: the request was interrupted.
    pub const EINTR: Self = Self(4);
    /// `EAGAIN`: the reply to an INTERRUPT whose request is not known (yet).
    pub const EAGAIN: Self = Self(11);
    /// `ENOSYS`: an opcode the library does not implement.
    pub const ENOSYS: Self = Self(38);
    /// `EPROTO`: an INIT of a protocol version the library does not support.
    pub const EPROTO: Self = Self(71);
}

/// The 16 bytes of `fuse_out_header` for a reply of `payload_len` bytes to `unique`, with
/// `error` (0 for success). The errno is negated with wrapping (`i32::MIN` stays itself). A
/// reply of more than `u32::MAX - 16` payload bytes saturates `len` at `u32::MAX`; FUSE replies
/// are far below that.
#[must_use]
pub fn encode_out_header(
    unique: u64,
    error: Option<Errno>,
    payload_len: usize,
) -> [u8; OUT_HEADER_LEN] {
    let mut h = [0u8; OUT_HEADER_LEN];
    let len = u32::try_from(OUT_HEADER_LEN + payload_len).unwrap_or(u32::MAX);
    h[0..4].copy_from_slice(&len.to_le_bytes());
    let err = error.map_or(0, |e| e.0.wrapping_neg());
    h[4..8].copy_from_slice(&err.to_le_bytes());
    h[8..16].copy_from_slice(&unique.to_le_bytes());
    h
}

/// The device a request was read from: a cloned `/dev/fuse` descriptor (`write(2)`/`writev(2)` on
/// it) or a handle to the owning io_uring queue's submit channel. A reply is written as one
/// message of the concatenated `parts` (header first).
pub trait ReplyDevice: Send + Sync {
    /// Writes one reply.
    fn send(&self, parts: &[&[u8]]) -> std::io::Result<()>;
}

/// How a completion ended.
#[derive(Debug)]
pub enum Completion {
    /// The reply was written to the originating device.
    Written,
    /// The request was already answered (`EINTR` by an INTERRUPT): nothing was written.
    AlreadyAnswered,
    /// The device refused the write (the connection is gone, or the kernel rejected the reply).
    DeviceError(std::io::Error),
}

/// The right to answer one request, on the device that read it. Completing it consumes it, and
/// the take of the request's slot is atomic against an INTERRUPT, so the request is answered once.
///
/// A replier dropped without being completed (a handler that panics, returns early or forgets it)
/// takes the slot and writes an `EIO` reply on its originating device (a device error is
/// ignored), so the kernel request is never left unanswered and the table entry is never leaked.
/// If an INTERRUPT or a completion already took the slot, the drop writes nothing; nor does it
/// once the session is handed over (`Serving::hand_over` on Linux), since the next server
/// answers the request after its RESEND.
#[must_use = "a replier must be completed with ok or error; dropping it answers EIO"]
pub struct Replier {
    pub(crate) unique: u64,
    pub(crate) device: Arc<dyn ReplyDevice>,
    pub(crate) slot: Arc<Slot>,
}

impl fmt::Debug for Replier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Replier")
            .field("unique", &self.unique)
            .finish_non_exhaustive()
    }
}

impl Replier {
    /// The request's `unique`.
    #[must_use]
    pub fn unique(&self) -> u64 {
        self.unique
    }

    /// The token an INTERRUPT of this request cancels.
    #[must_use]
    pub fn cancel_token(&self) -> CancelToken {
        CancelToken {
            slot: self.slot.clone(),
        }
    }

    /// Answers with success and `payload` (the reply body after the header, in parts).
    pub fn ok(self, payload: &[&[u8]]) -> Completion {
        self.complete(None, payload)
    }

    /// Answers with `errno`.
    pub fn error(self, errno: Errno) -> Completion {
        self.complete(Some(errno), &[])
    }

    fn complete(self, error: Option<Errno>, payload: &[&[u8]]) -> Completion {
        // The take is here, so the drop of `self` at the end finds the slot taken.
        if !self.slot.take() {
            return Completion::AlreadyAnswered;
        }
        self.write(error, payload)
    }

    fn write(&self, error: Option<Errno>, payload: &[&[u8]]) -> Completion {
        let len = payload.iter().map(|p| p.len()).sum();
        let header = encode_out_header(self.unique, error, len);
        let mut parts: Vec<&[u8]> = Vec::with_capacity(payload.len() + 1);
        parts.push(&header);
        parts.extend_from_slice(payload);
        match self.device.send(&parts) {
            Ok(()) => Completion::Written,
            Err(e) => Completion::DeviceError(e),
        }
    }
}

impl Drop for Replier {
    fn drop(&mut self) {
        if self.slot.take() && !self.slot.handed_over() {
            // Nobody can act on a device error here: the connection is gone or the kernel
            // rejected the reply, and the request is over either way.
            let _ = self.write(Some(Errno::EIO), &[]);
        }
    }
}
