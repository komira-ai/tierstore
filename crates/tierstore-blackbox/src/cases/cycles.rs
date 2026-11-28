//! T1: `cycles` Prepare, Mount, read and Release cycles of one tree, one after another, at the
//! node's defaults. Every cycle must succeed at each step's first ask, and no placement may be
//! left `settle` after the last Release. A node whose Release does not free the place
//! (komira-ai/tierstore#619) fails at the cycle after `trees.max_placements` (1024 by default):
//! the Mount is refused for too many placements.

use super::{Outcome, Params, cycle, drained, source_of};
use crate::rig::Rig;
use crate::tree::Tree;

pub(super) async fn run<R: Rig>(rig: &mut R, p: &Params, out: &mut Outcome) -> Result<(), String> {
    let tree =
        Tree::new("t1-cycles", p.cycle_files, p.cycle_file_bytes).map_err(|e| e.to_string())?;
    rig.put(&tree).await?;
    let since = rig.now();
    for n in 0..p.cycles {
        if let Err(step) = cycle(rig, &source_of("t1", n), &tree).await {
            out.fail(format!("cycle {} of {}: {step}", n + 1, p.cycles));
            break;
        }
    }
    if out.passed() {
        let took = rig.now().saturating_sub(since);
        out.note(format!("{} cycles in {took:?}", p.cycles));
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
