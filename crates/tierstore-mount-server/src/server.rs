//! The server: connections, the handshake, and a task per request.

use std::num::NonZeroUsize;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::Arc;

use tierstore_fetch_api::engine::{Engine, PackDigests, PackSpec, WaitOutcome};
use tierstore_fetch_api::entry::{Entry, EntryError, EntrySpec, EntryStore};
use tierstore_fetch_api::ids::{ByteRange, CellRun, Class, HolderId, JobId, PackId, Rank, TreeRef};
use tierstore_fetch_api::page::{CELL_SHIFT, CounterRegion, StatePageLayout};
use tierstore_fetch_api::wire::{Epoch, Frame, Message, PROTOCOL_VERSION, PackHandle, RefusalCode};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore, mpsc, watch};
use tokio::task::JoinSet;

use crate::MountServerError;
use crate::framing::{Incoming, ReadError, read_frame, send_frame};
use crate::lifeline;
use crate::registry::{
    BadDescriptors, CounterMapper, Holder, OpenPack, Registry, check_descriptors,
};
use crate::stats::Stats;
use crate::trees::{TreeError, Trees};

/// What the server drives.
#[derive(Clone)]
pub struct Parts {
    /// The node cache's entries (T1 on Linux).
    pub store: Arc<dyn EntryStore>,
    /// The fetch engine (T3).
    pub engine: Arc<dyn Engine>,
    /// The tree lists of the node.
    pub trees: Arc<dyn Trees>,
    /// Maps a holder's counter memfd (T1).
    pub counters: Arc<dyn CounterMapper>,
}

/// The server's settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// The most requests one connection runs at once; at this many the server stops reading the
    /// connection until one is answered. One connection is one mount, so the bound is per mount
    /// and the same on every node of both deployment profiles of section 21: a mount's waits are
    /// coalesced per 8 cells (5.4), so the parked reads of its kernel queues need far fewer
    /// waits than [`Config::DEFAULT_MAX_IN_FLIGHT`].
    pub max_in_flight: NonZeroUsize,
}

impl Config {
    /// The default of [`Config::max_in_flight`]: 16384, about twice the storm of 5.2 item 4
    /// (`depth x queues x 1.5` parked reads: 64 x 88 x 1.5 = 8448) even before coalescing.
    pub const DEFAULT_MAX_IN_FLIGHT: NonZeroUsize = match NonZeroUsize::new(16384) {
        Some(n) => n,
        None => NonZeroUsize::MIN,
    };
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_in_flight: Self::DEFAULT_MAX_IN_FLIGHT,
        }
    }
}

/// The server side of the mount protocol. Clones share one server.
#[derive(Clone)]
pub struct MountServer {
    shared: Arc<Shared>,
}

struct Shared {
    parts: Parts,
    config: Config,
    epoch: Epoch,
    registry: Registry,
    /// Held while a holder is registered or ended, so a handshake and the end of the same holder
    /// id never interleave (the end's `drop_holder` must not drop a registration that came after
    /// it). Held across the mapping of a counter memfd and `drop_holder`, which touch no disk.
    holders: AsyncMutex<()>,
    stats: watch::Sender<Stats>,
}

/// A reply on its way to the writer.
struct Outgoing {
    bytes: Vec<u8>,
    fds: Vec<OwnedFd>,
    refused: bool,
    /// Released when the reply is written or dropped.
    _permit: Option<OwnedSemaphorePermit>,
}

/// How a holder ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    HungUp,
    Released,
}

impl MountServer {
    /// A server of `parts` under `epoch` (from [`crate::random_epoch`]; a test passes its own).
    #[must_use]
    pub fn new(parts: Parts, epoch: Epoch, config: Config) -> Self {
        Self {
            shared: Arc::new(Shared {
                parts,
                config,
                epoch,
                registry: Registry::default(),
                holders: AsyncMutex::new(()),
                stats: watch::Sender::new(Stats::default()),
            }),
        }
    }

    /// The epoch every reply carries.
    #[must_use]
    pub fn epoch(&self) -> Epoch {
        self.shared.epoch
    }

    /// The counts now.
    #[must_use]
    pub fn stats(&self) -> Stats {
        *self.shared.stats.borrow()
    }

