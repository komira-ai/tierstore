//! T3: the upstream stalled for `stall` while `prepares` Prepares of new sources of one tree arrive
//! over `arrival`, evenly. The stall holds every read of a pack (the files' bytes); the tree's list
//! and sidecar are served, so the node places each tree and queues the reads of its files, as
//! a burst of jobs does when the store hangs. The resident memory, read every `sample` from the
//! idle size before the first Prepare, must stay under idle plus `memory_bound` at every sample,
//! and grow by at most `level_growth` over the stall's last `level_window`; the stall must have
//! held requests of the node (else the case proved nothing). Within `drain` of the stall's end
//! the queue gauges must be 0. A node with no bound on the heap of its queued reads
//! (komira-ai/tierstore#613) grows by every Prepare's reads.

use std::time::Duration;

use objstore_s3::s3::fake::Serving;
use tierstore_trees_api::provider::v1::Source;

use super::{Memory, Outcome, Params, drained, source_of};
use crate::rig::Rig;
use crate::tree::{PACK_PREFIX, Tree};

pub(super) async fn run<R: Rig>(rig: &mut R, p: &Params, out: &mut Outcome) -> Result<(), String> {
    let tree =
        Tree::new("t3-stall", p.stall_files, p.stall_file_bytes).map_err(|e| e.to_string())?;
    rig.put(&tree).await?;
    let mut memory = Memory::idle(rig)?;
    out.note(format!(
        "the idle resident memory: {} MiB",
        memory.idle >> 20
    ));
    rig.serve_as(Serving::Stalled {
        prefix: PACK_PREFIX.to_owned(),
    })
    .await?;
    let start = rig.now();
    let mut next = start;
    let sources: Vec<Source> = (0..p.prepares).map(|n| source_of("t3", n)).collect();
    let mut refused = 0usize;
    for (n, source) in sources.iter().enumerate() {
        let due = start + p.arrival.mul_f64(n as f64 / p.prepares.max(1) as f64);
        sample_until(rig, &mut memory, &mut next, due, p.sample).await?;
        if let Err(refusal) = rig.prepare(source, tree.key()).await {
            refused += 1;
            if refused == 1 {
                out.fail(format!("Prepare {} under the stall: {refusal}", n + 1));
            }
        }
    }
    if refused > 1 {
        out.fail(format!(
            "{refused} of {} Prepares refused under the stall",
            p.prepares
        ));
    }
    let end = start + p.stall;
    sample_until(rig, &mut memory, &mut next, end, p.sample).await?;
    let held = rig.stalled().await?;
    if held == 0 {
        out.fail("the upstream held no request: the stall never reached the node's reads");
    } else {
        out.note(format!("the stall held {held} requests of the node"));
    }
    for over in memory.over(p.memory_bound) {
        out.fail(over);
    }
    let growth = memory.growth_from(end.saturating_sub(p.level_window));
    if growth > p.level_growth {
        out.fail(format!(
            "the resident memory grew {} MiB over the last {:?} of the stall, over {} MiB: it did not level off",
            growth >> 20,
            p.level_window,
            p.level_growth >> 20
        ));
    }
    out.note(format!(
        "the peak resident memory: {} MiB over {} samples",
        memory.peak() >> 20,
        memory.samples.len()
    ));
    rig.serve_as(Serving::All).await?;
    let ended = rig.now();
    loop {
        let idle = rig.gauges().await?.idle();
        let waited = rig.now().saturating_sub(ended);
        match idle {
            Ok(()) => {
                out.note(format!("the queues were idle {waited:?} after the stall"));
                break;
            }
            Err(busy) if waited >= p.drain => {
                out.fail(format!(
                    "the queues were not idle {:?} after the stall: {busy}",
                    p.drain
                ));
                break;
            }
            Err(_) => rig.pause(p.look).await,
        }
    }
    for source in &sources {
        if let Err(refusal) = rig.release(source).await {
            out.fail(format!("the Release of {}: {refusal}", source.run));
            break;
        }
    }
    match drained(rig, p.settle, p.look).await? {
        Ok(waited) => out.note(format!(
            "no placement left {waited:?} after the last Release"
        )),
        Err(left) => out.fail(format!(
            "{left} placements left {:?} after the last Release",
            p.settle
        )),
    }
    Ok(())
}

/// Reads the memory at each `next` until the rig's time `until`, `every` apart.
async fn sample_until<R: Rig>(
    rig: &mut R,
    memory: &mut Memory,
    next: &mut Duration,
    until: Duration,
    every: Duration,
) -> Result<(), String> {
    loop {
        let now = rig.now();
        if now >= *next {
            memory.sample(rig)?;
            *next = now + every;
        } else if now >= until {
            return Ok(());
        } else {
            rig.pause((*next).min(until) - now).await;
        }
    }
}
