//! The bound on a restart where the kernel cannot send the requests again (`docs/design/
//! virtual-build-fs.md` section 7.2).
//!
//! A process that takes over a connection from one that died cannot know the requests the dead one
//! had read and not answered. Where the kernel has `FUSE_NOTIFY_RESEND` (6.9 and later) it asks
//! for them again. Where it has not (6.8), they wait on the connection for an answer no one can
//! give, and the jobs that issued them hang. The bound is the abort of the connection: the kernel
//! then fails every request that waits, with an error the job's runtime reads as an infrastructure
//! failure, and the unit starts again on a new mount.
//!
//! The kernel shows each connection's file in `/sys/fs/fuse/connections/<n>/`: `waiting`, the
//! number of requests that wait for an answer (those not yet read and those read and not
//! answered), and `abort`, which a write of anything aborts the connection with. `<n>` is the
//! minor number of the mount's device, which `/proc/self/mountinfo` gives. A process that serves
//! the connection finds `waiting` at zero at some moment if it answers everything it is asked, at
//! the speed of a lookup; a hung request keeps it above zero for good. So the watch looks at
//! `waiting` every [`SAMPLE`] for `fs.recover_wait`: a look that finds zero ends it, and none
//! that does is a hang. This is a bound and not a proof: a connection kept busy at every look
//! for the whole of `fs.recover_wait` is aborted too, which `fs.recover_wait` is there to set
//! apart from the busiest the node's builds make it.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where the kernel shows the connections of FUSE mounts.
pub(super) const SYS_CONNECTIONS: &str = "/sys/fs/fuse/connections";

/// How often the number of waiting requests is looked at.
pub(super) const SAMPLE: Duration = Duration::from_millis(100);

/// What the watch found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// A look found no request waiting.
    Answered,
    /// Every look found a request waiting.
    Hung,
    /// The number could not be read, so nothing is known.
    Unwatched,
}

/// The connection number of the FUSE mount at `mountpoint` in the text of `/proc/self/mountinfo`:
/// the minor number of its device. The last mount at the path counts.
pub(super) fn connection_of(mountinfo: &str, mountpoint: &Path) -> Option<u64> {
    mountinfo
        .lines()
        .filter_map(|line| {
            let (before, after) = line.split_once(" - ")?;
            let fields: Vec<&str> = before.split(' ').collect();
            let fstype = after.split(' ').next()?;
            let (at, device) = (fields.get(4)?, fields.get(2)?);
            let fuse = fstype == "fuse" || fstype.starts_with("fuse.");
            (fuse && Path::new(at) == mountpoint).then(|| device.split_once(':')?.1.parse().ok())?
        })
        .next_back()
}

/// The kernel's view of one connection.
#[derive(Clone, Debug)]
pub(super) struct Connection {
    dir: PathBuf,
}

impl Connection {
    /// The connection numbered `number` below `root` (the kernel's directory of them).
    pub(super) fn new(root: &Path, number: u64) -> Self {
        Self {
            dir: root.join(number.to_string()),
        }
    }

    /// The number of requests that wait, if the kernel says.
    pub(super) fn waiting(&self) -> Option<u64> {
        std::fs::read_to_string(self.dir.join("waiting"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// Aborts the connection: every request that waits fails, and so does every later one.
    ///
    /// # Errors
    /// What the system said.
    pub(super) fn abort(&self) -> io::Result<()> {
        std::fs::write(self.dir.join("abort"), "1")
    }
}

/// Looks at the number of waiting requests up to `looks` times, calling `pause` between looks.
pub(super) fn watch(
    mut waiting: impl FnMut() -> Option<u64>,
    mut pause: impl FnMut(),
    looks: u32,
) -> Verdict {
    for _ in 0..looks {
        match waiting() {
            Some(0) => return Verdict::Answered,
            None => return Verdict::Unwatched,
            Some(_) => pause(),
        }
    }
    Verdict::Hung
}

/// The looks that make up `wait`, at least one.
pub(super) fn looks_in(wait: Duration) -> u32 {
    u32::try_from(wait.as_millis() / SAMPLE.as_millis())
        .unwrap_or(u32::MAX)
        .max(1)
}

/// Watches `connection` for `wait` on a thread of its own, sleeping between looks; a hang aborts
/// the connection and calls `on_hang`.
pub(super) fn guard(
    connection: Connection,
    wait: Duration,
    on_hang: impl FnOnce() + Send + 'static,
) -> std::thread::JoinHandle<Verdict> {
    std::thread::spawn(move || {
        let looks = looks_in(wait);
        let verdict = watch(
            || connection.waiting(),
            || std::thread::sleep(SAMPLE),
            looks,
        );
        let (secs, number) = (wait.as_secs(), connection.dir.display());
        match verdict {
            Verdict::Hung => {
                tracing::error!("tierstore fs: {secs}s unanswered on {number}: aborting");
                if let Err(why) = connection.abort() {
                    tracing::error!("tierstore fs: cannot abort the connection: {why}");
                }
                on_hang();
            }
            Verdict::Unwatched => {
                tracing::warn!("tierstore fs: {number} shows no waiting count; nothing is watched");
            }
            Verdict::Answered => {}
        }
        verdict
    })
}

#[cfg(test)]
#[path = "recover_tests.rs"]
mod tests;
