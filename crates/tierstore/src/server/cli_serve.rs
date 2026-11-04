//! The serving of a storage node: [`run`] and [`run_serving`], the addresses they bind, the lock
//! file of the hot tier and the notes a drain leaves on stderr.

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::Notify;

use crate::{Backend, Proxy, Reconciler, TokioSleeper};

use super::cli::{Cli, Error, OciSetup, build};
use super::cli_probe::probe_tiers;
use super::cli_reconcile::{Gate, reconcile, reconcile_gated};
use super::drain::late_of;
use super::listen::{bind_all, local_addrs};
use super::oci::{OciFront, OciListener, OciMode};
use super::stop_watcher::{oci_cut_note, serve_oci, stop_watcher};
use super::{Drained, Server};

/// How long a process that does not hold the `--lock-file` waits between two tries of it.
const LOCK_RETRY: Duration = Duration::from_secs(1);

/// The addresses [`run_serving`] is bound to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Addresses {
    /// The S3 front, `/healthz`, `/readyz`, `/usagez` and `/metricz`: the first address of
    /// [`s3_all`](Self::s3_all).
    pub s3: SocketAddr,
    /// Every address the S3 front is bound to, in the order of the `--listen` flags.
    pub s3_all: Vec<SocketAddr>,
    /// The OCI read front, if `--oci-listen` was given: the first address of
    /// [`oci_all`](Self::oci_all).
    pub oci: Option<SocketAddr>,
    /// Every address the OCI front is bound to; empty when there is none.
    pub oci_all: Vec<SocketAddr>,
    /// How the OCI front carries its connections: `Some` exactly when [`oci`](Self::oci) is.
    pub oci_mode: Option<OciMode>,
}

impl Addresses {
    /// The addresses of the sockets bound: the S3 front's, which are at least one, and the OCI
    /// front's, if it is served.
    ///
    /// # Errors
    /// A socket's address cannot be read, or the S3 front has none.
    pub(super) fn of(s3_all: Vec<SocketAddr>, oci: Option<&OciListener>) -> std::io::Result<Self> {
        let oci_all = oci.map(OciListener::local_addrs).transpose()?;
        let oci_all = oci_all.unwrap_or_default();
        let none = || std::io::Error::other("--listen is needed");
        Ok(Self {
            s3: s3_all.first().copied().ok_or_else(none)?,
            s3_all,
            oci: oci_all.first().copied(),
            oci_all,
            oci_mode: oci.map(OciListener::mode),
        })
    }
}

/// Binds the S3 front's sockets, one for each of `addrs`.
///
/// # Errors
/// An address cannot be bound; the message names it.
pub(super) fn bind_s3(addrs: &[SocketAddr]) -> Result<Vec<TcpListener>, Error> {
    bind_all(addrs).map_err(|(addr, why)| format!("binding {addr}: {why}").into())
}

/// Opens the proxy and serves it until `stop` resolves, and then drains.
///
/// The tiers are tested first (see [`self_test`](crate::self_test)): a tier that fails is an error before the
/// proxy is opened or the address bound. `ready` is called with the address the server is bound
/// to, before the first request is taken. Returns when the drain is over.
///
/// # Errors
/// A flag that is not valid with the others, a key file or a tier that cannot be used, a tier
/// that fails the self-test (a [`SelfTestError`](crate::SelfTestError)), a proxy that cannot be opened, or an address
/// that cannot be bound.
pub async fn run(
    cli: &Cli,
    stop: impl Future<Output = ()> + Send + 'static,
    ready: impl FnOnce(SocketAddr),
) -> Result<Drained, Error> {
    run_serving(cli, stop, |bound| ready(bound.s3)).await
}

