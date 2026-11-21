//! The client's runtime: one task that owns the connection, the table of open packs and every
//! outstanding request, fed by the readers' commands and the server's frames (5.4).
//!
//! **The connection.** The driver connects, sends the handshake with the counter memfd and the
//! lifeline (the same two descriptors on every connection), and on `HandshakeOk` takes the reply's
//! epoch as the connection's. It then re-sends `open-pack` for every pack of the table, with the
//! pack's region (the server answers the same region; the view of the reply replaces the pack's),
//! and once a pack is open again re-issues every `wait` outstanding on it. Replies stamped with
//! another epoch than the connection's are dropped, and so are replies to requests that are not
//! in flight on this connection (request ids are never reused). When the connection ends (the
//! server closed it, a frame was not one, or the server refused an `open-pack` or a `wait` for a
//! handle or a holder it does not know; a refused `ensure` is a dropped hint), the driver sleeps a backoff step (50 ms doubling to 2 s, with
//! jitter, from the injected [`Timer`]) and connects again; an accepted handshake resets the
//! backoff. A reader's view of a pack stays usable through a reconnect: the server's eviction
//! waits for the grace window after a restart (7), and the re-open swaps the view.
//!
//! **Ends.** A refused handshake and an accepted `release` end the client: every outstanding and
//! later request is answered with the reason, and no connection is made again.
//!
//! Nothing here blocks: every call is a channel send, a frame queued for the writer task, or a
//! cheap system call on a memfd (`ftruncate`, `mmap`, `fcntl`, `pread` of a 64-byte header).

use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tierstore_fetch_api::engine::WaitOutcome;
use tierstore_fetch_api::entry::StateView;
use tierstore_fetch_api::ids::{ByteRange, CellRun, Class, HolderId, JobId, PackId, Rank};
use tierstore_fetch_api::page::{CELL_SHIFT, StatePageLayout};
use tierstore_fetch_api::wire::{
    Epoch, Frame, Message, PROTOCOL_VERSION, PackHandle, RefusalCode, WireError,
};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, watch};

use crate::backoff::Backoff;
use crate::coalesce::{Added, Part, Windows};
use crate::counters::{CounterMemfd, PackCounters};
use crate::error::ClientError;
use crate::framing::{Incoming, ReadError, read_frame, send_frame};
use crate::link::{Connector, Sleep, Timer};
use crate::pack::{OpenSpec, Pack, ReadWait};
use crate::stats::Stats;
use crate::ticket::Answer;
use crate::view::PackView;

/// What a reader asks of the client's runtime.
#[derive(Debug)]
pub(crate) enum Command {
    /// Open a pack (or find it open).
    Open {
        spec: OpenSpec,
        answer: Answer<Arc<Pack>>,
    },
    /// Wait for the parts of one read, `(window, need, cover)` each.
    Wait {
        pack: PackId,
        parts: Vec<(u32, CellRun, CellRun)>,
        job: JobId,
        read: Arc<ReadWait>,
    },
    /// Prefetch runs of an open pack.
    Ensure {
        pack: PackId,
        runs: Vec<CellRun>,
        class: Class,
        job: JobId,
        rank: Rank,
    },
    /// Release the holder.
    Release { answer: Answer<()> },
}

/// How a reader's [`Pack`] reaches the client's runtime. It holds the channel weakly: the client
/// runs while a [`crate::PackClient`] lives, and a pack left behind answers
/// [`ClientError::Stopped`].
#[derive(Debug, Clone)]
pub(crate) struct Commands {
    tx: mpsc::WeakUnboundedSender<Command>,
    ready_local: Arc<AtomicU64>,
}

impl Commands {
    pub(crate) fn new(tx: mpsc::WeakUnboundedSender<Command>, ready_local: Arc<AtomicU64>) -> Self {
        Self { tx, ready_local }
    }

