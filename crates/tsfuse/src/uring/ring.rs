//! What the io_uring transport asks of a ring (5.2 item 4), and the memory it shares with the
//! kernel.
//!
//! [`Ring`] is the boundary between the transport's logic (register, commit-and-fetch, the
//! hybrid routing, in [`super::queue`]) and the system calls that carry it ([`super::sys`], the
//! one module with `unsafe` code). The transport speaks FUSE's two ring commands through it:
//! `FUSE_IO_URING_CMD_REGISTER` hands the kernel an entry's two buffers, and
//! `FUSE_IO_URING_CMD_COMMIT_AND_FETCH` answers the request an entry holds and gives the entry
//! back for the next one.
//!
//! **The buffers.** Each entry has a header buffer (`struct fuse_uring_req_header`, 288 bytes) and
//! a payload buffer, [`EntryBufs`]. The kernel writes a request into them and reads the reply
//! from them through the addresses the REGISTER passed, so their memory is shared with the kernel
//! for the life of the ring. It is held as `AtomicU64` words: memory Rust allows to change under a
//! shared reference, read and written here with relaxed atomic loads and stores, so no `unsafe` is
//! needed to touch it. Who touches an entry's buffers, and when, is the protocol of the kernel
//! (`fs/fuse/dev_uring.c`): from a REGISTER or a COMMIT_AND_FETCH until the entry's completion,
//! only the kernel; from the completion until the next COMMIT_AND_FETCH, only the server, and
//! there only the one replier of the request the entry holds.

use std::io;
use std::os::fd::RawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The size of `struct fuse_uring_req_header`: `in_out` (128), `op_in` (128) and
/// `struct fuse_uring_ent_in_out` (32).
pub const ENTRY_HEADER_LEN: usize = 288;
/// Where the in header of a request (`struct fuse_in_header`) and the out header of its reply
/// (`struct fuse_out_header`) are, in the header buffer.
pub(crate) const IN_OUT: usize = 0;
/// Where the request's first argument (its "per op header") is, in the header buffer.
pub(crate) const OP_IN: usize = 128;
/// The room for the first argument.
pub(crate) const OP_IN_LEN: usize = 128;
/// `fuse_uring_ent_in_out.commit_id`, in the header buffer: the unique to commit the reply under.
pub(crate) const COMMIT_ID: usize = 256 + 8;
/// `fuse_uring_ent_in_out.payload_sz`, in the header buffer: the bytes of the payload buffer the
/// request (from the kernel) or the reply (to the kernel) uses.
pub(crate) const PAYLOAD_SZ: usize = 256 + 16;

/// `FUSE_IO_URING_CMD_REGISTER`.
pub(crate) const CMD_REGISTER: u32 = 1;
/// `FUSE_IO_URING_CMD_COMMIT_AND_FETCH`.
pub(crate) const CMD_COMMIT_AND_FETCH: u32 = 2;

/// Bytes shared with the kernel, held as `AtomicU64` words (see the module documentation).
pub(crate) struct Buf {
    words: Box<[AtomicU64]>,
}

