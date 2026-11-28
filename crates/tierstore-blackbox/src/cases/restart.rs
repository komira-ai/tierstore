//! The node killed (SIGKILL) and started again between a Prepare and its Mount, twice: once right
//! after the Prepare's answer, while the tree is being prepared, and once after its placement is
//! on disk. Asking again as the agent does (a Mount, and after "unavailable" a Prepare and a Mount
//! again), each source must get its tree, read as the tree, within the agent's budget.

use super::{Outcome, Params, drained, obtain, source_of};
use crate::rig::Rig;
use crate::tree::Tree;

pub(super) async fn run<R: Rig>(rig: &mut R, p: &Params, out: &mut Outcome) -> Result<(), String> {
    let tree =
        Tree::new("restart", p.cycle_files, p.cycle_file_bytes).map_err(|e| e.to_string())?;
    rig.put(&tree).await?;
    let early = source_of("restart", 0);
    if let Err(refusal) = rig.prepare(&early, tree.key()).await {
        out.fail(format!("the first Prepare: {refusal}"));
        return Ok(());
    }
    rig.restart().await?;
    let before = rig.placements()?;
    let placed = source_of("restart", 1);
    if let Err(refusal) = rig.prepare(&placed, tree.key()).await {
        out.fail(format!("the second Prepare: {refusal}"));
        return Ok(());
    }
    let since = rig.now();
    while rig.placements()? <= before {
        if rig.now().saturating_sub(since) >= p.settle {
            out.fail(format!(
                "the second source was not placed within {:?} of its Prepare",
                p.settle
            ));
            return Ok(());
        }
        rig.pause(p.look).await;
    }
    rig.restart().await?;
    out.note("killed right after a Prepare, and again once a tree was placed");
    for source in [&early, &placed] {
        let until = rig.now() + p.tree_wait;
        if let Err(step) = obtain(rig, source, &tree, until, p.look).await {
            out.fail(format!("{} after the restart: {step}", source.run));
        }
    }
    for source in [&early, &placed] {
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
