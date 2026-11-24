//! A build node's hand-over to the next image of its own program (`docs/design/one-service.md`
//! section 3.4), the live update of a Mac, whose one launchd job has one process.
//!
//! SIGUSR2 (on macOS; `launchctl kill SIGUSR2 system/<label>`) asks for it ([`stop_or_next`]). The
//! process then stops as on SIGTERM: the S3 and OCI fronts drain (`server.drain_lb_wait`, then
//! `server.drain_deadline`), and the loops beside them end. The listening sockets stay open
//! meanwhile, as copies made at the start ([`NextImage::hand`] gets them), so new connections wait
//! in their backlog. Then the cache starts no write, and its writes in flight end, as at `ready`
//! of a hand-over; its lock is kept, disarmed, so nothing of this process unlocks it. The offer of
//! the sockets and the lock is written into a `socketpair`, and the process executes its program
//! path (`std::env::current_exe`, where the update installed the new binary) with its arguments and
//! `--handed 0` ([`exec_next`], `handoff/exec.rs`). The pid stays; launchd sees no exit.
//!
//! The new image, started with `--handed 0`, opens its cache read-only (the lock is held by the
//! description in the offer), takes the sockets and the lock from the offer, and serves on the
//! sockets at once; it takes the write role with the handed lock ([`take_write_role`]: the stage
//! emptied, the objects scanned again, the index replaced) before any write of its cache, as a
//! successor does on `written`. Until then its misses are served from the upstream without a fill.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::server::cli::Error;
use crate::server::config::Settings;
use crate::server::handoff::{Handing, HandoffError, Image, Socket, handed_input};
use crate::server::local_cache::LocalCache;
use crate::server::lock::Received;

/// Whether the process hands over to its next image when it stops, and what it hands over.
#[derive(Default)]
pub(in crate::server) struct NextImage {
    /// SIGUSR2 came: the stop is a hand-over.
    asked: AtomicBool,
    /// What the fronts and the cache left to hand over, once they are done.
    handing: std::sync::Mutex<Option<Handing>>,
}

impl NextImage {
    /// The stop that comes is a hand-over to the next image.
    pub(in crate::server) fn ask(&self) {
        self.asked.store(true, Ordering::SeqCst);
    }

    /// Whether the stop is a hand-over to the next image.
    pub(in crate::server) fn asked(&self) -> bool {
        self.asked.load(Ordering::SeqCst)
    }

    /// The hand-over's step once the fronts have drained and the loops have ended: the lock of
    /// `cache`, if it holds it, is taken out for the offer; then the cache starts no write, and
    /// this returns once its writes in flight have ended. `sockets` (copies of the fronts'
    /// listening sockets, which the drain did not close) are kept for the offer with the lock.
    ///
    /// # Errors
    /// What the system said of the copy of the lock's descriptor.
    pub(in crate::server) async fn hand(
        &self,
        sockets: Vec<Socket>,
        cache: &LocalCache,
    ) -> std::io::Result<()> {
        // `None` while this image has not yet taken the write role with a lock it was handed: the
        // next image then takes the lock at a tick, once this process's descriptor is gone.
        let lock = cache.lock_to_hand()?;
        cache.hand_writes();
        cache.writes_ended().await;
        let handing = Handing {
            sockets,
            control: None,
            lock,
        };
        *self
            .handing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handing);
        Ok(())
    }

    /// The next image, its offer written, when this stop handed over ([`hand`](Self::hand) ran);
    /// `None` for a stop. `program` is what is executed and `args` its arguments, as
    /// `std::env::args_os` gives them.
    ///
    /// # Errors
    /// The offer cannot be written ([`Image::prepare`]).
    pub(in crate::server) async fn image(
        &self,
        program: impl FnOnce() -> std::io::Result<PathBuf>,
        args: impl IntoIterator<Item = std::ffi::OsString>,
    ) -> Result<Option<Image>, HandoffError> {
        let handing = self
            .handing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some(handing) = handing else {
            return Ok(None);
        };
        let program = program().map_err(HandoffError::io("finding this process's program"))?;
        Image::prepare(&handing, program, args).await.map(Some)
    }
}

