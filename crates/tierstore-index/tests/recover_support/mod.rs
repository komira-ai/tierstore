//! Helpers of the recovery tests: byte builders, a scripted node that writes the files and the
//! index the way a node does, the comparison of two indexes through their public API, and the
//! store kinds the generic tests run on.

#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "shared by several test files, each using part of it; a failed unwrap is the failure a test reports"
)]

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tierstore_format::block::{BlockBuilder, BlockGeometry};
use tierstore_format::descriptor::{Code, PlacedShard, ShardLoc, StripeDescriptor};
use tierstore_format::footer::{FooterSubject, ShardFooter, footer_offset};
use tierstore_format::ids::{BlockId, DriveId, ExpiryClass, Generation, NodeId, StripeId};
use tierstore_format::names::{ColdName, HotName};
use tierstore_format::record::{RecordFlags, RecordIn};
use tierstore_format::stamp::Stamp;
use tierstore_format::tomb::{LogEntry, Named, Tombstone};
use tierstore_format::trailer::{BlockKind, cell_crcs};
use tierstore_index::backend::{Backend, MemBackend};
use tierstore_index::fs::Fs;
use tierstore_index::fs::fake::{FakeFs, Torn};
use tierstore_index::kv::{IndexConfig, KvIndex};
use tierstore_index::recover::{RecoverError, Recovery, RecoveryReport};
use tierstore_index::tomblog::TombLog;
use tierstore_index::tombs::TombInfo;
use tierstore_store_api::index::{
    EntryLocation, Index, IndexEntry, LiveCount, Lookup, PutOutcome, ReplayMark,
};

/// The node under test.
pub const NODE: NodeId = NodeId(1);

/// The bytes a scan step reads in tests: small, so that windows are refilled many times.
pub const WINDOW: u64 = 200;

/// A geometry with room for a chain: blocks of 1 MiB with a reserve of 256 KiB.
pub fn geometry() -> BlockGeometry {
    BlockGeometry::new(1 << 20, 1 << 18).unwrap()
}

pub fn cfg() -> IndexConfig {
    IndexConfig {
        node: NODE,
        geometry: geometry(),
    }
}

pub fn block(n: u64) -> BlockId {
    BlockId::new(NODE, n).unwrap()
}

pub fn stamp(n: u64) -> Stamp {
    Stamp::from_raw(n)
}

pub fn gen_of(data: &[u8]) -> Generation {
    Generation::of(data)
}

/// The path of a block's segment file.
pub fn seg_path(b: BlockId) -> String {
    HotName::Segment(b).path()
}

pub fn idx_path(b: BlockId) -> String {
    HotName::SegmentIndex(b).path()
}

pub fn blk_path(b: BlockId) -> String {
    HotName::BlockIndex(b).path()
}

pub fn rec_in<'a>(key: &'a [u8], data: &'a [u8], at: u64, flags: RecordFlags) -> RecordIn<'a> {
    RecordIn {
        flags,
        key,
        data,
        generation: gen_of(data),
        stamp: stamp(at),
        object_offset: 0,
    }
}

/// The entry a record at `offset` of `block` makes.
pub fn entry(block: BlockId, offset: u64, data: &[u8], at: u64) -> IndexEntry {
    IndexEntry {
        generation: gen_of(data),
        location: EntryLocation::Block { block, offset },
        length: data.len() as u64,
        stamp: stamp(at),
        flags: RecordFlags(0),
        prior_blocks: Vec::new(),
        promoted: None,
    }
}

/// A block built from records: its bytes with the trailer, the trailer alone and the offset of
/// each record.
pub struct Built {
    /// The records and the trailer.
    pub bytes: Vec<u8>,
    /// The trailer alone: the `.idx` file.
    pub trailer: Vec<u8>,
    /// Offsets of the records, in order.
    pub offsets: Vec<u64>,
}

/// A record to build: key, data, stamp, flags.
pub type Rec<'a> = (&'a [u8], &'a [u8], u64, RecordFlags);