    /// Sends `command`; when the client has stopped, the command (and the answer in it) is
    /// dropped, so its ticket answers [`ClientError::Stopped`].
    pub(crate) fn send(&self, command: Command) {
        if let Some(tx) = self.tx.upgrade() {
            let _ = tx.send(command);
        }
    }

    /// A read was found ready with no request.
    pub(crate) fn ready_local(&self) {
        self.ready_local.fetch_add(1, Ordering::Relaxed);
    }
}

/// Who a part of a read reports to, and the job it reads for.
#[derive(Debug, Clone)]
struct Joiner {
    read: Arc<ReadWait>,
    job: JobId,
}

/// A request in flight on the current connection.
#[derive(Debug)]
enum Pending {
    Handshake,
    Open(PackId),
    Wait(PackId, u32),
    Ensure,
    Release,
}

/// One pack of the table.
struct Slot {
    /// The spec it was first opened with; a re-open names its tree and index.
    spec: OpenSpec,
    layout: StatePageLayout,
    counters: PackCounters,
    /// The pack, once a first `open-pack` was answered.
    pack: Option<Arc<Pack>>,
    /// Its handle on the current connection.
    handle: Option<PackHandle>,
    /// Opens waiting for the first `open-pack`.
    waiters: Vec<Answer<Arc<Pack>>>,
    windows: Windows<Joiner>,
    /// Why the pack can no longer be served.
    broken: Option<ClientError>,
}

/// A descriptor a frame carries: the counter memfd and the lifeline are sent on every handshake
/// and stay the client's, so a frame shares them rather than owning copies.
type SharedFd = Arc<dyn AsFd + Send + Sync>;

/// A frame for the writer, with the descriptors it carries.
struct Outgoing {
    bytes: Vec<u8>,
    fds: Vec<SharedFd>,
}

/// What a connection's tasks report: a frame, or the connection's end. The writer reports a
/// failed write too, so the channel stays open after the reader's end: the end is an item, never
/// the channel's close.
enum Inbound {
    Frame(Incoming),
    /// The server closed the connection between frames.
    Closed,
    /// A frame could not be read, or a write failed.
    Failed(ReadError),
}

/// A connection: its stream, the writer's queue and the reader's and writer's reports.
struct Conn {
    stream: Arc<UnixStream>,
    out: mpsc::UnboundedSender<Outgoing>,
    frames: mpsc::UnboundedReceiver<Inbound>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        // The reader and the writer hold the stream: shutting it down ends both (the reader sees
        // the end, a write in progress fails, and an idle writer sees its queue closed), and
        // then the last of them closes it. A stream already shut down by the server is fine.
        let _ = rustix::net::shutdown(&*self.stream, rustix::net::Shutdown::Both);
    }
}

type Connecting = Pin<Box<dyn Future<Output = io::Result<UnixStream>> + Send>>;

/// Where the connection is.
enum Link {
    Connecting(Connecting),
    Sleeping(Sleep),
    Up(Conn),
    /// The client ended: no connection is made again.
    Idle,
}

/// The connection and the requests in flight on it.
struct Wire {
    link: Link,
    /// The connection's epoch, once its handshake is accepted.
    conn_epoch: Option<Epoch>,
    /// The epoch of the last accepted handshake (0 before the first): what requests carry.
    epoch: Epoch,
    inflight: BTreeMap<u64, Pending>,
    next_id: u64,
}

impl Wire {
    /// Replaces the link; the requests in flight on the old connection are forgotten (ids are
    /// never reused, so a late reply to one is unknown).
    fn set(&mut self, link: Link) {
        self.link = link;
        self.conn_epoch = None;
        self.inflight.clear();
    }

    /// Whether requests other than the handshake can be sent now.
    fn handshaken(&self) -> bool {
        self.conn_epoch.is_some()
    }

