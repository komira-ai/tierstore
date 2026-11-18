//! The dispatch interface: a request, and the file system that handles it.

use crate::opcode::Opcode;
use crate::reply::Replier;

/// The length of `fuse_in_header`.
pub const IN_HEADER_LEN: usize = 40;

/// `fuse_in_header`, decoded: `len u32, opcode u32, unique u64, nodeid u64, uid u32, gid u32,
/// pid u32, total_extlen u16, padding u16`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestHeader {
    /// The request's length, header included.
    pub len: u32,
    /// The raw opcode.
    pub opcode: u32,
    /// The request id a reply names.
    pub unique: u64,
    /// The node the request is about.
    pub nodeid: u64,
    /// The caller's uid.
    pub uid: u32,
    /// The caller's gid.
    pub gid: u32,
    /// The caller's pid.
    pub pid: u32,
}

impl RequestHeader {
    /// The header at the start of `buf`, if it holds one whose `len` fits `buf`.
    #[must_use]
    pub fn parse(buf: &[u8]) -> Option<Self> {
        let h = buf.get(..IN_HEADER_LEN)?;
        let u32_at = |at: usize| u32::from_le_bytes([h[at], h[at + 1], h[at + 2], h[at + 3]]);
        let u64_at = |at: usize| u64::from(u32_at(at)) | (u64::from(u32_at(at + 4)) << 32);
        let header = Self {
            len: u32_at(0),
            opcode: u32_at(4),
            unique: u64_at(8),
            nodeid: u64_at(16),
            uid: u32_at(24),
            gid: u32_at(28),
            pid: u32_at(32),
        };
        let len = header.len as usize;
        (len >= IN_HEADER_LEN && len <= buf.len()).then_some(header)
    }

    /// The implemented opcode, or `None` for one answered `ENOSYS`.
    #[must_use]
    pub fn op(&self) -> Option<Opcode> {
        Opcode::from_code(self.opcode)
    }
}

/// A request read from a device: its header and its body, parsed in place in the reader's pooled
/// buffer ([`crate::abi::Operation::parse`] parses the body per opcode).
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    /// The header.
    pub header: RequestHeader,
    /// The bytes after the header, up to `header.len`.
    pub body: &'a [u8],
}

impl<'a> Request<'a> {
    /// The request at the start of `buf`.
    #[must_use]
    pub fn parse(buf: &'a [u8]) -> Option<Self> {
        let header = RequestHeader::parse(buf)?;
        // `parse` checked that `len` is within `buf` and at least the header.
        let body = &buf[IN_HEADER_LEN..header.len as usize];
        Some(Self { header, body })
    }
}

/// What [`Fs::handle`] did with a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Handled {
    /// Answered inline (metadata from the mapped image, a ready read): no `await` happened.
    Done,
    /// Parked: the operation copied the few fields it needs, the reader's buffer is free, and the
    /// replier completes later, from any task, on its originating device.
    Parked,
}

/// A file system served by `tsfuse`.
///
/// The reader that read the request has registered it in the session's
/// [`crate::Tokens`] before it calls `handle` (cancel registration at read time), so the replier
/// it passes is cancellable from the moment the request exists. `handle` runs on the reader's
/// dedicated thread and must not block on the network, a fill or a hash; a read whose cells are
/// not ready parks.
///
/// Section 14 rows: "Replier: with N > 1 clones, a parked reply completed from another task is
/// written to its originating device; INTERRUPT read by another clone is answered once"; "All
/// reader threads blocked in a fake `pread` while `wait` completions still flow"; the kernel
/// conformance suite of 5.2 (T-F7).
pub trait Fs: Send + Sync {
    /// Handles one request; answers it with `replier` now or later.
    fn handle(&self, req: Request<'_>, replier: Replier) -> Handled;
}