pub fn build(kind: BlockKind, records: &[Rec<'_>]) -> Built {
    let mut b = BlockBuilder::new(geometry(), kind).unwrap();
    let mut bytes = Vec::new();
    let mut offsets = Vec::new();
    for (key, data, at, flags) in records {
        let rec = b.append(&rec_in(key, data, *at, *flags)).unwrap();
        // `append` returns the alignment padding, zero bytes, and then the record.
        let pad = rec.iter().take_while(|x| **x == 0).count() as u64;
        offsets.push(bytes.len() as u64 + pad);
        bytes.extend(rec);
    }
    let sealed = b.finish().unwrap();
    let mut all = bytes;
    all.extend_from_slice(&sealed.bytes);
    Built {
        bytes: all,
        trailer: sealed.bytes,
        offsets,
    }
}

/// The index of a node on `backend`.
pub fn index_on<B: Backend>(backend: B) -> KvIndex<B> {
    KvIndex::open(backend, cfg()).unwrap()
}

/// What recovery runs on.
pub struct Disks {
    pub hot: Arc<FakeFs>,
    pub cold: Vec<Arc<FakeFs>>,
    pub tomb: TombLog,
}

impl Disks {
    pub fn new(hot: Arc<FakeFs>, cold: Vec<Arc<FakeFs>>) -> Self {
        let tomb = TombLog::open(hot.clone(), NODE, 1 << 20).unwrap();
        Self { hot, cold, tomb }
    }

    pub fn hot_only(hot: FakeFs) -> Self {
        Self::new(Arc::new(hot), Vec::new())
    }

    pub fn recovery(&self) -> Recovery<'_> {
        Recovery {
            hot: &*self.hot,
            cold: self.cold.iter().map(|c| &**c as &dyn Fs).collect(),
            tomb: &self.tomb,
            geometry: geometry(),
            window: WINDOW,
        }
    }

    /// Runs recovery on `index` and makes it ready.
    pub fn recover<B: Backend>(&self, index: &KvIndex<B>) -> Result<RecoveryReport, RecoverError> {
        let report = self.recovery().run(index)?;
        index.set_ready(true);
        Ok(report)
    }

    /// Rebuilds `index` and makes it ready.
    pub fn rebuild<B: Backend>(&self, index: &KvIndex<B>) -> Result<RecoveryReport, RecoverError> {
        let report = self.recovery().rebuild(index)?;
        index.set_ready(true);
        Ok(report)
    }
}

/// A kind of store the generic tests run on.
pub trait Stores {
    type B: Backend;
    /// An index on an empty store.
    fn fresh(&self) -> KvIndex<Self::B>;
    /// The index a restart makes: the old one is gone and the store holds what was flushed with
    /// the replay mark.
    fn restart(&self, old: KvIndex<Self::B>) -> KvIndex<Self::B>;
}

/// The in-memory store.
pub struct Mem;

impl Stores for Mem {
    type B = MemBackend;

    fn fresh(&self) -> KvIndex<MemBackend> {
        index_on(MemBackend::new())
    }

    fn restart(&self, old: KvIndex<MemBackend>) -> KvIndex<MemBackend> {
        index_on(old.backend().crashed(0))
    }
}

/// A store on disk (fjall or redb) in a scratch directory; a restart drops the index and opens
/// the same directory again.
pub struct OnDisk<B> {
    dir: tierstore_testkit::dir::TestDir,
    open: fn(&std::path::Path) -> B,
}

impl<B: Backend> OnDisk<B> {
    fn path(&self) -> std::path::PathBuf {
        self.dir.path().join("index")
    }
}

/// The fjall store.
pub fn fjall_store() -> OnDisk<tierstore_index::backend::fjall::FjallBackend> {
    OnDisk {
        dir: tierstore_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "recover-fjall"),
        open: |p| {
            tierstore_index::backend::fjall::FjallBackend::open(p, &Default::default()).unwrap()
        },
    }
}

/// The redb store.
pub fn redb_store() -> OnDisk<tierstore_index::backend::redb::RedbBackend> {
    OnDisk {
        dir: tierstore_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "recover-redb"),
        open: |p| tierstore_index::backend::redb::RedbBackend::open(p, &Default::default()).unwrap(),
    }
}

impl<B: Backend> Stores for OnDisk<B> {
    type B = B;

    fn fresh(&self) -> KvIndex<B> {
        index_on((self.open)(&self.path()))
    }

