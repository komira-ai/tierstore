//! The start of a build node: a [`LocalCache`] in front of one upstream, served by the S3 front
//! as a read cache, or as a cache that writes through when the file says `write_through`.
//!
//! The settings come from a file of the build-node shape (see
//! [`config_cache`](super::config_cache)). Before the address is bound the start asks the file
//! system under the cache the questions the layout stands on (see [`check_local`]; a file system
//! that answers one wrongly is exit status 3). The upstream is not tested or probed: an upstream
//! that cannot be reached, or a key that may not write, shows as the ordinary error of the first
//! request that meets it. A `budget` above the size of the file system less the `free_floor` is
//! refused as flags that cannot go together (exit status 2); a file system whose size cannot be
//! read, or that reports 0 bytes, is not judged: the start says so on stderr and goes on.
//!
//! With `write_through` the node writes to the upstream; without it the upstream is read with a
//! read-only key. What tierstore requires of the upstream is in the operations guide, and is not
//! verified.
//!
//! With an `[oci]` table the registry front runs beside the S3 one, over the same cache: a blob
//! or a manifest it reads is a hit on the disk or a fill from the upstream, as an S3 GET is. Its
//! flags and files are checked as a storage node's are (see `oci_setup_of`), before the cache is
//! opened, and it drains with the S3 front.
//!
//! The cache directory has a lock (`<path>/.lock`, see [`local_cache`](super::local_cache)). The
//! process that holds it is read-write; one that starts while another holds it is read-only: it
//! does not make the probe of the file system (the probe writes to the directory), `/readyz`
//! says `ready read-only`, and a task tries the lock every second, so that it becomes
//! read-write when the holder has exited. A start that is alone holds the lock before it binds
//! the address, so it is read-write from its first answer.
//!
//! With `server.handoff_socket` the start gets its listening sockets as a storage node's does
//! (see [`bind_fronts`]): after the cache is open it takes the S3 sockets, the loopback ones
//! included, and the OCI ones from the instance that answers on the control socket, or binds its
//! own where none answers, and then answers there itself. The instance it took them from drains
//! and exits 0; the new one is read-only until then, by the lock above. A refusal, other addresses
//! than the settings name, or no end of the hand-off within `server.handoff_wait` fails the start
//! (exit status 1) with nothing bound.
//!
//! With `local_cache.fill_socket` (by default `fill.sock` in the state directory) the cache also
//! answers the fill socket (see [`fill_socket`](super::fill_socket)): bound while the process holds the
//! lock of the cache directory, through the drain of the S3 front, and closed when the drain ends.
//!
//! On a Mac, SIGUSR2 hands the process over to the next image of its own program instead of a stop
//! (see [`next_image`]): the fronts drain as on SIGTERM with their listening sockets kept open, the
//! NFS front of an `fs` interface ends its connections and holds its state with the mount left as it
//! is, the cache's writes end, and the process executes its program path with the offer of the
//! sockets (the NFS one and the control socket of the file system among them) and the cache lock on
//! its standard input (`--handed 0`), which the new image reads whole before it serves anything and
//! takes as it would from a running instance.
//!
//! On Linux, with an `[[interface]]` of type `fs` the start serves the mount of the virtual build
//! file system too (see [`Service`]): after the cache and the tree store are open it takes the
//! mount from a running instance, or serves the descriptor the service manager kept, or mounts
//! afresh, and only then binds the trees front and takes or binds the S3 and OCI sockets; a start
//! that fails after that leaves the mount as a stop does. Every build node's start then tells the
//! service manager `READY=1` where one listens. The mount is served while the fronts drain and is
//! left once they have; a mount that cannot be served on stops the fronts as a signal does, and the
//! start ends in failure.
//!
//! On macOS, with an `[[interface]]` of type `fs` the start serves the NFS front of the file system
//! too (see [`NfsService`]): after the cache and the tree store are open it serves the front (on the
//! sockets it was handed, or on its own) and mounts the export where the `mount` listing does not
//! show it mounted, and only then binds the trees front and takes or binds the S3 and OCI sockets; a
//! start that fails after that leaves the mount where it is. A stop drains the fronts, serving the
//! mount, and then unmounts while the NFS front still answers; an NFS server that stops by itself
//! stops the fronts as a signal does, and the start ends in failure.
//!
//! On SIGTERM or SIGINT the process drains as a storage node does; the eviction pass, the lock
//! task, the scrub and the fill socket are the only things running beside the server, and they stop
//! with it. Nothing is owed to the upstream: no journal, no lease, no copy in flight.
//!
//! A storage node with a cache in front of its own proxy (see
//! [`combined_cli`](super::combined_cli)) uses the same cache directory start (`open`), the same
//! sockets (`bind_cache_s3`, or the hand-off) and the same loops; its upstream is the proxy.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