    /// Waits until the counts satisfy `done`, and returns them.
    pub async fn wait_for(&self, mut done: impl FnMut(&Stats) -> bool) -> Stats {
        let mut rx = self.shared.stats.subscribe();
        // The sender lives in `self`, so the channel never closes while this waits.
        let stats = rx.wait_for(|s| done(s)).await.map(|s| *s);
        stats.unwrap_or_default()
    }

    /// Accepts mount connections on `listener` and serves each in its own task, until accepting
    /// fails.
    ///
    /// # Errors
    /// [`MountServerError::Accept`] when the listening socket fails.
    pub async fn serve(&self, listener: UnixListener) -> Result<(), MountServerError> {
        loop {
            let (stream, _) = listener.accept().await.map_err(MountServerError::Accept)?;
            let server = self.clone();
            tokio::spawn(async move { server.serve_connection(stream).await });
        }
    }

    /// Serves one mount connection until the mount closes it or sends what is not a frame. The
    /// requests it has in flight then end with it; the holder it served does not.
    pub async fn serve_connection(&self, stream: UnixStream) {
        let shared = &self.shared;
        shared.bump(|s| s.connections += 1);
        let stream = Arc::new(stream);
        let (tx, rx) = mpsc::unbounded_channel();
        let writer = tokio::spawn(write_loop(shared.clone(), stream.clone(), rx));
        let permits = Arc::new(Semaphore::new(shared.config.max_in_flight.get()));
        let mut tasks = JoinSet::new();
        let mut bound: Option<Arc<Holder>> = None;
        loop {
            while tasks.try_join_next().is_some() {}
            // The semaphore is never closed, so this always holds a permit.
            let permit = permits.clone().acquire_owned().await.ok();
            let Incoming { frame, fds } = match read_frame(&stream).await {
                Ok(Some(incoming)) => incoming,
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!(error = %e, "mount connection ended");
                    if !matches!(e, ReadError::Io(_)) {
                        shared.bump(|s| s.protocol_errors += 1);
                    }
                    break;
                }
            };
            let Frame { id, epoch, message } = frame;
            // The permit goes with the reply: it is released when the reply is written, so a
            // mount that stops reading stops the reading of its requests.
            let reply = Replier {
                tx: tx.clone(),
                id,
                epoch: shared.epoch,
                permit,
            };
            match message {
                Message::Handshake { holder, version } => {
                    let (message, holder) =
                        shared.handshake(bound.as_ref(), holder, version, fds).await;
                    // A refused handshake leaves the connection as it was.
                    bound = holder.or(bound);
                    reply.send(message, Vec::new());
                }
                Message::Release { holder } => {
                    let message;
                    (message, bound) = shared.release(bound, epoch, holder).await;
                    reply.send(message, Vec::new());
                }
                Message::OpenPack {
                    tree,
                    pack,
                    holder,
                    region,
                } => {
                    let (shared, bound) = (shared.clone(), bound.clone());
                    tasks.spawn(async move {
                        let (message, fds) = shared
                            .open_pack(bound, epoch, tree, pack, holder, region)
                            .await;
                        reply.send(message, fds);
                    });
                }
                Message::Wait { pack, range, job } => {
                    let (shared, bound) = (shared.clone(), bound.clone());
                    tasks.spawn(async move {
                        let message = shared.wait(bound, epoch, pack, range, job).await;
                        reply.send(message, Vec::new());
                    });
                }
                Message::Ensure {
                    pack,
                    runs,
                    class,
                    job,
                    rank,
                } => {
                    let (shared, bound) = (shared.clone(), bound.clone());
                    tasks.spawn(async move {
                        let message = shared.ensure(bound, epoch, pack, &runs, class, job, rank);
                        reply.send(message, Vec::new());
                    });
                }
                server_kind => {
                    tracing::warn!(?server_kind, "a mount sent a server's message");
                    shared.bump(|s| s.protocol_errors += 1);
                    break;
                }
            }
        }
        tasks.shutdown().await;
        // The mount closed the connection or broke the protocol: replies not yet written are
        // dropped with it (a mount re-issues its requests after it reconnects), and a writer
        // blocked on a peer that stopped reading does not hold the connection open.
        writer.abort();
        let _ = writer.await;
        drop(tx);
        shared.bump(|s| s.connections_closed += 1);
    }
}

