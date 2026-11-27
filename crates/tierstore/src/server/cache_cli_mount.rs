//! The start of a build node whose file has an `fs` interface, on Linux: the mount and the S3 and
//! OCI sockets of a running instance, got before any socket of a front is bound
//! (`docs/design/one-service.md` sections 2.3, 3.1 and 4.2), and the mount offered in its turn.
//!
//! The running instance on `server.handoff_socket` is asked `take 1`, `ready`, `mount`, with the
//! cache lock. An instance of the one process that holds the mount offers it there with its
//! sockets and its lock, and the start takes all of it in that one conversation
//! ([`take_mount_and_sockets`]): the device is put in the store before `ack`, the runtime state is
//! read again after `done`, `ready` is said once the mount is made ready to serve, the device is
//! read after the `bye`, and the cache takes the write role at that instance's `written`. Nothing
//! is asked on `take-fs 1`.
//!
//! An instance that holds no mount refuses the line and is asked for its sockets and lock alone:
//! the S3 process of the two-process layout, or a process of this layout whose mount a `take-fs 1`
//! took. Then the mount comes from the instance that answers `take-fs 1` in the runtime directory,
//! in the order of the switch (section 4.2, [`switch`]): both offers are checked before either is
//! accepted, and the mount's is accepted first. Where nothing answers on `server.handoff_socket`,
//! the mount is taken on `take-fs 1`, adopted or made, and the sockets are bound.

use std::net::SocketAddr;
use std::path::Path;

use crate::server::cli::{Error, HandingError};
use crate::server::cli_serve::{Got, ask_sockets};
use crate::server::handoff::{Handoff, Offer, Offered};
use crate::server::local_cache::LocalCache;
use crate::server::local_cache::trees::TreeStore;
use crate::server::treefs::{Mounted, Served, Service};

/// The addresses of a start's own S3 and OCI fronts, and its cache's lock file: what it asks a
/// running instance for.
pub(super) struct Own<'a> {
    pub(super) s3: &'a [SocketAddr],
    pub(super) oci: &'a [SocketAddr],
    pub(super) lock: &'a Path,
}

/// The mount of the `fs` interface of `service`, and the sockets of the running instance on
/// `handoff` (none when the file has no `fs` interface, or no hand-off), as the module says: in
/// one conversation from an instance that holds the mount, else in the order of the switch. The
/// sockets come back taken, or taken with `ready` said ([`Got::Said`]), for [`bind_fronts`]
/// (`cli_serve.rs`); none where no instance answers on `handoff`.
///
/// [`bind_fronts`]: crate::server::cli_serve::bind_fronts
///
/// # Errors
/// What the mount's start said ([`StartError`](crate::server::treefs::StartError)), or the hand-off of the
/// sockets ([`HandingError`], boxed). A running instance that refused, did not keep to the
/// protocol or did not answer in time serves on as it was.
pub(super) async fn take_mount_and_sockets(
    service: Service,
    stores: (LocalCache, Option<TreeStore>),
    handoff: Option<&Handoff>,
    own: &Own<'_>,
) -> Result<(Served, Option<Got>), Error> {
    let (handoff, mounted) = match (handoff, service.mounted()) {
        (Some(handoff), Ok(mounted)) => (handoff, mounted),
        // No hand-off: the mount is taken on `take-fs 1`, adopted or made, and the sockets bound.
        (None, Ok(mounted)) => return switch(mounted.service(), stores, None).await,
        // No `fs` interface: no mount, and the sockets are got as any node's.
        (_, Err(service)) => return switch(*service, stores, None).await,
    };
    let want = Some(mounted.want());
    let asked = ask_sockets(handoff, (own.s3, own.oci), own.lock, want)
        .await
        .map_err(HandingError::into_boxed)?;
    match asked {
        Some(offered) if offered.has_mount() => take_handed(mounted, stores, offered).await,
        asked => switch(mounted.service(), stores, asked).await,
    }
}

/// The one hand-over: the offer of an instance that holds the mount, accepted. A take-over of the
/// mount that fails after `done` ends the connection before `ready`, and that instance resumes and
/// serves on; one that says `ready` and gets no `bye` leaves the mount to that instance, which
/// gave up first and serves on.
///
/// # Errors
/// As [`take_mount_and_sockets`].
async fn take_handed(
    mounted: Mounted,
    stores: (LocalCache, Option<TreeStore>),
    offered: Offered,
) -> Result<(Served, Option<Got>), Error> {
    let (sockets, mount, ready) = offered
        .accept_with_mount()
        .await
        .map_err(|why| HandingError::from(why).into_boxed())?;
    let taking = mounted.take_handed(stores, mount, ready).await?;
    let mut served = taking.serve().await?;
    let predecessor = served.predecessor();
    Ok((served, Some(Got::Said(Box::new(sockets), predecessor))))
}

/// The mount of the `fs` interface of `service`, and the sockets `asked` offers, in the order of
/// the switch from two processes to one (`docs/design/one-service.md` section 4.2): both offers
/// are checked before either is accepted, and the mount's is accepted first. `asked` was checked
/// when it was asked for; the mount's offer is checked as it is taken, and a mount that is not
/// taken (refused, mismatched, not answered) refuses `asked` too, so both running instances serve
/// on as they were. The sockets are accepted once the mount's instance has said `done` and before
/// it is told `ready`: sockets that cannot be accepted leave the mount to that instance, which
/// serves it again. Only then is it told `ready`, and the mount read.
///
/// # Errors
/// As [`take_mount_and_sockets`].
async fn switch(
    service: Service,
    stores: (LocalCache, Option<TreeStore>),
    asked: Option<Offered>,
) -> Result<(Served, Option<Got>), Error> {
    let taking = match service.take(stores).await {
        Ok(taking) => taking,
        Err(why) => {
            if let Some(asked) = asked {
                asked.refuse().await;
            }
            return Err(why.into());
        }
    };
    let sockets = match asked {
        Some(asked) => match asked.accept().await {
            Ok(inherited) => Some(Got::Taken(Box::new(inherited))),
            Err(why) => {
                taking.abandon().await;
                return Err(HandingError::from(why).into_boxed());
            }
        },
        None => None,
    };
    Ok((taking.serve().await?, sockets))
}

/// `offer`, offering the mount of `served` too, where it serves one (`Offer::with_mount`): a
/// successor that asks for it in the one hand-over takes it with the sockets and the lock.
pub(super) fn with_mount(offer: Offer, served: Option<&Served>) -> Offer {
    match served.and_then(Served::offered) {
        Some(mount) => offer.with_mount(mount),
        None => offer,
    }
}