use crate::{Backend, Clock, Proxy, SystemClock};

use super::build_info::{COMMIT, VERSION};
use super::cli::{Addresses, Error, OciSetup, S3Flags, oci_setup_of, open_tier, probe_run_id};
#[cfg(target_os = "linux")]
use super::cli_serve::ask_sockets;
use super::cli_serve::{Bound, Got, bind_fronts, bind_s3, cut_note, sockets_of, stopped_by};
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
use super::config::fs::not_served;
use super::config_cache::CacheSettings;
use super::drain::late_of;
use super::fill_socket;
use super::front::{InvalidFlags, server_config};
use super::handoff::Inherited;
#[cfg(target_os = "linux")]
use super::handoff::Offered;
use super::handoff::{Handed, Handing, Handoff, Image, Own, Want, read_handed};
use super::listen::{LoopbackBind, bind_set, bind_with, joined};
use super::local_cache::trees::{TreeStore, TreesConfig};
use super::local_cache::{CacheConfig, Disk, Every, LocalCache, Mode, SystemDisk, check_local};
use super::oci::OciListener;
use super::provider::{self, Provider, ProviderFlags};
use super::put_ingest::PutIngest;
use super::stop_watcher::{
    oci_cut_note, serve_fronts, serve_oci, serve_trees, stop_watcher, trees_cut_note,
};
#[cfg(any(target_os = "macos", test))]
use super::treefs::{NfsServed, NfsService};
#[cfg(target_os = "linux")]
use super::treefs::{Served, Service};
use super::trees::{TreesSetup, trees_setup_of};
use super::{Drained, Server, ServerConfig};
use next_image::{NextImage, stop_or_next, take_write_role};

/// What the start of a node gets from the system it runs on: on Linux, the service manager and
/// the FUSE mount of the `fs` interface of a build node's file (see [`Service`]); on macOS, the
/// NFS front of that interface (see [`NfsService`]); none for a storage node, which has neither.
#[derive(Default)]
pub(super) struct OnSystem {
    /// The service manager and the mount of the `fs` interface.
    #[cfg(target_os = "linux")]
    pub(super) service: Option<Service>,
    /// The NFS front of the `fs` interface, if the file has one.
    #[cfg(any(target_os = "macos", test))]
    pub(super) nfs: Option<NfsService>,
}

impl OnSystem {
    /// What the start of a build node with `settings` gets from this process's system. On a system
    /// that is neither Linux nor macOS an `fs` interface is not served, and the start says so on
    /// stderr.
    fn of_process(settings: &CacheSettings) -> Self {
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        if let Some(line) = not_served(settings.fs.as_ref()) {
            tracing::warn!("{line}");
        }
        Self {
            #[cfg(target_os = "linux")]
            service: Some(Service::of_process(settings.fs.clone())),
            #[cfg(target_os = "macos")]
            nfs: settings.fs.clone().map(NfsService::of_process),
            #[cfg(all(test, not(target_os = "macos")))]
            nfs: None,
        }
    }
}

/// The times of the loops that run beside the server of a cache, from its settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Loops {
    /// How often the eviction pass runs (`local_cache.pass_interval`).
    pub(super) pass: Duration,
    /// How often a cache that does not hold the lock of its directory tries it again, and one that
    /// holds it checks that the lock file is still the file at the path
    /// (`local_cache.lock_interval`).
    pub(super) lock: Duration,
    /// The time between two ticks of the scrub (`local_cache.scrub_tick`).
    pub(super) tick: Duration,
    /// The bytes the scrub reads at each tick, or `None` for no scrub.
    pub(super) per_tick: Option<u64>,
    /// The fewest ticks between the starts of two rounds of the scrub: `scrub_min_round` rounded up
    /// to whole ticks, so a round never starts sooner than configured.
    pub(super) min_round: u64,
}

impl Loops {
    /// The loops of a cache with `config`, whose scrub reads `rate` bytes a second (none for no
    /// scrub) and starts a round at most every `min_round`.
    pub(super) fn of(config: &CacheConfig, rate: Option<u64>, min_round: Duration) -> Self {
        let tick = config.scrub_tick;
        let millis = tick.as_millis().max(1);
        let per_tick = |rate: u64| {
            // Rounded up, and at least a byte, so that a small rate on a short tick still moves.
            u64::try_from(
                u128::from(rate)
                    .saturating_mul(millis)
                    .div_ceil(1000)
                    .max(1),
            )
            .unwrap_or(u64::MAX)
        };
        Self {
            pass: config.pass_interval,
            lock: config.lock_interval,
            tick,
            per_tick: rate.map(per_tick),
            min_round: u64::try_from(min_round.as_millis().div_ceil(millis)).unwrap_or(u64::MAX),
        }
    }
}

