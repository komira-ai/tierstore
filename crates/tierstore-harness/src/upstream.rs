//! A per-cell gated upstream: [`CellUpstream`] implements `Upstream` so that every part's body
//! yields one cell-piece at a time, and only when the test has released it (section 14: "a fake
//! upstream with scripted per-part and per-cell completion order and corruption").
//!
//! - Every `get` is a part with a [`PartId`], its index in request order (a `get` that fails at
//!   once has one too, with no pieces). The response is returned at once; its body is the range as
//!   pieces, a piece being the intersection of the range with one cell of the object (so the first
//!   and the last piece of an unaligned range are partial).
//! - A piece is yielded only after [`CellUpstream::release`] (or one of its variants) released it.
//!   Within a part the pieces come in order; across parts, in whatever order the test releases
//!   them. A body that is pending when a piece is released is woken. The end of a body, and the
//!   error of [`Script::FailAfter`], need no release: they come as soon as every piece before them
//!   was yielded.
//! - [`Script`]s queue per (key, device) and globally; the per-device queue wins; the default is
//!   [`Script::Serve`] with no signals. A scripted [`Script::Fail`] is answered before the object
//!   is looked up; every other script answers `NotFound` for an unknown key, then applies the
//!   `If-Match` rule of the fake of `tierstore-fetch-api` (a request that names another generation
//!   is `412`, except under [`Script::ServeAs`]), then a transport error for a range outside the
//!   object.
//! - Accounting: [`CellUpstream::gets`], [`CellUpstream::bytes_delivered`],
//!   [`CellUpstream::cell_deliveries`], [`CellUpstream::pending_pieces`],
//!   [`CellUpstream::is_cancelled`], [`CellUpstream::locates`], [`CellUpstream::reports`]; and a
//!   [`Progress`] that marks `"get"` per request and `"piece"` per piece yielded, for a test to
//!   await instead of time.
//!
//! `locate` answers the next scripted answer of the key, else the object's location, else a
//! definitive miss.

use std::collections::{BTreeMap, VecDeque};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use bytes::Bytes;
use futures::{Stream, StreamExt};
use tierstore_fetch_api::ids::{ByteRange, PackKey};
use tierstore_fetch_api::upstream::{
    DeviceRef, DriveSignals, GetRequest, Located, PartResponse, Report, Source, Upstream,
    UpstreamError,
};
use tierstore_format::ids::Generation;

pub use tierstore_fetch_api::fake::FakeObject;

use crate::progress::{Progress, lock};

/// A part: its index in request order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PartId(pub usize);

/// What the upstream does with the next `get` it takes the script for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Script {
    /// Serves the range, with these signals.
    Serve(DriveSignals),
    /// Serves the range with the first byte of the piece of object cell `cell` flipped, if the
    /// range covers that cell.
    CorruptCell {
        /// The object cell.
        cell: u64,
    },
    /// Serves the first `pieces` pieces (all of them, if fewer), then a transport error.
    FailAfter {
        /// Pieces yielded before the error.
        pieces: usize,
    },
    /// Fails the request with this error.
    Fail(UpstreamError),
    /// Answers, then the body never yields, whatever is released.
    Hang,
    /// Serves the range under this generation, without the `If-Match` check.
    ServeAs(Generation),
}

#[derive(Debug, Clone)]
struct Piece {
    cell: u64,
    bytes: Bytes,
}

#[derive(Debug)]
struct PartState {
    key: PackKey,
    pieces: Vec<Piece>,
    released: usize,
    delivered: usize,
    hang: bool,
    tail: Option<UpstreamError>,
    waker: Option<Waker>,
    cancelled: bool,
}

impl PartState {
    fn new(key: PackKey, pieces: Vec<Piece>) -> Self {
        Self {
            key,
            pieces,
            released: 0,
            delivered: 0,
            hang: false,
            tail: None,
            waker: None,
            cancelled: false,
        }
    }
}

