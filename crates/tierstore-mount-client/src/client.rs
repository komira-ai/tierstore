//! [`PackClient`]: the mount's side of the protocol, started on a runtime of its own.

use std::fmt;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rustix::net::{AddressFamily, SocketFlags, SocketType, socketpair};
use tierstore_fetch_api::ids::HolderId;
use tokio::sync::{mpsc, watch};

use crate::backoff::{Backoff, random_seed};
use crate::counters::CounterMemfd;
use crate::driver::{Command, Commands, Driver, Parts};
use crate::error::StartError;
use crate::link::{Connector, Timer};
use crate::pack::{OpenSpec, Pack};
use crate::stats::Stats;
use crate::ticket::Ticket;

/// The client's settings. Both deployment profiles of section 21 use the defaults: the protocol is
/// node-local (a mount and the node's cache on one build or combined node), so neither the number
/// of storage nodes nor their drives changes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// The first reconnect delay before jitter: 50 ms (5.4).
    pub backoff_first_ms: u64,
    /// The largest reconnect delay: 2 s (5.4).
    pub backoff_max_ms: u64,
    /// The seed of the reconnect jitter; `None` draws one from the system's random source, so
    /// the mounts of a node do not reconnect in step. A test passes one.
    pub jitter_seed: Option<u64>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            backoff_first_ms: 50,
            backoff_max_ms: 2000,
            jitter_seed: None,
        }
    }
}

/// What the readers share with the client's runtime.
struct Shared {
    holder: HolderId,
    commands: mpsc::UnboundedSender<Command>,
    ready_local: Arc<AtomicU64>,
    stats: watch::Receiver<Stats>,
    /// The mount's end of the lifeline, open for the client's life (5.1): dropping the last
    /// [`PackClient`] closes it, and the server then ends the holder.
    _lifeline: OwnedFd,
}

/// The mount's side of the mount-to-tierstore protocol (5.1, 5.4). Clones share one client.
///
/// [`PackClient::start`] creates the mount's counter memfd and lifeline, and runs the protocol on
/// a **runtime of its own**, on a thread of its own: the reader threads of `tsfuse` block in the
/// kernel (a `pread`, a splice, a page fault of the tree image), and a blocked reader must never
/// stall the `wait` completions that unblock other reads. Readers call the client from any
/// thread; every call returns at once, and an answer that comes later comes as a [`Ticket`].
///
/// The client runs while a `PackClient` lives. When the last one is dropped, the runtime stops,
/// the lifeline closes (so the server ends the holder and drops its holds), and the tickets still
/// outstanding answer [`crate::ClientError::Stopped`].
///
/// Section 14 rows: "Wait coalescing: at most one outstanding `wait` per 8 cells"; "Server restart
/// with a stale-epoch reply in flight: dropped; the `wait` re-issued"; "All reader threads blocked
/// in a fake `pread` while `wait` completions still flow"; the mount's side of "Mount whose
/// protocol connection flaps keeps counters, regions and holds" and of "A mount grows its counter
/// memfd past the server's mapping"; "Mount's state mapping is read-only" (the mount refuses a
/// state memfd that is not sealed for mounts).
#[derive(Clone)]
pub struct PackClient {
    shared: Arc<Shared>,
}

impl fmt::Debug for PackClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PackClient")
            .field("holder", &self.shared.holder)
            .finish_non_exhaustive()
    }
}

impl PackClient {
    /// Starts a client for the mount `holder` (the same id for the mount's whole life) that
    /// reaches the server through `connector` and sleeps between attempts on `timer`.
    ///
    /// # Errors
    /// The counter memfd, the lifeline, the jitter's seed, the runtime or its thread could not be
    /// made.
    pub fn start(
        holder: HolderId,
        connector: Arc<dyn Connector>,
        timer: Arc<dyn Timer>,
        config: Config,
    ) -> Result<Self, StartError> {
        let memfd = CounterMemfd::create().map_err(StartError::Counters)?;
        let (kept, sent) = lifeline().map_err(StartError::Lifeline)?;
        let seed = match config.jitter_seed {
            Some(seed) => seed,
            None => random_seed().map_err(StartError::Random)?,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(StartError::Runtime)?;
        let (tx, rx) = mpsc::unbounded_channel();
        let (stats_tx, stats) = watch::channel(Stats::default());
        let ready_local = Arc::new(AtomicU64::new(0));
        let driver = Driver::new(Parts {
            holder,
            memfd,
            lifeline: Arc::new(sent),
            connector,
            timer,
            backoff: Backoff::new(config.backoff_first_ms, config.backoff_max_ms, seed),
            commands: Commands::new(tx.downgrade(), ready_local.clone()),
            stats: stats_tx,
        });
        std::thread::Builder::new()
            .name("tierstore-mount-client".into())
            .spawn(move || runtime.block_on(driver.run(rx)))
            .map_err(StartError::Runtime)?;
        Ok(Self {
            shared: Arc::new(Shared {
                holder,
                commands: tx,
                ready_local,
                stats,
                _lifeline: kept,
            }),
        })
    }

    /// The mount's holder id.
    #[must_use]
    pub fn holder(&self) -> HolderId {
        self.shared.holder
    }

    /// Opens a pack, or finds it open: the table of open packs is keyed by [`OpenSpec::pack`], so
    /// a pack is opened once for the mount's life whatever tree names it, with one counter region.
    /// Before the first connection is up the open waits for it. A refusal of the server
    /// ([`crate::ClientError::OpenRefused`]) is the reader's miss path.
    pub fn open(&self, spec: OpenSpec) -> Ticket<Arc<Pack>> {
        let (answer, ticket) = Ticket::new();
        // A send fails only when the runtime stopped; the answer then drops and the ticket
        // answers `Stopped`.
        let _ = self.shared.commands.send(Command::Open { spec, answer });
        ticket
    }

    /// Releases the holder (a clean unmount): the server drops its holds. Every outstanding and
    /// later request then answers [`crate::ClientError::Released`].
    pub fn release(&self) -> Ticket<()> {
        let (answer, ticket) = Ticket::new();
        let _ = self.shared.commands.send(Command::Release { answer });
        ticket
    }

    /// The counts now.
    #[must_use]
    pub fn stats(&self) -> Stats {
        let mut stats = *self.shared.stats.borrow();
        stats.waits_ready_local += self.shared.ready_local.load(Ordering::Relaxed);
        stats
    }

    /// Blocks the calling thread until the counts of the client's runtime satisfy `done`, and
    /// returns them (reads found ready by a reader are not in what `done` sees). Call it from a
    /// thread of its own, never from a task of an async runtime.
    pub fn wait_for(&self, mut done: impl FnMut(&Stats) -> bool) -> Stats {
        let mut rx = self.shared.stats.clone();
        // The sender lives in the runtime; once it stops, the counts no longer change.
        let _ = futures::executor::block_on(rx.wait_for(|s| done(s)));
        self.stats()
    }
}

/// A lifeline: the end the mount keeps and the end it sends.
fn lifeline() -> std::io::Result<(OwnedFd, OwnedFd)> {
    Ok(socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )?)
}
