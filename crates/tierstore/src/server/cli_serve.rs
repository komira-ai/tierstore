//! The serving of a storage node: [`run`] and [`run_serving`], the addresses they bind, the lock
//! file of the hot tier and the notes a drain leaves on stderr.

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::Notify;

use crate::{Backend, Proxy, TokioSleeper};

use super::cli::{Cli, Error, OciSetup, build};
use super::cli_reconcile::{Gate, reconcile, reconcile_gated};
use super::drain::late_of;
use super::first_beat::wait_first_beat;
use super::handoff::{Front, Handoff, HandoffError, Inherited, Offer, Socket, Want, take};
use super::listen::{bind_all_with, duplicates, local_addrs};
use super::memory_format::start_format;
use super::oci::{OciError, OciFront, OciListener, OciMode};
use super::rounds::Rounds;
use super::stop_watcher::{oci_cut_note, serve_fronts, serve_oci, stop_watcher};
use super::{Drained, Server};

/// How long a process that does not hold the `--lock-file` waits between two tries of it.
const LOCK_RETRY: Duration = Duration::from_secs(1);

/// The addresses [`run_serving`] is bound to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Addresses {
    /// The S3 front, `/healthz`, `/readyz`, `/usagez` and `/metricz`: the first address of
    /// [`s3_all`](Self::s3_all).
    pub s3: SocketAddr,
    /// Every address the S3 front is bound to, in the order of the addresses of the `s3` interfaces.
    pub s3_all: Vec<SocketAddr>,
    /// The OCI read front, if an `oci` interface was given: the first address of
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
        let none = || std::io::Error::other("an s3 interface is needed");
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
pub(super) fn bind_s3(addrs: &[SocketAddr], backlog: i32) -> Result<Vec<TcpListener>, Error> {
    bind_all_with(addrs, backlog).map_err(|(addr, why)| format!("binding {addr}: {why}").into())
}

/// Opens the proxy and serves it until `stop` resolves, and then drains.
///
/// The tiers are not tested or probed. The proxy is opened over them, which reads and writes the
/// hot tier's own state, so a hot tier that cannot be reached is an error here; the cold tier is
/// first asked by the first request that needs it. `ready` is called with the address the server
/// is bound to, before the first request is taken. Returns when the drain is over. What it says on the
/// way is `tracing` events: a caller that installs no subscriber sees none of it.
///
/// # Errors
/// A flag that is not valid with the others, a key file or a tier that cannot be used, a proxy
/// that cannot be opened, or an address that cannot be bound.
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
    run_handing(cli, None, stop, ready)
        .await
        .map_err(HandingError::into_boxed)
}

/// [`run_serving`] as an instance of a live update: with a [`Handoff`], it first asks the
/// instance that answers on the control socket for its listening sockets (and takes them if their
/// addresses are the ones the settings name), binding its own only where none answers, and then
/// answers the control socket itself, so that the next update can take from it. When a successor
/// takes its sockets it stops, as if `stop` had resolved: it drains, with the requests in flight
/// finished, and returns.
///
/// # Errors
/// As [`run_serving`], and a hand-off that is refused, that offers other addresses than the
/// settings name, or that is not ended in time ([`HandingError::Handoff`]).
pub async fn run_handing(
    cli: &Cli,
    handoff: Option<&Handoff>,
    stop: impl Future<Output = ()> + Send + 'static,
    ready: impl FnOnce(Addresses),
) -> Result<Drained, HandingError> {
    let mut built = build(cli)?;
    let gate = open_gate(cli).map_err(Error::from)?;
    start_format(cli, built.hot.as_ref()).await?;
    let (hot, cold) = (Arc::clone(&built.hot), Arc::clone(&built.cold));
    let departure = built.proxy.departure;
    let heartbeat = built.proxy.heartbeat;
    let opened = Proxy::open(hot, cold, built.proxy).await;
    let proxy = Arc::new(opened.map_err(|why| Error::from(format!("opening the proxy: {why}")))?);
    proxy.start_heartbeat(Arc::new(TokioSleeper));
    wait_first_beat(&proxy, heartbeat).await;
    let beating = Arc::clone(&proxy);
    built.server.proxy = Some(Arc::clone(&proxy));
    let oci_backend: Arc<dyn Backend> = proxy.clone();
    let listen = built.server.listen;
    let server = Server::new(proxy, built.server);
    let oci_addrs = built
        .oci
        .as_ref()
        .map_or_else(Vec::new, |setup| setup.listen.clone());
    let taken = match handoff {
        Some(handoff) => {
            take(
                handoff,
                &Want {
                    s3: &cli.listen,
                    oci: &oci_addrs,
                },
            )
            .await?
        }
        None => None,
    };
    let (listeners, inherited_oci) = match taken {
        Some(Inherited { s3, oci }) => (s3, Some(oci)),
        None => (bind_s3(&cli.listen, listen.backlog)?, None),
    };
    let oci = bind_oci_from(built.oci, oci_backend, inherited_oci).await?;
    let offer = match handoff {
        Some(handoff) => Some(
            offer_of(handoff, &listeners, oci.as_ref())?.with_accept_pause(listen.accept_pause),
        ),
        None => None,
    };
    let stop = stopped_by(stop, offer);
    ready(Addresses::of(local_addrs(&listeners)?, oci.as_ref())?);
    let oci_drain = oci.as_ref().map(OciListener::drain);
    let (late_s3, late_oci) = (Arc::clone(&server), oci_drain.clone());
    let (watcher, oci_stopped) =
        stop_watcher(stop, Arc::clone(&server), oci_drain, cli.drain_lb_wait);
    let halt = Arc::new(Notify::new());
    let halting = Arc::clone(&halt);
    let serving = async move {
        // The proxy beats until its drain is over: the in-flight requests of both fronts are
        // finished or cut.
        let ended = serve_fronts(
            server.serve_all(listeners),
            serve_oci(oci, oci_stopped),
            beating.stop_heartbeat(),
        )
        .await;
        halting.notify_one();
        ended
    };
    let reconciler = Rounds::new(built.hot, built.cold, built.reconciler, departure);
    let reconciling = async move {
        match gate {
            Some(gate) => reconcile_gated(reconciler, halt, gate).await,
            None => reconcile(reconciler, halt).await,
        }
    };
    let ((drained, oci_cut), ()) = tokio::join!(serving, reconciling);
    watcher.abort();
    super::build_info::stderr_log::notes(&cut_note(drained));
    super::build_info::stderr_log::notes(&oci_cut_note(oci_cut));
    super::build_info::stderr_log::notes(&late_of(&late_s3, late_oci.as_ref()));
    Ok(drained)
}