/// Answers one request.
struct Replier {
    tx: mpsc::UnboundedSender<Outgoing>,
    id: u64,
    epoch: Epoch,
    /// The request's place under [`Config::max_in_flight`], held until its reply is written.
    permit: Option<OwnedSemaphorePermit>,
}

impl Replier {
    fn send(self, message: Message, fds: Vec<OwnedFd>) {
        let refused = matches!(message, Message::Refused { .. });
        let frame = Frame {
            id: self.id,
            epoch: self.epoch,
            message,
        };
        // A reply's body is at most 25 bytes, far under the limit, so encoding cannot fail.
        let bytes = frame.encode().unwrap_or_default();
        // The writer is gone only when the peer is, and then nobody waits for the reply.
        let _ = self.tx.send(Outgoing {
            bytes,
            fds,
            refused,
            _permit: self.permit,
        });
    }
}

/// Writes the replies of one connection in the order they complete.
async fn write_loop(
    shared: Arc<Shared>,
    stream: Arc<UnixStream>,
    mut rx: mpsc::UnboundedReceiver<Outgoing>,
) {
    while let Some(out) = rx.recv().await {
        let fds: Vec<BorrowedFd<'_>> = out.fds.iter().map(AsFd::as_fd).collect();
        if let Err(e) = send_frame(&stream, &out.bytes, &fds).await {
            tracing::debug!(error = %e, "a mount connection stopped taking replies");
            shared.bump(|s| s.replies_lost += 1);
            break;
        }
        shared.bump(|s| {
            s.replies += 1;
            s.refusals += u64::from(out.refused);
        });
    }
}

const fn refused(code: RefusalCode) -> Message {
    Message::Refused { code }
}

/// Runs `f` on a blocking thread. A panic of `f` is this task's panic.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(panicked)
}

/// Resumes the panic of a blocking task (or, when the runtime cancelled it, panics).
fn panicked<T>(e: tokio::task::JoinError) -> T {
    std::panic::resume_unwind(e.into_panic())
}

/// The part of `range` inside a pack of `size` bytes: a mount's read past the end is short.
fn clamp(range: ByteRange, size: u64) -> ByteRange {
    let offset = range.offset.min(size);
    let end = range.end().unwrap_or(u64::MAX).min(size);
    ByteRange {
        offset,
        len: end - offset,
    }
}

/// The parts of `runs` inside a pack of `cells` cells, empty parts dropped.
fn clip(runs: &[CellRun], cells: u32) -> Vec<CellRun> {
    runs.iter()
        .filter(|r| r.first < cells && r.count > 0)
        .map(|r| CellRun {
            first: r.first,
            count: r.count.min(cells - r.first),
        })
        .collect()
}

/// The data fd and the state memfd of `entry`, duplicated so the reply owns them.
fn entry_fds(entry: &dyn Entry) -> Option<Vec<OwnedFd>> {
    let fds = entry.fds()?;
    let data = fds.data.try_clone_to_owned().ok()?;
    let state = fds.state.try_clone_to_owned().ok()?;
    Some(vec![data, state])
}

impl Shared {
    fn bump(&self, f: impl FnOnce(&mut Stats)) {
        self.stats.send_modify(f);
    }

