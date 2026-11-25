//! The start of a node over the cold tier alone (see
//! [`config_cold`](crate::server::config::cold)): the S3 front, and the OCI front when the file
//! has an `oci` interface, served over the cold store with nothing between.
//!
//! The cold store is not tested or probed at start: one that cannot be reached, or a key that may
//! not write, shows as the ordinary error of the first request that meets it. Nothing runs beside
//! the fronts: no heartbeat, no reconciler, no cache. On SIGTERM or SIGINT the node drains as a
//! storage node does, and owes the store nothing when it exits.
//!
//! With `server.handoff_socket` the start gets its listening sockets as a storage node's does
//! (see [`bind_fronts`]): it takes the S3 and OCI sockets of the instance that answers on the
//! control socket, or binds its own where none answers, and then answers there itself. The
//! sockets are the addresses the file names, as a storage node's are (no loopback socket is
//! added), so a storage node's instance hands its sockets to a start of this shape over the same
//! addresses.

use std::future::Future;
use std::sync::Arc;

use crate::{Backend, Clock, SystemClock};

use crate::server::build_info::{COMMIT, VERSION};
use crate::server::cli::{
    Addresses, Error, HandingError, S3Flags, oci_setup_of, open_tier, signals,
};
use crate::server::config::cold::ColdSettings;
use crate::server::drain::late_of;
use crate::server::front::server_config;
use crate::server::listen::joined;
use crate::server::oci::OciListener;
use crate::server::stop_watcher::{oci_cut_note, serve_fronts, serve_oci, stop_watcher};
use crate::server::{Drained, Server};

use super::{Bound, bind_fronts, bind_s3, cut_note, stopped_by};

/// Serves the fronts over `cold` until `stop` resolves, and then drains. `ready` is called with
/// the addresses once the sockets are bound or taken, before the first request is taken.
///
/// # Errors
/// The front's settings and key files, the OCI front's, an address that cannot be bound, or a
/// hand-off that is refused, mismatched or not ended within `server.handoff_wait`.
pub(in crate::server) async fn serve_cold(
    settings: &ColdSettings,
    cold: Arc<dyn Backend>,
    stop: impl Future<Output = ()> + Send + 'static,
    ready: impl FnOnce(Addresses),
) -> Result<Drained, Error> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let mut config = server_config(&settings.front, clock, true)?;
    let oci = oci_setup_of(&settings.oci, &config, true)?;
    config.oci_metrics = oci.as_ref().map(|setup| Arc::clone(&setup.metrics));
    let listen = config.listen;
    let server = Server::new(Arc::clone(&cold), config);
    let addrs = &settings.front.listen;
    let Bound {
        s3: listeners,
        oci,
        addresses,
        offer,
        lock: _,
    } = bind_fronts(
        settings.handoff.as_ref(),
        None,
        addrs,
        || bind_s3(addrs, listen.backlog),
        oci,
        cold,
    )
    .await
    .map_err(HandingError::into_boxed)?;
    let offer = offer.map(|offer| offer.with_accept_pause(listen.accept_pause));
    let stop = stopped_by(stop, offer);
    ready(addresses);
    let oci_drain = oci.as_ref().map(OciListener::drain);
    let (late_s3, late_oci) = (Arc::clone(&server), oci_drain.clone());
    let lb_wait = settings.front.drain_lb_wait;
    let (watcher, oci_stopped) = stop_watcher(stop, Arc::clone(&server), oci_drain, lb_wait);
    let (drained, oci_cut) = serve_fronts(
        server.serve_all(listeners),
        serve_oci(oci, oci_stopped),
        std::future::ready(()),
    )
    .await;
    watcher.abort();
    crate::server::build_info::stderr_log::notes(&cut_note(drained));
    crate::server::build_info::stderr_log::notes(&oci_cut_note(oci_cut));
    crate::server::build_info::stderr_log::notes(&late_of(&late_s3, late_oci.as_ref()));
    Ok(drained)
}

/// Opens the cold store the settings name and serves the fronts over it until `stop` resolves.
///
/// # Errors
/// As [`serve_cold`], a request timeout of zero, and a cold store the settings do not make.
pub(in crate::server) async fn run_cold(
    settings: &ColdSettings,
    stop: impl Future<Output = ()> + Send + 'static,
    ready: impl FnOnce(Addresses),
) -> Result<Drained, Error> {
    if settings.s3_request_timeout.is_zero() {
        return Err("--s3-request-timeout must be longer than zero".into());
    }
    let tier = &settings.cold;
    let s3 = S3Flags {
        endpoint: tier.endpoint.as_deref(),
        more_endpoints: &tier.more_endpoints,
        region: tier.region.as_deref(),
        credentials: tier.credentials_file.as_deref(),
        source: tier.credential_source.as_ref(),
        conditional_delete: tier.conditional_delete,
    };
    let cold = open_tier(
        "--cold",
        &tier.spec,
        &s3,
        settings.s3_request_timeout,
        &settings.s3_client,
    )?;
    serve_cold(settings, cold, stop, ready).await
}

/// The line a start says on stderr once it serves: the release, the addresses, the shape and the
/// commit.
pub(in crate::server) fn announce_line(bound: &Addresses) -> String {
    let mut line = format!(
        "tierstore {VERSION} listen={} mode=cold-only",
        joined(&bound.s3_all)
    );
    if bound.oci.is_some() {
        let how = bound.oci_mode.unwrap_or_default().as_str();
        line.push_str(&format!(" oci-listen={} ({how})", joined(&bound.oci_all)));
    }
    format!("{line} commit={COMMIT}\n")
}

/// Runs the node until SIGTERM or SIGINT.
///
/// # Errors
/// As [`run_cold`], and signal handlers that cannot be installed.
pub(in crate::server) async fn serve_cold_until_signalled(
    settings: ColdSettings,
) -> Result<(), Error> {
    let stop = signals()?;
    let announce = |bound: Addresses| {
        crate::server::build_info::stderr_log::answer(&announce_line(&bound));
    };
    run_cold(&settings, stop, announce).await?;
    Ok(())
}

#[cfg(test)]
#[path = "cold_cli_tests.rs"]
mod tests;
