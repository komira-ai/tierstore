//! The start of a storage node with a local cache: a [`Proxy`] over the two tiers, a
//! [`LocalCache`] in front of it, the S3 front (and the OCI front, if there is one) over the
//! cache, and the reconciler beside them.
//!
//! The settings come from a file with `[hot_store]`, `[cold_store]` and `[local_cache]` (see
//! [`config_combined`](super::config_combined)). The start runs, in this order: the flags'
//! checks, the lock file of the hot tier (the reconciler's [`Gate`]), the self-test of both tiers
//! (which writes probe keys under `_tiering/selftest/` and deletes them), the proxy (which writes
//! its registration `_tiering/proxies/<id>` to the hot tier), then the cache directory (its lock, the
//! file-system probe if this process holds the lock, the budget against the file system), and
//! last the addresses. Only the flags' checks and the lock file come before a write. A refusal
//! of the cache directory, its file system, its budget or an address comes after the self-test
//! and the proxy's registration, so it leaves the registration (a JSON object, see `Registration`) in the hot
//! tier, and the heartbeat, which starts with the proxy, may already have rewritten it with a
//! positive count; the exit status is the one the same refusal has in the other starts.
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

use crate::{Backend, Clock, Proxy, Reconciler, SystemClock, TokioSleeper};

use super::build_info::{COMMIT, VERSION};
use super::cache_cli::{LOCK_INTERVAL, PASS_INTERVAL, bind_cache_s3, open};
use super::cli::{Addresses, Error, build, signals};
use super::cli_probe::{probe_run_id, probe_tiers};
use super::cli_reconcile::{reconcile, reconcile_gated};
use super::cli_serve::{bind_oci, cut_note, open_gate};
use super::config_combined::CombinedSettings;
use super::drain::late_of;
use super::listen::{joined, local_addrs};
use super::local_cache::{Disk, Every, SCRUB_TICK, SystemDisk};
use super::oci::OciListener;
use super::stop_watcher::{oci_cut_note, serve_fronts, serve_oci, stop_watcher};
use super::{Drained, Server};

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
    proxy.start_heartbeat(Arc::new(TokioSleeper));
    let beating = Arc::clone(&proxy);
    built.server.proxy = Some(Arc::clone(&proxy));
    let upstream: Arc<dyn Backend> = proxy;
    let run = probe_run_id(&cli.proxy_id, SystemClock.now(), std::process::id());
    let cache = open(&settings.cache.config, &upstream, &disk, &run).await?;
    built.server.cache = Some(cache.clone());
    built.server.cache_writes = true;
    // Both fronts read, and write, through the cache.
    let backend: Arc<dyn Backend> = Arc::new(cache.clone());
    let server = Server::new(Arc::clone(&backend), built.server);
    let listeners = bind_cache_s3(&cli.listen)?;
    let oci = bind_oci(built.oci, backend).await?;
    ready(Addresses::of(local_addrs(&listeners)?, oci.as_ref())?);
    let oci_drain = oci.as_ref().map(OciListener::drain);
    let (late_s3, late_oci) = (Arc::clone(&server), oci_drain.clone());
    let lb_wait = cli.drain_lb_wait;
    let (watcher, oci_stopped) = stop_watcher(stop, Arc::clone(&server), oci_drain, lb_wait);
    // One Notify for each loop: `notify_one` wakes one waiter, and keeps the wake for a loop that
    // is not waiting just now.
    let halts: [Arc<Notify>; 4] = std::array::from_fn(|_| Arc::new(Notify::new()));
    let [halt, maintain_halt, lock_halt, scrub_halt] = halts.clone();
    let serving = async move {
        // The proxy beats until its drain is over: the in-flight requests of both fronts are
        // finished or cut.
        let ended = serve_fronts(
            server.serve_all(listeners),
            serve_oci(oci, oci_stopped),
            beating.stop_heartbeat(),
        )
        .await;
        for halting in &halts {
            halting.notify_one();
        }
        ended
    };
    let reconciler = Reconciler::new(built.hot, built.cold, built.reconciler);
    let interval = cli.reconcile_interval;
    let reconciling = async move {
        match gate {
            Some(gate) => reconcile_gated(reconciler, interval, halt, gate).await,
            None => reconcile(reconciler, interval, halt).await,
        }
    };
    let maintaining = cache.maintain(PASS_INTERVAL, maintain_halt);
    let holding = cache.hold(LOCK_INTERVAL, lock_halt);
    let min_round = settings.cache.scrub_min_round.as_secs() / SCRUB_TICK.as_secs();
    let scrubbing = cache.scrub(
        settings.cache.scrub_rate,
        min_round,
        Every::new(SCRUB_TICK),
        scrub_halt,
    );
    let ((drained, oci_cut), (), (), (), ()) =
        tokio::join!(serving, reconciling, maintaining, holding, scrubbing);
    watcher.abort();
    eprint!("{}", cut_note(drained));
    eprint!("{}", oci_cut_note(oci_cut));
    eprint!("{}", late_of(&late_s3, late_oci.as_ref()));
    Ok(drained)
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