    fn restart(&self, old: KvIndex<B>) -> KvIndex<B> {
        drop(old);
        index_on((self.open)(&self.path()))
    }
}

/// Runs each generic test function of the list as a test of the module of a store kind. To run
/// them on the fjall and redb stores, add one line `on_stores!(fjall_store, FjallStores, ...)`
/// with the same names, where `FjallStores` implements `Stores` by opening its store in a
/// `TestDir` and, in `restart`, dropping the old index and opening the same directory.
#[macro_export]
macro_rules! on_stores {
    ($module:ident, $kind:expr, $($name:ident),* $(,)?) => {
        mod $module {
            $(
                #[test]
                fn $name() {
                    let kind = $kind;
                    super::$name(&kind);
                }
            )*
        }
    };
}

/// The observable state of an index: every key's `get`, the whole `list`, the live counts of the
/// blocks and the tombstones without their log positions.
#[derive(Debug, PartialEq, Eq)]
pub struct View {
    pub gets: Vec<(Vec<u8>, Lookup)>,
    pub list: Vec<(Vec<u8>, IndexEntry)>,
    pub live: Vec<(BlockId, LiveCount)>,
    pub tombs: Vec<TombInfo>,
}

pub fn view<B: Backend>(index: &KvIndex<B>, keys: &[&[u8]], blocks: &[BlockId]) -> View {
    index.set_ready(true);
    let mut tombs = index.tombstones().unwrap();
    for t in &mut tombs {
        t.position = None;
    }
    View {
        gets: keys
            .iter()
            .map(|k| (k.to_vec(), index.get(k).unwrap()))
            .collect(),
        list: index.list(b"", None, usize::MAX).unwrap(),
        live: blocks
            .iter()
            .map(|b| (*b, index.live(*b).unwrap()))
            .collect(),
        tombs,
    }
}

/// An operation of a scripted workload.
#[derive(Debug, Clone, Copy)]
pub enum Op {
    /// A PUT of a key with data at a stamp.
    Put(&'static str, &'static str, u64),
    /// A DELETE of a key at a stamp.
    Delete(&'static str, u64),
    /// The open segment is sealed: its trailer is appended and its `.idx` is written.
    Seal,
    /// The index is flushed with the replay mark.
    Mark,
}

/// A point a crash can happen at: the files and the store as the crash leaves them, and how many
/// operations were acknowledged by then.
pub struct Snap {
    pub acked: usize,
    pub files: Vec<FakeFs>,
    pub stores: Vec<MemBackend>,
}

/// The effect of an acknowledged operation on the index.
#[derive(Clone)]
enum Effect {
    Put(Vec<u8>, IndexEntry),
    Delete(Tombstone),
    None,
}

/// A node that writes its files and its index the way a node does.
pub struct Node {
    pub fs: Arc<FakeFs>,
    pub tomb: TombLog,
    pub index: KvIndex<MemBackend>,
    seg: u64,
    builder: BlockBuilder,
    effects: Vec<Effect>,
    pub snaps: Vec<Snap>,
    snapping: bool,
    acked: usize,
}

impl Node {
    pub fn new(snapping: bool) -> Self {
        let fs = Arc::new(FakeFs::new());
        let tomb = TombLog::open(fs.clone(), NODE, 1 << 20).unwrap();
        let index = index_on(MemBackend::new());
        index.set_ready(true);
        Self {
            fs,
            tomb,
            index,
            seg: 1,
            builder: BlockBuilder::new(geometry(), BlockKind::Segment).unwrap(),
            effects: Vec::new(),
            snaps: Vec::new(),
            snapping,
            acked: 0,
        }
    }

    /// The block of the open segment.
    pub fn open_block(&self) -> BlockId {
        block(self.seg)
    }

    fn snap(&mut self) {
        if !self.snapping {
            return;
        }
        let torn = [Torn::Lost, Torn::Kept, Torn::Prefix(7), Torn::Prefix(70)];
        let backend = self.index.backend();
        let stores = (0..=backend.unpersisted())
            .map(|k| backend.crashed(k))
            .collect();
        self.snaps.push(Snap {
            acked: self.acked,
            files: torn.iter().map(|t| self.fs.crash(*t)).collect(),
            stores,
        });
    }

