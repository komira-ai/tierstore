//! The start of a storage node with a local cache: a [`Proxy`] over the two tiers, a
//! [`LocalCache`] in front of it, the S3 front (and the OCI front, if there is one) over the
//! cache, and the reconciler beside them.
//!
//! The settings come from a file with `[hot_store]`, `[cold_store]` and `[local_cache]` (see
//! [`config_combined`](super::config_combined)). The start runs, in this order: the flags'
//! checks, the lock file of the hot tier (the reconciler's [`Gate`]), the self-test of both tiers
//! (which writes probe keys under `_tiering/selftest/` and deletes them), the proxy (which writes
//! its marker `_tiering/proxies/<id>` to the hot tier), then the cache directory (its lock, the
//! file-system probe if this process holds the lock, the budget against the file system), and
//! last the addresses. Only the flags' checks and the lock file come before a write. A refusal
//! of the cache directory, its file system, its budget or an address comes after the self-test
//! and the proxy's marker, so it leaves the marker (an empty object, with no lease) in the hot
//! tier; the exit status is the one the same refusal has in the other starts.
//!
//! Left out, because the upstream of the cache is this node's own proxy: the HEAD of a probe key
//! and the write self-test of the upstream, which a build node makes (the tiers have just been
//! tested, and the probe keys of that test are under `_tiering/`, which both the proxy and the
//! cache refuse), and the sweep of its probe keys.
//!
//! The cache directory has its own lock beside the hot tier's lock file. A second process
//! started beside the first serves at once from the proxy; its cache is `ready read-only` until
//! the first one exits, and its reconciler waits for the lock file or the lease.
//!
//! On SIGTERM or SIGINT the process drains as the other starts do: both fronts, then the
//! reconciler (which gives its lease up), the eviction pass, the lock task and the scrub, which
//! stop with the server.

use std::future::Future;
use std::sync::Arc;

use tokio::sync::Notify;

use crate::{Backend, Clock, Proxy, Reconciler, SystemClock};

use super::Drained;
use super::build_info::{COMMIT, VERSION};
use super::cache_cli::{Fronts, open, serve_front};
use super::cli::{
    Addresses, Error, build, open_gate, probe_run_id, probe_tiers, reconcile, reconcile_gated,
    signals,
};
use super::config_combined::CombinedSettings;
use super::listen::joined;
use super::local_cache::Disk;

/// Serves the node the settings describe, with its cache on `disk`, until `stop` resolves, and
/// then drains. `ready` is called with the addresses the fronts are bound to, before the first
/// request is taken.
///
/// # Errors
/// As [`run_serving`](super::cli::run_serving), and a cache directory or file system that fails
/// the check of the start.
pub(super) async fn serve_combined(
    settings: &CombinedSettings,
    disk: Arc<dyn Disk>,
    stop: impl Future<Output = ()> + Send + 'static,
    ready: impl FnOnce(Addresses),
) -> Result<Drained, Error> {
    let cli = &settings.node;
    let mut built = build(cli)?;
    let gate = open_gate(cli)?;
    probe_tiers(
        cli,
        built.hot.as_ref(),
        built.cold.as_ref(),
        cli.backend_check_wait,
    )
    .await?;
    let (hot, cold) = (Arc::clone(&built.hot), Arc::clone(&built.cold));
    let opened = Proxy::open(hot, cold, built.proxy).await;
    let proxy = Arc::new(opened.map_err(|why| format!("opening the proxy: {why}"))?);
    built.server.proxy = Some(Arc::clone(&proxy));
    let upstream: Arc<dyn Backend> = proxy;
    let run = probe_run_id(&cli.proxy_id, SystemClock.now(), std::process::id());
    let cache = open(&settings.cache.config, &upstream, &disk, &run).await?;
    // Both fronts read, and write, through the cache.
    built.server.cache_writes = true;
    let beside_halt = Arc::new(Notify::new());
    let reconciler = Reconciler::new(built.hot, built.cold, built.reconciler);
    let interval = cli.reconcile_interval;
    let halt = Arc::clone(&beside_halt);
    let reconciling = async move {
        match gate {
            Some(gate) => reconcile_gated(reconciler, interval, halt, gate).await,
            None => reconcile(reconciler, interval, halt).await,
        }
    };
    let fronts = Fronts {
        listen: &cli.listen,
        lb_wait: cli.drain_lb_wait,
        scrub_rate: settings.cache.scrub_rate,
        scrub_min_round: settings.cache.scrub_min_round,
        sweep: None,
        beside_halt,
    };
    let (server, oci) = (built.server, built.oci);
    serve_front(cache, server, oci, fronts, stop, ready, reconciling).await
}

/// Runs the node until SIGTERM or SIGINT.
pub(super) async fn serve_combined_until_signalled(
    settings: CombinedSettings,
) -> Result<(), Error> {
    let stop = signals()?;
    let announce = |bound: Addresses| {
        let listen = joined(&bound.s3_all);
        let mut line = format!("tierstore {VERSION} listen={listen} mode=storage-cache");
        if bound.oci.is_some() {
            let how = bound.oci_mode.unwrap_or_default().as_str();
            line.push_str(&format!(" oci-listen={} ({how})", joined(&bound.oci_all)));
        }
        println!("{line} commit={COMMIT}");
    };
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    serve_combined(&settings, disk, stop, announce).await?;
    Ok(())
}