#[derive(Debug, Default)]
struct State {
    objects: BTreeMap<PackKey, FakeObject>,
    located: BTreeMap<PackKey, VecDeque<Result<Located, UpstreamError>>>,
    device_scripts: BTreeMap<(PackKey, DeviceRef), VecDeque<Script>>,
    global_scripts: VecDeque<Script>,
    gets: Vec<GetRequest>,
    parts: Vec<PartState>,
    delivered_bytes: u64,
    cells: BTreeMap<(PackKey, u64), u64>,
    locates: Vec<PackKey>,
    reports: Vec<(PackKey, Source, ByteRange, Report)>,
}

impl State {
    fn part(&mut self, id: PartId) -> &mut PartState {
        self.parts
            .get_mut(id.0)
            .unwrap_or_else(|| panic!("no part {} was requested", id.0))
    }
}

/// An upstream in memory whose parts deliver cell by cell as the test releases them.
#[derive(Debug)]
pub struct CellUpstream {
    cell_size: u64,
    state: Arc<Mutex<State>>,
    progress: Progress,
}

fn transport(req: &GetRequest, reason: &str) -> UpstreamError {
    UpstreamError::Transport {
        key: req.key.clone(),
        device: req.source.device,
        reason: reason.to_owned(),
    }
}

/// The pieces of the request's range, one per cell it touches.
fn pieces_of(
    req: &GetRequest,
    object: &FakeObject,
    cell_size: u64,
) -> Result<Vec<Piece>, UpstreamError> {
    let end = req
        .range
        .end()
        .filter(|e| *e <= object.bytes.len() as u64)
        .ok_or_else(|| transport(req, "range outside the object"))?;
    let mut pieces = Vec::new();
    let mut at = req.range.offset;
    while at < end {
        let cell = at / cell_size;
        let stop = end.min((cell + 1) * cell_size);
        // `at <= stop <= end <= len`, and the bytes are in memory, so these fit a usize.
        let bytes = object.bytes.slice(at as usize..stop as usize);
        pieces.push(Piece { cell, bytes });
        at = stop;
    }
    Ok(pieces)
}

/// The part a script and an object make of a request, and the generation and signals of the
/// response.
fn build(
    req: &GetRequest,
    object: Option<&FakeObject>,
    script: Script,
    cell_size: u64,
) -> Result<(PartState, Generation, DriveSignals), UpstreamError> {
    let mut signals = DriveSignals::default();
    let mut generation = None;
    let mut corrupt = None;
    let mut fail_after = None;
    let mut hang = false;
    match script {
        Script::Fail(e) => return Err(e),
        Script::Serve(s) => signals = s,
        Script::CorruptCell { cell } => corrupt = Some(cell),
        Script::FailAfter { pieces } => fail_after = Some(pieces),
        Script::Hang => hang = true,
        Script::ServeAs(g) => generation = Some(g),
    }
    let object = object.ok_or_else(|| UpstreamError::NotFound {
        key: req.key.clone(),
        device: req.source.device,
    })?;
    if generation.is_none() && req.generation.is_some_and(|g| g != object.generation) {
        return Err(UpstreamError::PreconditionFailed {
            key: req.key.clone(),
            device: req.source.device,
            actual: Some(object.generation),
        });
    }
    let mut pieces = pieces_of(req, object, cell_size)?;
    if let Some(piece) = pieces.iter_mut().find(|p| Some(p.cell) == corrupt) {
        let mut flipped = piece.bytes.to_vec();
        flipped.iter_mut().take(1).for_each(|b| *b ^= 0xff);
        piece.bytes = Bytes::from(flipped);
    }
    let tail = fail_after.map(|n| {
        pieces.truncate(n);
        transport(req, "stream ended early")
    });
    let mut part = PartState::new(req.key.clone(), pieces);
    part.hang = hang;
    part.tail = tail;
    Ok((part, generation.unwrap_or(object.generation), signals))
}

