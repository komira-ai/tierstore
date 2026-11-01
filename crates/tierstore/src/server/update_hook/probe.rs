//! What the hook reads off the answers of an instance: `/readyz` and the build gauge of
//! `/metricz`.

use super::ports::{PortError, Reply};

/// The mode a build node's cache says it is in, or none for a node that has no such mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheMode {
    /// `ready read-only`: the instance serves and does not write; it does not hold the lock.
    ReadOnly,
    /// `ready read-write`: it holds the lock.
    ReadWrite,
    /// `ready`: a node with no cache lock (a storage node), which says no mode.
    Plain,
}

/// The mode in a `/readyz` answer; `None` when the instance is not ready: no answer, a status
/// other than 200 (draining is 503), or a body that is none of the three.
#[must_use]
pub fn readiness(reply: &Result<Reply, PortError>) -> Option<CacheMode> {
    let reply = reply.as_ref().ok().filter(|r| r.status == 200)?;
    match reply.body.trim() {
        "ready read-only" => Some(CacheMode::ReadOnly),
        "ready read-write" => Some(CacheMode::ReadWrite),
        "ready" => Some(CacheMode::Plain),
        _ => None,
    }
}

/// The `tierstore_build_info` gauge of an instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Build {
    /// The `version` label.
    pub version: String,
    /// The `commit` label, when there is one.
    pub commit: Option<String>,
    /// The `slot` label: the private port of the instance, when the build says it.
    pub slot: Option<u16>,
}

impl Build {
    /// Whether this is the instance on `slot` running `version`. `version` is the crate version
    /// the gauge names, or that, `+` and the commit. A build that names no slot is no instance.
    #[must_use]
    pub fn is(&self, slot: u16, version: &str) -> bool {
        let full = self
            .commit
            .as_ref()
            .map(|c| format!("{}+{c}", self.version));
        self.slot == Some(slot) && (self.version == version || full.as_deref() == Some(version))
    }
}

/// The value of the label `name` in the text between the braces of a sample.
fn label(labels: &str, name: &str) -> Option<String> {
    let start = labels.find(&format!("{name}=\""))? + name.len() + 2;
    let len = labels[start..].find('"')?;
    Some(labels[start..start + len].to_owned())
}

/// The build gauge in the text of `/metricz`; `None` if there is none or it names no version.
#[must_use]
pub fn build_of(metrics: &str) -> Option<Build> {
    let line = metrics
        .lines()
        .find(|line| line.starts_with("tierstore_build_info{"))?;
    let labels = line.split_once('{')?.1.split_once('}')?.0;
    Some(Build {
        version: label(labels, "version")?,
        commit: label(labels, "commit"),
        slot: label(labels, "slot").and_then(|s| s.parse().ok()),
    })
}