    fn ack(&mut self, e: Effect) {
        self.effects.push(e);
        self.acked += 1;
        self.snap();
    }

    /// Runs one operation, taking a snapshot after each of its steps when snapping.
    pub fn run(&mut self, op: Op) {
        match op {
            Op::Put(key, data, at) => self.put(key.as_bytes(), data.as_bytes(), at),
            Op::Delete(key, at) => self.delete(key.as_bytes(), at),
            Op::Seal => self.seal(),
            Op::Mark => {
                self.mark();
                self.ack(Effect::None);
            }
        }
    }

    pub fn put(&mut self, key: &[u8], data: &[u8], at: u64) {
        let path = seg_path(self.open_block());
        let offset = self.builder.len();
        let bytes = self
            .builder
            .append(&rec_in(key, data, at, RecordFlags(0)))
            .unwrap();
        self.fs.append(&path, &bytes).unwrap();
        self.snap();
        self.fs.sync(&path).unwrap();
        self.snap();
        let e = entry(self.open_block(), offset, data, at);
        let out = self.index.put_if_absent(key, e.clone()).unwrap();
        assert_eq!(
            out,
            PutOutcome::Created,
            "the workload only puts keys that are free"
        );
        self.ack(Effect::Put(key.to_vec(), e));
    }

    pub fn delete(&mut self, key: &[u8], at: u64) {
        let (generation, names) = match self.index.get(key).unwrap() {
            Lookup::Live(e) => {
                let EntryLocation::Block { block, .. } = e.location else {
                    unreachable!("the workload keeps its records in segments")
                };
                (e.generation, vec![Named::Block(block)])
            }
            _ => (Generation([0; 16]), Vec::new()),
        };
        let t = Tombstone {
            key: key.to_vec(),
            generation,
            stamp: stamp(at),
            names,
        };
        self.tomb.append(LogEntry::Tombstone(t.clone())).unwrap();
        self.snap();
        self.index.delete(&t).unwrap();
        self.ack(Effect::Delete(t));
    }

    /// Seals the open segment and opens the next.
    pub fn seal(&mut self) {
        let b = self.open_block();
        let sealed = self.builder.finish().unwrap();
        let path = seg_path(b);
        self.fs.append(&path, &sealed.bytes).unwrap();
        self.snap();
        self.fs.sync(&path).unwrap();
        self.snap();
        self.fs.append(&idx_path(b), &sealed.bytes).unwrap();
        self.snap();
        self.fs.sync(&idx_path(b)).unwrap();
        self.seg += 1;
        self.builder = BlockBuilder::new(geometry(), BlockKind::Segment).unwrap();
        self.ack(Effect::None);
    }

    /// Flushes the index with the mark of the open segment and the tomb cursors.
    pub fn mark(&mut self) {
        self.index.flush_mark(&self.current_mark()).unwrap();
    }

    /// How many operations were acknowledged.
    pub fn acked(&self) -> usize {
        self.acked
    }

    /// The mark of the node now: its open segment and the tomb cursors of its index.
    pub fn current_mark(&self) -> ReplayMark {
        ReplayMark {
            open_segments: vec![(self.open_block(), self.builder.len())],
            tomb_cursors: self.index.tomb_cursors().unwrap(),
        }
    }

    /// The index of the first `n` acknowledged operations of the workload, on a store that never
    /// restarted.
    pub fn reference(&self, n: usize) -> KvIndex<MemBackend> {
        let index = index_on(MemBackend::new());
        self.apply_to(&index, 0, n);
        index
    }

