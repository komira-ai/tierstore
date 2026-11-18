//! What the tests of the mount server share: a mount (its counter memfd, its lifeline and a client
//! connection that speaks the frozen frames), an entry store whose entries have real descriptors,
//! a counter mapper, tree lists, and a server wired to them with the fake engine.
//!
//! Nothing here reads a clock or sleeps: a test waits for a reply, or for a count of the server
//! ([`MountServer::wait_for`]), which are its progress events.

use std::collections::BTreeMap;
use std::io::{self, IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals, ftruncate, memfd_create};
use rustix::io::pwrite;
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, SocketFlags, SocketType, recvmsg, sendmsg, socketpair,
};
use tierstore_fetch_api::entry::{
    CellCounts, ClaimedCell, CommitReport, Entry, EntryError, EntryFds, EntryInfo, EntrySpec,
    EntryStore, EvictStep, HolderCounters, PunchReport, Settle, StateView,
};
use tierstore_fetch_api::fake::{FakeEngine, MemCounters, MemEntryStore};
use tierstore_fetch_api::ids::{CellRun, HolderId, PackId, PackKey, PackRoot, TreeRef};
use tierstore_fetch_api::list::{CellDigest, ListL2, NodeIndex, PackRecord};
use tierstore_fetch_api::page::{CounterRegion, MOUNT_SEALS, StatePageLayout};
use tierstore_fetch_api::wire::{Epoch, FRAME_HEADER_LEN, Frame, Message};
use tierstore_mount_server::{
    COUNTER_MEMFD_SEALS, Config, CounterMapper, MountServer, Parts, TreeError, Trees,
};
use tokio::io::Interest;
use tokio::net::UnixStream;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// The epoch of the server [`Fixture::new`] makes.
pub const EPOCH: Epoch = Epoch(0x00e9_0c11);

/// The tree every fixture serves, with the packs of [`PACKS`].
pub fn tree() -> TreeRef {
    TreeRef("tree-a".into())
}

/// The packs of [`tree`]: index, key and size. Pack 1 is 3 MiB and 5 bytes: four cells, the last
/// short; pack 2 is 40 cells (three groups).
pub const PACKS: [(&str, u64); 3] = [
    ("pack-0", 1 << 20),
    ("pack-1", (3 << 20) + 5),
    ("pack-2", 40 << 20),
];

/// The `PackId` of pack `i` of [`tree`].
pub fn pack_id(i: usize) -> PackId {
    PackId {
        key: PackKey(PACKS[i].0.into()),
        // The index is under 3.
        root: PackRoot::Digest([i as u8 + 1; 32]),
    }
}

/// The counter region of pack `i` at `offset`, with the pack's group count.
pub fn region(i: usize, offset: u64) -> CounterRegion {
    CounterRegion {
        offset,
        groups: layout(PACKS[i].1).group_count(),
    }
}

/// The state page layout of a pack of `size` bytes.
pub fn layout(size: u64) -> StatePageLayout {
    StatePageLayout::for_pack(size).unwrap()
}

/// The byte at `offset` of every pack's data file.
pub fn pack_byte(offset: u64) -> u8 {
    (offset % 251) as u8
}

// ---------------------------------------------------------------------------------------------
// Tree lists.

/// A list of packs whose records are given; each pack's root is [`pack_id`]'s.
#[derive(Debug, Default)]
pub struct FakeList {
    pub records: Vec<(PackRecord, PackRoot)>,
}

impl FakeList {
    /// The list of [`PACKS`].
    pub fn packs() -> Self {
        Self {
            records: PACKS
                .iter()
                .enumerate()
                .map(|(i, (key, size))| {
                    (
                        PackRecord {
                            key: PackKey((*key).into()),
                            pack_size: *size,
                            cell_first: 0,
                            cell_count: layout(*size).cell_count,
                            cell_shift: 20,
                            pack_flags: 0,
                            upload_part_size: 0,
                        },
                        pack_id(i).root,
                    )
                })
                .collect(),
        }
    }
}

impl ListL2 for FakeList {
    fn pack_count(&self) -> u32 {
        self.records.len() as u32
    }
    fn pack_record(&self, pack: u32) -> Option<PackRecord> {
        self.records.get(pack as usize).map(|(r, _)| r.clone())
    }
    fn cell_digest(&self, _pack: u32, _cell: u32) -> Option<CellDigest> {
        None
    }
    fn pack_root(&self, pack: u32) -> Option<PackRoot> {
        self.records.get(pack as usize).map(|(_, root)| *root)
    }
    fn dir_lookup(&self, _dir: NodeIndex, _name: &[u8]) -> Option<NodeIndex> {
        None
    }
}