impl Buf {
    /// A zeroed buffer of at least `len` bytes (a whole number of words).
    pub(crate) fn new(len: usize) -> Self {
        Self {
            words: (0..len.div_ceil(8)).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    /// The address the kernel is given.
    pub(crate) fn addr(&self) -> u64 {
        self.words.as_ptr() as usize as u64
    }

    /// Its length, bytes.
    pub(crate) fn len(&self) -> usize {
        self.words.len() * 8
    }

    /// Copies `out.len()` bytes from `offset`; `false` (and nothing copied) past the end.
    pub(crate) fn read(&self, offset: usize, out: &mut [u8]) -> bool {
        if offset
            .checked_add(out.len())
            .is_none_or(|end| end > self.len())
        {
            return false;
        }
        let mut done = 0;
        while done < out.len() {
            let at = offset + done;
            let (word, byte) = (at / 8, at % 8);
            let n = (8 - byte).min(out.len() - done);
            let bytes = self.words[word].load(Ordering::Relaxed).to_le_bytes();
            out[done..done + n].copy_from_slice(&bytes[byte..byte + n]);
            done += n;
        }
        true
    }

    /// Copies `data` to `offset`; `false` (and nothing copied) past the end.
    pub(crate) fn write(&self, offset: usize, data: &[u8]) -> bool {
        if offset
            .checked_add(data.len())
            .is_none_or(|end| end > self.len())
        {
            return false;
        }
        let mut done = 0;
        while done < data.len() {
            let at = offset + done;
            let (word, byte) = (at / 8, at % 8);
            let n = (8 - byte).min(data.len() - done);
            let cell = &self.words[word];
            let mut bytes = match n {
                8 => [0u8; 8],
                _ => cell.load(Ordering::Relaxed).to_le_bytes(),
            };
            bytes[byte..byte + n].copy_from_slice(&data[done..done + n]);
            cell.store(u64::from_le_bytes(bytes), Ordering::Relaxed);
            done += n;
        }
        true
    }

    /// The little-endian `u32` at `offset` (0 past the end).
    pub(crate) fn u32_at(&self, offset: usize) -> u32 {
        let mut b = [0u8; 4];
        self.read(offset, &mut b);
        u32::from_le_bytes(b)
    }

    /// The little-endian `u64` at `offset` (0 past the end).
    pub(crate) fn u64_at(&self, offset: usize) -> u64 {
        let mut b = [0u8; 8];
        self.read(offset, &mut b);
        u64::from_le_bytes(b)
    }
}

/// The two buffers of one ring entry: the header (`struct fuse_uring_req_header`) and the
/// payload, which holds the request's arguments after the first and the reply's payload; and the
/// iovec array a REGISTER points the kernel at (`{header, 288}`, `{payload, len}`).
pub(crate) struct EntryBufs {
    pub(crate) header: Buf,
    pub(crate) payload: Buf,
    iov: [u64; 4],
}

impl EntryBufs {
    /// The buffers of an entry whose payload holds `payload` bytes.
    pub(crate) fn new(payload: usize) -> Arc<Self> {
        let (header, payload) = (Buf::new(ENTRY_HEADER_LEN), Buf::new(payload));
        let iov = [
            header.addr(),
            header.len() as u64,
            payload.addr(),
            payload.len() as u64,
        ];
        Arc::new(Self {
            header,
            payload,
            iov,
        })
    }

    /// The address of the iovec array: as fixed as the entry, which never moves out of its
    /// `Arc`.
    pub(crate) fn iov_addr(&self) -> u64 {
        self.iov.as_ptr() as usize as u64
    }
}

/// A completion: the `user_data` of the command it completes and the kernel's result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cqe {
    pub(crate) user_data: u64,
    pub(crate) res: i32,
}

/// The `user_data` of every command of an entry: its queue and its index in the queue.
pub(crate) fn tag(qid: u16, entry: u32) -> u64 {
    u64::from(qid) << 32 | u64::from(entry)
}

/// The entry index of a tag.
pub(crate) fn entry_of(tag: u64) -> u32 {
    (tag & u64::from(u32::MAX)) as u32
}

/// A ring of one queue of a FUSE connection: the commands of its entries are queued here, sent
/// to the kernel by [`Ring::submit`], and complete into [`Ring::reap`]. A ring is used by the one
/// thread of its queue.
pub(crate) trait Ring: Send {
    /// Queues a REGISTER of `bufs` as an entry of queue `qid`, whose completions carry `tag`.
    fn register(&mut self, qid: u16, tag: u64, bufs: &Arc<EntryBufs>) -> io::Result<()>;

    /// Queues a COMMIT_AND_FETCH of the reply in the buffers of the entry `tag` of queue `qid`,
    /// to the request `commit_id`.
    fn commit(&mut self, qid: u16, tag: u64, commit_id: u64) -> io::Result<()>;

    /// Sends what is queued to the kernel; waits for nothing.
    fn submit(&mut self) -> io::Result<()>;

    /// Appends the completions posted so far to `out`.
    fn reap(&mut self, out: &mut Vec<Cqe>);

    /// A descriptor that polls readable when a completion is posted.
    fn readiness(&self) -> RawFd;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_buffer_round_trips_bytes_at_every_alignment_and_length() {
        let buf = Buf::new(40);
        assert_eq!(buf.len(), 40);
        for offset in 0..12 {
            for len in [0, 1, 3, 7, 8, 9, 15, 16, 17, 27] {
                let data: Vec<u8> = (0..len).map(|i| (offset * 31 + i + 1) as u8).collect();
                assert!(buf.write(offset, &data));
                let mut back = vec![0u8; len];
                assert!(buf.read(offset, &mut back));
                assert_eq!(back, data, "offset {offset} len {len}");
            }
        }
    }

    #[test]
    fn a_partial_word_write_keeps_the_neighbouring_bytes() {
        let buf = Buf::new(16);
        assert!(buf.write(0, &[0xaa; 16]));
        assert!(buf.write(3, &[1, 2, 3]));
        let mut all = [0u8; 16];
        assert!(buf.read(0, &mut all));
        assert_eq!(
            all,
            [
                0xaa, 0xaa, 0xaa, 1, 2, 3, 0xaa, 0xaa, 0xaa, 0xaa, 0xaa, 0xaa, 0xaa, 0xaa, 0xaa,
                0xaa
            ]
        );
    }

