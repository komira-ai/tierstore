//! The rig of the large-object tests: a cache (`super::super::testing::Rig`) with a real
//! [`FetchEngine`] attached, over the in-memory entry store and the fake upstream of
//! `tierstore-fetch-api`, on an injected clock that no test moves. The fake
//! upstream holds every object: a stored one has a location that names its generation and a hot
//! copy. Its `get` answers `412` to an `If-Match` other than the object's generation.

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures::StreamExt as _;
use tierstore_fetch::timer::ManualTimer;
use tierstore_fetch::{EngineConfig, EngineParts, FetchEngine};
use tierstore_fetch_api::budget::{Budget, Charge, Observation, Permit};
use tierstore_fetch_api::clock::ManualClock;
use tierstore_fetch_api::entry::EntryStore;
use tierstore_fetch_api::fake::MemEntryStore;
use tierstore_fetch_api::fake::{FakeObject, FakeUpstream};
use tierstore_fetch_api::ids::{ByteRange as PackRange, HolderId, PackId, PackKey, PackRoot};
use tierstore_fetch_api::upstream::{
    DeviceRef, GetRequest, Located, Location, PartResponse, Report, Source, SourceKind, Upstream,
    UpstreamError,
};
use tierstore_format::ids::{Generation, NodeId};
use tierstore_hashpool::HashPool;
use tokio::sync::{Notify, Semaphore};

use super::super::CacheConfig;
use super::super::testing::Rig;
use super::{LargeObjects, LargeParts};
use crate::{Backend, Error, Meta, Span, Window};

/// One cell: 1 MiB.
pub(in crate::server::local_cache) const MIB: u64 = 1 << 20;

/// The `whole_fill_limit` of the rig's cache: an object of more than one cell is large.
pub(in crate::server::local_cache) const LIMIT: u64 = MIB;

/// The holder of the front's reads.
pub(in crate::server::local_cache) const HOLDER: HolderId = HolderId(7);

/// A budget that grants every part at once, with the parts of 32 MiB of 4.4.
pub(in crate::server::local_cache) struct Unlimited;

#[async_trait::async_trait]
impl Budget for Unlimited {
    async fn acquire(&self, _charge: Charge) -> Permit {
        Permit::new(|| {})
    }

    fn observe(&self, _observation: Observation) {}

    fn part_size(&self, _pack_size: u64) -> u64 {
        32 * MIB
    }
}

/// The fake upstream, with every GET recorded in order, scripted `LOCATE` answers, and an
/// optional gate that holds `LOCATE`s until the test lets them go.
pub(in crate::server::local_cache) struct Gated {
    /// The fake that answers.
    pub fake: FakeUpstream,
    /// Woken at each `LOCATE` that reaches the upstream.
    arrived: Notify,
    /// `LOCATE` answers to give before the fake's own, in order.
    locates: Mutex<Vec<Result<Located, UpstreamError>>>,
    /// When set, every `LOCATE` waits for a permit of it before it is answered.
    locate_gate: Mutex<Option<Arc<Semaphore>>>,
    /// The `LOCATE`s that have reached the upstream (answered or waiting).
    locating: AtomicUsize,
}

impl Gated {
    fn new() -> Self {
        Self {
            fake: FakeUpstream::new(),
            arrived: Notify::new(),
            locates: Mutex::new(Vec::new()),
            locate_gate: Mutex::new(None),
            locating: AtomicUsize::new(0),
        }
    }

    /// Holds every `LOCATE` from now until a permit of the semaphore returned is added for it.
    pub(in crate::server::local_cache) fn hold_locates(&self) -> Arc<Semaphore> {
        let gate = Arc::new(Semaphore::new(0));
        *self.locate_gate.lock().unwrap() = Some(gate.clone());
        gate
    }

    /// Waits until `n` `LOCATE`s have reached the upstream (answered or held).
    pub(in crate::server::local_cache) async fn located(&self, n: usize) {
        loop {
            let arrived = self.arrived.notified();
            if self.locating.load(Ordering::SeqCst) >= n {
                return;
            }
            arrived.await;
        }
    }