/// What [`FakeTrees::list`] answers for a tree.
pub type ListAnswer = Result<Arc<dyn ListL2>, TreeError>;

/// Tree lists by reference, with a scripted error per tree and an optional gate that holds every
/// `list` until a number of them are waiting.
#[derive(Default)]
pub struct FakeTrees {
    lists: Mutex<BTreeMap<TreeRef, ListAnswer>>,
    gate: Mutex<Option<usize>>,
    waiting: AtomicUsize,
    opened: Notify,
    calls: AtomicUsize,
}

impl FakeTrees {
    /// [`tree`] with the list of [`PACKS`].
    pub fn new() -> Self {
        let trees = Self::default();
        trees.set(tree(), Ok(Arc::new(FakeList::packs())));
        trees
    }

    /// What `list` answers for `tree`.
    pub fn set(&self, tree: TreeRef, answer: ListAnswer) {
        self.lists.lock().unwrap().insert(tree, answer);
    }

    /// Every later `list` waits until `n` calls are waiting, then all go on.
    pub fn gate(&self, n: usize) {
        *self.gate.lock().unwrap() = Some(n);
    }

    /// The calls of `list` so far.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Trees for FakeTrees {
    async fn list(&self, tree: &TreeRef) -> Result<Arc<dyn ListL2>, TreeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let gate = *self.gate.lock().unwrap();
        if let Some(n) = gate {
            let opened = self.opened.notified();
            if self.waiting.fetch_add(1, Ordering::SeqCst) + 1 >= n {
                self.opened.notify_waiters();
            } else {
                opened.await;
            }
        }
        self.lists
            .lock()
            .unwrap()
            .get(tree)
            .cloned()
            .unwrap_or_else(|| Err(TreeError::Unknown(tree.clone())))
    }
}

// ---------------------------------------------------------------------------------------------
// The counter mapper.

/// Maps a counter memfd to [`MemCounters`] of the memfd's size, keeping each holder's so a test can
/// play the mount's side of the counters and count remaps. `limit` bounds how far a mapping may
/// grow; `fail` refuses every mapping.
#[derive(Default)]
pub struct MemMapper {
    pub mapped: Mutex<BTreeMap<HolderId, Arc<MemCounters>>>,
    pub calls: AtomicUsize,
    pub limit: Mutex<Option<u64>>,
    pub fail: AtomicBool,
}

impl MemMapper {
    /// The counters mapped for `holder`.
    pub fn counters(&self, holder: HolderId) -> Arc<MemCounters> {
        self.mapped.lock().unwrap()[&holder].clone()
    }
}

impl CounterMapper for MemMapper {
    fn map(&self, holder: HolderId, memfd: OwnedFd) -> Result<Arc<dyn HolderCounters>, EntryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let len = rustix::fs::fstat(&memfd).unwrap().st_size as u64;
        if self.fail.load(Ordering::SeqCst) {
            return Err(EntryError::CountersMap {
                len,
                source: io::Error::from(io::ErrorKind::OutOfMemory),
            });
        }
        let counters = Arc::new(match *self.limit.lock().unwrap() {
            Some(limit) => MemCounters::with_limit(len, limit),
            None => MemCounters::new(len),
        });
        self.mapped.lock().unwrap().insert(holder, counters.clone());
        Ok(counters)
    }
}

// ---------------------------------------------------------------------------------------------
// An entry store whose entries have descriptors.

/// An entry of [`MemEntryStore`] with a data memfd (the pack's bytes, [`pack_byte`]) and a state
/// memfd sealed with [`MOUNT_SEALS`]: what the Linux store (T1) passes.
pub struct FdEntry {
    /// [`Entry::fds`] answers `None` from now on, as if the descriptors were gone.
    pub hide_fds: AtomicBool,
    inner: Arc<dyn Entry>,
    data: Option<OwnedFd>,
    state: Option<OwnedFd>,
}

