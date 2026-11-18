//! The ranged serving of a large object from the present cells of its entry, and the `If-Match`
//! of a stored object's parts: which cells a read leaves, what a repeated read asks of the
//! upstream, the unaligned ranges, the ensure-ahead window, the hold on the entry, and the fills
//! that fail or are lost. Time is the engine's manual clock, which no test here moves; the tests
//! wait on the engine's progress and on the upstream's arrivals.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::StreamExt as _;
use tierstore_fetch::EngineConfig;
use tierstore_fetch_api::cell::CellByte;
use tierstore_fetch_api::entry::{
    CellCounts, ClaimedCell, CommitReport, Entry, EntryError, EntryFds, EntryInfo, EntrySpec,
    EntryStore, EvictStep, HolderCounters, PunchReport, Settle, StateView,
};
use tierstore_fetch_api::fake::MemEntry;
use tierstore_fetch_api::ids::{CellRun, HolderId, PackId};
use tierstore_fetch_api::page::CounterRegion;
use tierstore_fetch_api::upstream::{Located, Location};

use super::testing::{LargeRig, MIB, collect, generation, hot, object, span};
use crate::{ByteRange, Error, Span, Window};

/// The cells of the entry that are servable, in order.
fn servable(entry: &MemEntry) -> Vec<u32> {
    let state = entry.state();
    (0..state.cell_count())
        .filter(|&cell| state.load(cell).is_some_and(CellByte::servable))
        .collect()
}

/// The bytes the GETs from the `from`-th on asked for, as the first byte and the end of each.
fn asked(rig: &LargeRig, from: usize) -> Vec<(u64, u64)> {
    rig.gets()[from..]
        .iter()
        .map(|get| (get.range.offset, get.range.offset + get.range.len))
        .collect()
}

/// Whether `error` is an `Unavailable` whose text has `what`.
fn unavailable(error: &Error, what: &str) -> bool {
    matches!(error, Error::Unavailable(text) if text.contains(what))
}

/// A store that gives the front no entry for its first `misses` calls of `get`, then the
/// delegate's. The engine fills the delegate itself.
struct Missing {
    inner: Arc<dyn EntryStore>,
    misses: Arc<AtomicUsize>,
}

impl Missing {
    fn wrap(inner: Arc<dyn EntryStore>, misses: usize) -> Arc<dyn EntryStore> {
        Self::shared(inner, Arc::new(AtomicUsize::new(misses)))
    }

    /// The same, with the count of misses left in `misses`, which the test may set.
    fn shared(inner: Arc<dyn EntryStore>, misses: Arc<AtomicUsize>) -> Arc<dyn EntryStore> {
        Arc::new(Self { inner, misses })
    }
}

impl EntryStore for Missing {
    fn open(&self, spec: &EntrySpec) -> Result<Arc<dyn Entry>, EntryError> {
        self.inner.open(spec)
    }

    fn get(&self, pack: &PackId) -> Option<Arc<dyn Entry>> {
        let miss = self
            .misses
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if miss { None } else { self.inner.get(pack) }
    }

    fn remove(&self, pack: &PackId) -> Result<bool, EntryError> {
        self.inner.remove(pack)
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
        self.inner.register_holder(holder, counters)
    }

    fn attach(
        &self,
        pack: &PackId,
        holder: HolderId,
        region: CounterRegion,
    ) -> Result<(), EntryError> {
        self.inner.attach(pack, holder, region)
    }

    fn drop_holder(&self, holder: HolderId) -> u32 {
        self.inner.drop_holder(holder)
    }
}

/// A store whose entries show the front a cell as absent for the first `after` loads of their
/// state (the loads that follow a read), then as it is.
struct Flaky {
    inner: Arc<dyn EntryStore>,
    hidden: Arc<AtomicUsize>,
}

impl Flaky {
    fn wrap(inner: Arc<dyn EntryStore>, after: usize) -> Arc<dyn EntryStore> {
        Self::shared(inner, Arc::new(AtomicUsize::new(after)))
    }

    /// The same, with the count of loads left to hide in `hidden`, which the test may set.
    fn shared(inner: Arc<dyn EntryStore>, hidden: Arc<AtomicUsize>) -> Arc<dyn EntryStore> {
        Arc::new(Self { inner, hidden })
    }
}