/// [`run`], telling `ready` the address of the OCI front too when it is served.
///
/// The OCI front drains as the S3 front does: from the stop on every answer says
/// `Connection: close`; it keeps serving through `--drain-lb-wait`, then takes no new connection,
/// closes each idle connection with a lingering close and each other one in the same way once its
/// answer is out, and waits for them within `--drain-deadline`.
///
/// # Errors
/// As [`run`], and the OCI front's flags and files (see [`OciError`](super::oci::OciError)).
pub async fn run_serving(
    cli: &Cli,
    stop: impl Future<Output = ()> + Send + 'static,
    ready: impl FnOnce(Addresses),
) -> Result<Drained, Error> {
    let mut built = build(cli)?;
    let gate = open_gate(cli)?;
    let (hot, cold) = (built.hot.as_ref(), built.cold.as_ref());
    probe_tiers(cli, hot, cold, cli.backend_check_wait).await?;
    let (hot, cold) = (Arc::clone(&built.hot), Arc::clone(&built.cold));
    let opened = Proxy::open(hot, cold, built.proxy).await;
    let proxy = Arc::new(opened.map_err(|why| format!("opening the proxy: {why}"))?);
    proxy.start_heartbeat(Arc::new(TokioSleeper));
    let beating = Arc::clone(&proxy);
    built.server.proxy = Some(Arc::clone(&proxy));
    let oci_backend: Arc<dyn Backend> = proxy.clone();
    let server = Server::new(proxy, built.server);
    let listeners = bind_s3(&cli.listen)?;
    let oci = bind_oci(built.oci, oci_backend).await?;
    ready(Addresses::of(local_addrs(&listeners)?, oci.as_ref())?);
    let oci_drain = oci.as_ref().map(OciListener::drain);
    let (late_s3, late_oci) = (Arc::clone(&server), oci_drain.clone());
    let (watcher, oci_stopped) =
        stop_watcher(stop, Arc::clone(&server), oci_drain, cli.drain_lb_wait);
    let halt = Arc::new(Notify::new());
    let halting = Arc::clone(&halt);
    let serving = async move {
        let drained = server.serve_all(listeners).await;
        // The proxy beats until its drain is over: its in-flight requests are finished or cut.
        beating.stop_heartbeat().await;
        halting.notify_one();
        drained
    };
    let oci_serving = serve_oci(oci, oci_stopped);
    let reconciler = Reconciler::new(built.hot, built.cold, built.reconciler);
    let interval = cli.reconcile_interval;
    let reconciling = async move {
        match gate {
            Some(gate) => reconcile_gated(reconciler, interval, halt, gate).await,
            None => reconcile(reconciler, interval, halt).await,
        }
    };
    let (drained, (), oci_cut) = tokio::join!(serving, reconciling, oci_serving);
    watcher.abort();
    eprint!("{}", cut_note(drained));
    eprint!("{}", oci_cut_note(oci_cut));
    eprint!("{}", late_of(&late_s3, late_oci.as_ref()));
    Ok(drained)
}

/// Takes the lock file of the hot tier, if the flags name one: one try now, so that a lock file
/// that cannot be used stops the start; the reconciler's loop takes it later if another process
/// holds it now.
///
/// # Errors
/// A lock file that cannot be opened.
pub(super) fn open_gate(cli: &Cli) -> Result<Option<Gate>, LockFileError> {
    match &cli.lock_file {
        Some(path) => match Gate::open(path, LOCK_RETRY) {
            Ok(gate) => Ok(Some(gate)),
            Err(source) => Err(LockFileError {
                path: path.clone(),
                source,
            }),
        },
        None => Ok(None),
    }
}

/// A lock file that cannot be opened: the file and what the system said.
#[derive(Debug, thiserror::Error)]
#[error("--lock-file {}: {source}", path.display())]
pub(super) struct LockFileError {
    path: PathBuf,
    source: std::io::Error,
}

/// What [`run`] reports on stderr of a drain: a line when the drain deadline cut requests, and
/// nothing when it cut none.
pub(super) fn cut_note(drained: Drained) -> String {
    match drained.abandoned {
        0 => String::new(),
        cut => format!("tierstore: {cut} requests were cut at the drain deadline\n"),
    }
}

/// Binds the OCI front over `backend` as `setup` says: none when there is no setup. The backend
/// is whatever the node reads from: the proxy of a storage node, the cache of a build node.
///
/// # Errors
/// An address that cannot be bound.
pub(super) async fn bind_oci(
    setup: Option<OciSetup>,
    backend: Arc<dyn Backend>,
) -> Result<Option<OciListener>, Error> {
    let Some(setup) = setup else {
        return Ok(None);
    };
    let front = Arc::new(OciFront::new(backend, setup.prefix, setup.metrics));
    let bound = OciListener::bind_all(&setup.listen, setup.acceptor, front, setup.limits).await;
    Ok(Some(bound?))
}