impl FdEntry {
    fn new(inner: Arc<dyn Entry>, with_fds: bool) -> Self {
        if !with_fds {
            return Self {
                hide_fds: AtomicBool::new(false),
                inner,
                data: None,
                state: None,
            };
        }
        let size = inner.size();
        let data = memfd_create("pack-data", MemfdFlags::CLOEXEC).unwrap();
        let bytes: Vec<u8> = (0..size).map(pack_byte).collect();
        write_all(&data, &bytes);
        let layout = layout(size);
        let state = memfd_create(
            "pack-state",
            MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
        )
        .unwrap();
        ftruncate(&state, layout.len()).unwrap();
        write_all(&state, &layout.header());
        fcntl_add_seals(&state, SealFlags::from_bits_retain(MOUNT_SEALS)).unwrap();
        Self {
            hide_fds: AtomicBool::new(false),
            inner,
            data: Some(data),
            state: Some(state),
        }
    }

    /// The data memfd.
    pub fn data_fd(&self) -> BorrowedFd<'_> {
        self.data.as_ref().unwrap().as_fd()
    }

    /// The state memfd.
    pub fn state_fd(&self) -> BorrowedFd<'_> {
        self.state.as_ref().unwrap().as_fd()
    }
}

fn write_all(fd: &OwnedFd, bytes: &[u8]) {
    let mut at = 0;
    while at < bytes.len() {
        at += pwrite(fd, &bytes[at..], at as u64).unwrap();
    }
}

impl Entry for FdEntry {
    fn pack(&self) -> &PackId {
        self.inner.pack()
    }
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn state(&self) -> &dyn StateView {
        self.inner.state()
    }
    fn claim(&self, run: CellRun) -> Vec<ClaimedCell> {
        self.inner.claim(run)
    }
    fn abandon(&self, cell: ClaimedCell) {
        self.inner.abandon(cell);
    }
    fn publish(&self, cell: ClaimedCell, bytes: &[u8]) -> Result<(), EntryError> {
        self.inner.publish(cell, bytes)
    }
    fn mark_bad(&self, cell: ClaimedCell) {
        self.inner.mark_bad(cell);
    }
    fn evict_cell_cas(&self, cell: u32, step: EvictStep) -> bool {
        self.inner.evict_cell_cas(cell, step)
    }
    fn punch(&self, runs: &[CellRun]) -> Result<PunchReport, EntryError> {
        self.inner.punch(runs)
    }
    fn settle(&self, cell: u32, how: Settle) -> bool {
        self.inner.settle(cell, how)
    }
    fn group_readers(&self, group: u32) -> u64 {
        self.inner.group_readers(group)
    }
    fn counts(&self) -> CellCounts {
        self.inner.counts()
    }
    fn last_use(&self) -> u64 {
        self.inner.last_use()
    }
    fn touch(&self, now_ms: u64, group: u32) {
        self.inner.touch(now_ms, group);
    }
    fn touch_epoch(&self, group: u32) -> Option<u32> {
        self.inner.touch_epoch(group)
    }
    fn read_cell(&self, cell: u32) -> Result<Vec<u8>, EntryError> {
        self.inner.read_cell(cell)
    }
    fn fds(&self) -> Option<EntryFds<'_>> {
        if self.hide_fds.load(Ordering::SeqCst) {
            return None;
        }
        Some(EntryFds {
            data: self.data.as_ref()?.as_fd(),
            state: self.state.as_ref()?.as_fd(),
        })
    }
    fn hold(&self, holder: HolderId) {
        self.inner.hold(holder);
    }
    fn release(&self, holder: HolderId) -> bool {
        self.inner.release(holder)
    }
    fn holders(&self) -> u32 {
        self.inner.holders()
    }
    fn commit_batch(&self) -> Result<CommitReport, EntryError> {
        self.inner.commit_batch()
    }
}

/// [`MemEntryStore`] whose entries are [`FdEntry`]s, with scripted failures.
#[derive(Default)]
pub struct FdStore {
    pub inner: MemEntryStore,
    entries: Mutex<BTreeMap<PackId, Arc<FdEntry>>>,
    /// `open` fails with an I/O error.
    pub fail_open: AtomicBool,
    /// Entries are made without descriptors (as the in-process store's are).
    pub no_fds: AtomicBool,
    /// `register_holder` fails.
    pub fail_register: AtomicBool,
    /// `register_holder` panics.
    pub panic_register: AtomicBool,
    /// The holders `drop_holder` was called for, in order.
    pub dropped: Mutex<Vec<HolderId>>,
    /// The next `attach`, once done, says so on the first channel and waits on the second.
    pub attach_pause: Mutex<Option<AttachPause>>,
}