    /// The next `LOCATE` (of any key) answers `answer`.
    pub(in crate::server::local_cache) fn script_locate(
        &self,
        answer: Result<Located, UpstreamError>,
    ) {
        self.locates.lock().unwrap().push(answer);
    }
}

#[async_trait::async_trait]
impl Upstream for Gated {
    async fn locate(&self, key: &PackKey) -> Result<Located, UpstreamError> {
        let gate = self.locate_gate.lock().unwrap().clone();
        self.locating.fetch_add(1, Ordering::SeqCst);
        self.arrived.notify_waiters();
        if let Some(gate) = gate {
            gate.acquire().await.unwrap().forget();
        }
        let scripted = {
            let mut locates = self.locates.lock().unwrap();
            (!locates.is_empty()).then(|| locates.remove(0))
        };
        match scripted {
            Some(answer) => {
                // Recorded by the fake as every `LOCATE` is.
                let _ = self.fake.locate(key).await;
                answer
            }
            None => self.fake.locate(key).await,
        }
    }

    async fn get(&self, req: GetRequest) -> Result<PartResponse, UpstreamError> {
        self.fake.get(req).await
    }

    async fn report(&self, key: &PackKey, source: &Source, range: PackRange, report: Report) {
        self.fake.report(key, source, range, report).await;
    }
}

/// A cache with the large-object path attached.
pub(in crate::server::local_cache) struct LargeRig {
    /// The cache, its memory upstream (for the small objects of `obj/`) and its clock.
    pub rig: Rig,
    /// The engine's upstream.
    pub upstream: Arc<Gated>,
    /// The engine's entry store.
    pub store: Arc<MemEntryStore>,
    /// The engine.
    pub engine: Arc<FetchEngine>,
    /// The path, attached to the cache.
    pub large: Arc<LargeObjects>,
}

impl LargeRig {
    /// A rig named `name`, whose cache's `whole_fill_limit` is [`LIMIT`].
    pub(in crate::server::local_cache) fn new(name: &str) -> Self {
        Self::with(name, usize::MAX, EngineConfig::default())
    }

    /// The same, with a book of at most `limit` objects and the engine's settings `config`.
    pub(in crate::server::local_cache) fn with(
        name: &str,
        limit: usize,
        config: EngineConfig,
    ) -> Self {
        Self::build(name, limit, config, |_| {}, |store| store)
    }

    /// The general rig: the cache's settings changed by `change` after the limit is set to
    /// [`LIMIT`], and the entry store the front reads from made by `front` out of the engine's
    /// (the engine always fills the in-memory store; `front` may wrap it).
    pub(in crate::server::local_cache) fn build(
        name: &str,
        limit: usize,
        config: EngineConfig,
        change: impl FnOnce(&mut CacheConfig),
        front: impl FnOnce(Arc<dyn EntryStore>) -> Arc<dyn EntryStore>,
    ) -> Self {
        let rig = Rig::with(name, |cache| {
            cache.whole_limit = LIMIT;
            change(cache);
        });
        let clock = ManualClock::new();
        let upstream = Arc::new(Gated::new());
        let store = Arc::new(MemEntryStore::new());
        let parts = EngineParts {
            store: store.clone(),
            upstream: upstream.clone(),
            budget: Arc::new(Unlimited),
            // An object of the S3 front has no cell digests: the engine never asks the pool.
            verifier: Arc::new(
                HashPool::start(NonZeroUsize::MIN, Arc::new(clock.clone())).unwrap(),
            ),
            // No test moves the engine's clock: no timer of the engine fires.
            timer: Arc::new(ManualTimer::new(clock)),
        };
        let engine = Arc::new(FetchEngine::new(
            parts,
            config,
            &tokio::runtime::Handle::current(),
        ));
        let large = Arc::new(LargeObjects::with_limit(
            LargeParts {
                engine: engine.clone(),
                upstream: upstream.clone(),
                store: front(store.clone() as Arc<dyn EntryStore>),
                holder: HOLDER,
            },
            limit,
        ));
        assert!(rig.cache.route_large(large.clone()));
        Self {
            rig,
            upstream,
            store,
            engine,
            large,
        }
    }