    /// Sends `message` on the connection; the request's id, `None` when there is no connection.
    /// An encoding error is the caller's (only a request too large for a frame fails to encode).
    fn send(
        &mut self,
        message: Message,
        fds: Vec<SharedFd>,
        pending: Pending,
    ) -> Result<Option<u64>, WireError> {
        let Link::Up(conn) = &self.link else {
            return Ok(None);
        };
        let id = self.next_id + 1;
        let bytes = Frame {
            id,
            epoch: self.epoch,
            message,
        }
        .encode()?;
        self.next_id = id;
        // The writer is gone only when the connection is, which the reader reports.
        let _ = conn.out.send(Outgoing { bytes, fds });
        self.inflight.insert(id, pending);
        Ok(Some(id))
    }
}

/// What woke the driver.
enum Event {
    Command(Command),
    Connected(io::Result<UnixStream>),
    Woke,
    Inbound(Inbound),
}

/// The client's runtime state.
pub(crate) struct Driver {
    holder: HolderId,
    memfd: CounterMemfd,
    /// The end of the lifeline sent at every handshake.
    lifeline: Arc<OwnedFd>,
    connector: Arc<dyn Connector>,
    timer: Arc<dyn Timer>,
    backoff: Backoff,
    commands: Commands,
    stats: watch::Sender<Stats>,
    slots: BTreeMap<PackId, Slot>,
    /// Every region allocated, by pack: a pack whose first open was refused keeps its region for
    /// the next open (the server may have attached it).
    regions: BTreeMap<PackId, PackCounters>,
    wire: Wire,
    releases: Vec<Answer<()>>,
    release_sent: bool,
    end: Option<ClientError>,
}

/// What [`Driver::new`] takes.
pub(crate) struct Parts {
    pub(crate) holder: HolderId,
    pub(crate) memfd: CounterMemfd,
    pub(crate) lifeline: Arc<OwnedFd>,
    pub(crate) connector: Arc<dyn Connector>,
    pub(crate) timer: Arc<dyn Timer>,
    pub(crate) backoff: Backoff,
    pub(crate) commands: Commands,
    pub(crate) stats: watch::Sender<Stats>,
}

impl Driver {
    pub(crate) fn new(p: Parts) -> Self {
        Self {
            holder: p.holder,
            memfd: p.memfd,
            lifeline: p.lifeline,
            connector: p.connector,
            timer: p.timer,
            backoff: p.backoff,
            commands: p.commands,
            stats: p.stats,
            slots: BTreeMap::new(),
            regions: BTreeMap::new(),
            wire: Wire {
                link: Link::Idle,
                conn_epoch: None,
                epoch: Epoch(0),
                inflight: BTreeMap::new(),
                next_id: 0,
            },
            releases: Vec::new(),
            release_sent: false,
            end: None,
        }
    }

    /// Runs until every sender of `commands` is dropped.
    pub(crate) async fn run(mut self, mut commands: mpsc::UnboundedReceiver<Command>) {
        self.wire.set(Link::Connecting(self.connect()));
        loop {
            let event = tokio::select! {
                command = commands.recv() => match command {
                    Some(c) => Event::Command(c),
                    None => return,
                },
                event = next(&mut self.wire.link) => event,
            };
            match event {
                Event::Command(c) => self.command(c),
                Event::Connected(Ok(stream)) => {
                    self.bump(|s| s.connects += 1);
                    self.up(stream);
                }
                Event::Connected(Err(e)) => {
                    tracing::debug!(error = %e, "connecting to the tierstore server failed");
                    self.bump(|s| s.connect_failures += 1);
                    self.sleep();
                }
                Event::Woke => self.wire.set(Link::Connecting(self.connect())),
                Event::Inbound(Inbound::Frame(incoming)) => self.reply(incoming),
                Event::Inbound(Inbound::Failed(e)) => {
                    tracing::warn!(error = %e, "the tierstore connection failed");
                    if !matches!(e, ReadError::Io(_)) {
                        self.bump(|s| s.protocol_errors += 1);
                    }
                    self.lost();
                }
                Event::Inbound(Inbound::Closed) => {
                    tracing::info!("the tierstore server closed the connection");
                    self.lost();
                }
            }
            self.gauge();
        }
    }