impl CellUpstream {
    /// An upstream with no object whose cells are `cell_size` bytes (the real size is
    /// `1 << CELL_SHIFT`; tests use small ones).
    ///
    /// # Panics
    ///
    /// When `cell_size` is not a power of two.
    #[must_use]
    pub fn new(cell_size: u64) -> Self {
        assert!(
            cell_size.is_power_of_two(),
            "the cell size {cell_size} is not a power of two"
        );
        Self {
            cell_size,
            state: Arc::default(),
            progress: Progress::new(),
        }
    }

    /// The cell size.
    #[must_use]
    pub fn cell_size(&self) -> u64 {
        self.cell_size
    }

    /// The progress events: `"get"` once per request, `"piece"` once per piece yielded.
    #[inline(never)]
    #[must_use]
    pub fn progress(&self) -> Progress {
        self.progress.clone()
    }

    /// Adds or replaces an object under `key`.
    pub fn insert(&self, key: PackKey, object: FakeObject) {
        lock(&self.state).objects.insert(key, object);
    }

    /// Queues the answer of the next `locate` of `key`.
    pub fn script_locate(&self, key: PackKey, answer: Result<Located, UpstreamError>) {
        lock(&self.state)
            .located
            .entry(key)
            .or_default()
            .push_back(answer);
    }

    /// Queues a script for the next `get` of `key` from `device`; these win over the global queue.
    pub fn script(&self, key: PackKey, device: DeviceRef, script: Script) {
        lock(&self.state)
            .device_scripts
            .entry((key, device))
            .or_default()
            .push_back(script);
    }

    /// Queues a script for the next `get` that has none of its own key and device.
    pub fn script_global(&self, script: Script) {
        lock(&self.state).global_scripts.push_back(script);
    }

    /// Releases the next `n` pieces of `part` (the ones it has left, if fewer) and wakes its body
    /// if that is waiting.
    ///
    /// # Panics
    ///
    /// When `part` was never requested.
    pub fn release(&self, part: PartId, n: usize) {
        let waker = {
            let mut s = lock(&self.state);
            let p = s.part(part);
            p.released = p.released.saturating_add(n).min(p.pieces.len());
            p.waker.take()
        };
        waker.into_iter().for_each(Waker::wake);
    }

    /// Releases every piece of `part`.
    ///
    /// # Panics
    ///
    /// When `part` was never requested.
    pub fn release_all(&self, part: PartId) {
        self.release(part, usize::MAX);
    }

    /// Releases one piece of each part listed, in the order listed (a part listed twice gets two).
    ///
    /// # Panics
    ///
    /// When a part was never requested.
    pub fn release_in_order(&self, parts: &[PartId]) {
        parts.iter().for_each(|p| self.release(*p, 1));
    }

    /// Every request so far, in order; the index of a request is its [`PartId`].
    #[must_use]
    pub fn gets(&self) -> Vec<GetRequest> {
        lock(&self.state).gets.clone()
    }

    /// The bytes yielded to bodies so far, over every part.
    #[must_use]
    pub fn bytes_delivered(&self) -> u64 {
        lock(&self.state).delivered_bytes
    }

    /// How many pieces of each (object, cell) were yielded so far; a cell never yielded is
    /// absent.
    #[must_use]
    pub fn cell_deliveries(&self) -> BTreeMap<(PackKey, u64), u64> {
        lock(&self.state).cells.clone()
    }

    /// The pieces of `part` that its body has not yielded yet, released or not.
    ///
    /// # Panics
    ///
    /// When `part` was never requested.
    #[must_use]
    pub fn pending_pieces(&self, part: PartId) -> usize {
        let mut s = lock(&self.state);
        let p = s.part(part);
        p.pieces.len() - p.delivered
    }

