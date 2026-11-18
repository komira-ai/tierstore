//! The mount-to-tierstore protocol (5.4): one long-lived multiplexed connection per mount over a
//! unix socket, request ids, out-of-order completion, descriptors passed with `SCM_RIGHTS`.
//!
//! **Frame** (little-endian): length `u32` of what follows it, magic `TP` (`u16` 0x5054), version
//! `u8` (1), kind `u8`, request id `u64`, epoch `u64`, descriptor count `u8`, 3 reserved zero
//! bytes, then the body. A frame that carries descriptors is sent with exactly that many in one
//! `SCM_RIGHTS` message, in the order [`Message::fds`] names; a receiver refuses a frame whose
//! count is not the kind's.
//!
//! **Epoch.** The server stamps every connection and reply with a random `u64` epoch; the mount
//! drops replies of any other epoch ([`Epoch::accepts`]). Every request but the handshake carries
//! the epoch the mount was handshaken under; the server refuses one of another epoch (`wait` and
//! `ensure` with [`RefusalCode::UnknownHandle`], `open-pack` and `release` with
//! [`RefusalCode::NotHandshaken`]), since handles are numbered per server. On reconnect the mount
//! re-handshakes with the same holder id (its counter memfd and lifeline are unchanged and are
//! sent again), re-sends `open-pack` and re-issues every parked `wait`, which is idempotent.
//!
//! **Handshake.** The mount sends its counter memfd (one for its whole life, sealed with
//! [`crate::page::F_SEAL_SHRINK`]: the server maps it, and refuses one that could shrink under the
//! mapping) and one end of a socketpair it keeps open for its whole life, the lifeline. The server
//! ends a holder only on the lifeline's `POLLHUP`, never because the protocol connection dropped
//! (5.1).

use crate::engine::WaitOutcome;
use crate::ids::{ByteRange, CellRun, Class, HolderId, JobId, Rank, TreeRef};
use crate::page::CounterRegion;

/// The frame magic.
pub const FRAME_MAGIC: u16 = u16::from_le_bytes(*b"TP");

/// The protocol version this release speaks.
pub const PROTOCOL_VERSION: u8 = 1;

/// The bytes of a frame before its body (after the length field).
pub const FRAME_HEADER_LEN: usize = 24;

/// The most bytes a frame's body may have.
pub const MAX_BODY: usize = 1 << 20;

/// A server epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Epoch(pub u64);

impl Epoch {
    /// Whether a mount whose current epoch is `self` accepts a reply stamped `reply`: only the
    /// same epoch; a reply of a previous server is dropped (section 14: "Server restart with a
    /// stale-epoch reply in flight: dropped; the `wait` re-issued").
    #[must_use]
    pub fn accepts(self, reply: Epoch) -> bool {
        self == reply
    }
}

/// A pack the server opened for a mount: the handle `wait` and `ensure` name. Handles are the
/// server's; after a reconnect to a new epoch the mount re-opens and gets new ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackHandle(pub u64);

/// A descriptor a frame carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FdRole {
    /// The mount's counter memfd.
    CounterMemfd,
    /// The mount's end of its lifeline socketpair.
    Lifeline,
    /// The pack's data file, opened by the server (the mount never reopens by path).
    DataFd,
    /// The pack's sealed state memfd.
    StateMemfd,
}

/// Why the server refused a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefusalCode {
    /// The protocol version is not one the server speaks.
    Version,
    /// The tree's list is not L2 (3.5): the caller's miss path.
    ListVersionUnsupported,
    /// No such tree, or no such pack in it.
    UnknownPack,
    /// A pack handle the server does not know (a previous epoch's).
    UnknownHandle,
    /// The counter region is misaligned, overlaps another, or overflows.
    BadRegion,
    /// A request before the handshake, or a holder that is not this connection's.
    NotHandshaken,
}

impl RefusalCode {
    fn code(self) -> u16 {
        match self {
            Self::Version => 1,
            Self::ListVersionUnsupported => 2,
            Self::UnknownPack => 3,
            Self::UnknownHandle => 4,
            Self::BadRegion => 5,
            Self::NotHandshaken => 6,
        }
    }

