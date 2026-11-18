//! `tsfuse`, the FUSE library of tierstore (`docs/design/fetch-cache-serve.md`, 5.2): this crate
//! holds the dispatch interface frozen by the interface-freeze change (section 18, row T-I) and
//! the kernel ABI, [`abi`] (row T-F1): the wire structs, the parse of each request, the reply
//! payloads and INIT negotiation. The `/dev/fuse` and io_uring transports (T-F2, T-F5), the
//! splice reply path (T-F4) and takeover (T-F6) build on them.
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
//! registered is answered `EAGAIN` on the device that read the INTERRUPT, which makes the kernel
//! queue it again.
//!
//! Section 14 rows: "Replier: with N > 1 clones, a parked reply completed from another task is
//! written to its originating device; INTERRUPT read by another clone is answered once (reply xor
//! EINTR)"; "Ported fuser tests, `tsfuse` per-opcode encode/decode, kernel conformance suite";
//! "Takeover with N > 1 clones and parked reads on several"; "All reader threads blocked in a fake
//! `pread` while `wait` completions still flow".

#![forbid(unsafe_code)]

pub mod abi;
mod fs;
mod opcode;
mod reply;
mod tokens;

pub use fs::{Fs, Handled, Request, RequestHeader};
pub use opcode::Opcode;
pub use reply::{Completion, Errno, OUT_HEADER_LEN, Replier, ReplyDevice, encode_out_header};
pub use tokens::{CancelToken, Interrupted, Tokens};
