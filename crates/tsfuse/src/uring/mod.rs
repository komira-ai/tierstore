//! The io_uring transport (5.2 item 4; row T-F5 of section 18): rings, REGISTER,
//! COMMIT_AND_FETCH, the hybrid `/dev/fuse` reader for INTERRUPT and FORGET, the depth, the
//! accounting of the memory the entries register, and the fallback to `/dev/fuse`.
//!
//! **Choosing the transport.** [`crate::Session::init_uring`] decides before it answers INIT,
//! because a connection granted `FUSE_OVER_IO_URING` holds every ordinary request in the kernel
//! until each queue has a registered entry (`fuse_block_alloc`, K10): a session that asked for it
//! and then did not register would wedge the mount. It asks only when every one of these holds,
//! in this order, and otherwise serves on `/dev/fuse`, saying why ([`Fallback`]):
//! 1. `fuse.enable_uring` is on ([`ENABLE_URING`], read at startup; set at boot by policy, 5.2);
//! 2. the kernel offered `FUSE_OVER_IO_URING` at INIT (it does only while the parameter is on);
//! 3. the possible CPUs can be read ([`POSSIBLE_CPUS`]): the kernel has one queue per possible
//!    CPU, and every queue must register;
//! 4. the memory the entries register, [`UringMemory`], is within [`UringConfig::memory_limit`];
//! 5. a ring can be made for every queue (`io_uring_setup` may be refused by
//!    `kernel.io_uring_disabled`, a seccomp profile or a memory limit).
//!
//! [`crate::Session::serve`] then registers every queue's entries before the ring carries
//! anything; if the kernel refuses a REGISTER it turns io_uring off for the connection
//! (`fc->io_uring = 0`), requests flow on `/dev/fuse` again, and the session serves there
//! ([`Fallback::Register`]). Every queue but the last registers first: the ring becomes ready
//! (requests go to it) only when the last queue's first entry registers, so a refusal of any
//! other queue's REGISTER can never come after the ring is ready, which would wedge the mount
//! (K10).
//!
//! **Hybrid.** In io_uring mode the kernel still sends FORGET, BATCH_FORGET and INTERRUPT, and the
//! requests forced or queued before the ring was ready, on `/dev/fuse` (K11): the session keeps its
//! `/dev/fuse` readers serving beside the rings. An INTERRUPT whose request no `/dev/fuse` reader
//! holds is held until the request's ring entry delivers it ([`crate::Tokens`]), and a ring entry
//! carrying a request that takes no reply is committed at once.
//!
//! **No takeover.** A request held by a ring entry is not resent by `FUSE_NOTIFY_RESEND` (K14), so
//! [`crate::Session::resume`] refuses the handover of a session granted io_uring.

mod queue;
mod ring;
pub(crate) mod serve;
mod sys;

use std::io;
use std::num::NonZeroU16;
use std::path::Path;
use std::sync::Arc;

use crate::abi::{InitFlags, Negotiated};
use crate::dev::DevFd;

pub(crate) use queue::{Shared, UringReader};
pub(crate) use ring::ENTRY_HEADER_LEN;
pub(crate) use ring::Ring;

/// The module parameter that turns FUSE over io_uring on (`Y`) or off (`N`).
pub const ENABLE_URING: &str = "/sys/module/fuse/parameters/enable_uring";
/// The kernel's list of possible CPUs (for example `0-87`): one FUSE queue each.
pub const POSSIBLE_CPUS: &str = "/sys/devices/system/cpu/possible";
/// The smallest payload buffer the kernel accepts (`FUSE_MIN_READ_BUFFER`).
pub(crate) const MIN_PAYLOAD: u32 = 8192;

/// How a session uses io_uring (5.2 item 4).
///
/// The defaults serve both deployment profiles of section 21: their build nodes have 88 possible
/// CPUs, and a tree mount with `max_pages` 32 (128 KiB payloads, the largest buffered READ, K21)
/// registers `88 x 5 x (288 B + 128 KiB)`, 55 MiB, within the 64 MiB rule. A mount with
/// `max_pages` 256 (1 MiB payloads, [`crate::abi::InitConfig::tree`]) registers 88 MiB at depth 1
/// and so falls back to `/dev/fuse`: an io_uring tree mount asks for `max_pages` 32.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UringConfig {
    /// Entries registered on every queue: the requests one CPU can have in the server at once.
    /// A parked read holds its entry, and a request waits for a free entry of its own CPU's
    /// queue (K12).
    pub depth: NonZeroU16,
    /// The most memory every queue's entries may register together, bytes; past it the session
    /// serves on `/dev/fuse`.
    pub memory_limit: u64,
}

