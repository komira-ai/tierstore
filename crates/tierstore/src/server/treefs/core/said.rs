//! The lines said on stderr about the requests that failed with `EIO`, and the count of those not
//! said.
//!
//! A herd of jobs can meet one failure many times at once: every open of a header whose blob
//! cannot be had. So the lines about one tree and one kind of request ([`Failing`]) are said at
//! most once in [`SAY_EVERY_MS`] on the core's clock, each with the count of the failures since the
//! line before that were not said. That count is never lost: the sweep for idle trees
//! (`core/implicit.rs`, every quarter of `fs.tree_idle_secs`, at least a second and at most a
//! minute apart) says the count of every tree whose last line is [`SAY_EVERY_MS`] old, and a tree
//! that leaves memory says its counts as it leaves ([`Lines::unsaid`]).

use std::collections::HashMap;
use std::sync::Mutex;

use crate::tree_manifest::TreeDigest;

use crate::server::local_cache::descriptors::Descriptors;

use super::super::locked;
use super::Core;
use super::blob::{FailLog, SAY_EVERY_MS};

/// What kind of request failed: the lines of each kind are counted apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::server::treefs) enum Failing {
    /// An open of a file: its bytes could not be had (`core/blob.rs`).
    Open,
    /// A read of a file that is open: the cache file could not be read.
    Read,
    /// A read of a tree into memory, or of the table of tree numbers (`core/memory.rs`).
    Tree,
    /// Any other request: a listing, a lookup, a fault the code does not expect.
    Other,
}

impl Failing {
    /// The words of `count` failures of this kind, for the count of those not said.
    fn words(self, count: u64) -> String {
        let (one, many) = match self {
            Self::Open => ("failed open", "failed opens"),
            Self::Read => ("failed read of an open file", "failed reads of open files"),
            Self::Tree => ("failed read", "failed reads"),
            Self::Other => ("other failed request", "other failed requests"),
        };
        format!("{count} more {}", if count == 1 { one } else { many })
    }
}

/// The line kept of one tree and kind: when it was last said, the failures not said since, and
/// the digest of the tree where it is known.
#[derive(Debug, Default)]
struct Said {
    log: FailLog,
    digest: Option<TreeDigest>,
}

/// The lines of every tree and kind that failed lately, by tree number (0: no tree) and kind.
#[derive(Debug, Default)]
pub(in crate::server::treefs) struct Lines(Mutex<HashMap<(u64, Failing), Said>>);

impl Lines {
    /// Counts one more failure of `what` in the tree `number` (of `digest`, where known) at `now`;
    /// whether to say it, with how many before it were not said (see [`FailLog::failed`]).
    pub(in crate::server::treefs) fn failed(
        &self,
        (number, what): (u64, Failing),
        digest: Option<&TreeDigest>,
        now: u64,
    ) -> Option<u64> {
        let mut lines = locked(&self.0);
        let said = lines.entry((number, what)).or_default();
        if said.digest.is_none() {
            said.digest = digest.copied();
        }
        said.log.failed(now)
    }

    /// The lines of the failures not said: of the tree `only` whatever the time, or, with none, of
    /// every tree whose last line was said [`SAY_EVERY_MS`] or more before `now`. A tree whose
    /// count is said here has its next failure said at once, as if it had none before; so does a
    /// tree with nothing left to say whose last line is that old, which is forgotten.
    pub(in crate::server::treefs) fn unsaid(&self, now: u64, only: Option<u64>) -> Vec<String> {
        let mut out = Vec::new();
        let mut lines = locked(&self.0);
        lines.retain(|&(number, what), said| {
            let due = match only {
                Some(only) => number == only,
                None => said.log.quiet_for(now) >= SAY_EVERY_MS,
            };
            if !due {
                return true;
            }
            let unsaid = said.log.take_unsaid();
            if unsaid > 0 {
                out.push(line((number, what), said.digest.as_ref(), unsaid));
            }
            false
        });
        out.sort();
        out
    }

    /// The trees and kinds that have a line kept.
    #[cfg(test)]
    pub(in crate::server::treefs) fn kept(&self) -> usize {
        locked(&self.0).len()
    }
}

/// The line that says `unsaid` failures of `what` in the tree `number`, of `digest` where known.
fn line((number, what): (u64, Failing), digest: Option<&TreeDigest>, unsaid: u64) -> String {
    let tree = match (number, digest) {
        (0, _) => String::new(),
        (number, Some(digest)) => format!(" of the tree {digest} (number {number})"),
        (number, None) => format!(" of the tree number {number}"),
    };
    format!(
        "tierstore fs: {}{tree} since the last line, not said",
        what.words(unsaid)
    )
}

impl Core {
    /// Says the line `line` of a failure of `what` in the tree `number` (0: of no tree), of
    /// `digest` where known, at warn, once in [`SAY_EVERY_MS`] for a tree and kind, with the count
    /// of those not said before it; the line is made only if it is said.
    pub(in crate::server::treefs) fn say_failed(
        &self,
        (number, what): (u64, Failing),
        digest: Option<&TreeDigest>,
        line: &dyn Fn() -> String,
    ) {
        if let Some(unsaid) = self.lines.failed((number, what), digest, self.now()) {
            let more = match unsaid {
                0 => String::new(),
                n => format!("; {} since the last line", what.words(n)),
            };
            tracing::warn!("{}{more}", line());
        }
    }

    /// What the thread that detaches idle trees does at each look besides (`core/implicit.rs`):
    /// tells the node how many files of the cache this process holds open and how many opens
    /// failed for lack of descriptors since it started (`descriptors`, for the node's metrics: a
    /// total, so a report lost loses nothing the next one carries), says the counts of failures
    /// not said whose last line is [`SAY_EVERY_MS`] old, and says the trees in memory if they
    /// changed (`Core::say_memory`, `core/memory.rs`).
    pub(in crate::server::treefs) fn look_around(&self) {
        let report = Descriptors {
            mount: self.settings.mount_id.clone(),
            process: u64::from(std::process::id()),
            held: self.open_descriptors() as u64,
            short: self.open_files.short(),
        };
        self.fills.report(report.line());
        for line in self.lines.unsaid(self.now(), None) {
            tracing::warn!("{line}");
        }
        self.say_memory();
    }
}

#[cfg(test)]
#[path = "said_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "said_core_tests.rs"]
mod core_tests;