    /// Applies the acknowledged operations `from..to` of the workload to `index`, and makes it ready.
    pub fn apply_to<B: Backend>(&self, index: &KvIndex<B>, from: usize, to: usize) {
        index.set_ready(true);
        for e in &self.effects[from..to] {
            match e {
                Effect::Put(key, entry) => {
                    index.put_if_absent(key, entry.clone()).unwrap();
                }
                Effect::Delete(t) => {
                    index.delete(t).unwrap();
                }
                Effect::None => {}
            }
        }
    }
}

/// The keys the workload tests compare over.
pub const KEYS: [&[u8]; 6] = [b"a", b"b", b"c", b"d", b"e", b"zz"];

/// The blocks the workload tests compare over.
pub fn blocks() -> Vec<BlockId> {
    (1..=6).map(block).collect()
}

/// A descriptor of `Ec { k: 2, m: 1 }` over `blocks` (one or two data blocks), with its data
/// shards and the parity shard on one drive.
pub fn stripe(id: u64, blocks: &[(BlockId, u64)], revision: u32) -> StripeDescriptor {
    let loc = ShardLoc {
        node: NODE,
        drive: DriveId([7; 16]),
    };
    let mut shards: Vec<PlacedShard> = (0..blocks.len())
        .map(|i| PlacedShard {
            index: i as u8,
            loc,
        })
        .collect();
    shards.push(PlacedShard { index: 2, loc });
    StripeDescriptor {
        stripe: StripeId::new(NODE, id).unwrap(),
        revision,
        code: Code::Ec { k: 2, m: 1 },
        blocks: blocks.to_vec(),
        shards,
        expiry: ExpiryClass(0),
        coordinator: NODE,
    }
}

/// The bytes of a cold file: `data`, zero padding to 4 KiB and the footer of `subject`.
pub fn cold_file(subject: FooterSubject, data: &[u8]) -> Vec<u8> {
    let footer = ShardFooter {
        subject,
        data_len: data.len() as u64,
        cell_crcs: cell_crcs(data),
        blake3: [0; 32],
    };
    let mut out = data.to_vec();
    out.resize(footer_offset(data.len() as u64) as usize, 0);
    out.extend(footer.encode().unwrap());
    out
}

/// The shard file of `desc` at `index` holding `data`, as `(path, bytes)`.
pub fn shard_file(desc: &StripeDescriptor, index: u8, data: &[u8]) -> (String, Vec<u8>) {
    let path = ColdName::Shard {
        stripe: desc.stripe,
        index,
    }
    .path();
    let subject = FooterSubject::Stripe {
        stripe: desc.clone(),
        index,
    };
    (path, cold_file(subject, data))
}

/// The copy file of `block`.
pub fn copy_file(block: BlockId, data: &[u8]) -> (String, Vec<u8>) {
    let subject = FooterSubject::Copy {
        block,
        length: data.len() as u64,
        expiry: ExpiryClass(0),
        copies: Vec::new(),
    };
    (ColdName::Copy(block).path(), cold_file(subject, data))
}

/// An [`Fs`] over a fake that fails the call numbered `fail_at` (counting from the arming) with an
/// I/O error, so that a test can fail each call of a recovery in turn.
pub struct Flaky {
    pub inner: Arc<FakeFs>,
    calls: AtomicUsize,
    fail_at: AtomicUsize,
}

impl Flaky {
    pub fn new(inner: Arc<FakeFs>) -> Self {
        Self {
            inner,
            calls: AtomicUsize::new(0),
            fail_at: AtomicUsize::new(usize::MAX),
        }
    }

    /// Makes call number `k` from now on fail.
    pub fn arm(&self, k: usize) {
        self.calls.store(0, Ordering::SeqCst);
        self.fail_at.store(k, Ordering::SeqCst);
    }

    /// The calls made since the arming.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn tick(&self) -> io::Result<()> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == self.fail_at.load(Ordering::SeqCst) {
            return Err(io::Error::other("injected failure"));
        }
        Ok(())
    }
}

impl Fs for Flaky {
    fn list(&self, dir: &str) -> io::Result<Vec<String>> {
        self.tick()?;
        self.inner.list(dir)
    }

    fn len(&self, path: &str) -> io::Result<u64> {
        self.tick()?;
        self.inner.len(path)
    }

    fn read_at(&self, path: &str, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        self.tick()?;
        self.inner.read_at(path, offset, len)
    }

    fn append(&self, path: &str, bytes: &[u8]) -> io::Result<()> {
        self.tick()?;
        self.inner.append(path, bytes)
    }

    fn truncate(&self, path: &str, len: u64) -> io::Result<()> {
        self.tick()?;
        self.inner.truncate(path, len)
    }

    fn sync(&self, path: &str) -> io::Result<()> {
        self.tick()?;
        self.inner.sync(path)
    }

    fn remove(&self, path: &str) -> io::Result<()> {
        self.tick()?;
        self.inner.remove(path)
    }
}