    #[test]
    fn a_buffer_refuses_a_range_past_its_end_and_changes_nothing() {
        let buf = Buf::new(9);
        assert_eq!(buf.len(), 16, "rounded up to whole words");
        assert!(buf.write(0, &[7; 16]));
        assert!(!buf.write(9, &[1; 8]));
        assert!(!buf.write(usize::MAX, &[1]));
        let mut out = [0u8; 8];
        assert!(!buf.read(9, &mut out));
        assert!(!buf.read(usize::MAX, &mut out));
        assert_eq!(out, [0; 8], "a refused read copies nothing");
        assert!(buf.read(8, &mut out));
        assert_eq!(out, [7; 8], "a refused write changed nothing");
        assert_eq!(buf.u32_at(16), 0);
        assert_eq!(buf.u64_at(12), 0);
    }

    #[test]
    fn integers_are_read_little_endian() {
        let buf = Buf::new(16);
        assert!(buf.write(4, &0x0102_0304_0506_0708u64.to_le_bytes()));
        assert_eq!(buf.u64_at(4), 0x0102_0304_0506_0708);
        assert_eq!(buf.u32_at(4), 0x0506_0708);
    }

    #[test]
    fn an_entry_has_a_header_of_288_bytes_and_its_payload() {
        let bufs = EntryBufs::new(128 * 1024);
        assert_eq!(bufs.header.len(), ENTRY_HEADER_LEN);
        assert_eq!(bufs.payload.len(), 128 * 1024);
        assert_ne!(bufs.header.addr(), bufs.payload.addr());
        assert_eq!(bufs.header.addr() % 8, 0);
        assert_eq!(
            bufs.iov,
            [bufs.header.addr(), 288, bufs.payload.addr(), 128 * 1024]
        );
        assert_eq!(bufs.iov_addr(), bufs.iov.as_ptr() as usize as u64);
    }

    /// The pinned uapi header of the farm kernel (`abi/fuse.h`, protocol 7.45).
    const HEADER: &str = include_str!("../../abi/fuse.h");

    /// The lines of `struct name { ... }` in the header, trimmed, without blanks and comments.
    fn fields(name: &str) -> Vec<String> {
        let start = HEADER.find(&format!("struct {name} {{")).unwrap();
        let body = &HEADER[start..];
        let body = &body[body.find('{').unwrap() + 1..body.find("};").unwrap()];
        body.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with("/*") && !l.starts_with('*'))
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn the_offsets_and_commands_are_the_pinned_headers() {
        assert!(HEADER.contains(&format!("#define FUSE_URING_IN_OUT_HEADER_SZ {OP_IN}")));
        assert!(HEADER.contains(&format!("#define FUSE_URING_OP_IN_OUT_SZ {OP_IN_LEN}")));
        assert_eq!(
            fields("fuse_uring_req_header"),
            [
                "char in_out[FUSE_URING_IN_OUT_HEADER_SZ];",
                "char op_in[FUSE_URING_OP_IN_OUT_SZ];",
                "struct fuse_uring_ent_in_out ring_ent_in_out;",
            ]
        );
        // `fuse_uring_ent_in_out` at 256: flags, then commit_id (+8), then payload_sz (+16); 32
        // bytes in all, so the header is 288.
        assert_eq!(
            fields("fuse_uring_ent_in_out"),
            [
                "uint64_t flags;",
                "uint64_t commit_id;",
                "uint32_t payload_sz;",
                "uint32_t padding;",
                "uint64_t reserved;",
            ]
        );
        assert_eq!(
            (COMMIT_ID, PAYLOAD_SZ),
            (OP_IN + OP_IN_LEN + 8, OP_IN + OP_IN_LEN + 16)
        );
        assert_eq!(ENTRY_HEADER_LEN, OP_IN + OP_IN_LEN + 32);
        // The 80-byte command area of the SQE (byte 48 on): flags, commit_id (+8), qid (+16),
        // as `sys::fuse_cmd` writes them.
        assert_eq!(
            fields("fuse_uring_cmd_req"),
            [
                "uint64_t flags;",
                "uint64_t commit_id;",
                "uint16_t qid;",
                "uint8_t padding[6];",
            ]
        );
        assert!(HEADER.contains(&format!("FUSE_IO_URING_CMD_REGISTER = {CMD_REGISTER},")));
        assert!(HEADER.contains(&format!(
            "FUSE_IO_URING_CMD_COMMIT_AND_FETCH = {CMD_COMMIT_AND_FETCH},"
        )));
    }

    #[test]
    fn a_tag_carries_the_queue_and_the_entry() {
        let t = tag(87, 4);
        assert_eq!(t >> 32, 87);
        assert_eq!(entry_of(t), 4);
        assert_eq!(entry_of(tag(u16::MAX, u32::MAX)), u32::MAX);
    }
}