impl UringConfig {
    /// The default depth: the most that fits [`Self::MEMORY_LIMIT`] with 128 KiB payloads on 88
    /// possible CPUs (K13).
    pub const DEPTH: NonZeroU16 = match NonZeroU16::new(5) {
        Some(depth) => depth,
        None => NonZeroU16::MIN,
    };
    /// The default limit: 64 MiB (5.2 item 4).
    pub const MEMORY_LIMIT: u64 = 64 << 20;
}

impl Default for UringConfig {
    fn default() -> Self {
        Self {
            depth: Self::DEPTH,
            memory_limit: Self::MEMORY_LIMIT,
        }
    }
}

/// The memory a connection's ring entries register (K13): every queue registers `depth` entries,
/// each a 288-byte header (`struct fuse_uring_req_header`) and a payload buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UringMemory {
    /// Queues: one per possible CPU.
    pub queues: u32,
    /// Entries per queue.
    pub depth: u16,
    /// The payload buffer of each entry, bytes.
    pub payload: u32,
    /// `queues x depth x (288 + payload)` (`uring_registered_bytes`), saturating at `u64::MAX`.
    pub bytes: u64,
}

impl UringMemory {
    /// The memory of `queues` queues of `depth` entries with `payload`-byte payloads.
    #[must_use]
    pub fn of(queues: u32, depth: u16, payload: u32) -> Self {
        let entry = ENTRY_HEADER_LEN as u64 + u64::from(payload);
        Self {
            queues,
            depth,
            payload,
            // At most 2^32 x 2^16 x 2^33: past `u64`, so it saturates (and is over any limit).
            bytes: u64::from(queues)
                .saturating_mul(u64::from(depth))
                .saturating_mul(entry),
        }
    }

    /// The payload buffer the kernel requires of every entry, fixed by the INIT terms:
    /// `max(8 KiB, max_write, max_pages x page)` (`fuse_uring_create`). A reply without
    /// `max_pages` (0) leaves the kernel's default of 32 pages.
    #[must_use]
    pub fn payload(max_write: u32, max_pages: u16, page: u32) -> u32 {
        let pages = match max_pages {
            0 => 32,
            n => u32::from(n),
        };
        MIN_PAYLOAD.max(max_write).max(pages.saturating_mul(page))
    }
}

/// The transport a session serves on (`fuse_transport_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportMode {
    /// `/dev/fuse` readers only, and why io_uring is not used.
    DevFuse(Fallback),
    /// io_uring rings (with the `/dev/fuse` readers beside them), and what they register.
    Uring(UringMemory),
}

/// Why a session serves on `/dev/fuse` and not on io_uring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    /// io_uring was not asked for ([`crate::Session::init`], [`crate::Session::resume`]).
    NotAsked,
    /// `fuse.enable_uring` is off, or cannot be read (no `fuse` module parameter).
    Disabled,
    /// The kernel did not offer `FUSE_OVER_IO_URING` at INIT.
    NotOffered,
    /// The possible CPUs ([`POSSIBLE_CPUS`]) could not be read.
    CpusUnknown,
    /// The entries would register more than [`UringConfig::memory_limit`].
    OverMemoryLimit {
        /// What they would register.
        memory: UringMemory,
        /// The limit.
        limit: u64,
    },
    /// A ring could not be made (`io_uring_setup` or a mapping failed with `errno`).
    RingSetup {
        /// The error.
        errno: i32,
    },
    /// The kernel refused a REGISTER on queue `qid` with `errno`; it then turns io_uring off for
    /// the connection.
    Register {
        /// The queue.
        qid: u16,
        /// The error.
        errno: i32,
    },
}

/// What the host says about io_uring, read at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Host {
    /// `fuse.enable_uring`.
    pub(crate) enabled: bool,
    /// The number of possible CPUs.
    pub(crate) cpus: Option<u32>,
    /// The page size, bytes.
    pub(crate) page: u32,
}

impl Host {
    /// The facts of this machine.
    pub(crate) fn read() -> Self {
        Self::read_from(Path::new(ENABLE_URING), Path::new(POSSIBLE_CPUS))
    }

    /// The facts as the two files say them.
    pub(crate) fn read_from(enable: &Path, possible: &Path) -> Self {
        let text = |p: &Path| std::fs::read_to_string(p).ok();
        Self {
            enabled: text(enable).is_some_and(|t| parse_enabled(&t)),
            cpus: text(possible).and_then(|t| parse_cpu_list(&t)),
            page: u32::try_from(rustix::param::page_size()).unwrap_or(4096),
        }
    }
}

/// Whether a boolean module parameter (`Y`, `N`, `1`, `0`) is on.
pub(crate) fn parse_enabled(text: &str) -> bool {
    matches!(text.trim(), "Y" | "y" | "1")
}

