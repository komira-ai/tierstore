//! The commit of the checkout, as a git directory of the input root: a build on the Mac that
//! asks git for its commit (`git rev-parse --short=12 HEAD`, what `tierstore`'s build script
//! embeds in `--version`) gets the checkout's, though git tracks no `.git` and the tracked files
//! alone are no repository.
//!
//! The directory is the least git takes as a repository: `.git/HEAD`, detached at the commit;
//! the commit's object, loose, as git stores it; and one ref of its own,
//! `refs/mac-farm/head`, so that `refs/` is there (an input root has no empty directory). The
//! commit's tree and parents are not sent: nothing on the Mac reads them.

#[cfg(test)]
#[path = "head_tests.rs"]
mod tests;

use std::io::Write as _;
use std::path::Path;
use std::process::Command as Process;

use super::tree::Entry;
use super::{spawn, succeeded};
use crate::Error;

/// The ref of the commit the git directory holds besides `HEAD`.
pub const REF: &str = "refs/mac-farm/head";

/// `commit`, when it is a full SHA-1: 40 lowercase hex digits.
fn full_sha(commit: &str) -> bool {
    commit.len() == 40
        && commit
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The entries of the git directory of `commit` (a full SHA-1) whose object's body (what
/// `git cat-file commit` prints) is `body`.
///
/// # Errors
/// [`Error::Failed`] for a commit that is not a full SHA-1, or an object that cannot be
/// compressed.
pub fn entries(commit: &str, body: &[u8]) -> Result<Vec<(String, Entry)>, Error> {
    if !full_sha(commit) {
        return Err(Error::Failed(format!(
            "git names the checkout's HEAD {commit:?}, which is not 40 lowercase hex digits"
        )));
    }
    let mut object = format!("commit {}\0", body.len()).into_bytes();
    object.extend_from_slice(body);
    let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    zlib.write_all(&object)
        .map_err(Error::spawn("compress the commit".to_owned()))?;
    let loose = zlib
        .finish()
        .map_err(Error::spawn("compress the commit".to_owned()))?;
    let file = |bytes: Vec<u8>| Entry::File {
        bytes,
        executable: false,
    };
    let line = format!("{commit}\n").into_bytes();
    Ok(vec![
        (".git/HEAD".to_owned(), file(line.clone())),
        (
            format!(".git/objects/{}/{}", &commit[..2], &commit[2..]),
            file(loose),
        ),
        (format!(".git/{REF}"), file(line)),
    ])
}

/// The commit `<git>` says the checkout `root` is at, and its object's body.
///
/// # Errors
/// [`Error`] when git cannot be started or fails.
pub fn read(git: &Path, root: &Path) -> Result<(String, Vec<u8>), Error> {
    let head = spawn(
        Process::new(git)
            .args(["rev-parse", "--verify", "HEAD"])
            .current_dir(root),
    )?;
    let commit = succeeded("git rev-parse HEAD", head)?.trim().to_owned();
    // The body as git wrote it, byte for byte: a commit need not be UTF-8.
    let body = Process::new(git)
        .args(["cat-file", "commit", &commit])
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(Error::spawn(format!("run {:?}", git)))?;
    if !body.status.success() {
        return Err(Error::Failed(format!(
            "git cat-file failed: {}",
            String::from_utf8_lossy(&body.stderr).trim_end()
        )));
    }
    Ok((commit, body.stdout))
}
