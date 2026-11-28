//! The task that carries a stop signal to the fronts of a node: the same three steps for every
//! start (the storage node's, the build node's and, where there is one, the combined node's).

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::Server;
use super::oci::{OciDrain, OciListener};

/// Spawns the task that waits for `stop`, begins the drain of the S3 front `server` and of the
/// OCI front (`oci`, if there is one), waits `lb_wait` for the load balancer to notice, and then
/// tells the receiver it returns that the OCI front is to stop taking connections.
///
/// Both fronts drain alike: `Connection: close` from the stop on, and at the end of the settle a
/// lingering close for each idle connection. The caller aborts the task once the fronts are done.
pub(super) fn stop_watcher(
    stop: impl Future<Output = ()> + Send + 'static,
    server: Arc<Server>,
    oci: Option<OciDrain>,
    lb_wait: Duration,
) -> (JoinHandle<()>, watch::Receiver<bool>) {
    let (oci_stop, oci_stopped) = watch::channel(false);
    let watcher = tokio::spawn(async move {
        stop.await;
        begin_drains(&server, oci.as_ref());
        tokio::time::sleep(lb_wait).await;
        let _ = oci_stop.send(true);
    });
    (watcher, oci_stopped)
}

/// Begins the drain of the S3 front `server` and of the OCI front, if there is one. The OCI front's
/// first, so that an answer of either front that comes after `/readyz` said 503 says
/// `Connection: close`.
fn begin_drains(server: &Server, oci: Option<&OciDrain>) {
    tracing::debug!("tierstore: drain begun");
    if let Some(oci) = oci {
        oci.begin();
    }
    server.begin_drain();
}

/// Serves the OCI front until `stopped` says so, the end of the settle; the connections it cut. 0
/// when there is none.
pub(super) async fn serve_oci(
    listener: Option<OciListener>,
    stopped: watch::Receiver<bool>,
) -> usize {
    match listener {
        Some(listener) => listener.serve(stopped).await,
        None => 0,
    }
}

/// Serves both fronts, and when both are done runs `after`: what they return, in order. The
/// proxy's heartbeat is stopped by `after`, so that it beats until the in-flight requests of the S3
/// front and of the OCI front are finished or cut, not only those of whichever front ends first.
pub(super) async fn serve_fronts<S, O>(
    s3: impl Future<Output = S>,
    oci: impl Future<Output = O>,
    after: impl Future<Output = ()>,
) -> (S, O) {
    let ended = tokio::join!(s3, oci);
    after.await;
    ended
}

#[cfg(test)]
#[path = "stop_watcher_tests.rs"]
mod tests;

/// What a drain reports on stderr when its deadline cut OCI connections: a line, and nothing when
/// it cut none.
pub(super) fn oci_cut_note(cut: usize) -> String {
    match cut {
        0 => String::new(),
        cut => format!("tierstore: {cut} OCI connections were cut at the drain deadline\n"),
    }
}
