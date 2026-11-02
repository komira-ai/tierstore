//! The task that carries a stop signal to the fronts of a node: the same three steps for every
//! start (the storage node's, the build node's and, where there is one, the combined node's).

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::Server;
use super::cli::begin_drains;
use super::oci::OciDrain;

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