/// Binds the S3 front of a node with a local cache: every configured address, then the loopback sockets that
/// [`bind_set`] adds because a local cache is defined.
///
/// # Errors
/// A configured address that cannot be bound (named), or a loopback socket that cannot ([`LoopbackBind`]).
pub(super) fn bind_cache_s3(
    configured: &[std::net::SocketAddr],
    backlog: i32,
) -> Result<Vec<tokio::net::TcpListener>, Error> {
    let mut listeners = bind_s3(configured, backlog)?;
    for addr in &bind_set(configured, true)[configured.len()..] {
        let socket = bind_with(*addr, backlog).map_err(|why| LoopbackBind { addr: *addr, why })?;
        listeners.push(socket);
    }
    Ok(listeners)
}

/// The size of the file system under `path` in bytes, or why it is not known: the system did not
/// answer, or it answered 0 (a file system that does not report its size, which a budget cannot
/// be compared with).
pub(super) fn size_of_file_system(disk: &dyn Disk, path: &Path) -> Result<u64, String> {
    match disk.space(path) {
        Ok(space) if space.total > 0 => Ok(space.total),
        Ok(_) => Err("the file system reports a size of 0 bytes".to_owned()),
        Err(why) => Err(format!(
            "the size of the file system could not be read: {why}"
        )),
    }
}

/// The line the start prints when the size of the file system is unknown and the budget is not
/// checked against it.
pub(super) fn unjudged_warning(why: &str, path: &Path) -> String {
    format!(
        "tierstore: warning: {why} (at {}); local_cache.budget is not checked against it",
        path.display()
    )
}

/// Opens the cache in `config`, checks its file system and the upstream, and says on stderr what
/// the start found.
pub(super) async fn open(
    config: &CacheConfig,
    upstream: &Arc<dyn Backend>,
    disk: &Arc<dyn Disk>,
    run: &str,
) -> Result<LocalCache, Error> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let (upstream, disk) = (Arc::clone(upstream), Arc::clone(disk));
    let opened = LocalCache::open_locked(config.clone(), upstream, Arc::clone(&disk), clock);
    let cache = opened.map_err(|why| {
        format!(
            "opening the local cache at {}: {why}",
            config.path.display()
        )
    })?;
    // The probe writes under `.stage/` and `obj/`: only the process that holds the lock may. A
    // read-only one is on the directory, and the file system, that the holder's check passed.
    match cache.mode() {
        Mode::ReadWrite => check_local(&*disk, cache.path(), run)?,
        Mode::ReadOnly => tracing::warn!(
            "tierstore: local cache at {}: another process holds the lock; read-only, and the \
             file system check is left to it",
            config.path.display()
        ),
    }
    // A file system whose size cannot be read is not judged: the start says so and goes on.
    match size_of_file_system(&*disk, cache.path()) {
        Ok(total) if config.budget > config.room(total) => {
            let text = format!(
                "local_cache.budget: {} bytes is more than the {} bytes of the file system at {} \
                 less its free_floor of {} percent",
                config.budget,
                total,
                config.path.display(),
                config.free_floor
            );
            return Err(Box::new(InvalidFlags(text)));
        }
        Ok(_) => {}
        Err(why) => tracing::warn!("{}", unjudged_warning(&why, cache.path())),
    }
    let (bytes, objects) = cache.held();
    tracing::info!(
        "tierstore: local cache at {}: {objects} objects, {bytes} bytes, {} files removed at \
         start, {}",
        config.path.display(),
        cache.counters().discarded_at_start(),
        cache.mode().as_str()
    );
    Ok(cache)
}

/// Serves `cache` until `stop` resolves, and then drains: a start with no hand-over to a next
/// image (the tests').
///
/// # Errors
/// A file system or an upstream that fails the check of the start, or an address that cannot be
/// bound.
#[cfg(test)]
pub(super) async fn serve_cache(
    settings: &CacheSettings,
    upstream: Arc<dyn Backend>,
    disk: Arc<dyn Disk>,
    stop: impl Future<Output = ()> + Send + 'static,
    ready: impl FnOnce(Addresses),
) -> Result<Drained, Error> {
    let system = OnSystem::of_process(settings);
    serve_cache_on(
        settings,
        (upstream, disk),
        system,
        stop,
        ready,
        Imaging::default(),
    )
    .await
}

