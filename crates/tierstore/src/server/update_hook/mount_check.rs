//! The mount check of `verify`: on a node whose file has a `mount-provider` interface, the new
//! release's mount provider is driven end to end, as jobscheduler's agent drives it for a job,
//! with the two trees of the check (`provider/check.rs`): one placed from its L2 sidecar, one
//! from its tree list alone, the two forms of tree a build farm holds.
//!
//! 1. `Release` of both of the check's sources, so that a check that ended halfway (a killed
//!    step) leaves nothing to the next one: the provider holds nothing of them after it.
//! 2. `Prepare` of each source with its tree's list, then `Mount` of each: the directory of its
//!    tree, which must be below the provider's mount.
//! 3. A read of every path below each directory, compared with the tree: every file's bytes,
//!    every directory and the link's target, and nothing else.
//! 4. `Release` of both, then a `Mount` of each, which must be answered `UNAVAILABLE` with
//!    [`NOT_PREPARED_HERE`]: the provider finds no placement at the source's path. A directory
//!    answered then is a placement that outlived its release; any other answer is judged as at
//!    any call.
//!
//! What fails the check, and so `verify`, with the provider's reason: a refusal of any call
//! (`FAILED_PRECONDITION`, `INVALID_ARGUMENT` or any status but `UNAVAILABLE`,
//! [`Reason::ProviderRefused`]), and a tree read back wrong, a directory outside the mount, or a
//! placement that outlives its release ([`Reason::MountedTreeWrong`]). Both are the release's: the
//! node's jobs would fail the same way. What defers it ([`Reason::NotReady`], hostkeeper asks
//! again and ends the deferrals itself at `verify_deadline_secs`): a provider that does not answer
//! (its socket not bound yet: the new instance binds it once it holds the cache's lock) or answers
//! `UNAVAILABLE` (a tree not placed within the call's wait). A check that fails or defers after
//! its first `Prepare` asks for the release of both sources before it ends (each asked even when
//! the other's is not answered), so that it leaves no placement the provider can release; a
//! placement it cannot is released by the next check's first step, or at `trees.ttl_max`; the trees' objects are the release's own
//! and nothing is written to the store.
//!
//! The check's sources are its own agent's, which no job's are, and it never sweeps: it is safe
//! while jobs run. A node without the interface has no provider, and nothing is checked.

use std::collections::BTreeMap;
use std::path::{Component, PathBuf};

use super::params::Params;
use super::ports::{Host, MountProvider, ProviderCode, ProviderFault, TreeNode};
use super::report::{Done, Reason};
use super::shape::{ProviderShape, provider_interface};
use crate::server::provider::NOT_PREPARED_HERE;
use crate::server::provider::check::{CheckTree, TREES};

/// Runs the check on the node whose file the params name: what to say of it when it passed (an
/// empty text on a node with no provider, or with no file named).
///
/// # Errors
/// The check failed or is deferred; the file cannot be read, or its provider entry is refused.
pub(super) fn on_node(
    host: &dyn Host,
    provider: &dyn MountProvider,
    params: &Params,
) -> Result<String, Done> {
    let Some(config) = &params.config_file else {
        return Ok(String::new());
    };
    let Some(text) = host.config(config)? else {
        return Ok(String::new());
    };
    let shape = provider_interface(&text).map_err(|why| Done::fail(Reason::ConfigRefused, why))?;
    match shape {
        Some(shape) => check(provider, &shape, params.verify_retry_secs),
        None => Ok(String::new()),
    }
}

/// Runs the check against the provider at `shape`: what to say of it.
///
/// # Errors
/// The check failed, or is deferred by `retry_secs`.
pub(super) fn check(
    provider: &dyn MountProvider,
    shape: &ProviderShape,
    retry_secs: u64,
) -> Result<String, Done> {
    let asked = Asked {
        provider,
        shape,
        retry_secs,
    };
    asked.release_all()?;
    let placed = asked.place_and_read();
    if placed.is_err() {
        // Whatever the provider made goes before the check ends; what it says of that is not the
        // check's answer, which is the first failure.
        let _ = asked.release_all();
    }
    placed?;
    asked.release_all()?;
    for tree in TREES {
        if let Err(done) = asked.gone(&tree) {
            let _ = asked.release_all();
            return Err(done);
        }
    }
    let forms: Vec<&str> = TREES.iter().map(|tree| tree.form).collect();
    Ok(format!(
        "the mount provider at {} placed, served and released a tree {}",
        shape.socket.display(),
        forms.join(" and one ")
    ))
}

/// The provider at a socket, as the check asks it.
struct Asked<'a> {
    provider: &'a dyn MountProvider,
    shape: &'a ProviderShape,
    retry_secs: u64,
}