    fn bump(&self, f: impl FnOnce(&mut Stats)) {
        self.stats.send_modify(f);
    }

    fn gauge(&self) {
        let n: usize = self.slots.values().map(|s| s.windows.len()).sum();
        self.stats.send_if_modified(|s| {
            let changed = s.windows_outstanding != n as u64;
            s.windows_outstanding = n as u64;
            changed
        });
    }

    fn connect(&self) -> Connecting {
        let connector = self.connector.clone();
        Box::pin(async move { connector.connect().await })
    }

    fn sleep(&mut self) {
        let sleep = self.timer.sleep(self.backoff.next_delay());
        self.wire.set(Link::Sleeping(sleep));
    }

    /// A new connection: starts its tasks and sends the handshake with the counter memfd and the
    /// lifeline.
    fn up(&mut self, stream: UnixStream) {
        let stream = Arc::new(stream);
        let (out, out_rx) = mpsc::unbounded_channel();
        let (frames_tx, frames) = mpsc::unbounded_channel();
        tokio::spawn(read_loop(stream.clone(), frames_tx.clone()));
        tokio::spawn(write_loop(stream.clone(), out_rx, frames_tx));
        self.wire.set(Link::Up(Conn {
            stream,
            out,
            frames,
        }));
        let handshake = Message::Handshake {
            holder: self.holder,
            version: PROTOCOL_VERSION,
        };
        let fds: Vec<SharedFd> = vec![self.memfd.file(), self.lifeline.clone()];
        // A handshake's body is fixed and small: it always encodes.
        let _ = self.wire.send(handshake, fds, Pending::Handshake);
    }

    /// The connection ended: every request in flight on it is forgotten (waits and opens stay,
    /// to be sent again), and the driver sleeps a backoff step before it connects again.
    fn lost(&mut self) {
        self.bump(|s| s.disconnects += 1);
        self.release_sent = false;
        for slot in self.slots.values_mut() {
            slot.handle = None;
            for (_, w) in slot.windows.iter_mut() {
                w.request = None;
            }
        }
        self.sleep();
    }

    // -----------------------------------------------------------------------------------------
    // Commands of the readers.

    fn command(&mut self, command: Command) {
        match command {
            Command::Open { spec, answer } => self.open(spec, answer),
            Command::Wait {
                pack,
                parts,
                job,
                read,
            } => self.wait(&pack, parts, job, &read),
            Command::Ensure {
                pack,
                runs,
                class,
                job,
                rank,
            } => self.ensure(&pack, runs, class, job, rank),
            Command::Release { answer } => self.release(answer),
        }
    }

    fn open(&mut self, spec: OpenSpec, answer: Answer<Arc<Pack>>) {
        if let Some(err) = &self.end {
            let _ = answer.send(Err(err.clone()));
            return;
        }
        if let Some(slot) = self.slots.get_mut(&spec.pack) {
            let shared = match (&slot.broken, &slot.pack) {
                (Some(err), _) => Err(err.clone()),
                // A pack id names one size.
                (None, _) if spec.size != slot.spec.size => Err(ClientError::SizeMismatch {
                    pack: spec.pack,
                    expected: slot.spec.size,
                    got: spec.size,
                }),
                (None, Some(pack)) => Ok(pack.clone()),
                (None, None) => {
                    slot.waiters.push(answer);
                    return;
                }
            };
            if shared.is_ok() {
                self.bump(|s| s.opens_shared += 1);
            }
            let _ = answer.send(shared);
            return;
        }
        let Some(layout) = StatePageLayout::for_pack(spec.size) else {
            let _ = answer.send(Err(ClientError::TooLarge {
                pack: spec.pack,
                size: spec.size,
            }));
            return;
        };
        let counters = match self.regions.get(&spec.pack) {
            Some(c) => c.clone(),
            None => match self.memfd.allocate(layout.group_count()) {
                Ok(c) => {
                    self.regions.insert(spec.pack.clone(), c.clone());
                    c
                }
                Err(e) => {
                    let _ = answer.send(Err(ClientError::Counters {
                        pack: spec.pack,
                        reason: e.to_string(),
                    }));
                    return;
                }
            },
        };
        let id = spec.pack.clone();
        self.slots.insert(
            id.clone(),
            Slot {
                spec,
                layout,
                counters,
                pack: None,
                handle: None,
                waiters: vec![answer],
                windows: Windows::default(),
                broken: None,
            },
        );
        if self.wire.handshaken() {
            self.send_open(&id);
        }
    }

