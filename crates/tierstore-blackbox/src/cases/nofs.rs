//! The node started without its `fs` interface: what a job of the build farm can run, an ordinary
//! user with no capability and no FUSE device, the package unpacked and not installed. The node
//! must start, answer `/readyz` 200, serve through its S3 front an object it reads from the
//! upstream through its cache, and answer a Prepare and a Mount "unavailable" with the reason
//! that no file system serves its mount point: the agent's start waits, and is not failed.

use super::{Outcome, Params, source_of, waits};
use crate::rig::Rig;
use crate::tree::Tree;

/// The reason a node without a file system gives a Prepare and a Mount.
pub(super) const NO_FS: &str = "no file system serves";

pub(super) async fn run<R: Rig>(rig: &mut R, p: &Params, out: &mut Outcome) -> Result<(), String> {
    let tree = Tree::new("start-without-fs", p.cycle_files, p.cycle_file_bytes)
        .map_err(|e| e.to_string())?;
    rig.put(&tree).await?;
    let (status, body) = rig.ready().await?;
    if status == 200 {
        out.note(format!("/readyz answered {}", body.trim()));
    } else {
        out.fail(format!("/readyz answered {status}: {}", body.trim()));
    }
    let (key, bytes) = &tree.objects()[0];
    match rig.front_read(key, bytes.len() as u64).await {
        Ok(read) if read == *bytes => out.note(format!(
            "the S3 front served the {} bytes of {key} from the upstream through its cache",
            bytes.len()
        )),
        Ok(read) => out.fail(format!(
            "the S3 front served {} bytes of {key} that are not the upstream's",
            read.len()
        )),
        Err(why) => out.fail(format!("the S3 front did not serve {key}: {why}")),
    }
    let source = source_of("start-without-fs", 0);
    let prepared = rig.prepare(&source, tree.key()).await.err();
    let mounted = rig.mount(&source).await.err();
    for (step, refusal) in [("Prepare", prepared), ("Mount", mounted)] {
        match refusal {
            None => out.fail(format!("{step} succeeded on a node with no file system")),
            Some(refusal) if waits(&refusal) && refusal.message.contains(NO_FS) => {
                out.note(format!("{step} was unavailable: {}", refusal.message));
            }
            Some(refusal) => out.fail(format!("{step}: {refusal}; not unavailable for {NO_FS}")),
        }
    }
    if let Err(refusal) = rig.release(&source).await {
        out.fail(format!("the Release of {}: {refusal}", source.run));
    }
    Ok(())
}