/// What a start of a build node has of the hand-over to the next image of its program
/// ([`next_image`]): where a stop that hands over leaves what it hands over, and the offer this
/// start was handed, if the image before it executed it.
#[derive(Default)]
pub(super) struct Imaging<'a> {
    /// Told by SIGUSR2 that the stop is a hand-over; the drain then leaves the sockets and the
    /// cache lock to it.
    pub(super) next: Option<&'a NextImage>,
    /// The offer this start was handed (`--handed 0`): its standard input, as it found it.
    pub(super) handed: Option<std::os::fd::OwnedFd>,
}

/// [`serve_cache`] with what `system` gives, and the hand-over to the next image of `imaging`.
///
/// # Errors
/// As [`serve_cache`], and a handed offer this start refuses (its addresses are not the file's).
pub(super) async fn serve_cache_on(
    settings: &CacheSettings,
    (upstream, disk): (Arc<dyn Backend>, Arc<dyn Disk>),
    system: OnSystem,
    stop: impl Future<Output = ()> + Send + 'static,
    ready: impl FnOnce(Addresses),
    imaging: Imaging<'_>,
) -> Result<Drained, Error> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let mut config = server_config(&settings.front, Arc::clone(&clock), true)?;
    // The front's flags and files are checked before the cache is opened, as the storage node's.
    let oci = oci_setup_of(&settings.oci, &config, true)?;
    let trees_front = trees_setup_of(
        &settings.trees_front,
        &config,
        settings.trees.max_request_bytes,
        true,
    )?;
    config.oci_metrics = oci.as_ref().map(|setup| Arc::clone(&setup.metrics));
    let run = probe_run_id(&settings.front.proxy_id, clock.now(), std::process::id());
    let cache = open(&settings.cache, &upstream, &disk, &run).await?;
    if settings.cache.write_through {
        config.cache_writes = true;
    }
    let fronts = Fronts {
        listen: &settings.front.listen,
        lb_wait: settings.front.drain_lb_wait,
        scrub_rate: settings.scrub_rate,
        scrub_min_round: settings.scrub_min_round,
        beside_halt: Arc::new(Notify::new()),
        beating: None,
        handoff: settings.handoff.as_ref(),
        fill_socket: settings.fill_socket.as_deref(),
        trees: Some(settings.trees.clone()),
        trees_front,
        provider: settings.provider.as_ref(),
        put_ingest: settings.put_ingest.as_deref(),
        system,
        imaging,
    };
    // On the heap: the serving of a node is a large future, and a caller's stack is not.
    Box::pin(serve_front(
        cache,
        config,
        oci,
        fronts,
        stop,
        ready,
        std::future::ready(()),
    ))
    .await
}

/// What [`serve_front`] needs besides the cache and the front's own settings.
pub(super) struct Fronts<'a> {
    /// The addresses of the S3 front.
    pub(super) listen: &'a [std::net::SocketAddr],
    /// How long the drain waits for the load balancer to notice.
    pub(super) lb_wait: Duration,
    /// The bytes the scrub reads a second, or `None` for no scrub.
    pub(super) scrub_rate: Option<u64>,
    /// The shortest time one round of the scrub takes.
    pub(super) scrub_min_round: Duration,
    /// Notified when the fronts have drained, for the loop the caller runs `beside` the cache.
    pub(super) beside_halt: Arc<Notify>,
    /// The proxy of a storage node, whose heartbeat is stopped when both fronts are done, so that
    /// it beats until the in-flight requests of the S3 front and of the OCI front are finished or
    /// cut.
    pub(super) beating: Option<Arc<Proxy>>,
    /// The hand-off of the listening sockets (`server.handoff_socket`), if the settings ask for
    /// one.
    pub(super) handoff: Option<&'a Handoff>,
    /// The fill socket (`local_cache.fill_socket`), bound while the cache holds the lock of its
    /// directory (see [`fill_socket`](super::fill_socket)); none for a node without one.
    pub(super) fill_socket: Option<&'a std::path::Path>,
    /// The node-local trees (`[trees]`), kept beside the cache; none for a node whose file has no
    /// such table (a storage node with a cache).
    pub(super) trees: Option<TreesConfig>,
    /// The trees front, if the settings have a `trees` interface.
    pub(super) trees_front: Option<TreesSetup>,
    /// The mount provider, if the settings have a `mount-provider` interface: served over the
    /// node's trees, while the cache holds the lock of its directory (see
    /// [`provider`](super::provider)).
    pub(super) provider: Option<&'a ProviderFlags>,
    /// The root of the put ingest (`local_cache.put_ingest`), ingested while the cache holds the
    /// lock of its directory (see [`put_ingest`](super::put_ingest)); none for a node without one.
    pub(super) put_ingest: Option<&'a std::path::Path>,
    /// What the node gets from its system: the service manager, and the mount of the `fs`
    /// interface, which is got before the sockets of the fronts and left after they drain.
    pub(super) system: OnSystem,
    /// The hand-over to the next image of the program, of a build node ([`next_image`]).
    pub(super) imaging: Imaging<'a>,
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
/// What the mount's start said ([`StartError`]), or the hand-off of the sockets
/// ([`HandingError`](super::cli::HandingError), boxed).
#[cfg(target_os = "linux")]
async fn take_mount_and_sockets(
    service: Service,
    stores: (LocalCache, Option<TreeStore>),
    asked: Option<Offered>,
) -> Result<(Served, Option<Inherited>), Error> {
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
            Ok(inherited) => Some(inherited),
            Err(why) => {
                taking.abandon().await;
                return Err(super::cli::HandingError::from(why).into_boxed());
            }
        },
        None => None,
    };
    Ok((taking.serve().await?, sockets))
}