/// The two channels of [`FdStore::attach_pause`].
pub type AttachPause = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);

impl FdStore {
    /// The entry of pack `i` of [`tree`].
    pub fn entry(&self, i: usize) -> Arc<FdEntry> {
        self.entries.lock().unwrap()[&pack_id(i)].clone()
    }

    /// The holders of pack `i`'s entry (0 when it has none).
    pub fn holders_of(&self, i: usize) -> u32 {
        self.inner.get(&pack_id(i)).map_or(0, |e| e.holders())
    }
}

impl EntryStore for FdStore {
    fn open(&self, spec: &EntrySpec) -> Result<Arc<dyn Entry>, EntryError> {
        if self.fail_open.load(Ordering::SeqCst) {
            return Err(EntryError::Io {
                pack: spec.pack.clone(),
                op: "open",
                source: io::Error::from(io::ErrorKind::PermissionDenied),
            });
        }
        let inner = self.inner.open(spec)?;
        let mut entries = self.entries.lock().unwrap();
        let entry = entries
            .entry(spec.pack.clone())
            .or_insert_with(|| Arc::new(FdEntry::new(inner, !self.no_fds.load(Ordering::SeqCst))));
        Ok(entry.clone())
    }
    fn get(&self, pack: &PackId) -> Option<Arc<dyn Entry>> {
        self.entries
            .lock()
            .unwrap()
            .get(pack)
            .map(|e| e.clone() as Arc<dyn Entry>)
    }
    fn remove(&self, pack: &PackId) -> Result<bool, EntryError> {
        let removed = self.inner.remove(pack)?;
        if removed {
            self.entries.lock().unwrap().remove(pack);
        }
        Ok(removed)
    }
    fn packs(&self) -> Vec<PackId> {
        self.inner.packs()
    }
    fn entries(&self) -> Vec<EntryInfo> {
        self.inner.entries()
    }
    fn register_holder(
        &self,
        holder: HolderId,
        counters: Arc<dyn HolderCounters>,
    ) -> Result<(), EntryError> {
        assert!(
            !self.panic_register.load(Ordering::SeqCst),
            "register_holder panicked"
        );
        if self.fail_register.load(Ordering::SeqCst) {
            return Err(EntryError::UnknownHolder { holder });
        }
        self.inner.register_holder(holder, counters)
    }
    fn attach(
        &self,
        pack: &PackId,
        holder: HolderId,
        region: CounterRegion,
    ) -> Result<(), EntryError> {
        self.inner.attach(pack, holder, region)?;
        if let Some((attached, proceed)) = self.attach_pause.lock().unwrap().take() {
            attached.send(()).unwrap();
            proceed.recv().unwrap();
        }
        Ok(())
    }
    fn drop_holder(&self, holder: HolderId) -> u32 {
        self.dropped.lock().unwrap().push(holder);
        self.inner.drop_holder(holder)
    }
}

// ---------------------------------------------------------------------------------------------
// The server.

/// A server of the fakes above and [`FakeEngine`].
pub struct Fixture {
    pub server: MountServer,
    pub store: Arc<FdStore>,
    pub engine: Arc<FakeEngine>,
    pub trees: Arc<FakeTrees>,
    pub mapper: Arc<MemMapper>,
}

impl Fixture {
    /// A server under [`EPOCH`] with the default configuration.
    pub fn new() -> Self {
        Self::with(EPOCH, Config::default())
    }

    /// A server under `epoch` with `config`.
    pub fn with(epoch: Epoch, config: Config) -> Self {
        let store = Arc::new(FdStore::default());
        let engine = Arc::new(FakeEngine::new());
        let trees = Arc::new(FakeTrees::new());
        let mapper = Arc::new(MemMapper::default());
        let server = MountServer::new(
            Parts {
                store: store.clone(),
                engine: engine.clone(),
                trees: trees.clone(),
                counters: mapper.clone(),
            },
            epoch,
            config,
        );
        Self {
            server,
            store,
            engine,
            trees,
            mapper,
        }
    }