    fn from_code(code: u16) -> Option<Self> {
        Some(match code {
            1 => Self::Version,
            2 => Self::ListVersionUnsupported,
            3 => Self::UnknownPack,
            4 => Self::UnknownHandle,
            5 => Self::BadRegion,
            6 => Self::NotHandshaken,
            _ => return None,
        })
    }
}

/// A message of the protocol: requests from the mount (kinds below 128), replies from the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// Mount: who it is. Carries the counter memfd and the lifeline.
    Handshake {
        /// The mount's holder id, the same across reconnects.
        holder: HolderId,
        /// The protocol version the mount speaks.
        version: u8,
    },
    /// Mount: open a pack of a tree for this holder, its counters at `region`.
    OpenPack {
        /// The tree.
        tree: TreeRef,
        /// The pack's index in the tree's list.
        pack: u32,
        /// The holder (the handshake's).
        holder: HolderId,
        /// The pack's region in the counter memfd.
        region: CounterRegion,
    },
    /// Mount: wait until `range` is ready; answered by [`Message::Waited`].
    Wait {
        /// The pack.
        pack: PackHandle,
        /// The bytes (5.4: a wait covers the remaining span up to the next 8 cells).
        range: ByteRange,
        /// The job reading, for fairness.
        job: JobId,
    },
    /// Mount: prefetch runs of a pack.
    Ensure {
        /// The pack.
        pack: PackHandle,
        /// The runs.
        runs: Vec<CellRun>,
        /// The class.
        class: Class,
        /// The job.
        job: JobId,
        /// The rank.
        rank: Rank,
    },
    /// Mount: drop every hold of the holder (a clean unmount; a dead mount's lifeline hangs up).
    Release {
        /// The holder.
        holder: HolderId,
    },
    /// Server: the handshake is accepted under the frame's epoch.
    HandshakeOk,
    /// Server: the pack is open. Carries the data fd and the state memfd.
    PackOpened {
        /// The handle.
        pack: PackHandle,
        /// The pack's size.
        pack_size: u64,
        /// The region the server reads the holder's counters at (the one asked, or, on a
        /// re-open after a reconnect, the one the holder had).
        region: CounterRegion,
    },
    /// Server: a `wait` ended.
    Waited(WaitOutcome),
    /// Server: the `ensure` is queued.
    Ensured,
    /// Server: the holder's holds are dropped.
    Released,
    /// Server: the request is refused.
    Refused {
        /// Why.
        code: RefusalCode,
    },
}

impl Message {
    fn kind(&self) -> u8 {
        match self {
            Self::Handshake { .. } => 1,
            Self::OpenPack { .. } => 2,
            Self::Wait { .. } => 3,
            Self::Ensure { .. } => 4,
            Self::Release { .. } => 5,
            Self::HandshakeOk => 129,
            Self::PackOpened { .. } => 130,
            Self::Waited(_) => 131,
            Self::Ensured => 132,
            Self::Released => 133,
            Self::Refused { .. } => 134,
        }
    }

    /// The descriptors a frame of this message carries, in order.
    #[must_use]
    pub fn fds(&self) -> &'static [FdRole] {
        match self {
            Self::Handshake { .. } => &[FdRole::CounterMemfd, FdRole::Lifeline],
            Self::PackOpened { .. } => &[FdRole::DataFd, FdRole::StateMemfd],
            _ => &[],
        }
    }
}

/// One frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The request id; a reply has its request's.
    pub id: u64,
    /// The epoch: the mount's current one on a request (0 before the first handshake), the
    /// server's on a reply.
    pub epoch: Epoch,
    /// The message.
    pub message: Message,
}

/// Why bytes are not a frame.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    /// Fewer bytes than the frame needs.
    #[error("frame: {need} bytes needed, {have} present")]
    Short {
        /// Bytes needed.
        need: usize,
        /// Bytes present.
        have: usize,
    },
    /// A body longer than [`MAX_BODY`].
    #[error("frame: body of {0} bytes is too long")]
    TooLong(usize),
    /// Not the frame magic.
    #[error("frame: bad magic {0:#06x}")]
    BadMagic(u16),
    /// A protocol version this release does not speak.
    #[error("frame: version {0} is not supported")]
    Version(u8),
    /// A kind this release does not know.
    #[error("frame: unknown kind {0}")]
    UnknownKind(u8),
    /// The descriptor count is not the kind's.
    #[error("frame: kind {kind} carries {expected} descriptors, the frame says {got}")]
    FdCount {
        /// The kind.
        kind: u8,
        /// What the kind carries.
        expected: usize,
        /// What the frame says.
        got: usize,
    },
    /// A field holds a value the protocol does not allow, or the body has bytes left over.
    #[error("frame: {0}")]
    Invalid(&'static str),
}

