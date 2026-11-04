//! Embeds the commit the binary is built from as `TIERSTORE_BUILD_COMMIT` and its version as
//! `TIERSTORE_BUILD_VERSION`, which `--version`, the start line and `tierstore_build_info` name
//! (`src/server/build_info.rs`).
//!
//! The version is the crate's, from `Cargo.toml`. The release workflow writes the version of the
//! tag it is about to publish into `Cargo.toml` and `Cargo.lock` of its checkout before it
//! builds (`cargo xtask release set-version`), so the binary reports its tag.
//!
//! The commit is `git rev-parse --short=12 HEAD` in the checkout being built, or `unknown` where
//! that fails (a source archive, no `git`): a build never fails for want of a commit. The rule
//! is in `src/build_commit.rs`. Uncommitted changes are not marked. Cargo runs this again when
//! `HEAD` moves or the branch it names gets a new commit.
//!
//! The build reads no environment variable of its own: neither value can be overridden.

#![expect(
    clippy::disallowed_macros,
    reason = "cargo reads the directives of a build script from its standard output"
)]

use std::path::PathBuf;
use std::process::Command;

#[path = "src/build_commit.rs"]
mod build_commit;

/// `git` with `args` in this crate's directory: its trimmed stdout, if it succeeded.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    out.status.success().then(|| text.trim().to_owned())
}

/// A path inside the git directory, as git resolves it (worktrees included).
fn git_path(name: &str) -> Option<PathBuf> {
    git(&["rev-parse", "--git-path", name]).map(PathBuf::from)
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let commit = build_commit::choose(git(&["rev-parse", "--short=12", "HEAD"]));
    println!("cargo:rustc-env=TIERSTORE_BUILD_COMMIT={commit}");
    println!(
        "cargo:rustc-env=TIERSTORE_BUILD_VERSION={}",
        env!("CARGO_PKG_VERSION")
    );
    // Watched: HEAD (a checkout), the ref it names (a commit), and packed-refs (where that ref
    // lives once packed).
    let branch = git(&["symbolic-ref", "-q", "HEAD"]);
    let watched = [Some("HEAD"), branch.as_deref(), Some("packed-refs")];
    for path in watched.into_iter().flatten().filter_map(git_path) {
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}