    fn wait(
        &mut self,
        pack: &PackId,
        parts: Vec<(u32, CellRun, CellRun)>,
        job: JobId,
        read: &Arc<ReadWait>,
    ) {
        // A pack exists only once its slot does, and a slot is removed only before its pack
        // exists; the end of the client breaks every slot.
        let slot = self.slots.get_mut(pack);
        let broken = slot
            .as_ref()
            .map_or(Some(ClientError::Stopped), |s| s.broken.clone());
        let (Some(slot), None) = (slot, broken.clone()) else {
            for _ in &parts {
                read.part_done(Err(broken.clone().unwrap_or(ClientError::Stopped)));
            }
            return;
        };
        let mut new = Vec::new();
        let (mut joined, mut queued) = (0, 0);
        for (window, need, cover) in parts {
            let joiner = Joiner {
                read: read.clone(),
                job,
            };
            match slot.windows.add(Part {
                window,
                need,
                cover,
                joiner,
            }) {
                Added::New(_) => new.push(window),
                Added::Joined => joined += 1,
                Added::Queued => queued += 1,
            }
        }
        self.bump(|s| {
            s.wait_parts_joined += joined;
            s.wait_parts_queued += queued;
        });
        for window in new {
            self.send_wait(pack, window, false);
        }
    }

    fn ensure(&mut self, pack: &PackId, runs: Vec<CellRun>, class: Class, job: JobId, rank: Rank) {
        let handle = self.slots.get(pack).and_then(|s| s.handle);
        // Every slot loses its handle when the client ends.
        let sent = match handle {
            Some(handle) => {
                let message = Message::Ensure {
                    pack: handle,
                    runs,
                    class,
                    job,
                    rank,
                };
                // Too many runs for one frame is dropped as a hint is.
                matches!(
                    self.wire.send(message, Vec::new(), Pending::Ensure),
                    Ok(Some(_))
                )
            }
            None => false,
        };
        self.bump(|s| {
            if sent {
                s.ensures_sent += 1;
            } else {
                s.ensures_dropped += 1;
            }
        });
    }