/// Whether this system hands a build node over to its next image: macOS, whose one launchd job has
/// one process. On Linux the a/b hand-over of the units is the live update, and SIGUSR2 is left
/// as it is.
pub(in crate::server) const HANDS_TO_NEXT_IMAGE: bool = cfg!(target_vendor = "apple");

/// SIGTERM or SIGINT, or, where this system hands over to the next image
/// ([`HANDS_TO_NEXT_IMAGE`]), SIGUSR2, which first tells `next` that the stop is a hand-over.
///
/// # Errors
/// A handler cannot be installed.
pub(in crate::server) fn stop_or_next(
    next: Arc<NextImage>,
) -> std::io::Result<impl Future<Output = ()> + Send + 'static> {
    let asking = HANDS_TO_NEXT_IMAGE.then(tokio::signal::unix::SignalKind::user_defined2);
    stop_or_next_on(next, asking)
}

/// [`stop_or_next`], with `asking` the signal that asks for the hand-over, if any.
///
/// # Errors
/// A handler cannot be installed.
fn stop_or_next_on(
    next: Arc<NextImage>,
    asking: Option<tokio::signal::unix::SignalKind>,
) -> std::io::Result<impl Future<Output = ()> + Send + 'static> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut asked = asking.map(signal).transpose()?;
    Ok(async move {
        let asked = async {
            match asked.as_mut() {
                Some(signal) => signal.recv().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
            _ = asked => {
                tracing::info!("tierstore: SIGUSR2: handing over to the next image of the program");
                next.ask();
            }
        }
    })
}

/// The new image's step of the lock it was handed: the cache, opened read-only because the
/// offer's descriptor holds the lock, takes the write role with it (on a thread that may block,
/// for the scan); no process is left to hand it back, so the cache may offer it in its turn at
/// once. A take that fails is said: the received descriptor is closed, which frees the lock, and
/// the next tick takes it.
pub(in crate::server) async fn take_write_role(cache: LocalCache, received: Received) {
    let taking = cache.clone();
    let took = tokio::task::spawn_blocking(move || taking.take_handed(received)).await;
    match took.map_err(std::io::Error::other).and_then(|took| took) {
        Ok(tick) => tracing::info!("tierstore: the handed cache lock: {tick:?}"),
        Err(why) => tracing::warn!(
            "tierstore: the handed cache lock was not taken ({why}); the next tick tries the lock"
        ),
    }
    cache.predecessor_gone();
}

/// Executes `image`, if a stop handed over to it; nothing for a stop. It returns, with the reason,
/// only when the exec failed: the process then exits, and launchd starts the job again, which
/// binds its sockets itself.
///
/// # Errors
/// Why the exec failed.
pub(in crate::server) fn exec_next(image: Option<Image>) -> Result<(), Error> {
    let Some(image) = image else {
        return Ok(());
    };
    let why = image.exec();
    Err(format!("the next image could not be executed: {why}").into())
}

/// The offer of a start given `--handed` (`handed`): a copy of its standard input. Only a build node
/// is handed over to its next image.
///
/// # Errors
/// The exit status: 2 for `--handed` with a file of another shape, 1 when standard input cannot be
/// taken.
pub(in crate::server) fn handed_offer(
    handed: bool,
    settings: &Settings,
) -> Result<Option<std::os::fd::OwnedFd>, std::process::ExitCode> {
    if !handed {
        return Ok(None);
    }
    if !matches!(settings, Settings::Cache(_)) {
        tracing::error!(
            "tierstore: --handed: only a build node (a file with [local_cache] and [cold_store]) \
             is handed over to the next image of its program"
        );
        return Err(std::process::ExitCode::from(2));
    }
    handed_input().map(Some).map_err(|why| {
        tracing::error!("tierstore: --handed: the offer on standard input: {why}");
        std::process::ExitCode::from(1)
    })
}

#[cfg(test)]
#[path = "next_image_tests.rs"]
mod tests;
