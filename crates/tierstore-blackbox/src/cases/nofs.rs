//! The node started without its `fs` interface and its mount provider: what a job of the build
//! farm can run, an ordinary user with no capability and no FUSE device, the package unpacked and
//! not installed (a node file with a provider and no `fs` interface is refused at load). The node
//! must start, answer `/readyz` 200, serve through its S3 front an object it reads from the
//! upstream through its cache, and give a Prepare no answer: it serves no provider.

use super::{Outcome, Params, source_of};
use crate::provider::Code;
use crate::rig::Rig;
use crate::tree::Tree;

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
    match rig.prepare(&source, tree.key()).await {
        Ok(()) => out.fail("a Prepare was answered by a node with no mount provider"),
        Err(refusal) if refusal.code == Code::NoAnswer => {
            out.note(format!("a Prepare had no answer: {}", refusal.message));
        }
        Err(refusal) => out.fail(format!(
            "a Prepare: {refusal}; not no answer, as of a node with no mount provider"
        )),
    }
    Ok(())
}