    fn release(&mut self, answer: Answer<()>) {
        match &self.end {
            Some(ClientError::Released) => {
                let _ = answer.send(Ok(()));
            }
            Some(err) => {
                let _ = answer.send(Err(err.clone()));
            }
            None => {
                self.releases.push(answer);
                self.send_release();
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Requests.

    fn send_open(&mut self, pack: &PackId) {
        let Some(slot) = self.slots.get(pack) else {
            return;
        };
        let message = Message::OpenPack {
            tree: slot.spec.tree.clone(),
            pack: slot.spec.index,
            holder: self.holder,
            region: slot.counters.region(),
        };
        match self
            .wire
            .send(message, Vec::new(), Pending::Open(pack.clone()))
        {
            Ok(Some(_)) => self.bump(|s| s.opens_sent += 1),
            Ok(None) => {}
            Err(e) => self.fail_slot(
                pack,
                ClientError::Encode {
                    pack: pack.clone(),
                    reason: e.to_string(),
                },
            ),
        }
    }

    /// Sends the wait of `window` if the pack is open on the connection; `reissue` counts it as
    /// sent again after a reconnect.
    fn send_wait(&mut self, pack: &PackId, window: u32, reissue: bool) {
        let Some(slot) = self.slots.get_mut(pack) else {
            return;
        };
        let (Some(handle), Some(w)) = (slot.handle, slot.windows.get_mut(window)) else {
            return;
        };
        let message = Message::Wait {
            pack: handle,
            range: ByteRange {
                offset: u64::from(w.cells.first) << CELL_SHIFT,
                len: u64::from(w.cells.count) << CELL_SHIFT,
            },
            job: w.first().map_or(JobId::default(), |j| j.job),
        };
        // A wait's body is fixed and small: it always encodes; the pack has a handle only while
        // the connection is up.
        w.request = self
            .wire
            .send(message, Vec::new(), Pending::Wait(pack.clone(), window))
            .ok()
            .flatten();
        self.bump(|s| {
            s.waits_sent += 1;
            s.waits_reissued += u64::from(reissue);
        });
    }

    fn send_release(&mut self) {
        if self.release_sent || !self.wire.handshaken() {
            return;
        }
        let message = Message::Release {
            holder: self.holder,
        };
        self.release_sent = matches!(
            self.wire.send(message, Vec::new(), Pending::Release),
            Ok(Some(_))
        );
    }

    // -----------------------------------------------------------------------------------------
    // Replies.

    fn reply(&mut self, incoming: Incoming) {
        let Incoming {
            frame: Frame { id, epoch, message },
            fds,
        } = incoming;
        let Some(pending) = self.wire.inflight.remove(&id) else {
            tracing::debug!(id, "a reply to no request in flight was dropped");
            self.bump(|s| s.unknown_replies += 1);
            return;
        };
        if self.wire.conn_epoch.is_some_and(|e| e != epoch) {
            // The request stays in flight: its own reply may still come.
            self.wire.inflight.insert(id, pending);
            tracing::debug!(id, ?epoch, "a reply of another epoch was dropped");
            self.bump(|s| s.stale_replies += 1);
            return;
        }
        match pending {
            Pending::Handshake => self.handshake_reply(epoch, message),
            Pending::Open(pack) => self.open_reply(&pack, message, fds),
            Pending::Wait(pack, window) => self.wait_reply(&pack, window, message),
            Pending::Ensure => match message {
                Message::Ensured | Message::Refused { .. } => {}
                other => self.protocol_error(&other),
            },
            Pending::Release => self.release_reply(message),
        }
    }

    fn protocol_error(&mut self, message: &Message) {
        tracing::warn!(?message, "a reply of another kind than its request's");
        self.bump(|s| s.protocol_errors += 1);
        self.lost();
    }

    fn handshake_reply(&mut self, epoch: Epoch, message: Message) {
        match message {
            Message::HandshakeOk => {
                self.wire.conn_epoch = Some(epoch);
                let changed = self.wire.epoch != Epoch(0) && self.wire.epoch != epoch;
                self.wire.epoch = epoch;
                self.backoff.reset();
                self.bump(|s| {
                    s.handshakes += 1;
                    s.epoch_changes += u64::from(changed);
                });
                let packs: Vec<PackId> = self
                    .slots
                    .iter()
                    .filter(|(_, s)| s.broken.is_none())
                    .map(|(id, _)| id.clone())
                    .collect();
                for pack in packs {
                    self.send_open(&pack);
                }
                if !self.releases.is_empty() {
                    self.send_release();
                }
            }
            Message::Refused { code } => {
                tracing::error!(?code, "the tierstore server refused the handshake");
                self.finish(ClientError::HandshakeRefused(code));
            }
            other => self.protocol_error(&other),
        }
    }

    fn open_reply(&mut self, id: &PackId, message: Message, fds: Vec<OwnedFd>) {
        match message {
            Message::PackOpened {
                pack: handle,
                pack_size,
                region,
            } => self.opened(id, handle, pack_size, region, fds),
            Message::Refused {
                code: RefusalCode::UnknownHandle | RefusalCode::NotHandshaken,
            } => {
                // The server no longer knows this connection's holder: handshake again.
                tracing::warn!(pack = ?id, "open-pack refused for the holder; reconnecting");
                self.lost();
            }
            Message::Refused { code } => self.fail_slot(
                id,
                ClientError::OpenRefused {
                    pack: id.clone(),
                    code,
                },
            ),
            other => self.protocol_error(&other),
        }
    }

    fn opened(
        &mut self,
        id: &PackId,
        handle: PackHandle,
        pack_size: u64,
        region: tierstore_fetch_api::page::CounterRegion,
        fds: Vec<OwnedFd>,
    ) {
        let Some(slot) = self.slots.get_mut(id) else {
            return;
        };
        if pack_size != slot.spec.size {
            let err = ClientError::SizeMismatch {
                pack: id.clone(),
                expected: slot.spec.size,
                got: pack_size,
            };
            return self.fail_slot(id, err);
        }
        if region != slot.counters.region() {
            let err = ClientError::RegionMismatch {
                pack: id.clone(),
                asked: slot.counters.region(),
                got: region,
            };
            return self.fail_slot(id, err);
        }
        let view = match PackView::from_fds(pack_size, fds) {
            Ok(view) => view,
            Err(source) => {
                let err = ClientError::BadFds {
                    pack: id.clone(),
                    source,
                };
                return self.fail_slot(id, err);
            }
        };
        slot.handle = Some(handle);
        let reopened = slot.pack.is_some();
        // Counted before any answer is sent: a reader that has its pack sees it counted.
        self.stats.send_modify(|s| {
            s.reopens += u64::from(reopened);
            s.packs_opened += u64::from(!reopened);
        });
        match &slot.pack {
            Some(pack) => pack.swap_view(view),
            None => {
                let pack = Arc::new(Pack::new(
                    id.clone(),
                    slot.layout,
                    slot.counters.clone(),
                    view,
                    self.commands.clone(),
                ));
                slot.pack = Some(pack.clone());
                for answer in slot.waiters.drain(..) {
                    let _ = answer.send(Ok(pack.clone()));
                }
            }
        }
        let windows: Vec<u32> = slot
            .windows
            .iter_mut()
            .filter(|(_, w)| w.request.is_none())
            .map(|(w, _)| w)
            .collect();
        for window in windows {
            self.send_wait(id, window, true);
        }
    }

    /// The pack cannot be served: its opens and waits end with `err`. A pack never opened is
    /// forgotten (a later open tries again, with the same region); an open pack is broken for the
    /// client's life.
    fn fail_slot(&mut self, id: &PackId, err: ClientError) {
        tracing::warn!(pack = ?id, error = %err, "a pack cannot be served");
        let Some(slot) = self.slots.get_mut(id) else {
            return;
        };
        // The pack is broken before any wait or read of it is answered, so that a caller woken
        // by the answer finds it broken.
        if let Some(pack) = &slot.pack {
            pack.break_with(err.clone());
        }
        for answer in slot.waiters.drain(..) {
            let _ = answer.send(Err(err.clone()));
        }
        for joiner in slot.windows.drain() {
            joiner.read.part_done(Err(err.clone()));
        }
        if slot.pack.is_some() {
            slot.handle = None;
            slot.broken = Some(err);
        } else {
            self.slots.remove(id);
        }
    }

    fn wait_reply(&mut self, id: &PackId, window: u32, message: Message) {
        let outcome = match message {
            Message::Waited(outcome) => outcome,
            Message::Refused { code } => {
                // A handle the server does not know: open the packs again on a new handshake.
                tracing::warn!(pack = ?id, ?code, "wait refused; reconnecting");
                return self.lost();
            }
            other => return self.protocol_error(&other),
        };
        // A broken pack's windows were drained: nothing waits on this one.
        let Some((slot, ended)) = self.slots.get_mut(id).and_then(|slot| {
            let ended = slot.windows.end(window, outcome)?;
            Some((slot, ended))
        }) else {
            return;
        };
        for (joiner, outcome) in ended.done {
            joiner.read.part_done(Ok(outcome));
        }
        let view = slot.pack.as_ref().map(|p| p.view());
        let mut local = 0;
        let next: Vec<Part<Joiner>> = ended
            .next
            .into_iter()
            .filter(|p| {
                let ready = view.as_ref().is_some_and(|v| v.ready(p.need));
                if ready {
                    p.joiner.read.part_done(Ok(WaitOutcome::Ready));
                    local += 1;
                }
                !ready
            })
            .collect();
        let restarted = slot.windows.restart(next);
        // Counted once every answer is sent: a test that sees the count finds its ticket answered.
        self.bump(|s| {
            s.waits_answered += 1;
            s.waits_ready_local += local;
        });
        if let Some((window, _)) = restarted {
            self.send_wait(id, window, false);
        }
    }

    fn release_reply(&mut self, message: Message) {
        self.release_sent = false;
        match message {
            Message::Released => self.finish(ClientError::Released),
            Message::Refused { code } => {
                for answer in self.releases.drain(..) {
                    let _ = answer.send(Err(ClientError::ReleaseRefused(code)));
                }
            }
            other => self.protocol_error(&other),
        }
    }

    /// The client ends with `err`: every request is answered with it, every pack is broken, and
    /// no connection is made again. Releases are answered `Ok` when the end is the release.
    fn finish(&mut self, err: ClientError) {
        self.end = Some(err.clone());
        self.wire.set(Link::Idle);
        for slot in self.slots.values_mut() {
            // Broken before its waits and reads are answered, as in `fail_slot`.
            if let Some(pack) = &slot.pack {
                pack.break_with(err.clone());
            }
            for answer in slot.waiters.drain(..) {
                let _ = answer.send(Err(err.clone()));
            }
            for joiner in slot.windows.drain() {
                joiner.read.part_done(Err(err.clone()));
            }
            slot.handle = None;
            slot.broken = Some(err.clone());
        }
        let released = err == ClientError::Released;
        for answer in self.releases.drain(..) {
            let _ = answer.send(if released { Ok(()) } else { Err(err.clone()) });
        }
    }
}

/// The next event of the link (cancel-safe: the futures live in the link).
async fn next(link: &mut Link) -> Event {
    match link {
        Link::Connecting(f) => Event::Connected(f.await),
        Link::Sleeping(s) => {
            s.await;
            Event::Woke
        }
        // The writer task holds a sender for the connection's life, so `recv` ends only with an
        // item.
        Link::Up(conn) => Event::Inbound(conn.frames.recv().await.unwrap_or(Inbound::Closed)),
        Link::Idle => std::future::pending().await,
    }
}

/// Reads the connection's frames until it ends; its end is the last item.
async fn read_loop(stream: Arc<UnixStream>, frames: mpsc::UnboundedSender<Inbound>) {
    loop {
        let item = match read_frame(&stream).await {
            Ok(Some(incoming)) => Inbound::Frame(incoming),
            Ok(None) => Inbound::Closed,
            Err(e) => Inbound::Failed(e),
        };
        let last = !matches!(item, Inbound::Frame(_));
        // The driver may have dropped the connection: nobody reads the report then.
        let _ = frames.send(item);
        if last {
            return;
        }
    }
}

/// Writes the connection's frames in order until its queue closes; a failed write is reported as
/// the connection's end.
async fn write_loop(
    stream: Arc<UnixStream>,
    mut out: mpsc::UnboundedReceiver<Outgoing>,
    frames: mpsc::UnboundedSender<Inbound>,
) {
    while let Some(o) = out.recv().await {
        let fds: Vec<BorrowedFd<'_>> = o.fds.iter().map(|f| f.as_fd()).collect();
        if let Err(e) = send_frame(&stream, &o.bytes, &fds).await {
            let _ = frames.send(Inbound::Failed(ReadError::Io(e)));
            return;
        }
    }
}

#[cfg(test)]
#[path = "tests/driver_tests.rs"]
mod tests;
