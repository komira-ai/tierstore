//! What the tests of the mount client share.
//!
//! - [`ServerSide`]: the real mount server of T5 on a runtime of its own, over the real entry
//!   store of T1 in a scratch directory, with the fake engine (waits that park until the test
//!   completes them), fake tree lists, and T1's mapping of the mount's counter memfd. Its
//!   connector hands the server end of each new socket pair to the server; a restart ends every
//!   connection and serves the next ones under a new epoch.
//! - [`Script`]: a connector whose connections the test serves itself ([`Peer`]): it reads the
//!   client's frames and answers them frame by frame, descriptors included.
//! - Packs: [`PACKS`], their ids, specs and cells.
//!
//! Nothing here reads a clock or sleeps: the clients made here have a [`ManualClock`] for timer,
//! and a test waits for a count ([`PackClient::wait_for`], [`MountServer::wait_for`]) or a ticket.

use std::collections::BTreeMap;
use std::io::{IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream as StdStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use crate::{Config, Connector, OpenSpec, PackClient, Sleep, Stats, Timer};
use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals, ftruncate, memfd_create};
use rustix::io::pwrite;
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
};
use tierstore_entry::{FileEntry, FileEntryStore, MemfdCounters};
use tierstore_fetch_api::clock::ManualClock;
use tierstore_fetch_api::entry::{Entry, EntryError, HolderCounters};
use tierstore_fetch_api::fake::FakeEngine;
use tierstore_fetch_api::ids::{CellRun, HolderId, PackId, PackKey, PackRoot, TreeRef};
use tierstore_fetch_api::list::{CellDigest, ListL2, NodeIndex, PackRecord};
use tierstore_fetch_api::page::{CounterRegion, MOUNT_SEALS, StatePageLayout};
use tierstore_fetch_api::wire::{Epoch, FRAME_HEADER_LEN, Frame, Message, PackHandle};
use tierstore_mount_server::{
    Config as ServerConfig, CounterMapper, MountServer, Parts, Stats as ServerStats, TreeError,
    Trees,
};
use tierstore_testkit::dir::TestDir;
use tokio::net::UnixStream;
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

pub const MIB: u64 = 1 << 20;

/// The tree every server serves.
pub fn tree() -> TreeRef {
    TreeRef("tree-a".into())
}

/// The packs of [`tree`]: key and size. Pack 0 is 40 cells (five windows of 8, three groups of
/// 16); pack 1 is 3 MiB and 5 bytes (four cells, the last short); pack 2 is 1 MiB.
pub const PACKS: [(&str, u64); 3] = [
    ("pack-0", 40 * MIB),
    ("pack-1", 3 * MIB + 5),
    ("pack-2", MIB),
];

/// The id of pack `i` of [`tree`].
pub fn pack_id(i: usize) -> PackId {
    PackId {
        key: PackKey(PACKS[i].0.into()),
        root: PackRoot::Digest([i as u8 + 1; 32]),
    }
}

/// What a mount opens pack `i` of [`tree`] with.
pub fn spec(i: usize) -> OpenSpec {
    OpenSpec {
        tree: tree(),
        index: i as u32,
        pack: pack_id(i),
        size: PACKS[i].1,
    }
}

/// The run of `count` cells from `first`.
pub fn run(first: u32, count: u32) -> CellRun {
    CellRun { first, count }
}

/// The layout of a pack of `size` bytes.
pub fn layout(size: u64) -> StatePageLayout {
    StatePageLayout::for_pack(size).unwrap()
}

/// The byte at `offset` of every pack.
pub fn pack_byte(offset: u64) -> u8 {
    (offset % 251) as u8
}

/// A client config with a fixed jitter seed.
pub fn config() -> Config {
    Config {
        jitter_seed: Some(11),
        ..Config::default()
    }
}

// ---------------------------------------------------------------------------------------------
// Tree lists.

/// The list of a tree: each pack's record and root.
#[derive(Debug, Default)]
pub struct FakeList {
    pub records: Vec<(PackRecord, PackRoot)>,
}

