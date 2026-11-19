//! `tsfuse`, the FUSE library of tierstore (`docs/design/fetch-cache-serve.md`, 5.2): this crate
//! holds the dispatch interface frozen by the interface-freeze change (section 18, row T-I) and
//! the kernel ABI, [`abi`] (row T-F1): the wire structs, the parse of each request, the reply
//! payloads and INIT negotiation. On Linux it also holds the `/dev/fuse` transport (row T-F2):
//! [`mount`] (`mount(2)`, or `fusermount3`; unmount and abort), [`DevFd`] (a descriptor of a
//! connection, cloned with `FUSE_DEV_IOC_CLONE`, writing each reply with one `writev`), the
//! [`Transport`] trait and its `/dev/fuse` implementation [`DevReader`] on `AsyncFd`, and
//! [`Session`] (INIT, resume without INIT, clones, one reader thread per descriptor, stop). The
//! io_uring transport (T-F5), the splice reply path (T-F4) and takeover (T-F6) build on them.
//!
//! **Reply affinity.** A reply is matched by `unique` against the processing queue of the device
//! it is written to, and every cloned `/dev/fuse` descriptor has its own processing queue, so a
//! reply must be written to the descriptor that read the request. A [`Replier`] therefore carries
//! its originating device (a [`ReplyDevice`]: the cloned descriptor, or a handle to the owning
//! io_uring queue's submit channel) and completes only there; it is `Send + 'static`, so it may be
//! completed from any task.
//!
//! **Cancel registration at read time.** A reader registers `unique -> token` in the
//! [`Tokens`] table before [`Fs::handle`] runs, so an INTERRUPT that another clone reads between
//! the read and the park finds the entry. **Atomic complete.** Completing is a single atomic take
//! of the request's slot: exactly one of the real reply and the `EINTR` reply is ever written. An
//! A [`Replier`] dropped without being completed answers `EIO` on its originating device (and
//! leaves the table), so a request is never left unanswered; one already taken by an INTERRUPT
//! or a completion writes nothing on drop. An INTERRUPT for a request that is not (or no longer)
//! registered is answered `EAGAIN` on every descriptor of the session in turn: the kernel matches
//! that reply on the processing queue of the descriptor it is written to, so only the clone that
//! holds the request (read, not yet registered) takes it, and the kernel queues the INTERRUPT
//! again; on any other clone the write is refused with `ENOENT` and would lose the INTERRUPT
//! (`docs/design/tsfuse-kernel-findings.md`, K7).
//!
//! Section 14 rows: "Replier: with N > 1 clones, a parked reply completed from another task is
//! written to its originating device; INTERRUPT read by another clone is answered once (reply xor
//! EINTR)"; "Ported fuser tests, `tsfuse` per-opcode encode/decode, kernel conformance suite";
//! "Takeover with N > 1 clones and parked reads on several"; "All reader threads blocked in a fake
//! `pread` while `wait` completions still flow".

// `deny`, not `forbid`: the one call the kernel offers no safe wrapper for, the
// `FUSE_DEV_IOC_CLONE` ioctl in `dev.rs`, `expect`s it with its reason (AGENTS.md R2).
#![deny(unsafe_code)]

pub mod abi;
#[cfg(target_os = "linux")]
mod dev;
mod fs;
#[cfg(target_os = "linux")]
pub mod mount;
mod opcode;
mod reply;
#[cfg(target_os = "linux")]
mod session;
mod tokens;
#[cfg(target_os = "linux")]
mod transport;

pub use fs::{Fs, Handled, Request, RequestHeader};
pub use opcode::Opcode;
pub use reply::{Completion, Errno, OUT_HEADER_LEN, Replier, ReplyDevice, encode_out_header};
pub use tokens::{CancelToken, Interrupted, Tokens};

#[cfg(target_os = "linux")]
pub use dev::{DEV_FUSE, DevFd};
#[cfg(target_os = "linux")]
pub use session::{Serving, Session, SessionError, Stopper};
#[cfg(target_os = "linux")]
pub use transport::{DevReader, ReaderEnd, Received, Transport};