    /// Another server (a restart) of the same store, engine, trees and mapper under `epoch`.
    pub fn restart(&self, epoch: Epoch) -> MountServer {
        MountServer::new(
            Parts {
                store: self.store.clone(),
                engine: self.engine.clone(),
                trees: self.trees.clone(),
                counters: self.mapper.clone(),
            },
            epoch,
            Config::default(),
        )
    }

    /// A new connection to this fixture's server.
    pub fn connect(&self) -> Conn {
        Conn::to(&self.server)
    }
}

// ---------------------------------------------------------------------------------------------
// The mount.

/// What a mount process holds for its whole life: its holder id, its counter memfd, and its
/// lifeline (the end it keeps and the end it sends).
pub struct MountProc {
    pub holder: HolderId,
    pub counters: OwnedFd,
    pub lifeline: Option<OwnedFd>,
    pub lifeline_sent: OwnedFd,
}

impl MountProc {
    /// A mount of `holder` whose counter memfd is `len` bytes and sealed against shrinking.
    pub fn new(holder: u64, len: u64) -> Self {
        let counters = sealed_memfd(len, COUNTER_MEMFD_SEALS);
        let (keep, sent) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        Self {
            holder: HolderId(holder),
            counters,
            lifeline: Some(keep),
            lifeline_sent: sent,
        }
    }

    /// The handshake message.
    pub fn handshake(&self) -> Message {
        Message::Handshake {
            holder: self.holder,
            version: 1,
        }
    }

    /// The two descriptors of the handshake.
    pub fn fds(&self) -> [BorrowedFd<'_>; 2] {
        [self.counters.as_fd(), self.lifeline_sent.as_fd()]
    }

    /// The process exits: its end of the lifeline closes.
    pub fn exit(&mut self) {
        self.lifeline = None;
    }

    /// `open-pack` of pack `i` of [`tree`] with its counters at `offset`.
    pub fn open(&self, i: usize, offset: u64) -> Message {
        Message::OpenPack {
            tree: tree(),
            pack: i as u32,
            holder: self.holder,
            region: region(i, offset),
        }
    }
}

/// A memfd of `len` bytes with `seals` applied.
pub fn sealed_memfd(len: u64, seals: u32) -> OwnedFd {
    let fd = memfd_create("counters", MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING).unwrap();
    ftruncate(&fd, len).unwrap();
    if seals != 0 {
        fcntl_add_seals(&fd, SealFlags::from_bits_retain(seals)).unwrap();
    }
    fd
}

/// A client connection to a server, which serves the other end in a task.
pub struct Conn {
    pub stream: UnixStream,
    /// The epoch the client stamps its requests with: 0 until a handshake is answered, then the
    /// server's.
    pub epoch: Epoch,
    next_id: u64,
    pub served: JoinHandle<()>,
}

impl Conn {
    /// A connection to `server`.
    pub fn to(server: &MountServer) -> Self {
        let (client, served) = UnixStream::pair().unwrap();
        let server = server.clone();
        let served = tokio::spawn(async move { server.serve_connection(served).await });
        Self {
            stream: client,
            epoch: Epoch(0),
            next_id: 0,
            served,
        }
    }

    /// A connection to `server` whose server end passes credentials (`SO_PASSCRED`), so the
    /// kernel adds an `SCM_CREDENTIALS` message to what the server reads.
    pub fn passing_credentials(server: &MountServer) -> Self {
        let (client, served) = UnixStream::pair().unwrap();
        rustix::net::sockopt::set_socket_passcred(&served, true).unwrap();
        let server = server.clone();
        let served = tokio::spawn(async move { server.serve_connection(served).await });
        Self {
            stream: client,
            epoch: Epoch(0),
            next_id: 0,
            served,
        }
    }

    /// A client of a stream connected to a server that serves the other end elsewhere.
    pub fn client(stream: UnixStream) -> Self {
        Self {
            stream,
            epoch: Epoch(0),
            next_id: 0,
            served: tokio::spawn(async {}),
        }
    }