    /// Puts a stored object: its location names `generation` and a hot copy.
    pub(in crate::server::local_cache) fn stored(
        &self,
        key: &str,
        bytes: &Bytes,
        generation: Generation,
    ) {
        let size = bytes.len() as u64;
        let location = Location {
            generation,
            size: Some(size),
            sources: vec![hot(size)],
            conflicts: Vec::new(),
        };
        self.insert(key, bytes, generation, location);
    }

    /// Puts `key` in the fake upstream with `location`.
    pub(in crate::server::local_cache) fn insert(
        &self,
        key: &str,
        bytes: &Bytes,
        generation: Generation,
        location: Location,
    ) {
        let object = FakeObject {
            generation,
            bytes: bytes.clone(),
            location,
        };
        self.upstream.fake.insert(PackKey(key.to_owned()), object);
    }

    /// The GETs the upstream answered or holds, in order.
    pub(in crate::server::local_cache) fn gets(&self) -> Vec<GetRequest> {
        self.upstream.fake.gets()
    }

    /// The `LOCATE`s asked, in order.
    pub(in crate::server::local_cache) fn locates(&self) -> usize {
        self.upstream.fake.locates().len()
    }

    /// Opens a read through the S3 front's path ([`Backend::open_read`]).
    pub(in crate::server::local_cache) async fn open(
        &self,
        key: &str,
        span: Option<Span>,
    ) -> Result<crate::Opened, Error> {
        self.rig.cache.open_read(key, span).await
    }

    /// Reads `span` of `key` through the S3 front's path: what it says of the object, the window
    /// and every byte, or the error of the call or of the body.
    pub(in crate::server::local_cache) async fn read(
        &self,
        key: &str,
        span: Option<Span>,
    ) -> Result<(Meta, Window, Vec<u8>), Error> {
        let opened = self.open(key, span).await?;
        let (meta, window) = (opened.meta, opened.window);
        let bytes = collect(opened.body).await?;
        Ok((meta, window, bytes))
    }

    /// The entry of `key` at `generation`, if the engine opened one.
    pub(in crate::server::local_cache) fn entry(
        &self,
        key: &str,
        generation: Generation,
    ) -> Option<Arc<tierstore_fetch_api::fake::MemEntry>> {
        self.store.mem_entry(&pack_id(key, generation))
    }
}

/// Every byte of `body`, or its first error.
pub(in crate::server::local_cache) async fn collect(
    mut body: crate::ReadBody,
) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    while let Some(piece) = body.next().await {
        bytes.extend_from_slice(&piece?);
    }
    Ok(bytes)
}

/// The pack of an S3-front object.
pub(in crate::server::local_cache) fn pack_id(key: &str, generation: Generation) -> PackId {
    PackId {
        key: PackKey(key.to_owned()),
        root: PackRoot::Etag(generation),
    }
}

/// `len` bytes that differ from cell to cell and with `seed`.
pub(in crate::server::local_cache) fn object(len: u64, seed: u8) -> Bytes {
    (0..len)
        .map(|i| (i % 251) as u8 ^ ((i >> 20) as u8).wrapping_mul(17) ^ seed)
        .collect::<Vec<u8>>()
        .into()
}

/// A generation made of one byte.
pub(in crate::server::local_cache) fn generation(b: u8) -> Generation {
    Generation([b; 16])
}

/// A hot copy of the whole of an object of `len` bytes.
pub(in crate::server::local_cache) fn hot(len: u64) -> Source {
    Source {
        device: DeviceRef::Hot(NodeId(1)),
        kind: SourceKind::Hot {
            file: "seg-1".to_owned(),
            offset: 0,
        },
        extent: PackRange { offset: 0, len },
    }
}

/// The span `first..=last`.
pub(in crate::server::local_cache) fn span(first: u64, last: u64) -> Option<Span> {
    Some(Span::From {
        first,
        last: Some(last),
    })
}