    /// The handshake: checks the version and the descriptors, then registers the holder, or finds
    /// it registered with the same descriptors (a reconnect). The reply, and the holder the
    /// connection now serves.
    async fn handshake(
        self: &Arc<Self>,
        bound: Option<&Arc<Holder>>,
        id: HolderId,
        version: u8,
        fds: Vec<OwnedFd>,
    ) -> (Message, Option<Arc<Holder>>) {
        if version != PROTOCOL_VERSION {
            return (refused(RefusalCode::Version), None);
        }
        if bound.is_some_and(|b| b.id != id && !b.is_ended()) {
            tracing::warn!(holder = ?id, "a handshake for another holder on a bound connection");
            return (refused(RefusalCode::NotHandshaken), None);
        }
        // The frame's kind carries exactly two descriptors: the framing has checked the count.
        let mut fds = fds.into_iter();
        let checked = (fds.next().zip(fds.next()).ok_or(BadDescriptors::Missing))
            .and_then(|(c, l)| check_descriptors(&c, &l).map(|ids| (c, l, ids)));
        let (counters, lifeline, (counters_inode, lifeline_inode)) = match checked {
            Ok(checked) => checked,
            Err(e) => {
                tracing::warn!(holder = ?id, error = %e, "handshake refused");
                return (refused(RefusalCode::NotHandshaken), None);
            }
        };
        let _registering = self.holders.lock().await;
        if let Some(holder) = self.registry.live(id) {
            // A reconnect sends the same two descriptors again; the copies are closed here.
            if !holder.same_descriptors(counters_inode, lifeline_inode) {
                tracing::warn!(holder = ?id, "a handshake for a live holder with other descriptors");
                return (refused(RefusalCode::NotHandshaken), None);
            }
            self.bump(|s| s.handshakes += 1);
            return (Message::HandshakeOk, Some(holder));
        }
        // The watch starts first: a holder is registered only once its lifeline is watched.
        let watch = match lifeline::watch(lifeline, format!("lifeline-{}", id.0)) {
            Ok(watch) => watch,
            Err(e) => {
                tracing::error!(holder = ?id, error = %e, "the lifeline cannot be watched");
                return (refused(RefusalCode::NotHandshaken), None);
            }
        };
        let (mapper, store) = (self.parts.counters.clone(), self.parts.store.clone());
        let registered = blocking(move || {
            let mapped = mapper.map(id, counters)?;
            store.register_holder(id, mapped)
        })
        .await;
        if let Err(e) = registered {
            // Dropping the watch's `stop` ends its thread, which closes the lifeline.
            tracing::warn!(holder = ?id, error = %e, "the holder's counters could not be registered");
            return (refused(RefusalCode::NotHandshaken), None);
        }
        let holder = Arc::new(Holder::new(id, counters_inode, lifeline_inode));
        self.registry.insert(holder.clone());
        let (shared, watched) = (self.clone(), holder.clone());
        let lifeline::Watch { hangup, stop } = watch;
        tokio::spawn(async move {
            // The watch runs as long as this task holds `stop`: until the hangup.
            let _stop = stop;
            match hangup.await {
                Ok(Ok(())) => {
                    shared.end(&watched, End::HungUp).await;
                    // Counted once the end is done, whether it ended the holder or found it
                    // released.
                    shared.bump(|s| s.lifeline_hangups += 1);
                }
                // The holder may be alive: it is never ended but by a hangup or its release.
                failed => {
                    tracing::error!(holder = ?watched.id, ?failed, "watching a lifeline failed")
                }
            }
        });
        self.bump(|s| {
            s.holders_registered += 1;
            s.handshakes += 1;
        });
        (Message::HandshakeOk, Some(holder))
    }

    /// `release`: ends the connection's holder as its lifeline's hangup would. The reply, and the
    /// holder the connection serves after it (none once released).
    async fn release(
        self: &Arc<Self>,
        bound: Option<Arc<Holder>>,
        epoch: Epoch,
        id: HolderId,
    ) -> (Message, Option<Arc<Holder>>) {
        match bound {
            Some(holder) if holder.id == id && epoch == self.epoch && !holder.is_ended() => {
                self.end(&holder, End::Released).await;
                (Message::Released, None)
            }
            other => (refused(RefusalCode::NotHandshaken), other),
        }
    }

    /// Ends `holder`, once: forgets its handles and drops its counters, regions and holds in the
    /// entry store. A released holder's lifeline stays watched until it hangs up, which then
    /// finds it ended and does nothing.
    async fn end(&self, holder: &Arc<Holder>, how: End) {
        let _ending = self.holders.lock().await;
        if !self.registry.end(holder) {
            return;
        }
        let (store, id) = (self.parts.store.clone(), holder.id);
        let released = blocking(move || store.drop_holder(id)).await;
        tracing::info!(holder = ?id, ?how, entries = released, "holder ended");
        self.bump(|s| match how {
            End::HungUp => s.holders_hung_up += 1,
            End::Released => s.holders_released += 1,
        });
    }

    /// The connection's holder, if `epoch` is this server's and the holder is `id` and live.
    fn holder_for(
        &self,
        bound: Option<Arc<Holder>>,
        epoch: Epoch,
        id: Option<HolderId>,
    ) -> Option<Arc<Holder>> {
        bound.filter(|h| epoch == self.epoch && id.is_none_or(|id| id == h.id) && !h.is_ended())
    }