/// Why [`run_handing`] did not serve.
#[derive(Debug, thiserror::Error)]
pub enum HandingError {
    /// The settings, a key file, a tier, the lock file, the proxy or an S3 address could not be
    /// used (the start-up error [`run`] reports).
    #[error("{0}")]
    Start(Error),
    /// The OCI front could not be set up.
    #[error(transparent)]
    Oci(#[from] OciError),
    /// The hand-off was refused, mismatched or timed out.
    #[error(transparent)]
    Handoff(#[from] HandoffError),
    /// The address of a listening socket cannot be read, or a socket cannot be copied for the
    /// control socket.
    #[error("listening sockets: {0}")]
    Sockets(#[from] std::io::Error),
}

impl HandingError {
    /// The boxed start-up error of [`run`]: a [`Start`](Self::Start) gives back the error it holds,
    /// so that the caller still finds its type (the exit code of the binary follows it).
    pub(super) fn into_boxed(self) -> Error {
        match self {
            Self::Start(error) => error,
            other => Box::new(other),
        }
    }
}

impl From<Error> for HandingError {
    fn from(error: Error) -> Self {
        Self::Start(error)
    }
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
    Ok(bind_oci_from(setup, backend, None).await?)
}

/// [`bind_oci`], over the sockets of a running instance (`inherited`) in place of new ones: none
/// is bound.
///
/// # Errors
/// An address that cannot be bound ([`OciError::Bind`]).
async fn bind_oci_from(
    setup: Option<OciSetup>,
    backend: Arc<dyn Backend>,
    inherited: Option<Vec<TcpListener>>,
) -> Result<Option<OciListener>, OciError> {
    let Some(setup) = setup else {
        return Ok(None);
    };
    let front = OciFront::new(backend, setup.prefix, setup.metrics)
        .with_auth(setup.auth)
        .with_stage_read(setup.stage_read)
        .with_upstream(setup.upstream);
    let front = Arc::new(front);
    let listener = match inherited {
        Some(sockets) => OciListener::over(sockets, setup.acceptor, front, setup.limits),
        None => OciListener::bind_all(&setup.listen, setup.acceptor, front, setup.limits).await?,
    };
    Ok(Some(listener))
}

/// The control socket of `handoff`, offering a copy of each socket of the S3 front and of the OCI
/// front.
///
/// # Errors
/// A socket cannot be copied ([`HandingError::Sockets`]), or the control socket cannot be made
/// ([`HandingError::Handoff`]).
fn offer_of(
    handoff: &Handoff,
    s3: &[TcpListener],
    oci: Option<&OciListener>,
) -> Result<Offer, HandingError> {
    let mut sockets: Vec<Socket> = Vec::new();
    for copy in duplicates(s3)? {
        sockets.push(Socket::new(Front::S3, copy));
    }
    for copy in oci
        .map(OciListener::duplicates)
        .transpose()?
        .unwrap_or_default()
    {
        sockets.push(Socket::new(Front::Oci, copy));
    }
    Ok(Offer::bind(&handoff.socket, sockets)?)
}

/// `stop`, or the moment a successor has taken the sockets of `offer`, whichever is first.
async fn stopped_by(stop: impl Future<Output = ()> + Send + 'static, offer: Option<Offer>) {
    match offer {
        Some(offer) => tokio::select! {
            () = stop => {}
            () = offer.serve() => {}
        },
        None => stop.await,
    }
}
