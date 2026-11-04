//! The reconciler's loop of a storage node: rounds every interval until a halt (a round is a pass
//! over the journals or a step over the entries, see [`Rounds`]), and the lock file ([`Gate`]) a
//! process holds while it works.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

use super::lock::{self, LockFile};
use super::rounds::Rounds;

/// How long [`reconcile`] waits for the lease to be given up once it is halted.
const STOP_WAIT: Duration = Duration::from_secs(5);

/// Runs a round of `rounds` now and then every `interval`, until `halt` is notified. A round that
/// changed something on the entries is followed by the next at once. Then it gives the lease up.
/// The calls that failed in a round are reported on stderr.
///
/// A halt cuts the pass that is running, so that the process leaves within the drain deadline
/// whatever the pass is doing. Nothing goes on from a pass that is cut: the copies are
/// create-only and the checkpoints conditional, so the next reconciler does again what the pass
/// had not finished.
pub(crate) async fn reconcile(rounds: impl Into<Rounds>, interval: Duration, halt: Arc<Notify>) {
    let mut rounds = rounds.into();
    let _ = run_passes(&mut rounds, interval, &halt, None).await;
    let _ = tokio::time::timeout(STOP_WAIT, rounds.stop()).await;
}

/// What ended a run of passes.
#[derive(Debug, PartialEq, Eq)]
enum Ended {
    /// The halt.
    Halted,
    /// The lock file at the path is not the file that is held.
    Lost,
}

/// Runs passes as [`reconcile`] does, until the halt or, if there is a `lock`, until the lock
/// file is not the one at its path any more (looked at before each pass).
async fn run_passes(
    rounds: &mut Rounds,
    interval: Duration,
    halt: &Notify,
    lock: Option<&LockFile>,
) -> Ended {
    loop {
        if lock.is_some_and(|file| !file.is_current()) {
            return Ended::Lost;
        }
        let progress = tokio::select! {
            biased;
            () = halt.notified() => return Ended::Halted,
            round = rounds.round() => {
                for error in &round.failed {
                    eprintln!("tierstore: reconciler: {error}");
                }
                round.progress
            }
        };
        if progress {
            continue;
        }
        tokio::select! {
            () = halt.notified() => return Ended::Halted,
            () = tokio::time::sleep(interval) => {}
        }
    }
}

/// The lock file a storage node's reconciler works under (`--lock-file`).
#[derive(Debug)]
pub(crate) struct Gate {
    path: PathBuf,
    /// How long to wait between two tries of a lock that is held by another process.
    retry: Duration,
    /// The lock, if the try at the start took it; the reconciler's first run uses it.
    taken: Option<LockFile>,
}

impl Gate {
    /// Opens the lock file at `path` and tries the lock once, now.
    ///
    /// # Errors
    /// What the system said about the file: it cannot be made or opened, it is a link, or it is
    /// on a network file system.
    pub(crate) fn open(path: &Path, retry: Duration) -> std::io::Result<Self> {
        Ok(Self {
            path: path.to_owned(),
            retry,
            taken: lock::try_take(path)?,
        })
    }
}

/// Tries the lock every `retry` until it is taken (`Some`) or `halt` is notified (`None`). A try
/// that fails is said on stderr and tried again. It runs on a thread that may block, so nothing
/// waits on the lock on the runtime.
async fn wait_for_lock(path: &Path, retry: Duration, halt: &Notify) -> Option<LockFile> {
    loop {
        tokio::select! {
            biased;
            () = halt.notified() => return None,
            () = tokio::time::sleep(retry) => {}
        }
        let owned = path.to_owned();
        let trying = tokio::task::spawn_blocking(move || lock::try_take(&owned));
        match trying.await.expect("a try of the lock does not panic") {
            Ok(Some(file)) => return Some(file),
            Ok(None) => {}
            Err(why) => eprintln!("tierstore: lock file {}: {why}", path.display()),
        }
    }
}

/// [`reconcile`], but only while this process holds the lock file of `gate`: it does not touch
/// the tiers (no lease, no pass) until it holds the lock, takes it by trying every
/// `gate.retry` while another process holds it, and stops (giving the lease up) and waits for the
/// lock again if the lock file is replaced. It gives the lease up before it lets the lock go,
/// when it is halted as when the file was replaced, so the process that takes the lock next finds
/// the lease free at its first pass.
pub(crate) async fn reconcile_gated(
    rounds: impl Into<Rounds>,
    interval: Duration,
    halt: Arc<Notify>,
    mut gate: Gate,
) {
    let mut rounds = rounds.into();
    loop {
        let file = match gate.taken.take() {
            Some(file) => file,
            None => match wait_for_lock(&gate.path, gate.retry, &halt).await {
                Some(file) => file,
                // Halted before the lock was taken: no lease is held, none was taken since the
                // last stop.
                None => return,
            },
        };
        let ended = run_passes(&mut rounds, interval, &halt, Some(&file)).await;
        if ended == Ended::Lost {
            eprintln!(
                "tierstore: lock file {}: the file was replaced; the reconciler stops until the \
                 lock is taken again",
                gate.path.display()
            );
        }
        // The lease first, then the lock: a process waiting for the lock must not get it while
        // the lease is still held, or its first pass finds the lease taken and does nothing.
        let _ = tokio::time::timeout(STOP_WAIT, rounds.stop()).await;
        drop(file);
        if ended == Ended::Halted {
            return;
        }
    }
}

#[cfg(test)]
#[path = "cli_reconcile_tests.rs"]
mod tests;
