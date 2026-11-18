//! The `LOCATE` step and the lost path (4.1, 4.6, 10.6).
//!
//! A fill resolves once when it starts (open the entry, `LOCATE`, pin the generation, plan the
//! parts) and again, single flight, when a part finds its copy gone (404) or runs out of attempts:
//! `LOCATE` is asked once more (the upstream asks every live storage node and consults no
//! negative cache). A hit is served. A miss that is **not definitive**, or an error, waits and
//! asks again with backoff for up to `unavailable_wait`, so a reboot of the node holding the only
//! copy delays the fill and does not fail it. A definitive miss, or one still not definitive when
//! the wait ends, **loses** the fill: every waiter of an unsatisfied cell gets `lost`. A location
//! with no copy of the pinned generation (or of another size) **fails** the fill: its waiters get
//! EIO, and the next request starts a new fill life.

use std::collections::BTreeSet;
use std::sync::Arc;

use futures::FutureExt;
use tierstore_fetch_api::entry::EntrySpec;
use tierstore_fetch_api::ids::PackRoot;
use tierstore_fetch_api::upstream::{Located, Location, SourceKind};

use crate::engine::{Armed, Inner, State};
use crate::fill::{Fill, FillSched, Phase, Resolved, Resolving};
use crate::plan::{CELL_BYTES, PlanInput, plan, upload_cuts};
use crate::stats::TimerKind;

/// Starts fill `seq`: `Idle -> Starting`, and its first resolve.
pub(crate) fn start(inner: &Arc<Inner>, st: &mut State, seq: u64) {
    for fs in st.fills.get_mut(&seq).into_iter() {
        fs.phase = Phase::Starting;
        // The resolve runs as its own task; a waiter is woken by the fill when it ends.
        let _resolve = resolving(inner, fs);
    }
    st.stats.fills += 1;
}

/// The resolve of the fill in flight, or a new one.
pub(crate) fn resolving(inner: &Arc<Inner>, fs: &mut FillSched) -> Resolving {
    fs.resolving
        .get_or_insert_with(|| {
            let task = inner.rt.spawn(run(inner.clone(), fs.fill.clone()));
            async move { task.await.unwrap_or(Resolved::Failed) }
                .boxed()
                .shared()
        })
        .clone()
}

/// What the `LOCATE` loop found.
enum Found {
    At(Location),
    Lost,
    Failed,
}

async fn run(inner: Arc<Inner>, fill: Arc<Fill>) -> Resolved {
    let found = locate(&inner, &fill).await;
    let part_size = inner.parts.budget.part_size(fill.spec.size);
    let mut guard = inner.lock();
    let st = &mut *guard;
    let stats = &mut st.stats;
    // A resolving fill is not forgotten (`resolving` is set until here).
    let resolved = st.fills.get_mut(&fill.seq).map_or(Resolved::Failed, |fs| {
        fs.resolving = None;
        let resolved = match found {
            Found::At(location) => accept(&inner, fs, location, part_size),
            Found::Lost => Resolved::Lost,
            Found::Failed => Resolved::Failed,
        };
        match resolved {
            Resolved::Found => {
                fs.phase = Phase::Ready;
                // Wants taken before the entry was open may be on `recheck` or evicting cells.
                for e in fs.fill.entry.get().cloned().iter() {
                    let wanted: Vec<u32> = fs.wants.keys().copied().collect();
                    let s = crate::engine::scan(fs, e.state(), wanted.into_iter());
                    crate::engine::nudge(&inner, fs, e, &s);
                }
            }
            Resolved::Lost => {
                lose(fs);
                stats.packs_lost += 1;
                lost_ttl(&inner, &fill, fs.life);
            }
            Resolved::Failed => {
                fail_life(fs);
                stats.fills_failed += 1;
            }
        }
        resolved
    });
    st.refresh(fill.seq, &inner.config);
    st.retire(fill.seq);
    // A lost or failed fill leaves the active-pack window: every class looks again.
    inner.wake_classes(crate::engine::ALL_CLASSES);
    fill.bump();
    inner.bump();
    resolved
}