    /// Sends `message` with `fds` under the connection's epoch; the request id.
    pub async fn send(&mut self, message: Message, fds: &[BorrowedFd<'_>]) -> u64 {
        self.next_id += 1;
        let frame = Frame {
            id: self.next_id,
            epoch: self.epoch,
            message,
        };
        send_raw(&self.stream, &frame.encode().unwrap(), fds)
            .await
            .unwrap();
        self.next_id
    }

    /// The next reply, and its descriptors. Panics if the server closed the connection.
    pub async fn recv(&self) -> (Frame, Vec<OwnedFd>) {
        recv_frame(&self.stream)
            .await
            .expect("the server closed the connection")
    }

    /// Sends `message` and returns its reply, checking the reply's id; a `HandshakeOk` sets the
    /// connection's epoch to the reply's.
    pub async fn call(
        &mut self,
        message: Message,
        fds: &[BorrowedFd<'_>],
    ) -> (Frame, Vec<OwnedFd>) {
        let id = self.send(message, fds).await;
        let (frame, fds) = self.recv().await;
        assert_eq!(frame.id, id, "the reply of another request: {frame:?}");
        if frame.message == Message::HandshakeOk {
            self.epoch = frame.epoch;
        }
        (frame, fds)
    }

    /// The handshake of `mount`; its reply.
    pub async fn handshake(&mut self, mount: &MountProc) -> Message {
        self.call(mount.handshake(), &mount.fds()).await.0.message
    }

    /// Whether the server closed the connection: the next read sees the end, with no reply.
    pub async fn closed(&self) -> bool {
        recv_frame(&self.stream).await.is_none()
    }
}

/// Writes `bytes` with `fds` on the first `sendmsg`.
pub async fn send_raw(stream: &UnixStream, bytes: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<()> {
    let mut sent = 0;
    while sent < bytes.len() {
        let rights: &[BorrowedFd<'_>] = if sent == 0 { fds } else { &[] };
        let chunk = &bytes[sent..];
        sent += stream
            .async_io(Interest::WRITABLE, || {
                let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(16))];
                let mut control = SendAncillaryBuffer::new(&mut space);
                if !rights.is_empty() {
                    assert!(control.push(SendAncillaryMessage::ScmRights(rights)));
                }
                Ok(sendmsg(
                    stream,
                    &[IoSlice::new(chunk)],
                    &mut control,
                    SendFlags::NOSIGNAL,
                )?)
            })
            .await?;
    }
    Ok(())
}

/// Reads into `buf` until it is full or the stream ends; the bytes read.
async fn fill(stream: &UnixStream, buf: &mut [u8], fds: &mut Vec<OwnedFd>) -> usize {
    let mut got = 0;
    while got < buf.len() {
        let chunk = &mut buf[got..];
        let read = stream
            .async_io(Interest::READABLE, || {
                let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(4))];
                let mut control = RecvAncillaryBuffer::new(&mut space);
                let msg = recvmsg(
                    stream,
                    &mut [IoSliceMut::new(chunk)],
                    &mut control,
                    RecvFlags::CMSG_CLOEXEC,
                )?;
                for m in control.drain() {
                    if let RecvAncillaryMessage::ScmRights(rights) = m {
                        fds.extend(rights);
                    }
                }
                Ok(msg.bytes)
            })
            .await
            .unwrap_or(0);
        if read == 0 {
            break;
        }
        got += read;
    }
    got
}

/// The next frame and its descriptors; `None` at the end of the stream.
pub async fn recv_frame(stream: &UnixStream) -> Option<(Frame, Vec<OwnedFd>)> {
    let mut fds = Vec::new();
    let mut head = [0u8; 4];
    if fill(stream, &mut head, &mut fds).await < 4 {
        return None;
    }
    let len = u32::from_le_bytes(head) as usize;
    assert!(len >= FRAME_HEADER_LEN);
    let mut bytes = vec![0u8; 4 + len];
    bytes[..4].copy_from_slice(&head);
    assert_eq!(fill(stream, &mut bytes[4..], &mut fds).await, len);
    let (frame, used) = Frame::decode(&bytes).unwrap();
    assert_eq!(used, bytes.len());
    assert_eq!(fds.len(), frame.message.fds().len());
    Some((frame, fds))
}

/// Yields to the server's tasks until `done` holds: a wait for a progress event of a fake that has
/// no notification of its own. It never reads a clock.
pub async fn until(mut done: impl FnMut() -> bool) {
    while !done() {
        tokio::task::yield_now().await;
    }
}

/// The device and inode of `fd`.
pub fn inode(fd: BorrowedFd<'_>) -> (u64, u64) {
    let st = rustix::fs::fstat(fd).unwrap();
    (st.st_dev, st.st_ino)
}
