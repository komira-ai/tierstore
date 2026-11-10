//! The start of a storage node with a local cache: a [`Proxy`] over the two tiers, a
//! [`LocalCache`] in front of it, the S3 front (and the OCI front, if there is one) over the
//! cache, and the reconciler beside them.
//!
//! The settings come from a file with `[hot_store]`, `[cold_store]` and `[local_cache]` (see
//! [`config_combined`](super::config_combined)). The start runs, in this order: the
//! checks of the settings, the lock file of the hot tier (the reconciler's [`Gate`]), the proxy (which writes
//! its registration `_tiering/proxies/<id>` to the hot tier), then the cache directory (its lock, the
//! file-system probe if this process holds the lock, the budget against the file system), and
//! last the addresses (taken from the running instance with `server.handoff_socket`, as a
//! storage node's are, see [`bind_fronts`](super::cli_serve::bind_fronts)). The tiers are not tested. Only the checks of the settings and the lock file
//! come before a write. A refusal of the cache directory, its file system, its budget or an address
//! comes after the proxy's registration, so it leaves the registration (a JSON object, see
//! `Registration`) in the hot tier, and the heartbeat, which starts with the proxy, may already
//! have rewritten it with a positive count; the exit status is the one the same refusal has in
//! the other starts.
//!
//! The cache directory has its own lock beside the hot tier's lock file. A second process
//! started beside the first, such as the successor of a hand-off, serves at once from the proxy;
//! its cache is `ready read-only` until the first one exits, and its reconciler waits for the lock
//! file or the lease. A hand-off that is refused, mismatched or not ended within
//! `server.handoff_wait` fails the start (exit status 1) with nothing bound; the proxy's
//! registration is written by then, as with the other refusals that come after it.
//!
//! On SIGTERM or SIGINT the process drains as the other starts do: both fronts, then the
//! reconciler (which gives its lease up), the eviction pass, the lock task and the scrub, which
//! stop with the server.

use std::future::Future;
use std::sync::Arc;

use tokio::sync::Notify;

use crate::{Backend, Clock, Proxy, SystemClock, TokioSleeper};

use super::Drained;
use super::bucket::created_at;
use super::build_info::{COMMIT, VERSION};
use super::cache_cli::{Fronts, open, serve_front};
use super::cli::{Addresses, Error, build, probe_run_id, signals};
use super::cli_reconcile::{reconcile, reconcile_gated};
use super::cli_serve::open_gate;
use super::config_combined::CombinedSettings;
use super::first_beat::wait_first_beat;
use super::first_format::first_format;
use super::listen::joined;
use super::local_cache::{Disk, SystemDisk};
use super::memory_format::start_format;
use super::rounds::Rounds;

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
    start_format(cli, built.hot.as_ref()).await?;
    built.server.bucket_created = created_at(built.hot.as_ref(), built.server.bucket_created).await;
    let (hot, cold) = (Arc::clone(&built.hot), Arc::clone(&built.cold));
    let departure = built.proxy.departure;
    let heartbeat = built.proxy.heartbeat;
    let opened = Proxy::open(hot, cold, built.proxy).await;
    let proxy = Arc::new(opened.map_err(|why| format!("opening the proxy: {why}"))?);
    proxy.start_heartbeat(Arc::new(TokioSleeper));
    wait_first_beat(&proxy, heartbeat, cli.reconcile_tuning.first_beat_poll).await;
    first_format(cli, &proxy, built.hot.as_ref(), heartbeat).await?;
    let beating = Arc::clone(&proxy);
    built.server.proxy = Some(Arc::clone(&proxy));
    let upstream: Arc<dyn Backend> = proxy;
    let run = probe_run_id(&cli.proxy_id, SystemClock.now(), std::process::id());
    let cache = open(&settings.cache.config, &upstream, &disk, &run).await?;
    built.server.cache_writes = true;
    // Both fronts read, and write, through the cache.
    let beside_halt = Arc::new(Notify::new());
    let reconciler = Rounds::tuned(
        built.hot,
        built.cold,
        built.marks,
        built.reconciler,
        departure,
        &built.entry,
    )
    .map_err(Error::from)?;
    let halt = Arc::clone(&beside_halt);
    let reconciling = async move {
        match gate {
            Some(gate) => reconcile_gated(reconciler, halt, gate).await,
            None => reconcile(reconciler, halt).await,
        }
        tracing::debug!("tierstore: reconciler ended");
    };
    let fronts = Fronts {
        listen: &cli.listen,
        lb_wait: cli.drain_lb_wait,
        scrub_rate: settings.cache.scrub_rate,
        scrub_min_round: settings.cache.scrub_min_round,
        beside_halt,
        beating: Some(beating),
        handoff: cli.handoff.as_ref(),
        fill_socket: None,
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
        super::build_info::stderr_log::answer(&format!("{line} commit={COMMIT}\n"));
    };
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    serve_combined(&settings, disk, stop, announce).await?;
    Ok(())
}