struct Body {
    buf: Vec<u8>,
}

impl Body {
    fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn region(&mut self, r: CounterRegion) {
        self.u64(r.offset);
        self.u32(r.groups);
    }
}

struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        if self.buf.len() < n {
            return Err(WireError::Invalid("body ends inside a field"));
        }
        let (head, rest) = self.buf.split_at(n);
        self.buf = rest;
        Ok(head)
    }
    fn arr<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
        let mut a = [0u8; N];
        a.copy_from_slice(self.take(N)?);
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.arr::<1>()?[0])
    }
    fn u16(&mut self) -> Result<u16, WireError> {
        Ok(u16::from_le_bytes(self.arr()?))
    }
    fn u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_le_bytes(self.arr()?))
    }
    fn u64(&mut self) -> Result<u64, WireError> {
        Ok(u64::from_le_bytes(self.arr()?))
    }
    fn region(&mut self) -> Result<CounterRegion, WireError> {
        Ok(CounterRegion {
            offset: self.u64()?,
            groups: self.u32()?,
        })
    }
    fn class(&mut self) -> Result<Class, WireError> {
        match self.u8()? {
            0 => Ok(Class::Demand),
            1 => Ok(Class::Prepare),
            2 => Ok(Class::Background),
            _ => Err(WireError::Invalid("unknown class")),
        }
    }
    fn end(&self) -> Result<(), WireError> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(WireError::Invalid("bytes after the body"))
        }
    }
}

fn class_code(c: Class) -> u8 {
    match c {
        Class::Demand => 0,
        Class::Prepare => 1,
        Class::Background => 2,
    }
}