/// Reads the offer the image before this one handed over on `fd` (`handoff/exec.rs`) and takes it,
/// checked as a taker checks an offer: its S3 and OCI sockets must be on `s3` and `oci`, its NFS
/// socket on the address of `nfs` and its control socket at the path of `nfs` (an `fs` interface
/// on macOS; none without one, when an offer of either is refused), and its lock the file at
/// `lock`.
///
/// # Errors
/// A handed offer this start refuses ([`HandingError`](super::cli::HandingError), boxed).
async fn read_offer(
    fd: std::os::fd::OwnedFd,
    (s3, oci): (&[std::net::SocketAddr], &[std::net::SocketAddr]),
    nfs: Option<(std::net::SocketAddr, &Path)>,
    lock: &Path,
) -> Result<Handed, Error> {
    let nfs_listen: Vec<std::net::SocketAddr> = nfs.iter().map(|(listen, _)| *listen).collect();
    let want = Want {
        s3,
        oci,
        nfs: &nfs_listen,
    };
    let own = Own {
        control: nfs.map(|(_, control)| control),
        lock: Some(lock),
    };
    read_handed(fd, &want, &own)
        .await
        .map_err(|why| super::cli::HandingError::from(why).into_boxed())
}

/// Serves the S3 front (and the OCI front, if there is a setup) over `cache`, with the loops of
/// the cache and `beside`, until `stop` resolves, and then drains. It is the one serving of a
/// cache, whatever its upstream is: a store of a build node or the proxy of a storage node.
/// `beside` is a loop of the caller's that runs until `fronts.beside_halt` is notified.
///
/// With `fronts.handoff` the sockets are got as a storage node's are (see
/// [`bind_fronts`]): taken from the instance that answers on the control socket, or bound where
/// none answers, and then offered there; when a successor takes them it stops as if `stop` had
/// resolved, and drains. The cache directory lock keeps the successor read-only until this
/// process has exited, but for a successor that asks for the `ready` step (`take 1`, `ready`):
/// it is offered the lock too, waited for until `server.handoff_wait` after `done`, and this
/// cache starts no write from its `ready` on and says `written` once its writes in flight have
/// ended (`handoff/written.rs`). A request for a mount is refused: this instance holds none.
///
/// With `fronts.imaging.handed` the sockets and the cache lock are those the image this process
/// executed from handed over: the cache takes the write role with the lock ([`take_write_role`])
/// beside the serving. With `fronts.imaging.next`, a stop it was told is a hand-over leaves a copy
/// of each listening socket and the cache lock to it, once the fronts have drained and the cache's
/// writes have ended ([`NextImage::hand`]).
///
/// # Errors
/// An address that cannot be bound, or a hand-off that is refused, mismatched or not ended within
/// `server.handoff_wait` (a [`HandingError`](super::cli::HandingError), boxed).
pub(super) async fn serve_front(
    cache: LocalCache,
    mut config: ServerConfig,
    oci: Option<OciSetup>,
    fronts: Fronts<'_>,
    stop: impl Future<Output = ()> + Send + 'static,
    ready: impl FnOnce(Addresses),
    beside: impl Future<Output = ()>,
) -> Result<Drained, Error> {
    config.cache = Some(cache.clone());
    let trees = fronts
        .trees
        .map(|trees| TreeStore::open(cache.clone(), trees))
        .transpose()
        .map_err(|why| {
            format!(
                "opening the trees at {}: {why}",
                cache.config().path.display()
            )
        })?;
    config.trees.clone_from(&trees);
    let OnSystem {
        #[cfg(target_os = "linux")]
        service,
        #[cfg(any(target_os = "macos", test))]
        nfs,
    } = fronts.system;
    // The offer the image before this one handed over is read whole, and checked against this
    // start's settings, before anything is served: the NFS front's part of it (`nfs` and
    // `control`) is the start of the `fs` interface's, and the rest the fronts'.
    let Imaging { next, handed } = fronts.imaging;
    let lock_path = cache.lock_path();
    #[cfg(any(target_os = "macos", test))]
    let nfs_own = nfs
        .as_ref()
        .map(|service| (service.listen(), service.control_socket()));
    #[cfg(not(any(target_os = "macos", test)))]
    let nfs_own = None;
    let handed = match handed {
        Some(fd) => {
            let s3 = bind_set(fronts.listen, true);
            let oci_listen = oci
                .as_ref()
                .map_or_else(Vec::new, |setup| setup.listen.clone());
            Some(read_offer(fd, (&s3, &oci_listen), nfs_own, &lock_path).await?)
        }
        None => None,
    };
    let (fs_handed, handed) = match handed.map(Handed::split_fs) {
        Some((fs, rest)) => (Some(fs), Some(rest)),
        None => (None, None),
    };
    // On macOS the NFS front of the `fs` interface serves, and its export is mounted where it is
    // not mounted already, before any socket of a front is bound, as the mount is got first on
    // Linux.
    #[cfg(any(target_os = "macos", test))]
    let nfs_served: Option<NfsServed> = match nfs {
        Some(service) => Some(
            service
                .start((cache.clone(), trees.clone()), fs_handed)
                .await?,
        ),
        None => None,
    };
    #[cfg(not(any(target_os = "macos", test)))]
    drop(fs_handed);
    // The mount is got, and the instance it was taken from told that it is served, before any
    // socket of a front is bound: a start that cannot get it leaves the running instances serving
    // everything. Where the file has an `fs` interface and a hand-off, the running instance's
    // offer of its sockets is asked for and checked first, and accepted only after the mount's
    // ([`take_mount_and_sockets`]).
    #[cfg(not(target_os = "linux"))]
    let sockets: Option<Inherited> = None;
    #[cfg(target_os = "linux")]
    let (served, sockets) = match service {
        Some(service) => {
            let switch = fronts
                .handoff
                .filter(|_| service.has_mount() && handed.is_none());
            let asked = match switch {
                Some(handoff) => {
                    let oci_listen = oci
                        .as_ref()
                        .map_or_else(Vec::new, |setup| setup.listen.clone());
                    ask_sockets(handoff, &bind_set(fronts.listen, true), &oci_listen)
                        .await
                        .map_err(super::cli::HandingError::into_boxed)?
                }
                None => None,
            };
            let stores = (cache.clone(), trees.clone());
            let (served, sockets) =
                Box::pin(take_mount_and_sockets(service, stores, asked)).await?;
            (Some(served), sockets)
        }
        None => (None, None),
    };
    let listen = config.listen;
    // The registry front reads what the S3 front reads: through the cache.
    let backend: Arc<dyn Backend> = Arc::new(cache.clone());
    let server = Server::new(Arc::clone(&backend), config);
    // The trees sockets are bound beside a running instance's (`SO_REUSEPORT`, they are not handed
    // over), and before the S3 and OCI sockets are taken from it: a trees address that cannot be
    // bound fails the start while the running instance still serves.
    let got = handed.or(sockets).map(|taken| Got::Taken(Box::new(taken)));
    let fronts_bound = async {
        let trees_front = match fronts.trees_front {
            Some(setup) => Some(setup.bind().await?),
            None => None,
        };
        let trees_all = match &trees_front {
            Some(front) => front.local_addrs()?,
            None => Vec::new(),
        };
        let bound = bind_fronts(
            fronts.handoff,
            got,
            &bind_set(fronts.listen, true),
            || bind_cache_s3(fronts.listen, listen.backlog),
            oci,
            backend,
        )
        .await
        .map_err(super::cli::HandingError::into_boxed)?;
        Ok::<_, Error>((trees_front, trees_all, bound))
    };
    let (
        trees_front,
        trees_all,
        Bound {
            s3: listeners,
            oci,
            addresses,
            offer,
            lock,
        },
    ) = match fronts_bound.await {
        Ok(bound) => bound,
        Err(why) => {
            // The mount was served: it is left, as at a stop, for the next start.
            #[cfg(target_os = "linux")]
            if let Some(served) = served {
                served.abandon().await;
            }
            #[cfg(any(target_os = "macos", test))]
            if let Some(served) = nfs_served {
                served.abandon();
            }
            return Err(why);
        }
    };
    if let Some(received) = lock {
        drop(tokio::spawn(take_write_role(cache.clone(), received)));
    }
    // The copies the drain does not close: the next image gets them.
    let kept = next
        .map(|_| sockets_of(&listeners, oci.as_ref()))
        .transpose()?;
    let offer = offer.zip(fronts.handoff).map(|(offer, handoff)| {
        offer
            .with_accept_pause(listen.accept_pause)
            .with_ready_wait(handoff.wait)
            .with_lock(cache.clone())
    });
    // A mount that cannot be served on stops the fronts as a signal does.
    let failed = Arc::new(Notify::new());
    let failing = Arc::clone(&failed);
    let stop = async move {
        tokio::select! {
            () = stop => {}
            () = failing.notified() => {}
        }
    };
    let stop = stopped_by(stop, offer);
    let mut addresses = addresses;
    addresses.trees_all = trees_all;
    ready(addresses);
    #[cfg(target_os = "linux")]
    if let Some(served) = &served {
        served.ready();
    }
    let oci_drain = oci.as_ref().map(OciListener::drain);
    let (late_s3, late_oci) = (Arc::clone(&server), oci_drain.clone());
    let (watcher, oci_stopped) = stop_watcher(stop, Arc::clone(&server), oci_drain, fronts.lb_wait);
    let trees_stopped = oci_stopped.clone();
    // One Notify for each loop: `notify_one` wakes one waiter, and keeps the wake for a loop that
    // is not waiting just now.
    let halts: [Arc<Notify>; 10] = [
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        Arc::clone(&fronts.beside_halt),
    ];
    let [
        halt,
        lock_halt,
        scrub_halt,
        fill_halt,
        trees_halt,
        provider_halt,
        watch_halt,
        ingest_halt,
        fs_halt,
        _,
    ] = halts.clone();
    let beating = fronts.beating.clone();
    let serving = async move {
        let after = async {
            if let Some(proxy) = &beating {
                proxy.stop_heartbeat().await;
            }
        };
        let ended = serve_fronts(
            server.serve_all(listeners),
            async {
                tokio::join!(
                    serve_oci(oci, oci_stopped),
                    serve_trees(trees_front, trees_stopped)
                )
            },
            after,
        )
        .await;
        tracing::debug!("tierstore: fronts drained");
        for halting in &halts {
            halting.notify_one();
        }
        ended
    };
    let loops = Loops::of(cache.config(), fronts.scrub_rate, fronts.scrub_min_round);
    let maintaining = async {
        cache.maintain(loops.pass, halt).await;
        tracing::debug!("tierstore: maintain ended");
    };
    let holding = async {
        cache.hold(loops.lock, lock_halt).await;
        tracing::debug!("tierstore: hold ended");
    };
    let scrubbing = async {
        let every = Every::new(loops.tick);
        cache
            .scrub(loops.per_tick, loops.min_round, every, scrub_halt)
            .await;
        tracing::debug!("tierstore: scrub ended");
    };
    let filling = async {
        if let Some(path) = fronts.fill_socket {
            fill_socket::serve((cache.clone(), trees.clone()), path, fill_halt).await;
            tracing::debug!("tierstore: fill socket ended");
        }
    };
    let watching = async {
        cache.watch_fills(watch_halt).await;
        tracing::debug!("tierstore: fill watchdog ended");
    };
    let sweeping = async {
        if let Some(trees) = &trees {
            trees.maintain(loops.pass, trees_halt).await;
            tracing::debug!("tierstore: trees ended");
        }
    };
    // A file with a mount-provider interface is a build node's, which has a tree store.
    let provided = fronts.provider.zip(trees.clone());
    let providing = async {
        if let Some((flags, store)) = provided {
            let provider = Provider::new(store, cache.clone(), flags);
            provider::serve(provider, &cache, &flags.socket, provider_halt).await;
            tracing::debug!("tierstore: mount provider ended");
        }
    };
    let ingesting = async {
        if let Some(root) = fronts.put_ingest {
            let ingest = PutIngest::new(cache.clone(), root.to_owned());
            ingest.serve(ingest_halt).await;
            tracing::debug!("tierstore: put ingest ended");
        }
    };
    // The mount is served while the fronts drain, and left once they have (`fs_halt`); on macOS a
    // stop that hands over to the next image hands the NFS front over instead.
    let mounting = async {
        #[cfg(target_os = "linux")]
        if let Some(served) = served {
            let ended = served.serve(fs_halt, failed).await;
            tracing::debug!("tierstore: mount ended");
            return ended.map(|()| None).map_err(Error::from);
        }
        #[cfg(any(target_os = "macos", test))]
        if let Some(served) = nfs_served {
            let handing = || next.is_some_and(NextImage::asked);
            let ended = served.serve(fs_halt, failed, handing).await;
            tracing::debug!("tierstore: mount ended");
            return ended.map_err(Error::from);
        }
        let _unmounted = (fs_halt, failed);
        Ok::<Option<Handing>, Error>(None)
    };
    let ((drained, (oci_cut, trees_cut)), (), (), (), (), (), (), (), (), (), mounted) = tokio::join!(
        serving,
        beside,
        maintaining,
        holding,
        scrubbing,
        filling,
        sweeping,
        providing,
        watching,
        ingesting,
        mounting
    );
    watcher.abort();
    let (mounted, fs_handing) = match mounted {
        Ok(handing) => (Ok(()), handing),
        Err(why) => (Err(why), None),
    };
    if let (Some(next), Some(kept)) = (next, kept)
        && next.asked()
    {
        next.hand(kept, fs_handing, &cache).await?;
    }
    super::build_info::stderr_log::notes(&cut_note(drained));
    super::build_info::stderr_log::notes(&oci_cut_note(oci_cut));
    super::build_info::stderr_log::notes(&trees_cut_note(trees_cut));
    super::build_info::stderr_log::notes(&late_of(&late_s3, late_oci.as_ref()));
    mounted?;
    Ok(drained)
}