impl EntryStore for Flaky {
    fn open(&self, spec: &EntrySpec) -> Result<Arc<dyn Entry>, EntryError> {
        self.inner.open(spec)
    }

    fn get(&self, pack: &PackId) -> Option<Arc<dyn Entry>> {
        let inner = self.inner.get(pack)?;
        let view = View {
            inner: inner.clone(),
            hidden: self.hidden.clone(),
        };
        Some(Arc::new(Doubt { inner, view }))
    }

    fn remove(&self, pack: &PackId) -> Result<bool, EntryError> {
        self.inner.remove(pack)
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
        self.inner.register_holder(holder, counters)
    }

    fn attach(
        &self,
        pack: &PackId,
        holder: HolderId,
        region: CounterRegion,
    ) -> Result<(), EntryError> {
        self.inner.attach(pack, holder, region)
    }

    fn drop_holder(&self, holder: HolderId) -> u32 {
        self.inner.drop_holder(holder)
    }
}

/// The state of an entry, absent for as many loads as `hidden` still counts.
struct View {
    inner: Arc<dyn Entry>,
    hidden: Arc<AtomicUsize>,
}

impl StateView for View {
    fn cell_count(&self) -> u32 {
        self.inner.state().cell_count()
    }

    fn load(&self, cell: u32) -> Option<CellByte> {
        let hide = self
            .hidden
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if hide {
            None
        } else {
            self.inner.state().load(cell)
        }
    }
}

/// An entry that is its delegate but for the state it shows.
struct Doubt {
    inner: Arc<dyn Entry>,
    view: View,
}

impl Entry for Doubt {
    fn pack(&self) -> &PackId {
        self.inner.pack()
    }

    fn size(&self) -> u64 {
        self.inner.size()
    }