    /// `open-pack`: the pack's entry, the holder's counter region attached to it (the mapping
    /// covers it before this returns), and its two descriptors.
    async fn open_pack(
        self: Arc<Self>,
        bound: Option<Arc<Holder>>,
        epoch: Epoch,
        tree: TreeRef,
        pack: u32,
        id: HolderId,
        region: CounterRegion,
    ) -> (Message, Vec<OwnedFd>) {
        let Some(holder) = self.holder_for(bound, epoch, Some(id)) else {
            return (refused(RefusalCode::NotHandshaken), Vec::new());
        };
        let key = (tree, pack);
        if let Some((handle, open)) = self.registry.known(&holder, &key) {
            return opened(handle, &open, region);
        }
        let list = match self.parts.trees.list(&key.0).await {
            Ok(list) => list,
            Err(TreeError::Unsupported(_)) => {
                return (refused(RefusalCode::ListVersionUnsupported), Vec::new());
            }
            Err(e @ TreeError::Unknown(_)) => {
                tracing::warn!(error = %e, "open-pack refused");
                return (refused(RefusalCode::UnknownPack), Vec::new());
            }
        };
        let (Some(record), Some(root)) = (list.pack_record(pack), list.pack_root(pack)) else {
            return (refused(RefusalCode::UnknownPack), Vec::new());
        };
        if record.cell_shift != CELL_SHIFT {
            tracing::warn!(tree = ?key.0, pack, shift = record.cell_shift, "a pack of another cell size");
            return (refused(RefusalCode::UnknownPack), Vec::new());
        }
        let spec = PackSpec {
            id: PackId {
                key: record.key,
                root,
            },
            size: record.pack_size,
            digests: Some(PackDigests { list, pack }),
        };
        let (store, entry_spec) = (
            self.parts.store.clone(),
            EntrySpec {
                pack: spec.id.clone(),
                size: spec.size,
            },
        );
        let attached = blocking(move || {
            let entry = store.open(&entry_spec)?;
            // An entry with no descriptors (the in-process store) cannot be served to a mount;
            // it is refused before the holder takes a hold.
            let Some(fds) = entry_fds(entry.as_ref()) else {
                return Ok(None);
            };
            store.attach(&entry_spec.pack, id, region)?;
            Ok(Some((entry, fds)))
        })
        .await;
        let (entry, fds) = match attached {
            Ok(Some(attached)) => attached,
            Ok(None) => {
                tracing::warn!(pack = ?spec.id, "the entry has no descriptors to pass");
                return (refused(RefusalCode::UnknownPack), Vec::new());
            }
            Err(e) => {
                tracing::warn!(pack = ?spec.id, error = %e, "open-pack refused");
                return (refused(attach_refusal(&e)), Vec::new());
            }
        };
        let open = OpenPack {
            holder: id,
            spec,
            entry,
            region,
        };
        match self.registry.add(&holder, key, open) {
            Some((handle, open)) => (
                Message::PackOpened {
                    pack: handle,
                    pack_size: open.spec.size,
                    region: open.region,
                },
                fds,
            ),
            None => (refused(RefusalCode::NotHandshaken), Vec::new()),
        }
    }

    /// The open pack a `wait` or `ensure` names, if it is this connection's holder's under this
    /// server's epoch.
    fn handle(
        &self,
        bound: Option<Arc<Holder>>,
        epoch: Epoch,
        handle: PackHandle,
    ) -> Option<Arc<OpenPack>> {
        let holder = self.holder_for(bound, epoch, None)?;
        self.registry.open(holder.id, handle)
    }

    /// `wait`: until the range is ready, a cell of it fails, or the fill is lost.
    async fn wait(
        self: Arc<Self>,
        bound: Option<Arc<Holder>>,
        epoch: Epoch,
        handle: PackHandle,
        range: ByteRange,
        job: JobId,
    ) -> Message {
        let Some(open) = self.handle(bound, epoch, handle) else {
            return refused(RefusalCode::UnknownHandle);
        };
        let range = clamp(range, open.spec.size);
        if range.len == 0 {
            return Message::Waited(WaitOutcome::Ready);
        }
        Message::Waited(self.parts.engine.wait(&open.spec, range, job).await)
    }