/// Opens the upstream the settings name and serves the cache in front of it until `stop`
/// resolves: a start with no hand-over to a next image (the tests').
///
/// # Errors
/// As [`serve_cache`], and an upstream the settings do not make.
#[cfg(test)]
pub(super) async fn run_cache(
    settings: &CacheSettings,
    stop: impl Future<Output = ()> + Send + 'static,
    ready: impl FnOnce(Addresses),
) -> Result<Drained, Error> {
    run_cache_imaging(settings, stop, ready, Imaging::default()).await
}

/// [`run_cache`], with the hand-over to the next image of `imaging`.
///
/// # Errors
/// As [`serve_cache_on`].
async fn run_cache_imaging(
    settings: &CacheSettings,
    stop: impl Future<Output = ()> + Send + 'static,
    ready: impl FnOnce(Addresses),
    imaging: Imaging<'_>,
) -> Result<Drained, Error> {
    if settings.s3_request_timeout.is_zero() {
        return Err("--s3-request-timeout must be longer than zero".into());
    }
    let tier = &settings.upstream;
    let s3 = S3Flags {
        endpoint: tier.endpoint.as_deref(),
        more_endpoints: &tier.more_endpoints,
        region: tier.region.as_deref(),
        credentials: tier.credentials_file.as_deref(),
        conditional_delete: tier.conditional_delete,
    };
    let upstream = open_tier(
        "--cold",
        &tier.spec,
        &s3,
        settings.s3_request_timeout,
        &settings.s3_client,
    )?;
    let disk: Arc<dyn Disk> = Arc::new(SystemDisk);
    let system = OnSystem::of_process(settings);
    serve_cache_on(settings, (upstream, disk), system, stop, ready, imaging).await
}