/// The number of CPUs in a kernel CPU list (`0-87`, `0,2-5,7`); `None` if it is not one.
pub(crate) fn parse_cpu_list(text: &str) -> Option<u32> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut count = 0u32;
    for range in text.split(',') {
        let (lo, hi) = match range.split_once('-') {
            Some((lo, hi)) => (lo.parse::<u32>().ok()?, hi.parse::<u32>().ok()?),
            None => {
                let one = range.parse::<u32>().ok()?;
                (one, one)
            }
        };
        count = count.checked_add(hi.checked_sub(lo)?.checked_add(1)?)?;
    }
    Some(count)
}

/// Whether a session initialised with `negotiated` on `host` may use io_uring, and what its
/// entries would register.
///
/// # Errors
///
/// Why it may not (the first of the checks of the module documentation that fails, 1 to 4).
pub(crate) fn plan(
    config: &UringConfig,
    host: &Host,
    negotiated: &Negotiated,
) -> Result<UringMemory, Fallback> {
    if !host.enabled {
        return Err(Fallback::Disabled);
    }
    if !negotiated.offered.contains(InitFlags::OVER_IO_URING) {
        return Err(Fallback::NotOffered);
    }
    let queues = host.cpus.ok_or(Fallback::CpusUnknown)?;
    let reply = &negotiated.reply;
    let payload = UringMemory::payload(reply.max_write, reply.max_pages, host.page);
    let memory = UringMemory::of(queues, config.depth.get(), payload);
    if memory.bytes > config.memory_limit {
        return Err(Fallback::OverMemoryLimit {
            memory,
            limit: config.memory_limit,
        });
    }
    Ok(memory)
}

/// One queue, made ready before INIT is answered: its reader (the ring, registered with the
/// runtime) and the current-thread runtime its thread will run.
pub(crate) struct Prepared {
    // Dropped before the runtime it is registered with.
    pub(crate) reader: UringReader,
    pub(crate) runtime: tokio::runtime::Runtime,
    pub(crate) shared: Arc<Shared>,
}

/// The queues of a session that asked for io_uring at INIT, every one made before INIT was
/// answered (so that nothing that can fail for want of a resource is left for after it), and
/// registered when the session serves.
pub(crate) struct Rings {
    pub(crate) memory: UringMemory,
    pub(crate) queues: Vec<Prepared>,
}

impl std::fmt::Debug for Rings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rings")
            .field("memory", &self.memory)
            .finish_non_exhaustive()
    }
}

/// Makes the ring of each queue of `memory`; every ring's commands name `device`.
///
/// # Errors
///
/// [`Fallback::RingSetup`] with the first error.
pub(crate) fn kernel_rings(device: &Arc<DevFd>, memory: UringMemory) -> Result<Rings, Fallback> {
    make_rings(memory, &mut |_qid| {
        let ring = sys::KernelRing::new(device.clone(), u32::from(memory.depth))?;
        Ok(Box::new(ring))
    })
}

/// Makes each queue of `memory`: its ring with `make`, its thread's runtime, and its reader. A
/// queue costs three descriptors: the ring's, and the runtime's epoll and wake descriptors.
///
/// # Errors
///
/// [`Fallback::RingSetup`] with the first error (`EMFILE` when the descriptors run out).
pub(crate) fn make_rings(
    memory: UringMemory,
    make: &mut dyn FnMut(u16) -> io::Result<Box<dyn Ring>>,
) -> Result<Rings, Fallback> {
    let setup = |e: io::Error| Fallback::RingSetup {
        errno: e.raw_os_error().unwrap_or(5),
    };
    // A queue id is a `u16` in the kernel's command (`fuse_uring_cmd_req.qid`).
    if memory.queues > u32::from(u16::MAX) + 1 {
        return Err(Fallback::RingSetup { errno: 22 });
    }
    let mut queues = Vec::with_capacity(memory.queues as usize);
    for qid in (0..=u16::MAX).take(memory.queues as usize) {
        let ring = make(qid).map_err(setup)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .map_err(setup)?;
        let shared = Shared::new(qid);
        let reader = {
            let _context = runtime.enter();
            UringReader::new(ring, shared.clone(), memory.depth, memory.payload as usize)
                .map_err(setup)?
        };
        queues.push(Prepared {
            reader,
            runtime,
            shared,
        });
    }
    Ok(Rings { memory, queues })
}

/// The `/dev/fuse` read buffer and the ring payload a reader of `negotiated` needs: a ring
/// request is rebuilt in the reader's buffer as `/dev/fuse` would have read it.
pub(crate) fn reader_buffer(read_buffer: usize, memory: &UringMemory) -> usize {
    read_buffer.max(crate::fs::IN_HEADER_LEN + ring::OP_IN_LEN + memory.payload as usize)
}

#[cfg(test)]
mod tests;