impl Frame {
    /// The frame's bytes, length field first.
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut b = Body { buf: Vec::new() };
        match &self.message {
            Message::Handshake { holder, version } => {
                b.u64(holder.0);
                b.u8(*version);
            }
            Message::OpenPack {
                tree,
                pack,
                holder,
                region,
            } => {
                let len = u16::try_from(tree.0.len())
                    .map_err(|_| WireError::Invalid("tree ref longer than 65535 bytes"))?;
                b.u16(len);
                b.buf.extend_from_slice(tree.0.as_bytes());
                b.u32(*pack);
                b.u64(holder.0);
                b.region(*region);
            }
            Message::Wait { pack, range, job } => {
                b.u64(pack.0);
                b.u64(range.offset);
                b.u64(range.len);
                b.u64(job.0);
            }
            Message::Ensure {
                pack,
                runs,
                class,
                job,
                rank,
            } => {
                b.u64(pack.0);
                b.u8(class_code(*class));
                b.u64(job.0);
                b.u32(rank.0);
                // A run is 8 bytes: more than this many cannot fit a body, so the count fits a
                // `u32` below.
                if runs.len() > MAX_BODY / 8 {
                    return Err(WireError::TooLong(runs.len().saturating_mul(8)));
                }
                b.u32(runs.len() as u32);
                for r in runs {
                    b.u32(r.first);
                    b.u32(r.count);
                }
            }
            Message::Release { holder } => b.u64(holder.0),
            Message::HandshakeOk | Message::Ensured | Message::Released => {}
            Message::PackOpened {
                pack,
                pack_size,
                region,
            } => {
                b.u64(pack.0);
                b.u64(*pack_size);
                b.region(*region);
            }
            Message::Waited(outcome) => match outcome {
                WaitOutcome::Ready => b.u8(0),
                WaitOutcome::Eio { cell } => {
                    b.u8(1);
                    b.u32(*cell);
                }
                WaitOutcome::Lost => b.u8(2),
            },
            Message::Refused { code } => b.u16(code.code()),
        }
        if b.buf.len() > MAX_BODY {
            return Err(WireError::TooLong(b.buf.len()));
        }
        let mut out = Vec::with_capacity(4 + FRAME_HEADER_LEN + b.buf.len());
        // The body is at most MAX_BODY, so the length fits a u32.
        out.extend_from_slice(&((FRAME_HEADER_LEN + b.buf.len()) as u32).to_le_bytes());
        out.extend_from_slice(&FRAME_MAGIC.to_le_bytes());
        out.push(PROTOCOL_VERSION);
        out.push(self.message.kind());
        out.extend_from_slice(&self.id.to_le_bytes());
        out.extend_from_slice(&self.epoch.0.to_le_bytes());
        // At most two descriptors per kind.
        out.push(self.message.fds().len() as u8);
        out.extend_from_slice(&[0; 3]);
        out.extend_from_slice(&b.buf);
        Ok(out)
    }

    /// The frame at the start of `buf` and the bytes it takes; `Short` when more bytes are
    /// needed (a stream reader reads on and calls again).
    pub fn decode(buf: &[u8]) -> Result<(Self, usize), WireError> {
        if buf.len() < 4 {
            return Err(WireError::Short {
                need: 4,
                have: buf.len(),
            });
        }
        let mut len4 = [0u8; 4];
        len4.copy_from_slice(&buf[..4]);
        let len = u32::from_le_bytes(len4) as usize;
        if len < FRAME_HEADER_LEN {
            return Err(WireError::Invalid("length shorter than the frame header"));
        }
        if len - FRAME_HEADER_LEN > MAX_BODY {
            return Err(WireError::TooLong(len - FRAME_HEADER_LEN));
        }
        if buf.len() < 4 + len {
            return Err(WireError::Short {
                need: 4 + len,
                have: buf.len(),
            });
        }
        let mut r = Reader {
            buf: &buf[4..4 + len],
        };
        let magic = r.u16()?;
        if magic != FRAME_MAGIC {
            return Err(WireError::BadMagic(magic));
        }
        let version = r.u8()?;
        if version != PROTOCOL_VERSION {
            return Err(WireError::Version(version));
        }
        let kind = r.u8()?;
        let id = r.u64()?;
        let epoch = Epoch(r.u64()?);
        let fds = usize::from(r.u8()?);
        r.take(3)?;
        let message = match kind {
            1 => Message::Handshake {
                holder: HolderId(r.u64()?),
                version: r.u8()?,
            },
            2 => {
                let n = usize::from(r.u16()?);
                let tree = std::str::from_utf8(r.take(n)?)
                    .map_err(|_| WireError::Invalid("tree ref is not UTF-8"))?;
                Message::OpenPack {
                    tree: TreeRef(tree.to_owned()),
                    pack: r.u32()?,
                    holder: HolderId(r.u64()?),
                    region: r.region()?,
                }
            }
            3 => Message::Wait {
                pack: PackHandle(r.u64()?),
                range: ByteRange {
                    offset: r.u64()?,
                    len: r.u64()?,
                },
                job: JobId(r.u64()?),
            },
            4 => {
                let pack = PackHandle(r.u64()?);
                let class = r.class()?;
                let job = JobId(r.u64()?);
                let rank = Rank(r.u32()?);
                let n = r.u32()?;
                let mut runs = Vec::new();
                for _ in 0..n {
                    runs.push(CellRun {
                        first: r.u32()?,
                        count: r.u32()?,
                    });
                }
                Message::Ensure {
                    pack,
                    runs,
                    class,
                    job,
                    rank,
                }
            }
            5 => Message::Release {
                holder: HolderId(r.u64()?),
            },
            129 => Message::HandshakeOk,
            130 => Message::PackOpened {
                pack: PackHandle(r.u64()?),
                pack_size: r.u64()?,
                region: r.region()?,
            },
            131 => Message::Waited(match r.u8()? {
                0 => WaitOutcome::Ready,
                1 => WaitOutcome::Eio { cell: r.u32()? },
                2 => WaitOutcome::Lost,
                _ => return Err(WireError::Invalid("unknown wait outcome")),
            }),
            132 => Message::Ensured,
            133 => Message::Released,
            134 => Message::Refused {
                code: RefusalCode::from_code(r.u16()?)
                    .ok_or(WireError::Invalid("unknown refusal code"))?,
            },
            _ => return Err(WireError::UnknownKind(kind)),
        };
        r.end()?;
        let expected = message.fds().len();
        if fds != expected {
            return Err(WireError::FdCount {
                kind,
                expected,
                got: fds,
            });
        }
        Ok((Self { id, epoch, message }, 4 + len))
    }
}
