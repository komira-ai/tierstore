//! The cache lock after `ready` (`docs/design/one-service.md` section 3.2): the running
//! instance's `written`, and the successor's wait for it.
//!
//! An instance that offered its lock and was told `ready` starts no cache write from then on
//! (`LocalCache::hand_writes`), lets the writes in flight end and says `written`; the successor
//! takes the write role then, or when the connection ends first (the instance exited or crashed,
//! so it writes nothing more). The successor keeps its end of the connection until its own process
//! ends. When the connection ends early, the successor died: the instance, the lock's last other
//! holder, lets its writes in flight end and unlocks, so that the successor started again takes the
//! lock at its next tick rather than after this instance's whole drain.

use tokio::net::UnixStream;

use crate::server::local_cache::LocalCache;
use crate::server::lock::Received;

use super::wire;

/// The frame that says the running instance's cache writes have ended.
pub(super) const WRITTEN: &str = "written";

/// Reads frames from `stream` until it ends: whatever comes before the end is not the protocol
/// after `bye` and is skipped.
async fn closed(stream: &UnixStream) {
    while wire::recv(stream).await.is_ok() {}
}

/// The running instance after `bye`, when it offered its lock and the successor took the offer:
/// `written` once its writes in flight have ended, and the unlock when the successor is gone.
/// Runs until the successor's end of `stream` closes, or this process ends. A `written` that cannot
/// be sent ends it without the unlock: the successor may still live.
pub(super) async fn after_ready(stream: UnixStream, cache: LocalCache) {
    let gone = tokio::select! {
        () = cache.writes_ended() => false,
        () = closed(&stream) => true,
    };
    if !gone {
        if let Err(why) = wire::send(&stream, &[WRITTEN.to_owned()], &[]).await {
            // The successor may live and wait for the end of the connection: the lock is left
            // locked, to the end of this process.
            tracing::warn!("tierstore: hand-off: written was not sent ({why}); the lock is kept");
            return;
        }
        closed(&stream).await;
    }
    // The successor is gone; whatever this cache still wrote has ended once this returns.
    cache.writes_ended().await;
    match cache.unlock_handed() {
        Ok(()) => tracing::info!(
            "tierstore: hand-off: the successor is gone; the cache lock is unlocked for the one \
             started next"
        ),
        Err(why) => tracing::warn!("tierstore: hand-off: the cache lock was not unlocked: {why}"),
    }
}

/// How the instance a lock was taken from ended its part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wrote {
    /// It said `written`: it writes nothing more.
    Written,
    /// The connection ended first: it exited or crashed.
    Ended,
}

/// The successor's connection to the instance it took the lock from, after `bye`. It is held
/// until the successor's process ends, so that the instance tells its end from that of the
/// successor.
pub(crate) struct Predecessor(pub(super) UnixStream);

impl Predecessor {
    /// Waits for `written`, or for the end of the connection. Frames that are neither are skipped.
    pub(crate) async fn written(&self) -> Wrote {
        loop {
            match wire::recv(&self.0).await {
                Ok(frame) if frame.lines == [WRITTEN] => return Wrote::Written,
                Ok(_) => {}
                Err(_) => return Wrote::Ended,
            }
        }
    }

    /// Waits for the end of the connection: the instance has exited.
    pub(crate) async fn ended(&self) {
        closed(&self.0).await;
    }
}

/// The successor's side of the lock after `bye`: it takes the write role with `received` on
/// `written` or the end of the connection, and once the predecessor is gone it may offer the lock
/// in its turn. It runs until the predecessor has ended, and holds the connection until then.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the start of the one process follows the lock it took (one-service.md section 6, P3)"
    )
)]
pub(crate) async fn follow(predecessor: Predecessor, received: Received, cache: LocalCache) {
    let wrote = predecessor.written().await;
    let taking = cache.clone();
    let took = tokio::task::spawn_blocking(move || taking.take_handed(received)).await;
    match took.map_err(std::io::Error::other).and_then(|took| took) {
        Ok(tick) => tracing::info!("tierstore: hand-off: the cache lock: {tick:?}"),
        Err(why) => tracing::warn!(
            "tierstore: hand-off: the handed cache lock was not taken ({why}); the next tick tries it"
        ),
    }
    if wrote == Wrote::Written {
        predecessor.ended().await;
    }
    cache.predecessor_gone();
}

#[cfg(test)]
#[path = "written_tests.rs"]
mod tests;