impl Asked<'_> {
    /// Releases both sources.
    /// Releases both sources, the second also when the first is not released: the first
    /// failure is the answer.
    fn release_all(&self) -> Result<(), Done> {
        let mut first = Ok(());
        for tree in TREES {
            let released = self.provider.release(&self.shape.socket, tree.index);
            if let (Err(fault), Ok(())) = (released, &first) {
                first = Err(self.failed("Release", &tree, fault));
            }
        }
        first
    }

    /// Prepares both sources, mounts both, and reads both trees.
    fn place_and_read(&self) -> Result<(), Done> {
        for tree in TREES {
            let prepared = self
                .provider
                .prepare(&self.shape.socket, tree.index, &tree.key());
            prepared.map_err(|fault| self.failed("Prepare", &tree, fault))?;
        }
        let mut dirs = Vec::new();
        for tree in TREES {
            let mounted = self.provider.mount(&self.shape.socket, tree.index);
            let dir = mounted.map_err(|fault| self.failed("Mount", &tree, fault))?;
            dirs.push(self.below_mount(&tree, dir)?);
        }
        for (tree, dir) in TREES.iter().zip(&dirs) {
            let read = self.provider.read_tree(dir).map_err(|why| {
                wrong(format!(
                    "the tree {} at {} cannot be read: {why}",
                    tree.form,
                    dir.display()
                ))
            })?;
            if let Some(difference) = differs(&tree.nodes(), &read) {
                return Err(wrong(format!(
                    "the tree {} at {} is not the tree prepared: {difference}",
                    tree.form,
                    dir.display()
                )));
            }
        }
        Ok(())
    }

    /// `dir`, if it is below the provider's mount.
    fn below_mount(&self, tree: &CheckTree, dir: PathBuf) -> Result<PathBuf, Done> {
        let mount = &self.shape.mountpoint;
        // Below the mount, with nothing after it but names: no `..` that leads out of it.
        let below = dir.strip_prefix(mount).ok().filter(|rest| {
            rest.components().next().is_some()
                && rest
                    .components()
                    .all(|part| matches!(part, Component::Normal(_)))
        });
        if below.is_some() {
            return Ok(dir);
        }
        Err(wrong(format!(
            "Mount of the tree {} answered {}, which is not below the mount {}",
            tree.form,
            dir.display(),
            mount.display()
        )))
    }

    /// Asks for `tree`'s source after its release: the provider must find no placement of it.
    fn gone(&self, tree: &CheckTree) -> Result<(), Done> {
        match self.provider.mount(&self.shape.socket, tree.index) {
            Err(ProviderFault::Answered {
                code: ProviderCode::Unavailable,
                reason,
            }) if reason == NOT_PREPARED_HERE => Ok(()),
            Ok(dir) => Err(wrong(format!(
                "the placement of the tree {} outlived its Release: a Mount after it answered \
                 its directory {}",
                tree.form,
                dir.display()
            ))),
            // Any other answer is judged as at any call: silence and `UNAVAILABLE` defer, a
            // refusal fails.
            Err(fault) => Err(self.failed("Mount", tree, fault)),
        }
    }

    /// What `call` of `tree`'s source answered with `fault` makes of the check.
    fn failed(&self, call: &str, tree: &CheckTree, fault: ProviderFault) -> Done {
        let socket = self.shape.socket.display();
        match fault {
            ProviderFault::Silent(why) => Done::deferred(
                Reason::NotReady,
                format!("the mount provider at {socket} does not answer {call} yet: {why}"),
                self.retry_secs,
            ),
            ProviderFault::Answered {
                code: ProviderCode::Unavailable,
                reason,
            } => Done::deferred(
                Reason::NotReady,
                format!(
                    "the mount provider at {socket} answered {call} of the tree {} with \
                     UNAVAILABLE: {reason}",
                    tree.form
                ),
                self.retry_secs,
            ),
            ProviderFault::Answered { code, reason } => Done::fail(
                Reason::ProviderRefused,
                format!(
                    "the mount provider at {socket} refused {call} of the tree {} ({}) with \
                     {code:?}: {reason}",
                    tree.form,
                    tree.key()
                ),
            ),
        }
    }
}

/// A failure of the tree read back.
fn wrong(detail: String) -> Done {
    Done::fail(Reason::MountedTreeWrong, detail)
}

/// The first difference between the tree `want` and the tree `seen`, by path: a path missing, one
/// of another kind, a file of other bytes (said by their sizes, never their bytes), a link to
/// another target, or a path that is not in the tree.
fn differs(want: &BTreeMap<String, TreeNode>, seen: &BTreeMap<String, TreeNode>) -> Option<String> {
    for (path, node) in want {
        let Some(found) = seen.get(path) else {
            return Some(format!("{path} is missing"));
        };
        let difference = match (node, found) {
            (TreeNode::File(want), TreeNode::File(got)) if want != got => format!(
                "{path} has other bytes ({} bytes, {} in the tree)",
                got.len(),
                want.len()
            ),
            (TreeNode::Link(want), TreeNode::Link(got)) if want != got => {
                format!("{path} links to {got:?}, {want:?} in the tree")
            }
            (TreeNode::File(_), TreeNode::File(_))
            | (TreeNode::Link(_), TreeNode::Link(_))
            | (TreeNode::Dir, TreeNode::Dir) => continue,
            (want, got) => format!("{path} is {}, {} in the tree", kind(got), kind(want)),
        };
        return Some(difference);
    }
    seen.keys()
        .find(|path| !want.contains_key(*path))
        .map(|path| format!("{path} is not in the tree"))
}

/// The kind of `node`, for a detail.
fn kind(node: &TreeNode) -> &'static str {
    match node {
        TreeNode::File(_) => "a file",
        TreeNode::Dir => "a directory",
        TreeNode::Link(_) => "a link",
    }
}