async fn locate(inner: &Arc<Inner>, fill: &Arc<Fill>) -> Found {
    if fill.entry.get().is_none() {
        let store = inner.parts.store.clone();
        let spec = EntrySpec {
            pack: fill.spec.id.clone(),
            size: fill.spec.size,
        };
        match tokio::task::spawn_blocking(move || store.open(&spec)).await {
            Ok(Ok(entry)) => {
                // The entry is set once per fill: only the first resolve gets here.
                let _ = fill.entry.set(entry);
            }
            _ => return Found::Failed,
        }
    }
    let config = &inner.config;
    let deadline = inner.now().saturating_add(config.unavailable_wait_ms);
    let mut backoff = config.locate_backoff_ms;
    loop {
        inner.with_stats(|s| s.locates += 1);
        let answer = inner.parts.upstream.locate(&fill.spec.id.key).await;
        match answer {
            Ok(Located::Found(location)) => return Found::At(location),
            Ok(Located::DefinitiveMiss) => return Found::Lost,
            Ok(Located::NotDefinitive(_)) | Err(_) => {}
        }
        let now = inner.now();
        if now >= deadline {
            return Found::Lost;
        }
        let wait = inner.lock().jittered(backoff).min(deadline - now);
        Armed::new(inner, TimerKind::Locate, now + wait).await;
        backoff = backoff.saturating_mul(2).min(config.backoff_max_ms);
    }
}

/// Takes a found location into the fill: the pinned generation, the copies, the plan.
fn accept(inner: &Inner, fs: &mut FillSched, location: Location, part_size: u64) -> Resolved {
    let size_ok = location.size.is_none_or(|s| s == fs.fill.spec.size);
    let pinned = fs.pinned.or(location.generation);
    let held = match (pinned, location.generation) {
        (Some(p), Some(g)) => p == g || location.conflicts.contains(&p),
        _ => true,
    };
    if !(size_ok && held) {
        return Resolved::Failed;
    }
    fs.pinned = pinned;
    if fs.pieces.is_empty() {
        let legacy = location
            .sources
            .iter()
            .all(|s| matches!(s.kind, SourceKind::Legacy));
        let upload = if legacy {
            fs.fill.spec.upload_part_size
        } else {
            0
        };
        let mut cuts = upload_cuts(fs.fill.spec.size, upload);
        cuts.extend(extent_cuts(&location));
        let (first_touch, first_touch_cells) = fs.first_touch.unwrap_or((0, 0));
        fs.pieces = plan(
            &inner.config,
            PlanInput {
                pack_size: fs.fill.spec.size,
                part_size,
                first_touch,
                first_touch_cells,
            },
            &cuts,
        );
    }
    fs.location = Some(location);
    Resolved::Found
}

/// The cells at which a source's extent begins or ends inside the object (a chain's blocks), so
/// no part crosses one.
fn extent_cuts(location: &Location) -> BTreeSet<u32> {
    location
        .sources
        .iter()
        .flat_map(|s| [Some(s.extent.offset), s.extent.end()])
        .flatten()
        .filter(|b| b % CELL_BYTES == 0)
        // An offset inside the object has fewer than 2^32 cells; one past it is cut nowhere.
        .map(|b| u32::try_from(b / CELL_BYTES).unwrap_or(u32::MAX))
        .collect()
}

/// The fill's state as a new life starts: no location, no plan, the generation from the pack id.
fn reset(fs: &mut FillSched) {
    fs.life += 1;
    fs.phase = Phase::Idle;
    fs.location = None;
    fs.pieces.clear();
    fs.first_touch = None;
    fs.excluded.clear();
    fs.unavailable_since = None;
    fs.pinned = match fs.fill.spec.id.root {
        PackRoot::Etag(g) => Some(g),
        PackRoot::Digest(_) => None,
    };
}

/// The fill is lost: every op of it stops, and every waiter of an unsatisfied cell gets `lost`.
fn lose(fs: &mut FillSched) {
    reset(fs);
    fs.phase = Phase::Lost;
    fs.wants.clear();
}

/// The fill failed for its waiters (EIO), and the next request starts a new life.
pub(crate) fn fail_life(fs: &mut FillSched) {
    let cells = fs.fill.cells;
    fs.fail(0, cells);
    reset(fs);
}

/// After `lost_ttl` a lost fill is forgotten (or starts a new life if someone asks again).
fn lost_ttl(inner: &Arc<Inner>, fill: &Arc<Fill>, life: u64) {
    let deadline = inner.now().saturating_add(inner.config.lost_ttl_ms);
    let timer = Armed::new(inner, TimerKind::LostTtl, deadline);
    let inner = inner.clone();
    let fill = fill.clone();
    inner.rt.clone().spawn(async move {
        timer.await;
        let mut st = inner.lock();
        let lost = st.fills.get_mut(&fill.seq).into_iter();
        for fs in lost.filter(|fs| (fs.phase == Phase::Lost) & (fs.life == life)) {
            reset(fs);
        }
        st.retire(fill.seq);
        drop(st);
        fill.bump();
        inner.bump();
    });
}
