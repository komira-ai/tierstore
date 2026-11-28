//! The disk of the node's cache filled to its last byte (a file system of its own, the driver's
//! `--small-disk`), then `disk_cycles` cycles of a tree the node does not hold. Each answer must
//! be a tree read as the tree, a read the job sees fail (an error of the mount, never other
//! bytes), or a refusal with its reason (`UNAVAILABLE` or `FAILED_PRECONDITION`), never no answer;
//! each Release must succeed, and the node must live through all of it. Once the disk has room
//! again, a new source must get its tree within the agent's budget.

use super::{Outcome, Params, Step, drained, held, obtain, source_of};
use crate::provider::{Code, Refusal};
use crate::rig::Rig;
use crate::tree::{Tree, TreeFault};

/// Whether `refusal` is one a job's start can be told the reason of.
fn clear(refusal: &Refusal) -> bool {
    matches!(refusal.code, Code::Unavailable | Code::FailedPrecondition)
        && !refusal.message.is_empty()
}

pub(super) async fn run<R: Rig>(rig: &mut R, p: &Params, out: &mut Outcome) -> Result<(), String> {
    let tree =
        Tree::new("disk-full", p.disk_files, p.disk_file_bytes).map_err(|e| e.to_string())?;
    rig.put(&tree).await?;
    let filled = rig.fill_disk()?;
    out.note(format!("the cache's disk filled with {} MiB", filled >> 20));
    let (mut served, mut unreadable, mut refused) = (0, 0, 0);
    for n in 0..p.disk_cycles {
        let source = source_of("disk-full", n);
        match held(rig, &source, &tree).await {
            Ok(at) => match rig.read(&at, &tree).await {
                Ok(()) => served += 1,
                Err(TreeFault::Unreadable { .. }) => unreadable += 1,
                Err(fault) => out.fail(format!(
                    "{} with the disk full: a wrong read: {fault}",
                    source.run
                )),
            },
            Err(Step::Prepare(refusal) | Step::Mount(refusal)) if clear(&refusal) => refused += 1,
            Err(step) => out.fail(format!(
                "{} with the disk full: {step}; not a refusal with its reason",
                source.run
            )),
        }
        if let Err(refusal) = rig.release(&source).await {
            out.fail(format!(
                "the Release of {} with the disk full: {refusal}",
                source.run
            ));
        }
        if !rig.alive() {
            out.fail(format!(
                "the node died with its disk full, at cycle {} of {}",
                n + 1,
                p.disk_cycles
            ));
            return Ok(());
        }
    }
    out.note(format!(
        "with the disk full: {served} trees read, {unreadable} reads failed, {refused} refused with a reason"
    ));
    rig.free_disk()?;
    let source = source_of("disk-full", p.disk_cycles);
    let until = rig.now() + p.tree_wait;
    if let Err(step) = obtain(rig, &source, &tree, until, p.look).await {
        out.fail(format!("{} once the disk had room: {step}", source.run));
    }
    if let Err(refusal) = rig.release(&source).await {
        out.fail(format!("the Release of {}: {refusal}", source.run));
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