    /// Whether the body of `part` was dropped before it ended (or before its error).
    ///
    /// # Panics
    ///
    /// When `part` was never requested.
    #[must_use]
    pub fn is_cancelled(&self, part: PartId) -> bool {
        lock(&self.state).part(part).cancelled
    }

    /// Every key located so far, in order.
    #[must_use]
    pub fn locates(&self) -> Vec<PackKey> {
        lock(&self.state).locates.clone()
    }

    /// Every report so far, in order.
    #[must_use]
    pub fn reports(&self) -> Vec<(PackKey, Source, ByteRange, Report)> {
        lock(&self.state).reports.clone()
    }
}

/// The body of one part.
struct Body {
    state: Arc<Mutex<State>>,
    progress: Progress,
    part: PartId,
    done: bool,
}

impl Stream for Body {
    type Item = Result<Bytes, UpstreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        let mut guard = lock(&this.state);
        let s = &mut *guard;
        let p = s.part(this.part);
        if p.hang {
            return Poll::Pending;
        }
        if p.delivered == p.pieces.len() {
            this.done = true;
            return Poll::Ready(p.tail.take().map(Err));
        }
        if p.delivered == p.released {
            p.waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let piece = p.pieces[p.delivered].clone();
        p.delivered += 1;
        let key = p.key.clone();
        s.delivered_bytes += piece.bytes.len() as u64;
        *s.cells.entry((key, piece.cell)).or_default() += 1;
        drop(guard);
        this.progress.mark("piece");
        Poll::Ready(Some(Ok(piece.bytes)))
    }
}

impl Drop for Body {
    fn drop(&mut self) {
        if !self.done {
            lock(&self.state).part(self.part).cancelled = true;
        }
    }
}

impl CellUpstream {
    /// Logs a request and makes its part: the part, and what the response says or why it fails.
    fn start(
        &self,
        req: &GetRequest,
    ) -> (PartId, Result<(Generation, DriveSignals), UpstreamError>) {
        let mut s = lock(&self.state);
        s.gets.push(req.clone());
        let script = s
            .device_scripts
            .get_mut(&(req.key.clone(), req.source.device))
            .and_then(VecDeque::pop_front)
            .or_else(|| s.global_scripts.pop_front())
            .unwrap_or(Script::Serve(DriveSignals::default()));
        let built = build(req, s.objects.get(&req.key), script, self.cell_size);
        let part = PartId(s.parts.len());
        match built {
            Ok((state, generation, signals)) => {
                s.parts.push(state);
                (part, Ok((generation, signals)))
            }
            Err(e) => {
                s.parts.push(PartState::new(req.key.clone(), Vec::new()));
                (part, Err(e))
            }
        }
    }

    fn locate_now(&self, key: &PackKey) -> Result<Located, UpstreamError> {
        let mut s = lock(&self.state);
        s.locates.push(key.clone());
        if let Some(answer) = s.located.get_mut(key).and_then(VecDeque::pop_front) {
            return answer;
        }
        Ok(match s.objects.get(key) {
            Some(o) => Located::Found(o.location.clone()),
            None => Located::DefinitiveMiss,
        })
    }
}

#[async_trait::async_trait]
impl Upstream for CellUpstream {
    async fn locate(&self, key: &PackKey) -> Result<Located, UpstreamError> {
        self.locate_now(key)
    }

    async fn get(&self, req: GetRequest) -> Result<PartResponse, UpstreamError> {
        let (part, started) = self.start(&req);
        self.progress.mark("get");
        let (generation, signals) = started?;
        let body = Body {
            state: Arc::clone(&self.state),
            progress: self.progress.clone(),
            part,
            done: false,
        };
        Ok(PartResponse {
            generation,
            signals,
            body: body.boxed(),
        })
    }

    async fn report(&self, key: &PackKey, source: &Source, range: ByteRange, report: Report) {
        lock(&self.state)
            .reports
            .push((key.clone(), source.clone(), range, report));
    }
}