/// Runs the cache until SIGTERM or SIGINT, or, on a Mac, SIGUSR2: then it gives back the next image
/// of the program, its offer written, for the caller to execute once the runtime is gone. `handed`
/// is the offer this start was handed (`--handed 0`).
pub(super) async fn serve_cache_until_signalled(
    settings: CacheSettings,
    handed: Option<std::os::fd::OwnedFd>,
) -> Result<Option<Image>, Error> {
    let next = Arc::new(NextImage::default());
    let stop = stop_or_next(Arc::clone(&next))?;
    let mode = if settings.cache.write_through {
        "write-through-cache"
    } else {
        "read-cache"
    };
    let announce = |bound: Addresses| {
        let listen = joined(&bound.s3_all);
        let mut line = format!("tierstore {VERSION} listen={listen} mode={mode}");
        if bound.oci.is_some() {
            let how = bound.oci_mode.unwrap_or_default().as_str();
            line.push_str(&format!(" oci-listen={} ({how})", joined(&bound.oci_all)));
        }
        if !bound.trees_all.is_empty() {
            line.push_str(&format!(" trees-listen={}", joined(&bound.trees_all)));
        }
        super::build_info::stderr_log::answer(&format!("{line} commit={COMMIT}\n"));
    };
    let imaging = Imaging {
        // Elsewhere no copy of the sockets is kept: a drain closes them, as it did.
        next: next_image::HANDS_TO_NEXT_IMAGE.then_some(&*next),
        handed,
    };
    run_cache_imaging(&settings, stop, announce, imaging).await?;
    let program = std::env::current_exe;
    Ok(next.image(program, std::env::args_os()).await?)
}

#[path = "next_image.rs"]
pub(super) mod next_image;
