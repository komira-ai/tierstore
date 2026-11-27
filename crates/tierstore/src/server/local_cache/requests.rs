//! What `tierstore fs` tells the node of the requests its frontend answers, for the cache's
//! `/metricz`: how long each took, by its kind (`docs/design/virtual-build-fs.md` section 11, the
//! requests by kind and their latency of Q-B).
//!
//! The mount times every request of its frontend from the request's arrival to its answer, on its
//! own clock (`treefs/core/requests.rs`), and counts it under one of the fixed kinds of
//! [`RequestKind`]: over FUSE by its opcode (`treefs/node_fs.rs`), over NFS by the call of the
//! protocol layer into the file system (`treefs/nfs/fs.rs`); a request of any other kind, an
//! opcode the frontend does not serve among them, is `other`. At each look for idle trees (every
//! quarter of `fs.tree_idle_secs`, at most a minute apart) and when its readers stop for a
//! hand-over, if it answered a request since the report before, it reports, as `opens` are reported
//! (`opens.rs`), by calling the cache in its process (`LocalCache::requests_reported`):
//!
//! - `<mount>`: the id of the mount;
//! - `<process>`: the id of the mount process (its pid);
//! - then, for each kind in the order of [`KINDS`], the milliseconds of all those requests and how
//!   many fell in each bucket of [`BOUNDS_MS`](super::super::attaches::BOUNDS_MS) and above the last:
//!   running totals since that process started, so that a report lost loses nothing the next one
//!   carries.
//!
//! The node adds to `tierstore_fs_request_seconds` what each process's totals grew by since its
//! report before, by the rule of the opens ([`grown`], `opens.rs`): the totals of the
//! [`PROCESSES`](super::super::descriptors::PROCESSES) latest processes of a mount are kept, and at most
//! [`MOST_MOUNTS`] mounts; a report of another, when that many are, is counted nowhere.
//!
//! The report is advice, like the others of the mount: one is lost when the thread that reports it
//! has a queue of 1024.

use std::collections::BTreeMap;
use std::sync::Mutex;

use super::super::attaches::Histogram;
use super::super::descriptors::MOST_MOUNTS;
use super::super::{LocalCache, locked};
use super::{Spans, grown};

/// The family, with a `# TYPE` line, and its series, each written out for `docs/metrics.md`.
const REQUEST: &str = "tierstore_fs_request_seconds";
const REQUEST_BUCKET: &str = "tierstore_fs_request_seconds_bucket";
const REQUEST_SUM: &str = "tierstore_fs_request_seconds_sum";
const REQUEST_COUNT: &str = "tierstore_fs_request_seconds_count";

/// The kind of a request of the frontend: the label `kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::server) enum RequestKind {
    /// A lookup of a name in a directory: FUSE's LOOKUP, NFS's LOOKUP.
    Lookup,
    /// The attributes of an inode: FUSE's GETATTR, NFS's GETATTR and the attributes the protocol
    /// layer reads for the other procedures.
    Getattr,
    /// NFS's ACCESS.
    Access,
    /// FUSE's OPEN.
    Open,
    /// A read of a file: FUSE's READ, NFS's READ.
    Read,
    /// A listing: FUSE's READDIR and READDIRPLUS, NFS's READDIR and READDIRPLUS.
    Readdir,
    /// The target of a symbolic link: FUSE's READLINK, NFS's READLINK.
    Readlink,
    /// Any other request: a release, an OPENDIR, a STATFS, a refused change, an opcode the
    /// frontend does not serve.
    Other,
}

/// Every kind, in the order of a report and of the rendered series.
pub(in crate::server) const KINDS: [RequestKind; 8] = [
    RequestKind::Lookup,
    RequestKind::Getattr,
    RequestKind::Access,
    RequestKind::Open,
    RequestKind::Read,
    RequestKind::Readdir,
    RequestKind::Readlink,
    RequestKind::Other,
];

impl RequestKind {
    /// The word of the kind, as the label `kind`.
    pub(in crate::server) fn word(self) -> &'static str {
        match self {
            Self::Lookup => "lookup",
            Self::Getattr => "getattr",
            Self::Access => "access",
            Self::Open => "open",
            Self::Read => "read",
            Self::Readdir => "readdir",
            Self::Readlink => "readlink",
            Self::Other => "other",
        }
    }

    /// The place of the kind in [`KINDS`].
    pub(in crate::server) fn index(self) -> usize {
        // The kinds are declared in the order of `KINDS`.
        self as usize
    }
}

/// The requests of a mount process: the report `requests`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::server) struct Requests {
    /// The id of the mount.
    pub mount: String,
    /// The id of the mount process.
    pub process: u64,
    /// The requests of each kind of [`KINDS`] since the mount process started.
    pub spans: [Spans; KINDS.len()],
}

/// The reports taken so far.
#[derive(Debug, Default)]
struct State {
    /// By mount, the last totals of each of its latest processes, the latest last.
    mounts: BTreeMap<String, Vec<(u64, [Spans; KINDS.len()])>>,
    /// `tierstore_fs_request_seconds`, by [`KINDS`].
    requests: [Histogram; KINDS.len()],
}

/// The reports of the requests of the mounts of one cache.
#[derive(Debug, Default)]
pub(in crate::server) struct Answered {
    state: Mutex<State>,
}

impl Answered {
    /// Takes the report: what the totals of its process grew by added (module docs).
    pub(in crate::server) fn reported(&self, report: Requests) {
        let mut state = locked(&self.state);
        let State { mounts, requests } = &mut *state;
        if !mounts.contains_key(&report.mount) && mounts.len() >= MOST_MOUNTS {
            return;
        }
        let processes = mounts.entry(report.mount).or_default();
        let more = grown(processes, report.process, report.spans);
        for (histogram, more) in requests.iter_mut().zip(&more) {
            histogram.add(&more.buckets, more.ms);
        }
    }

    /// The family in the Prometheus text format, all of one moment: the histogram of each kind,
    /// in the order of [`KINDS`].
    pub(in crate::server) fn render(&self) -> String {
        let state = locked(&self.state);
        let mut text = format!("# TYPE {REQUEST} histogram\n");
        for (kind, histogram) in KINDS.into_iter().zip(&state.requests) {
            let names = [REQUEST_BUCKET, REQUEST_SUM, REQUEST_COUNT];
            let label = format!("kind=\"{}\",", kind.word());
            histogram.render(&mut text, names, &label);
        }
        text
    }
}

impl LocalCache {
    /// A mount reported the requests of its frontend.
    pub(in crate::server) fn requests_reported(&self, report: Requests) {
        self.inner.starts.requests.reported(report);
    }
}

#[cfg(test)]
#[path = "requests_tests.rs"]
mod tests;