    fn state(&self) -> &dyn StateView {
        &self.view
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
        self.inner.fds()
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

/// A rig whose front's entry store is `wrap` over the engine's.
fn wrapped(name: &str, wrap: impl FnOnce(Arc<dyn EntryStore>) -> Arc<dyn EntryStore>) -> LargeRig {
    LargeRig::build(name, usize::MAX, EngineConfig::default(), |_| {}, wrap)
}

/// Asserts that a read of a whole stored object sends the generation `LOCATE` named as `If-Match`
/// on every GET, gives the object's bytes, its size and its generation as the tag, and asks the
/// front's `LOCATE` once.
#[tokio::test]
async fn a_whole_read_of_a_stored_object_matches_its_generation_on_every_get() {
    let rig = LargeRig::new("cells-if-match");
    let (g, bytes) = (generation(3), object(6 * MIB + 77, 1));
    rig.stored("cas/whole", &bytes, g);
    let (meta, window, served) = rig.read("cas/whole", None).await.unwrap();
    assert_eq!(served, bytes.to_vec());
    assert_eq!(window, Window::Whole);
    assert_eq!(meta.size, bytes.len() as u64);
    assert_eq!(meta.tag, g.to_string());
    let gets = rig.gets();
    assert!(!gets.is_empty());
    assert!(gets.iter().all(|get| get.generation == Some(g)));
    let counts = rig.large.counters().counts();
    assert_eq!((counts.locates, counts.alone, counts.routed), (1, 0, 1));
    let engine_locates = rig.engine.stats().locates;
    assert_eq!(rig.locates() as u64, counts.locates + engine_locates);
}

/// Asserts that a read of cells 2 and 3 of a 10 MiB object leaves exactly those cells present in
/// the entry, and that a second read inside them sends no GET and no `LOCATE` at all (the book
/// knows the object, and the engine is not asked for present cells), starts no fill, and serves
/// the same bytes.
#[tokio::test]
async fn a_ranged_read_leaves_only_its_cells_and_a_repeat_asks_for_nothing() {
    let rig = LargeRig::new("cells-partial");
    let (g, bytes) = (generation(4), object(10 * MIB, 2));
    rig.stored("cas/ten", &bytes, g);
    let (_, window, served) = rig
        .read("cas/ten", span(2 * MIB, 4 * MIB - 1))
        .await
        .unwrap();
    assert_eq!(window, Window::Part(ByteRange::new(2 * MIB, 2 * MIB)));
    assert_eq!(served, bytes[(2 * MIB) as usize..(4 * MIB) as usize]);
    let entry = rig.entry("cas/ten", g).unwrap();
    assert_eq!(entry.state().cell_count(), 10);
    assert_eq!(servable(&entry), vec![2, 3]);
    let (gets, total_gets) = (rig.gets().len(), rig.engine.stats().total_gets());
    assert!(gets >= 1);
    let (locates, fills) = (rig.locates(), rig.engine.stats().fills);

    let (_, _, again) = rig
        .read("cas/ten", span(2 * MIB + 10, 3 * MIB + 10))
        .await
        .unwrap();
    assert_eq!(
        again,
        bytes[(2 * MIB + 10) as usize..(3 * MIB + 11) as usize]
    );
    assert_eq!(rig.gets().len(), gets);
    assert_eq!(rig.engine.stats().total_gets(), total_gets);
    assert_eq!((rig.locates(), rig.engine.stats().fills), (locates, fills));
    let counts = rig.large.counters().counts();
    assert_eq!((counts.locates, counts.known), (1, 1));
    assert_eq!(servable(&entry), vec![2, 3]);
}

/// Asserts that a read that overlaps a present cell and an absent one fetches only the absent
/// cell from the upstream, with the generation as `If-Match`, and serves the object's bytes.
#[tokio::test]
async fn a_read_over_present_and_absent_cells_fetches_only_the_absent_ones() {
    let rig = LargeRig::new("cells-overlap");
    let (g, bytes) = (generation(5), object(10 * MIB, 3));
    rig.stored("cas/ten", &bytes, g);
    rig.read("cas/ten", span(2 * MIB, 4 * MIB - 1))
        .await
        .unwrap();
    let before = rig.gets().len();
    let (_, _, served) = rig
        .read("cas/ten", span(3 * MIB + 5, 5 * MIB - 1))
        .await
        .unwrap();
    assert_eq!(served, bytes[(3 * MIB + 5) as usize..(5 * MIB) as usize]);
    let new = asked(&rig, before);
    assert!(!new.is_empty());
    assert!(
        new.iter()
            .all(|&(from, to)| from >= 4 * MIB && to <= 5 * MIB)
    );
    assert!(
        rig.gets()[before..]
            .iter()
            .all(|get| get.generation == Some(g))
    );
    let entry = rig.entry("cas/ten", g).unwrap();
    assert_eq!(servable(&entry), vec![2, 3, 4]);
}

/// Asserts that a read that begins and ends in the middle of cells serves exactly those bytes,
/// with the window the span names, and leaves the cells it touched.
#[tokio::test]
async fn a_read_from_the_middle_of_a_cell_to_the_middle_of_another_serves_its_bytes() {
    let rig = LargeRig::new("cells-unaligned");
    let (g, bytes) = (generation(6), object(8 * MIB, 4));
    rig.stored("cas/eight", &bytes, g);
    let (first, last) = (MIB + 100, 3 * MIB + 49);
    let (_, window, served) = rig.read("cas/eight", span(first, last)).await.unwrap();
    assert_eq!(
        window,
        Window::Part(ByteRange::new(first, last - first + 1))
    );
    assert_eq!(served, bytes[first as usize..=last as usize]);
    assert_eq!(servable(&rig.entry("cas/eight", g).unwrap()), vec![1, 2, 3]);
}

/// Asserts that a read of the short last cell, which is not a whole megabyte, serves its bytes
/// when the span runs past the end of the object, and that the window is cut at its end.
#[tokio::test]
async fn a_read_of_the_short_last_cell_serves_its_bytes() {
    let rig = LargeRig::new("cells-short-last");
    let (g, bytes) = (generation(7), object(5 * MIB + 1000, 5));
    rig.stored("cas/short", &bytes, g);
    let (_, window, served) = rig.read("cas/short", span(5 * MIB, 9 * MIB)).await.unwrap();
    assert_eq!(window, Window::Part(ByteRange::new(5 * MIB, 1000)));
    assert_eq!(served, bytes[(5 * MIB) as usize..]);
    assert_eq!(servable(&rig.entry("cas/short", g).unwrap()), vec![5]);
}

/// Asserts that a read of exactly one byte serves that byte, and fetches only its cell.
#[tokio::test]
async fn a_read_of_one_byte_serves_that_byte_and_leaves_one_cell() {
    let rig = LargeRig::new("cells-one-byte");
    let (g, bytes) = (generation(8), object(6 * MIB, 6));
    rig.stored("cas/one", &bytes, g);
    let at = 3 * MIB + 12_345;
    let (_, window, served) = rig.read("cas/one", span(at, at)).await.unwrap();
    assert_eq!(window, Window::Part(ByteRange::new(at, 1)));
    assert_eq!(served, vec![bytes[at as usize]]);
    assert_eq!(servable(&rig.entry("cas/one", g).unwrap()), vec![3]);
    let gets = asked(&rig, 0);
    assert!(
        gets.iter()
            .all(|&(from, to)| from >= 3 * MIB && to <= 4 * MIB)
    );
}

/// Asserts that a suffix range that starts in the cell before the last serves the object's last
/// bytes, and that a suffix longer than the object serves all of it.
#[tokio::test]
async fn a_suffix_range_serves_the_last_bytes_of_the_object() {
    let rig = LargeRig::new("cells-suffix");
    let (g, bytes) = (generation(9), object(5 * MIB + 1000, 7));
    rig.stored("cas/tail", &bytes, g);
    let size = bytes.len() as u64;
    let (_, window, served) = rig
        .read("cas/tail", Some(Span::Suffix(1500)))
        .await
        .unwrap();
    assert_eq!(window, Window::Part(ByteRange::new(size - 1500, 1500)));
    assert_eq!(served, bytes[(size - 1500) as usize..]);
    assert_eq!(servable(&rig.entry("cas/tail", g).unwrap()), vec![4, 5]);

    let (_, window, served) = rig
        .read("cas/tail", Some(Span::Suffix(size + 9)))
        .await
        .unwrap();
    assert_eq!(window, Window::Part(ByteRange::new(0, size)));
    assert_eq!(served, bytes.to_vec());
}

/// Asserts that a whole read of an object of more than 64 cells asks the engine for 64 cells
/// only while no more than the first piece is taken (the upstream is asked for no byte past
/// them, and cells 64 on stay absent), and that reading the rest serves every byte.
#[tokio::test]
async fn a_whole_read_ensures_only_a_window_ahead_and_serves_the_rest_when_read() {
    let rig = LargeRig::new("cells-ahead");
    let (g, bytes) = (generation(10), object(72 * MIB, 8));
    rig.stored("cas/big", &bytes, g);
    let opened = rig.open("cas/big", None).await.unwrap();
    assert_eq!(opened.window, Window::Whole);
    rig.engine.until(|s| s.cells_published >= 64).await;
    let entry = rig.entry("cas/big", g).unwrap();
    assert_eq!(servable(&entry), (0..64).collect::<Vec<u32>>());
    assert!(asked(&rig, 0).iter().all(|&(_, to)| to <= 64 * MIB));
    assert_eq!(rig.engine.stats().bytes_fetched, 64 * MIB);

    let served = collect(opened.body).await.unwrap();
    assert_eq!(served, bytes.to_vec());
    assert_eq!(servable(&entry), (0..72).collect::<Vec<u32>>());
}

/// Asserts that the entry is held once by the front's holder while a read's body is alive and
/// has served a piece, and released when the body is read to its end.
#[tokio::test]
async fn the_entry_is_held_while_the_body_is_read_and_released_at_its_end() {
    let rig = LargeRig::new("cells-hold-end");
    let (g, bytes) = (generation(11), object(4 * MIB, 9));
    rig.stored("cas/held", &bytes, g);
    let mut opened = rig.open("cas/held", None).await.unwrap();
    let entry = rig.entry("cas/held", g).unwrap();
    assert_eq!(entry.holders(), 1);
    let second = opened.body.next().await.unwrap().unwrap();
    assert!(!second.is_empty());
    assert_eq!(entry.holders(), 1);
    while let Some(piece) = opened.body.next().await {
        piece.unwrap();
    }
    drop(opened);
    assert_eq!(entry.holders(), 0);
}

/// Asserts that a body dropped part way releases the entry it held.
#[tokio::test]
async fn a_body_dropped_part_way_releases_the_entry() {
    let rig = LargeRig::new("cells-hold-drop");
    let (g, bytes) = (generation(12), object(6 * MIB, 10));
    rig.stored("cas/dropped", &bytes, g);
    let mut opened = rig.open("cas/dropped", None).await.unwrap();
    let entry = rig.entry("cas/dropped", g).unwrap();
    opened.body.next().await.unwrap().unwrap();
    assert_eq!(entry.holders(), 1);
    drop(opened);
    assert_eq!(entry.holders(), 0);
}

/// Asserts that a stored object whose every copy holds another generation than `LOCATE` named
/// fails the read with `Unavailable` at the open, counts the failed fill and the excluded copy,
/// and leaves the book without the object, so the next read asks `LOCATE` again.
#[tokio::test]
async fn a_fill_whose_copies_hold_another_generation_fails_and_the_book_forgets() {
    let rig = LargeRig::new("cells-mismatch");
    let bytes = object(3 * MIB, 11);
    let located = Location {
        generation: Some(generation(1)),
        size: Some(bytes.len() as u64),
        sources: vec![hot(bytes.len() as u64)],
        conflicts: Vec::new(),
    };
    rig.insert("cas/moved", &bytes, generation(2), located);
    let error = rig.read("cas/moved", None).await.unwrap_err();
    assert!(matches!(error, Error::Unavailable(_)), "{error:?}");
    let counts = rig.large.counters().counts();
    assert_eq!((counts.fill_failures, counts.locates), (1, 1));
    assert!(rig.engine.stats().copy_generation_mismatch >= 1);

    let error = rig.read("cas/moved", None).await.unwrap_err();
    assert!(matches!(error, Error::Unavailable(_)), "{error:?}");
    let counts = rig.large.counters().counts();
    assert_eq!(
        (counts.fill_failures, counts.locates, counts.known),
        (2, 2, 0)
    );
}

/// Asserts that a fill the engine loses (its `LOCATE` finds the object definitively gone) fails
/// the read with `Unavailable`, counts the failed fill, and leaves the book without the object.
#[tokio::test]
async fn a_lost_fill_fails_the_read_and_the_book_forgets() {
    let rig = LargeRig::new("cells-lost");
    let size = 3 * MIB;
    let located = Location {
        generation: Some(generation(13)),
        size: Some(size),
        sources: vec![hot(size)],
        conflicts: Vec::new(),
    };
    rig.upstream.script_locate(Ok(Located::Found(located)));
    rig.upstream.script_locate(Ok(Located::DefinitiveMiss));
    let error = rig.read("cas/gone", None).await.unwrap_err();
    assert!(matches!(error, Error::Unavailable(_)), "{error:?}");
    let counts = rig.large.counters().counts();
    assert_eq!((counts.fill_failures, counts.locates), (1, 1));
    assert!(rig.engine.stats().packs_lost >= 1);

    let error = rig.read("cas/gone", None).await.unwrap_err();
    assert!(matches!(error, Error::NotFound(_)), "{error:?}");
    assert_eq!(rig.large.counters().counts().locates, 2);
}

/// Asserts that a cell whose entry the front does not find open on its first two reads is waited
/// for again and served, without a failed fill.
#[tokio::test]
async fn an_entry_not_open_for_two_tries_still_serves_the_read() {
    let rig = wrapped("cells-reopen", |store| Missing::wrap(store, 2));
    let (g, bytes) = (generation(14), object(3 * MIB, 12));
    rig.stored("cas/late", &bytes, g);
    let (_, _, served) = rig.read("cas/late", None).await.unwrap();
    assert_eq!(served, bytes.to_vec());
    assert_eq!(rig.large.counters().counts().fill_failures, 0);
}

/// Asserts that a cell whose entry is never found open while it is read ends the read with
/// `Unavailable` ("went away") after three tries, counts a failed fill, and forgets the object,
/// so a read once the entry is found asks `LOCATE` again and is served.
#[tokio::test]
async fn an_entry_not_open_for_three_tries_ends_the_read() {
    let misses = Arc::new(AtomicUsize::new(usize::MAX));
    let shared = misses.clone();
    let rig = wrapped("cells-went-away", |store| Missing::shared(store, shared));
    let (g, bytes) = (generation(15), object(3 * MIB, 13));
    rig.stored("cas/away", &bytes, g);
    let error = rig.read("cas/away", None).await.unwrap_err();
    assert!(unavailable(&error, "went away 3 times"), "{error:?}");
    let counts = rig.large.counters().counts();
    assert_eq!((counts.fill_failures, counts.locates), (1, 1));
    misses.store(0, Ordering::SeqCst);
    let (_, _, again) = rig.read("cas/away", None).await.unwrap();
    assert_eq!(again, bytes.to_vec());
    assert_eq!(rig.large.counters().counts().locates, 2);
}

/// Asserts that a cell that is not servable when the front checks it after its read is read
/// again (twice here) and served, and that one that is never servable to the front ends the read
/// with `Unavailable` and a failed fill after three tries.
#[tokio::test]
async fn a_cell_not_servable_after_its_read_is_read_again_up_to_three_times() {
    let rig = wrapped("cells-post-read", |store| Flaky::wrap(store, 2));
    let (g, bytes) = (generation(16), object(2 * MIB, 14));
    rig.stored("cas/flaky", &bytes, g);
    let (_, _, served) = rig.read("cas/flaky", None).await.unwrap();
    assert_eq!(served, bytes.to_vec());
    assert_eq!(rig.large.counters().counts().fill_failures, 0);

    let hidden = Arc::new(AtomicUsize::new(usize::MAX));
    let rig = wrapped("cells-post-read-fail", |store| Flaky::shared(store, hidden));
    rig.stored("cas/flaky", &bytes, g);
    let error = rig.read("cas/flaky", None).await.unwrap_err();
    assert!(unavailable(&error, "went away 3 times"), "{error:?}");
    assert_eq!(rig.large.counters().counts().fill_failures, 1);
}

/// Asserts that an object overwritten in the upstream after the front learned its generation
/// serves no byte of the new object: the next ranged read fails as `Unavailable`, the engine's
/// fill fails, and no GET carries another generation.
#[tokio::test]
async fn an_overwrite_after_the_book_learned_the_object_serves_no_byte_of_the_new_one() {
    let rig = LargeRig::new("cells-overwrite");
    let (old, new) = (object(3 * MIB, 15), object(3 * MIB, 16));
    rig.stored("cas/over", &old, generation(20));
    let (_, _, first) = rig.read("cas/over", span(0, 10)).await.unwrap();
    assert_eq!(first, old[..11]);
    rig.stored("cas/over", &new, generation(21));
    let error = rig.read("cas/over", span(MIB, 2 * MIB)).await.unwrap_err();
    assert!(matches!(error, Error::Unavailable(_)), "{error:?}");
    assert!(rig.engine.stats().fills_failed >= 1);
    assert!(
        rig.gets()
            .iter()
            .all(|get| get.generation == Some(generation(20)))
    );
}

/// Asserts that a read around present cells asks the engine for the absent cells on both sides
/// of them and not for the present ones: no GET covers cells 2 and 3, the bytes are the object's,
/// and every cell of the read is present after it.
#[tokio::test]
async fn a_read_around_present_cells_fetches_the_absent_cells_on_both_sides_only() {
    let rig = LargeRig::new("cells-around");
    let (g, bytes) = (generation(17), object(8 * MIB, 17));
    rig.stored("cas/eight", &bytes, g);
    rig.read("cas/eight", span(2 * MIB, 4 * MIB - 1))
        .await
        .unwrap();
    let before = rig.gets().len();
    let (_, _, served) = rig.read("cas/eight", span(0, 6 * MIB - 1)).await.unwrap();
    assert_eq!(served, bytes[..(6 * MIB) as usize]);
    let new = asked(&rig, before);
    assert!(new.iter().any(|&(from, _)| from < 2 * MIB));
    assert!(new.iter().any(|&(_, to)| to > 4 * MIB));
    assert!(
        new.iter()
            .all(|&(from, to)| to <= 2 * MIB || from >= 4 * MIB)
    );
    let entry = rig.entry("cas/eight", g).unwrap();
    assert_eq!(servable(&entry), (0..6).collect::<Vec<u32>>());
}