impl FakeList {
    /// The list of `packs` (key and size), each rooted as [`pack_id`] roots the pack of its index.
    pub fn of(packs: &[(String, u64)]) -> Self {
        Self {
            records: packs
                .iter()
                .enumerate()
                .map(|(i, (key, size))| {
                    (
                        PackRecord {
                            key: PackKey(key.clone()),
                            pack_size: *size,
                            cell_first: 0,
                            cell_count: layout(*size).cell_count,
                            cell_shift: 20,
                            pack_flags: 0,
                            upload_part_size: 0,
                        },
                        PackRoot::Digest([i as u8 + 1; 32]),
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

/// Tree lists by reference.
#[derive(Default)]
pub struct FakeTrees {
    lists: Mutex<BTreeMap<TreeRef, Arc<dyn ListL2>>>,
}

impl FakeTrees {
    /// `tree` serves `list`.
    pub fn set(&self, tree: TreeRef, list: FakeList) {
        self.lists.lock().unwrap().insert(tree, Arc::new(list));
    }
}

#[async_trait::async_trait]
impl Trees for FakeTrees {
    async fn list(&self, tree: &TreeRef) -> Result<Arc<dyn ListL2>, TreeError> {
        self.lists
            .lock()
            .unwrap()
            .get(tree)
            .cloned()
            .ok_or_else(|| TreeError::Unknown(tree.clone()))
    }
}

// ---------------------------------------------------------------------------------------------
// The counter mapper.

/// T1's mapping of a holder's counter memfd, kept per holder so a test can count remaps.
#[derive(Default)]
pub struct Mapper {
    pub mapped: Mutex<BTreeMap<HolderId, Arc<MemfdCounters>>>,
}

impl Mapper {
    /// The mappings made of `holder`'s memfd: the first and every remap.
    pub fn maps(&self, holder: HolderId) -> u32 {
        self.mapped.lock().unwrap()[&holder].maps()
    }
}

impl CounterMapper for Mapper {
    fn map(&self, holder: HolderId, memfd: OwnedFd) -> Result<Arc<dyn HolderCounters>, EntryError> {
        let counters =
            Arc::new(
                MemfdCounters::new(memfd).map_err(|e| EntryError::CountersMap {
                    len: 0,
                    source: std::io::Error::other(e),
                })?,
            );
        self.mapped.lock().unwrap().insert(holder, counters.clone());
        Ok(counters)
    }
}

// ---------------------------------------------------------------------------------------------
// The real server.

/// The real mount server on a runtime of its own, and what it is made of.
pub struct ServerSide {
    pub runtime: Runtime,
    _dir: TestDir,
    pub store: Arc<FileEntryStore>,
    pub engine: Arc<FakeEngine>,
    pub trees: Arc<FakeTrees>,
    pub mapper: Arc<Mapper>,
    server: Mutex<MountServer>,
    conns: Mutex<Vec<JoinHandle<()>>>,
    /// Connects to refuse before the next one is served.
    pub refuse: AtomicUsize,
}

impl ServerSide {
    /// A server under epoch 1 serving [`tree`] with [`PACKS`].
    pub fn new(name: &str) -> Arc<Self> {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let store = Arc::new(FileEntryStore::new(dir.path()).unwrap());
        let engine = Arc::new(FakeEngine::new());
        let trees = Arc::new(FakeTrees::default());
        trees.set(tree(), FakeList::of(&PACKS.map(|(k, s)| (k.to_owned(), s))));
        let mapper = Arc::new(Mapper::default());
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let server = MountServer::new(
            Parts {
                store: store.clone(),
                engine: engine.clone(),
                trees: trees.clone(),
                counters: mapper.clone(),
            },
            Epoch(1),
            ServerConfig::default(),
        );
        Arc::new(Self {
            runtime,
            _dir: dir,
            store,
            engine,
            trees,
            mapper,
            server: Mutex::new(server),
            conns: Mutex::new(Vec::new()),
            refuse: AtomicUsize::new(0),
        })
    }

    /// The server now.
    pub fn server(&self) -> MountServer {
        self.server.lock().unwrap().clone()
    }

    /// Ends every connection (the server keeps its holders: a flap).
    pub fn drop_connections(&self) {
        for c in self.conns.lock().unwrap().drain(..) {
            c.abort();
        }
    }

    /// A restart: every connection ends, and the next ones are served by a new server under
    /// `epoch`, over the same entry store, engine, lists and mapper.
    pub fn restart(&self, epoch: u64) {
        let server = MountServer::new(
            Parts {
                store: self.store.clone(),
                engine: self.engine.clone(),
                trees: self.trees.clone(),
                counters: self.mapper.clone(),
            },
            Epoch(epoch),
            ServerConfig::default(),
        );
        *self.server.lock().unwrap() = server;
        self.drop_connections();
    }

    /// Blocks until the server's counts satisfy `done`.
    pub fn wait_for(&self, done: impl FnMut(&ServerStats) -> bool) -> ServerStats {
        let server = self.server();
        self.runtime.block_on(server.wait_for(done))
    }

    /// Blocks until the engine has `n` waits parked on pack `i`.
    pub fn parked(&self, i: usize, n: usize) {
        until(|| self.engine.parked(&pack_id(i)) == n);
    }

    /// The entry of pack `i` (opened by an `open-pack`).
    pub fn entry(&self, i: usize) -> Arc<FileEntry> {
        self.store.file_entry(&pack_id(i)).unwrap()
    }

    /// Publishes `cells` of pack `i`'s entry with the pack's bytes.
    pub fn publish(&self, i: usize, cells: CellRun) {
        let entry = self.entry(i);
        let size = PACKS[i].1;
        for cell in cells.cells() {
            let start = u64::from(cell) * MIB;
            let end = (start + MIB).min(size);
            let bytes: Vec<u8> = (start..end).map(pack_byte).collect();
            let mut won = entry.claim(run(cell, 1));
            let token = won.pop().expect("the cell was claimable");
            entry.publish(token, &bytes).unwrap();
        }
    }

    /// A connector to this server.
    pub fn connector(self: &Arc<Self>) -> Arc<dyn Connector> {
        Arc::new(ServerConnector { side: self.clone() })
    }

    /// A client of holder `holder` connected to this server, its timer `clock`.
    pub fn client(self: &Arc<Self>, holder: u64, clock: &ManualClock) -> PackClient {
        PackClient::start(
            HolderId(holder),
            self.connector(),
            Arc::new(clock.clone()),
            config(),
        )
        .unwrap()
    }
}

struct ServerConnector {
    side: Arc<ServerSide>,
}

#[async_trait::async_trait]
impl Connector for ServerConnector {
    async fn connect(&self) -> std::io::Result<UnixStream> {
        let refuse = &self.side.refuse;
        if refuse
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(std::io::ErrorKind::ConnectionRefused.into());
        }
        let (mine, theirs) = StdStream::pair()?;
        mine.set_nonblocking(true)?;
        theirs.set_nonblocking(true)?;
        let server = self.side.server();
        let conn = self.side.runtime.spawn(async move {
            let stream = UnixStream::from_std(theirs).unwrap();
            server.serve_connection(stream).await;
        });
        self.side.conns.lock().unwrap().push(conn);
        UnixStream::from_std(mine)
    }
}

// ---------------------------------------------------------------------------------------------
// A server the test plays.

/// A connector whose connections the test serves ([`Script::accept`]), and that refuses as many
/// connects as [`Script::refuse`] says first.
pub struct Script {
    tx: Mutex<mpsc::Sender<StdStream>>,
    rx: Mutex<mpsc::Receiver<StdStream>>,
    pub refuse: AtomicUsize,
    /// The client's end passes credentials (`SO_PASSCRED`): the kernel adds an
    /// `SCM_CREDENTIALS` message to every read of it.
    pub passcred: std::sync::atomic::AtomicBool,
}

impl Script {
    pub fn new() -> Arc<Self> {
        let (tx, rx) = mpsc::channel();
        Arc::new(Self {
            tx: Mutex::new(tx),
            rx: Mutex::new(rx),
            refuse: AtomicUsize::new(0),
            passcred: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// The next connection the client makes.
    pub fn accept(&self) -> Peer {
        Peer {
            stream: self.rx.lock().unwrap().recv().unwrap(),
        }
    }

    /// A client of `holder` connected through this script, its timer `timer`.
    pub fn client(self: &Arc<Self>, holder: u64, timer: Arc<dyn Timer>) -> PackClient {
        PackClient::start(HolderId(holder), self.clone(), timer, config()).unwrap()
    }
}

#[async_trait::async_trait]
impl Connector for Script {
    async fn connect(&self) -> std::io::Result<UnixStream> {
        if self
            .refuse
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(std::io::ErrorKind::ConnectionRefused.into());
        }
        let (mine, theirs) = StdStream::pair()?;
        mine.set_nonblocking(true)?;
        if self.passcred.load(Ordering::SeqCst) {
            rustix::net::sockopt::set_socket_passcred(&mine, true)?;
        }
        self.tx.lock().unwrap().send(theirs).unwrap();
        UnixStream::from_std(mine)
    }
}

/// The server end of one connection, served by the test with blocking reads and writes.
pub struct Peer {
    pub stream: StdStream,
}

impl Peer {
    /// The next frame and its descriptors; `None` when the client closed the connection.
    pub fn recv(&self) -> Option<(Frame, Vec<OwnedFd>)> {
        let mut fds = Vec::new();
        let mut head = [0u8; 4];
        if fill(&self.stream, &mut head, &mut fds) < 4 {
            return None;
        }
        let len = u32::from_le_bytes(head) as usize;
        assert!(len >= FRAME_HEADER_LEN);
        let mut bytes = vec![0u8; 4 + len];
        bytes[..4].copy_from_slice(&head);
        assert_eq!(fill(&self.stream, &mut bytes[4..], &mut fds), len);
        let (frame, _) = Frame::decode(&bytes).unwrap();
        assert_eq!(fds.len(), frame.message.fds().len());
        Some((frame, fds))
    }

    /// The next frame, which must be there.
    pub fn next(&self) -> (Frame, Vec<OwnedFd>) {
        self.recv().expect("the client closed the connection")
    }

    /// Sends raw bytes with `fds` on them.
    pub fn send_raw(&self, bytes: &[u8], fds: &[BorrowedFd<'_>]) {
        let mut sent = 0;
        while sent < bytes.len() {
            let rights: &[BorrowedFd<'_>] = if sent == 0 { fds } else { &[] };
            let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(32))];
            let mut control = SendAncillaryBuffer::new(&mut space);
            if !rights.is_empty() {
                assert!(control.push(SendAncillaryMessage::ScmRights(rights)));
            }
            sent += sendmsg(
                &self.stream,
                &[IoSlice::new(&bytes[sent..])],
                &mut control,
                SendFlags::NOSIGNAL,
            )
            .unwrap();
        }
    }

    /// Answers request `id` with `message` under `epoch`.
    pub fn reply(&self, id: u64, epoch: u64, message: Message, fds: &[BorrowedFd<'_>]) {
        let frame = Frame {
            id,
            epoch: Epoch(epoch),
            message,
        };
        self.send_raw(&frame.encode().unwrap(), fds);
    }

    /// Reads the handshake and accepts it under `epoch`; its frame and descriptors.
    pub fn handshake(&self, epoch: u64) -> (Frame, Vec<OwnedFd>) {
        let (frame, fds) = self.next();
        assert!(
            matches!(frame.message, Message::Handshake { version: 1, .. }),
            "{frame:?}"
        );
        self.reply(frame.id, epoch, Message::HandshakeOk, &[]);
        (frame, fds)
    }

    /// Reads an `open-pack` and answers it with a pack of `size` bytes as handle `handle`, the
    /// region it asked for, and `fds` (made by [`pack_fds`] when `None`).
    pub fn open(&self, epoch: u64, handle: u64, size: u64) -> (Frame, PackFds) {
        let (frame, _) = self.next();
        let Message::OpenPack { region, .. } = frame.message else {
            panic!("not an open-pack: {frame:?}");
        };
        let fds = pack_fds(size);
        self.reply(
            frame.id,
            epoch,
            Message::PackOpened {
                pack: PackHandle(handle),
                pack_size: size,
                region,
            },
            &[fds.data.as_fd(), fds.state.as_fd()],
        );
        (frame, fds)
    }

    /// Whether the client closed the connection: the next read sees its end.
    pub fn closed(&self) -> bool {
        self.recv().is_none()
    }
}

/// Reads into `buf` until it is full or the stream ends; the bytes read.
fn fill(stream: &StdStream, buf: &mut [u8], fds: &mut Vec<OwnedFd>) -> usize {
    let mut got = 0;
    while got < buf.len() {
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(4))];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let read = match recvmsg(
            stream,
            &mut [IoSliceMut::new(&mut buf[got..])],
            &mut control,
            RecvFlags::CMSG_CLOEXEC,
        ) {
            Ok(msg) => msg.bytes,
            Err(_) => 0,
        };
        for m in control.drain() {
            if let RecvAncillaryMessage::ScmRights(rights) = m {
                fds.extend(rights);
            }
        }
        if read == 0 {
            break;
        }
        got += read;
    }
    got
}

/// A pack's data memfd and state memfd as a server passes them.
pub struct PackFds {
    pub data: OwnedFd,
    pub state: OwnedFd,
}

/// The descriptors of a pack of `size` bytes: a data memfd and a state memfd sealed for mounts,
/// every cell `absent` (so a read waits).
pub fn pack_fds(size: u64) -> PackFds {
    pack_fds_with(size, size, MOUNT_SEALS, 0)
}

/// The descriptors of a pack whose state page header says `header_size` bytes, sealed with
/// `seals`, every cell's byte `cell`.
pub fn pack_fds_with(size: u64, header_size: u64, seals: u32, cell: u8) -> PackFds {
    let data = memfd_create("pack-data", MemfdFlags::CLOEXEC).unwrap();
    let bytes: Vec<u8> = (0..size).map(pack_byte).collect();
    write_all(&data, &bytes, 0);
    let l = layout(header_size);
    let state = memfd_create(
        "pack-state",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )
    .unwrap();
    ftruncate(&state, l.len()).unwrap();
    write_all(&state, &l.header(), 0);
    write_all(&state, &vec![cell; l.cell_count as usize], l.cells_offset());
    if seals != 0 {
        fcntl_add_seals(&state, SealFlags::from_bits_retain(seals)).unwrap();
    }
    PackFds { data, state }
}

fn write_all(fd: &OwnedFd, bytes: &[u8], mut at: u64) {
    let mut done = 0;
    while done < bytes.len() {
        let n = pwrite(fd, &bytes[done..], at).unwrap();
        done += n;
        at += n as u64;
    }
}

/// A region as the client allocates its first one for a pack of `size` bytes.
pub fn first_region(size: u64) -> CounterRegion {
    CounterRegion {
        offset: 0,
        groups: layout(size).group_count(),
    }
}

// ---------------------------------------------------------------------------------------------
// Time and progress.

/// A timer that records the delay of every sleep and sleeps on a manual clock.
#[derive(Default)]
pub struct Recorder {
    pub clock: ManualClock,
    pub delays: Mutex<Vec<u64>>,
}

impl Timer for Recorder {
    fn sleep(&self, ms: u64) -> Sleep {
        self.delays.lock().unwrap().push(ms);
        self.clock.sleep(ms)
    }
}

/// Spins (yielding the thread) until `done` holds: a wait for a progress event of a fake that
/// has no notification of its own. It never reads a clock.
pub fn until(mut done: impl FnMut() -> bool) {
    while !done() {
        std::thread::yield_now();
    }
}

/// Waits until the client sleeps on `clock`, then moves the clock past any backoff step.
pub fn wake(clock: &ManualClock) {
    until(|| clock.sleepers() > 0);
    clock.advance(2000);
}

/// Waits until the client's counts satisfy `done`.
pub fn stats_until(client: &PackClient, done: impl FnMut(&Stats) -> bool) -> Stats {
    client.wait_for(done)
}
