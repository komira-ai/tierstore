//! The upstream answering every request with 503 for `errors_for`, while `errors_sources` sources
//! ask for a tree the node does not hold yet, once a `look`: every answer must be "unavailable"
//! with its reason (the start waits), never a tree and never a refusal that fails the job; the
//! resident memory, read every `sample`, must stay under idle plus `memory_bound`. Once the
//! upstream serves again, every source must get its tree within the agent's budget.

use objstore_s3::s3::fake::Serving;
use tierstore_trees_api::provider::v1::Source;

use super::{Memory, Outcome, Params, Step, drained, held, obtain, source_of, waits};
use crate::rig::Rig;
use crate::tree::Tree;

pub(super) async fn run<R: Rig>(rig: &mut R, p: &Params, out: &mut Outcome) -> Result<(), String> {
    let tree = Tree::new("upstream-errors", p.cycle_files, p.cycle_file_bytes)
        .map_err(|e| e.to_string())?;
    rig.put(&tree).await?;
    let mut memory = Memory::idle(rig)?;
    rig.serve_as(Serving::Failing {
        prefix: String::new(),
        status: 503,
    })
    .await?;
    let sources: Vec<Source> = (0..p.errors_sources)
        .map(|n| source_of("errors", n))
        .collect();
    let start = rig.now();
    let mut next = start;
    let mut waited = 0usize;
    'asking: while rig.now().saturating_sub(start) < p.errors_for {
        for source in &sources {
            match held(rig, source, &tree).await {
                Ok(at) => {
                    out.fail(format!(
                        "{} got a tree at {} while every read of the upstream failed",
                        source.run,
                        at.display()
                    ));
                    break 'asking;
                }
                Err(Step::Prepare(refusal) | Step::Mount(refusal)) if waits(&refusal) => {
                    waited += 1;
                }
                Err(step) => {
                    out.fail(format!(
                        "{} while the upstream failed: {step}; not unavailable, so the start would not wait",
                        source.run
                    ));
                    break 'asking;
                }
            }
        }
        if rig.now() >= next {
            memory.sample(rig)?;
            next = rig.now() + p.sample;
        }
        rig.pause(p.look).await;
    }
    if waited == 0 && out.passed() {
        out.fail("no start waited: nothing asked while the upstream failed");
    }
    out.note(format!(
        "{waited} answers were unavailable while the upstream failed; the peak resident memory: {} MiB",
        memory.peak() >> 20
    ));
    for over in memory.over(p.memory_bound) {
        out.fail(over);
    }
    rig.serve_as(Serving::All).await?;
    for source in &sources {
        let until = rig.now() + p.tree_wait;
        if let Err(step) = obtain(rig, source, &tree, until, p.look).await {
            out.fail(format!("{} once the upstream served: {step}", source.run));
        }
    }
    for source in &sources {
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
