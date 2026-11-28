//! T2: a burst of Prepares past `trees.max_placements`. The case holds `max_placements` sources,
//! each prepared, mounted and read through the mount (so the mount holds each tree, as a running
//! job's does), then prepares `burst` more at once. Each Mount of the burst must be refused
//! "unavailable" with the reason, too many placements; none may succeed, and none may be a
//! refusal that fails the job. Then it releases the held sources: within `settle` of the
//! Releases, every source of the burst must get its tree, asked again as the agent asks. A node
//! whose Release leaves the place counted while the mount holds the tree
//! (komira-ai/tierstore#619) keeps refusing the burst.

use tierstore_trees_api::provider::v1::Source;

use super::{Outcome, Params, drained, held, obtain, source_of, waits};
use crate::rig::Rig;
use crate::tree::Tree;

/// The reason the node gives a Mount past `trees.max_placements`.
pub(super) const TOO_MANY: &str = "too many placements";

pub(super) async fn run<R: Rig>(rig: &mut R, p: &Params, out: &mut Outcome) -> Result<(), String> {
    let tree =
        Tree::new("t2-placements", p.cycle_files, p.cycle_file_bytes).map_err(|e| e.to_string())?;
    rig.put(&tree).await?;
    let most = usize::try_from(p.max_placements).unwrap_or(usize::MAX);
    let holding: Vec<Source> = (0..most).map(|n| source_of("t2-held", n)).collect();
    for (n, source) in holding.iter().enumerate() {
        let at = match held(rig, source, &tree).await {
            Ok(at) => at,
            Err(step) => {
                out.fail(format!("held source {} of {most}: {step}", n + 1));
                return Ok(());
            }
        };
        if let Err(fault) = rig.read(&at, &tree).await {
            out.fail(format!(
                "held source {} of {most}: the read: {fault}",
                n + 1
            ));
            return Ok(());
        }
    }
    let burst: Vec<Source> = (0..p.burst).map(|n| source_of("t2-burst", n)).collect();
    for source in &burst {
        if let Err(refusal) = rig.prepare(source, tree.key()).await {
            out.fail(format!("the burst's Prepare of {}: {refusal}", source.run));
            return Ok(());
        }
    }
    let mut refused = 0;
    for source in &burst {
        match rig.mount(source).await {
            Ok(at) => out.fail(format!(
                "the burst's Mount of {} past {most} placements answered {}",
                source.run,
                at.display()
            )),
            Err(refusal) if waits(&refusal) && refusal.message.contains(TOO_MANY) => refused += 1,
            Err(refusal) => out.fail(format!(
                "the burst's Mount of {} past {most} placements: {refusal}; not unavailable for {TOO_MANY}",
                source.run
            )),
        }
    }
    out.note(format!(
        "{refused} of {} Mounts past {most} placements were unavailable for {TOO_MANY}",
        p.burst
    ));
    for source in &holding {
        if let Err(refusal) = rig.release(source).await {
            out.fail(format!("the Release of {}: {refusal}", source.run));
        }
    }
    let released = rig.now();
    for source in &burst {
        if let Err(step) = obtain(rig, source, &tree, released + p.settle, p.look).await {
            out.fail(format!(
                "{} within {:?} of the Releases: {step}",
                source.run, p.settle
            ));
        }
    }
    if out.passed() {
        let took = rig.now().saturating_sub(released);
        out.note(format!(
            "the burst had its trees {took:?} after the Releases"
        ));
    }
    for source in &burst {
        if let Err(refusal) = rig.release(source).await {
            out.fail(format!("the Release of {}: {refusal}", source.run));
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
