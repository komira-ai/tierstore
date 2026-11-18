//! The cells a waiter or an op leaves to settle later: a `bad` cell's expiry, a waiter's second
//! look at a cell the evictor owns, and the re-hash of a `recheck` cell (section 8).

use std::sync::Arc;

use bytes::Bytes;
use tierstore_fetch_api::entry::{Entry, Settle};
use tierstore_fetch_api::verify::{CellJob, Verdict, VerifyPriority};

use crate::engine::{ALL_CLASSES, Armed, Inner};
use crate::fill::Fill;
use crate::stats::TimerKind;

/// After `bad_cell_ms` a bad cell becomes absent again (`bad -> absent`).
pub(crate) fn bad_expiry(inner: &Arc<Inner>, fill: &Arc<Fill>, entry: &Arc<dyn Entry>, cell: u32) {
    let deadline = inner.now().saturating_add(inner.config.bad_cell_ms);
    let sleep = Armed::new(inner, TimerKind::BadCell, deadline);
    let (inner, fill, entry) = (inner.clone(), fill.clone(), entry.clone());
    inner.rt.clone().spawn(async move {
        sleep.await;
        entry.settle(cell, Settle::ExpireBad);
        let mut st = inner.lock();
        for fs in st.fills.get_mut(&fill.seq).into_iter() {
            fs.mismatches.remove(&cell);
        }
        st.retire(fill.seq);
        drop(st);
        fill.bump();
        inner.bump();
    });
}

/// A waiter or an `ensure` saw a cell the evictor owns: look again after `evicting_poll_ms`
/// (the evictor does not tell the engine), and queue the cell if it became absent.
pub(crate) fn evict_poll(inner: &Arc<Inner>, fill: &Arc<Fill>) {
    let deadline = inner.now().saturating_add(inner.config.evicting_poll_ms);
    let sleep = Armed::new(inner, TimerKind::EvictPoll, deadline);
    let (inner, fill) = (inner.clone(), fill.clone());
    inner.rt.clone().spawn(async move {
        sleep.await;
        let mut st = inner.lock();
        for fs in st.fills.get_mut(&fill.seq).into_iter() {
            fs.evict_poll = false;
        }
        st.refresh(fill.seq, &inner.config);
        drop(st);
        inner.wake_classes(ALL_CLASSES);
        fill.bump();
    });
}

/// A wanted cell is `recheck` (restored after a start in another boot, section 8): its bytes are
/// re-hashed at the pool's lower priority; it becomes present on a match and absent (fetched) on
/// a mismatch, or when the pack has no cell digests.
pub(crate) fn recheck(inner: &Arc<Inner>, fill: &Arc<Fill>, entry: &Arc<dyn Entry>, cell: u32) {
    let (inner, fill, entry) = (inner.clone(), fill.clone(), entry.clone());
    inner.rt.clone().spawn(async move {
        let how = match recheck_verdict(&inner, &fill, entry.clone(), cell).await {
            Verdict::Match => Settle::RecheckOk,
            Verdict::Mismatch => Settle::RecheckFailed,
        };
        entry.settle(cell, how);
        let mut st = inner.lock();
        for fs in st.fills.get_mut(&fill.seq).into_iter() {
            fs.rechecking.remove(&cell);
        }
        let mask = st.refresh(fill.seq, &inner.config);
        st.retire(fill.seq);
        drop(st);
        inner.wake_classes(mask);
        fill.bump();
        inner.bump();
    });
}

async fn recheck_verdict(
    inner: &Arc<Inner>,
    fill: &Arc<Fill>,
    entry: Arc<dyn Entry>,
    cell: u32,
) -> Verdict {
    let read = tokio::task::spawn_blocking(move || entry.read_cell(cell)).await;
    let digest = fill
        .spec
        .digests
        .as_ref()
        .and_then(|d| d.list.cell_digest(d.pack, cell));
    let (Ok(Ok(bytes)), Some(expected)) = (read, digest) else {
        return Verdict::Mismatch;
    };
    let job = CellJob {
        cell,
        expected,
        bytes: Bytes::from(bytes),
        priority: VerifyPriority::Recheck,
    };
    inner.parts.verifier.verify(job).await.0
}