    /// `ensure`: hands the runs inside the pack to the engine, which returns at once.
    #[expect(
        clippy::too_many_arguments,
        reason = "the fields of the `ensure` message, and the connection's holder and epoch"
    )]
    fn ensure(
        &self,
        bound: Option<Arc<Holder>>,
        epoch: Epoch,
        handle: PackHandle,
        runs: &[CellRun],
        class: Class,
        job: JobId,
        rank: Rank,
    ) -> Message {
        let Some(open) = self.handle(bound, epoch, handle) else {
            return refused(RefusalCode::UnknownHandle);
        };
        let cells = StatePageLayout::for_pack(open.spec.size).map_or(0, |l| l.cell_count);
        let runs = clip(runs, cells);
        if !runs.is_empty() {
            self.parts
                .engine
                .ensure(&open.spec, &runs, class, job, rank);
        }
        Message::Ensured
    }
}

/// The `PackOpened` of a pack the holder already has open, if it asks for the region it has.
fn opened(handle: PackHandle, open: &OpenPack, region: CounterRegion) -> (Message, Vec<OwnedFd>) {
    if open.region != region {
        return (refused(RefusalCode::BadRegion), Vec::new());
    }
    match entry_fds(open.entry.as_ref()) {
        Some(fds) => (
            Message::PackOpened {
                pack: handle,
                pack_size: open.spec.size,
                region: open.region,
            },
            fds,
        ),
        None => (refused(RefusalCode::UnknownPack), Vec::new()),
    }
}

/// The refusal of an `open-pack` whose entry could not be opened or attached.
fn attach_refusal(e: &EntryError) -> RefusalCode {
    match e {
        // A region the store refuses, or one past what the server can map of the memfd.
        EntryError::BadRegion { .. } | EntryError::CountersMap { .. } => RefusalCode::BadRegion,
        // The holder ended while the pack was opened.
        EntryError::UnknownHolder { .. } => RefusalCode::NotHandshaken,
        // The pack cannot be cached here (too large, another size under the same id, or I/O).
        _ => RefusalCode::UnknownPack,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_is_clamped_to_the_pack() {
        let r = |offset, len| ByteRange { offset, len };
        assert_eq!(clamp(r(0, 10), 100), r(0, 10));
        assert_eq!(clamp(r(90, 20), 100), r(90, 10));
        assert_eq!(clamp(r(100, 5), 100), r(100, 0));
        assert_eq!(clamp(r(200, 5), 100), r(100, 0));
        assert_eq!(clamp(r(u64::MAX, u64::MAX), 100), r(100, 0));
    }

    #[test]
    fn runs_are_clipped_to_the_pack_and_empty_ones_dropped() {
        let r = |first, count| CellRun { first, count };
        assert_eq!(
            clip(
                &[r(0, 2), r(3, 0), r(4, 10), r(9, 1), r(u32::MAX, u32::MAX)],
                6
            ),
            vec![r(0, 2), r(4, 2)]
        );
    }

    #[test]
    fn each_entry_error_is_refused_with_its_code() {
        let pack = PackId {
            key: tierstore_fetch_api::ids::PackKey("k".into()),
            root: tierstore_fetch_api::ids::PackRoot::Digest([0; 32]),
        };
        let region = CounterRegion {
            offset: 0,
            groups: 1,
        };
        let io = || std::io::Error::from(std::io::ErrorKind::Other);
        let cases = [
            (
                EntryError::BadRegion {
                    pack: pack.clone(),
                    region,
                },
                RefusalCode::BadRegion,
            ),
            (
                EntryError::CountersMap {
                    len: 8,
                    source: io(),
                },
                RefusalCode::BadRegion,
            ),
            (
                EntryError::UnknownHolder {
                    holder: HolderId(1),
                },
                RefusalCode::NotHandshaken,
            ),
            (
                EntryError::TooLarge {
                    pack: pack.clone(),
                    size: 1,
                },
                RefusalCode::UnknownPack,
            ),
            (
                EntryError::Io {
                    pack,
                    op: "open",
                    source: io(),
                },
                RefusalCode::UnknownPack,
            ),
        ];
        for (e, code) in cases {
            assert_eq!(attach_refusal(&e), code, "{e}");
        }
    }

    #[test]
    fn the_default_bound_is_the_documented_one() {
        assert_eq!(Config::default().max_in_flight.get(), 16384);
    }
}
