//! The new instance's side: asking a running instance for its listening sockets, and in the one
//! hand-over (`docs/design/one-service.md` section 3.1) for its mount and its cache lock too.

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::os::fd::{AsFd as _, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rustix::io::{FdFlags, fcntl_setfd};
use tokio::net::{TcpListener, UnixStream};

use crate::server::lock::Received;

use super::mount::{self, CONTROL, FUSE, MountWant, TakenMount, check_control, check_listening};
use super::offer::{BYE, LOCK, MOUNT, NO_MOUNT, NOT_UNDERSTOOD, READY, TAKE};
use super::wire::{self, Frame};
use super::written::Predecessor;
use super::{Front, Handoff, HandoffError, Socket, line};

/// The addresses a new instance's settings name, front by front, in the order of its interfaces.
pub(crate) struct Want<'a> {
    pub(crate) s3: &'a [SocketAddr],
    pub(crate) oci: &'a [SocketAddr],
    pub(crate) nfs: &'a [SocketAddr],
}

/// What a start of the one process asks for besides the sockets ([`offered_with`]).
pub(crate) struct Ask<'a> {
    /// The mount, when its file has an `fs` interface.
    pub(crate) mount: Option<MountWant<'a>>,
    /// The path of its cache's lock, when it has a cache: the lock is taken when the running
    /// instance offers the same file.
    pub(crate) lock: Option<&'a Path>,
    /// Whether a lock offered under another path than [`lock`](Self::lock) (the running
    /// instance's file names another cache directory) is closed, and left with the running
    /// instance, rather than refused with `nak`: the two caches are two directories then.
    pub(crate) leaves_another_lock: bool,
}

/// The sockets taken from a running instance, in the order it serves them, with what the taker
/// needs of them made before it answered `ack`: their addresses, and a copy of each for its own
/// offer. Nothing that can fail is left for after the running instance's `done`.
pub(crate) struct Inherited {
    pub(crate) s3: Vec<TcpListener>,
    pub(crate) oci: Vec<TcpListener>,
    pub(crate) nfs: Vec<TcpListener>,
    /// The address of each of [`s3`](Self::s3).
    pub(crate) s3_addrs: Vec<SocketAddr>,
    /// The address of each of [`oci`](Self::oci).
    pub(crate) oci_addrs: Vec<SocketAddr>,
    /// The address of each of [`nfs`](Self::nfs).
    pub(crate) nfs_addrs: Vec<SocketAddr>,
    /// A copy of each socket, in the order the running instance offered them, for the taker's
    /// own control socket.
    pub(crate) copies: Vec<Socket>,
    /// The connection on which this taker says `ready`, when it asked for that step
    /// ([`take_with_ready`]) and the running instance takes part in it. While it is held the
    /// running instance has not handed the sockets over: it ends when `ready` is sent, and goes
    /// on serving when the connection ends without it.
    pub(crate) ready: Option<Ready>,
    /// The mount, when it was asked for ([`offered_with`]) and the running instance held it. The
    /// device is not read before [`Ready::send_keeping`] has returned.
    pub(crate) mount: Option<TakenMount>,
    /// A descriptor of the running instance's cache lock, when it offered it and this taker asked
    /// for it: it is the lock file of [`Ask::lock`]. It becomes the cache's lock on `written`
    /// (`written::follow`); dropped before, it only closes.
    pub(crate) lock: Option<Received>,
}

/// The end of a hand-off that asked for the `ready` step: the connection to the running instance.
pub(crate) struct Ready(UnixStream);

impl Ready {
    /// Tells the running instance that this one serves the sockets: it ends.
    ///
    /// # Errors
    /// The running instance is gone from the connection: it did not wait for `ready` (it went on
    /// serving) and this instance must not serve beside it.
    pub(crate) async fn send(self) -> Result<(), HandoffError> {
        self.send_keeping().await.map(drop)
    }

    /// [`send`](Self::send), keeping the connection: the running instance says `written` on it
    /// once its cache writes have ended (`written::follow`), and tells the end of this process
    /// from the end of the connection, so it is held until this process ends.
    ///
    /// # Errors
    /// As [`send`](Self::send).
    pub(crate) async fn send_keeping(self) -> Result<Predecessor, HandoffError> {
        let io = HandoffError::io("telling the running instance it can end");
        wire::send(&self.0, &[READY.to_owned()], &[])
            .await
            .map_err(HandoffError::io("telling the running instance it can end"))?;
        // The running instance ends only after it has read `ready` and answered `bye`; a
        // connection that closes instead means it gave up first and serves on.
        let answer = wire::recv(&self.0).await.map_err(io)?;
        if answer.lines == [BYE] {
            Ok(Predecessor(self.0))
        } else {
            Err(HandoffError::Protocol(
                "something other than bye after ready",
            ))
        }
    }
}

/// A copy of a listening socket's descriptor, as a listener of `std`.
type Copy = fn(&TcpListener) -> std::io::Result<std::net::TcpListener>;

/// The copy of `listener` the taker offers on: the same socket.
pub(super) fn duplicate(listener: &TcpListener) -> std::io::Result<std::net::TcpListener> {
    Ok(listener.as_fd().try_clone_to_owned()?.into())
}

/// What a request asks for: each asks for what the one before asks, and one more step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Request {
    /// `take 1`: the sockets.
    Plain,
    /// `take 1`, `ready`: the `ready` step, and the cache lock where there is one.
    Ready,
    /// `take 1`, `ready`, `mount`: the mount as well.
    Mount,
}

impl Request {
    /// The lines of the request.
    fn lines(self) -> Vec<String> {
        let all = [TAKE, READY, MOUNT];
        let n = match self {
            Self::Plain => 1,
            Self::Ready => 2,
            Self::Mount => 3,
        };
        all[..n].iter().map(|line| (*line).to_owned()).collect()
    }

    /// The request to ask again with after a refusal for `why`, if there is one: an instance
    /// that does not understand a step, or holds no mount, is asked without it.
    fn fallback(self, why: &[String]) -> Option<Self> {
        let not_understood = why == [NOT_UNDERSTOOD];
        match self {
            Self::Mount if not_understood || why == [NO_MOUNT] => Some(Self::Ready),
            Self::Ready if not_understood => Some(Self::Plain),
            _ => None,
        }
    }
}

/// What the exchange may vary for a test: the copy of the sockets, and the user the running
/// instance must be.
#[derive(Clone, Copy)]
struct Knobs {
    copy: Copy,
    uid: u32,
}

impl Knobs {
    /// The copy of the sockets, and this process's own user.
    fn real() -> Self {
        Self {
            copy: duplicate,
            uid: rustix::process::geteuid().as_raw(),
        }
    }
}

/// Takes the listening sockets of the instance that answers at `handoff.socket`.
///
/// `Ok(None)` when no instance answers there: there is no file, or a file that no one listens on
/// (an instance that was killed leaves it). The caller then binds its own sockets.
///
/// # Errors
/// The running instance refused (another taker is at it), is another user, listens on other
/// addresses than `want` names, offered a descriptor that is not a listening socket, did not end
/// the hand-off within `handoff.wait`, or did not keep to the protocol.
pub(crate) async fn take(
    handoff: &Handoff,
    want: &Want<'_>,
) -> Result<Option<Inherited>, HandoffError> {
    let mut ask = Ask {
        mount: None,
        lock: None,
        leaves_another_lock: false,
    };
    take_as(handoff, want, Request::Plain, &mut ask, Knobs::real()).await
}

/// [`take`], asking the running instance to wait for this one's `ready` (the connection is in
/// [`Inherited::ready`]): until it is sent, or the connection ends, the running instance has
/// handed nothing over, so a taker that dies before it serves leaves the running instance
/// serving. A running instance of a release that does not know the step refuses the request as
/// not understood, and the taker asks again without it: then `ready` is `None` and the running
/// instance ends at `done`, as it did.
///
/// # Errors
/// As [`take`].
pub(crate) async fn take_with_ready(
    handoff: &Handoff,
    want: &Want<'_>,
) -> Result<Option<Inherited>, HandoffError> {
    let mut ask = Ask {
        mount: None,
        lock: None,
        leaves_another_lock: false,
    };
    take_as(handoff, want, Request::Ready, &mut ask, Knobs::real()).await
}

/// The one hand-over: [`take_with_ready`], asking for the mount as well when `ask.mount` names it
/// (`take 1`, `ready`, `mount`), and taking the cache lock where the running instance offers the
/// file `ask.lock` names. An instance that holds no mount, or does not know the step, refuses the
/// mount, and is asked again without it ([`Inherited::mount`] is then `None`); one that does not
/// know the `ready` step is asked with `take 1` alone, as by [`take_with_ready`].
///
/// Before `ack` the taker checks the whole offer: the addresses, the mount point and control
/// socket and lock paths against its own, the kind of each descriptor (the listening sockets
/// listen, the device passes `ask.mount`'s check, the lock is the regular file at its path), and
/// runs `ask.mount`'s `prepare` with the device; any of them failing is a `nak`, and the running
/// instance goes on as it was. The wait for the offer is `handoff.wait`; from `ack` to `done`, the
/// mount's wait when the mount was offered.
///
/// # Errors
/// As [`take`], and: the offer of the mount or the lock names other paths
/// ([`HandoffError::Mismatch`]), a descriptor is not of its kind or `prepare` failed
/// ([`HandoffError::Io`]), or the offer is not what was asked for ([`HandoffError::Protocol`]).
#[cfg(test)]
pub(crate) async fn take_service(
    handoff: &Handoff,
    want: &Want<'_>,
    mut ask: Ask<'_>,
) -> Result<Option<Inherited>, HandoffError> {
    let request = if ask.mount.is_some() {
        Request::Mount
    } else {
        Request::Ready
    };
    take_as(handoff, want, request, &mut ask, Knobs::real()).await
}

/// The exchange of `request`, within the waits of `handoff` and of `ask`'s mount.
async fn take_as(
    handoff: &Handoff,
    want: &Want<'_>,
    request: Request,
    ask: &mut Ask<'_>,
    knobs: Knobs,
) -> Result<Option<Inherited>, HandoffError> {
    let quiesce = ask
        .mount
        .as_ref()
        .map_or(Duration::ZERO, |mount| mount.wait);
    let exchanging = exchange(handoff, want, (request, ask), knobs);
    match tokio::time::timeout(handoff.wait.saturating_add(quiesce), exchanging).await {
        Ok(taken) => taken,
        Err(_) => Err(HandoffError::Timeout(handoff.socket.clone())),
    }
}

/// Whether a socket at `offered` serves what `wanted` asks for: the same address, or, for a port
/// of 0 (the system picks one), the same host.
fn covers(wanted: SocketAddr, offered: SocketAddr) -> bool {
    wanted == offered || (wanted.port() == 0 && wanted.ip() == offered.ip())
}

/// Whether `offered` are the sockets `wanted` asks for, in the same order.
fn all_cover(wanted: &[SocketAddr], offered: &[SocketAddr]) -> bool {
    wanted.len() == offered.len() && wanted.iter().zip(offered).all(|(w, o)| covers(*w, *o))
}

/// A line of an offer, as this release reads it.
enum Entry {
    /// A listening socket of a front.
    Socket(Front, SocketAddr),
    /// The `fuse`, `control` or `lock` line, with what follows its name.
    Named(&'static str, String),
    /// A line this release does not know: skipped, its descriptor closed.
    Unknown,
}

/// The entry of the line `<name> <rest>`.
fn entry(text: &str) -> Result<Entry, HandoffError> {
    let protocol = || HandoffError::Protocol("a socket line that is not <front> <address>");
    let (name, rest) = text.split_once(' ').ok_or_else(protocol)?;
    if let Some(front) = Front::named(name) {
        return Ok(Entry::Socket(front, rest.parse().map_err(|_| protocol())?));
    }
    Ok([FUSE, CONTROL, LOCK]
        .into_iter()
        .find(|known| *known == name)
        .map_or(Entry::Unknown, |known| Entry::Named(known, rest.to_owned())))
}

/// The lines and descriptors of an offer, by what they are.
#[derive(Default)]
pub(super) struct Parts {
    /// The sockets: the front of each (none for a front this release does not know) and its
    /// descriptor.
    pub(super) sockets: Vec<(Option<Front>, OwnedFd)>,
    /// The addresses of the sockets of the fronts this release knows.
    addrs: Vec<(Front, SocketAddr)>,
    /// The `fuse`, `control` and `lock` lines, by name, with their descriptors.
    pub(super) named: Vec<(&'static str, String, OwnedFd)>,
}

impl Parts {
    /// The lines `rest` of an offer, with `fds`, one for each.
    pub(super) fn of(rest: &[String], fds: Vec<OwnedFd>) -> Result<Self, HandoffError> {
        if rest.len() != fds.len() {
            return Err(HandoffError::Protocol(
                "another number of sockets than lines",
            ));
        }
        let mut parts = Self::default();
        for (text, fd) in rest.iter().zip(fds) {
            match entry(text)? {
                Entry::Socket(front, addr) => {
                    parts.addrs.push((front, addr));
                    parts.sockets.push((Some(front), fd));
                }
                Entry::Named(name, value) => {
                    if parts.named.iter().any(|(had, _, _)| *had == name) {
                        return Err(HandoffError::Protocol("an offer that names a line twice"));
                    }
                    parts.named.push((name, value, fd));
                }
                // A front this release does not know is skipped; the descriptor is closed below.
                Entry::Unknown => parts.sockets.push((None, fd)),
            }
        }
        Ok(parts)
    }

    /// Whether the sockets of the offer whose lines are `rest` are those `want` names, front by
    /// front and in order.
    ///
    /// # Errors
    /// [`HandoffError::Mismatch`], with the offer's lines and what `want` names.
    pub(super) fn cover(&self, want: &Want<'_>, rest: &[String]) -> Result<(), HandoffError> {
        if all_cover(want.s3, &self.of_front(Front::S3))
            && all_cover(want.oci, &self.of_front(Front::Oci))
            && all_cover(want.nfs, &self.of_front(Front::Nfs))
        {
            return Ok(());
        }
        let named = |front: Front, addrs: &[SocketAddr]| -> Vec<String> {
            addrs.iter().map(|addr| line(front, *addr)).collect()
        };
        Err(HandoffError::Mismatch {
            offered: rest.to_vec(),
            wanted: [
                named(Front::S3, want.s3),
                named(Front::Oci, want.oci),
                named(Front::Nfs, want.nfs),
            ]
            .concat(),
        })
    }

    /// The addresses of `front`'s sockets.
    fn of_front(&self, front: Front) -> Vec<SocketAddr> {
        let of = self.addrs.iter().filter(|(f, _)| *f == front);
        of.map(|(_, addr)| *addr).collect()
    }

    /// The line `name`, taken out.
    pub(super) fn take(&mut self, name: &str) -> Option<(String, OwnedFd)> {
        let at = self.named.iter().position(|(had, _, _)| *had == name)?;
        let (_, value, fd) = self.named.remove(at);
        Some((value, fd))
    }
}

/// Why an offer is not taken, and the `nak` that says so.
async fn nak(stream: &UnixStream, why: HandoffError) -> Result<Option<Offered>, HandoffError> {
    wire::send(stream, &["nak".to_owned()], &[])
        .await
        .map_err(HandoffError::io("refusing the offer"))?;
    Err(why)
}

/// An offer of the sockets (and of what [`Ask`] adds to them) that this taker has checked whole
/// and not yet answered: the running instance serves on as it was, waiting for `ack` or `nak`.
/// [`Offered::accept`] says `ack` and [`Offered::refuse`] says `nak`; dropped, the connection ends,
/// which the running instance takes for a `nak`.
///
/// The start that switches a node from two processes to one asks both for their offers before it
/// accepts either (`docs/design/one-service.md` section 4.2): it holds this one while it takes the
/// mount from the other process, and refuses it when that fails.
pub(crate) struct Offered {
    stream: UnixStream,
    /// What the offer gives, all made before `ack`, with the mount and the lock already in it.
    inherited: Inherited,
    request: Request,
    /// The control socket, for the error of a wait that ran out.
    socket: PathBuf,
    /// The wait from `ack` to `done`: with the mount, its quiesce's too.
    wait: Duration,
}

impl Offered {
    /// Whether the running instance offered its mount: it holds it, and this offer takes it.
    pub(crate) fn has_mount(&self) -> bool {
        self.inherited.mount.is_some()
    }

    /// [`accept`](Self::accept) of an offer with the mount ([`has_mount`](Self::has_mount)): the
    /// sockets, the mount and the connection on which `ready` is said, apart.
    ///
    /// # Errors
    /// As [`accept`](Self::accept), and an offer without the mount, which is not answered: the
    /// connection ends, which the running instance takes for a `nak`.
    pub(crate) async fn accept_with_mount(
        mut self,
    ) -> Result<(Inherited, TakenMount, Ready), HandoffError> {
        let Some(mount) = self.inherited.mount.take() else {
            return Err(HandoffError::Protocol(
                "an offer without the mount it was asked for",
            ));
        };
        let mut inherited = self.accept().await?;
        // A request for the mount asks for the `ready` step: the connection is there.
        let ready = inherited.ready.take();
        ready
            .map(|ready| (inherited, mount, ready))
            .ok_or(HandoffError::Protocol("an offer without the ready step"))
    }

    /// Says `ack`, and waits for the running instance's `done`, within the wait of the hand-off.
    ///
    /// # Errors
    /// The running instance is gone, answered something other than `done`, or did not answer
    /// within the wait.
    pub(crate) async fn accept(self) -> Result<Inherited, HandoffError> {
        let socket = self.socket.clone();
        match tokio::time::timeout(self.wait, self.answer()).await {
            Ok(accepted) => accepted,
            Err(_) => Err(HandoffError::Timeout(socket)),
        }
    }

    /// Says `ack` and reads `done`, with no wait of its own.
    async fn answer(self) -> Result<Inherited, HandoffError> {
        let Self {
            stream,
            mut inherited,
            request,
            ..
        } = self;
        let io = HandoffError::io;
        wire::send(&stream, &["ack".to_owned()], &[])
            .await
            .map_err(io("accepting the offer"))?;
        let done = wire::recv(&stream)
            .await
            .map_err(io("waiting for the end of the hand-off"))?;
        if done.lines != ["done"] {
            return Err(HandoffError::Protocol(
                "something other than done after ack",
            ));
        }
        inherited.ready = (request != Request::Plain).then_some(Ready(stream));
        Ok(inherited)
    }

    /// Says `nak`: the running instance goes on as it was. A `nak` that cannot be sent is not an
    /// error: the connection ends here, which the running instance takes for one.
    pub(crate) async fn refuse(self) {
        let _ = wire::send(&self.stream, &["nak".to_owned()], &[]).await;
    }
}

/// Asks the instance that answers at `handoff.socket` for its listening sockets (`take 1`) and
/// checks the offer whole, as [`take`] does, without answering it: `Ok(None)` where no instance
/// answers. The wait for the offer is `handoff.wait`, and so is the wait of
/// [`Offered::accept`].
///
/// # Errors
/// As [`take`], for what is found before `ack`; an offer found wrong was refused with `nak`.
#[cfg(test)]
pub(crate) async fn offered(
    handoff: &Handoff,
    want: &Want<'_>,
) -> Result<Option<Offered>, HandoffError> {
    let mut ask = Ask {
        mount: None,
        lock: None,
        leaves_another_lock: false,
    };
    offer_of(handoff, want, (Request::Plain, &mut ask), Knobs::real()).await
}

/// Asks the instance that answers at `handoff.socket` for its listening sockets with the `ready`
/// step, for the mount as well where `ask.mount` names it (`take 1`, `ready`, `mount`), and for the
/// cache lock at `ask.lock`, and checks the offer whole without answering it, as `offered` does for
/// `take 1` alone: what the start of a node whose file has an `fs` interface asks of the running
/// instance on `server.handoff_socket` (`docs/design/one-service.md` sections 3.1 and 3.2). The
/// [`Inherited`] that [`Offered::accept`] gives then holds the connection for `ready`, the mount
/// where the running instance held it ([`Offered::has_mount`]; `ask.mount`'s `prepare` ran with its
/// device before this returned), and the lock where it offered it, so that the cache takes the
/// write role at the running instance's `written` rather than after that instance's drain.
///
/// A running instance that holds no mount, or does not know the `mount` line, is asked again
/// without it, and one that does not know the `ready` step with `take 1` alone, as by
/// [`take_with_ready`]: no `ready`, no lock, the behaviour of a release before them. With
/// [`Ask::leaves_another_lock`], a lock it offers of another file (its file names another cache
/// directory than this start's) is closed and left with it: this start's own cache is another
/// directory, which that lock does not hold.
///
/// # Errors
/// As [`take`], for what is found before `ack`, and: the offer of the mount or the lock names
/// other paths ([`HandoffError::Mismatch`]), a descriptor is not of its kind or `prepare` failed
/// ([`HandoffError::Io`]), or the offer is not what was asked for ([`HandoffError::Protocol`]).
/// An offer found wrong was refused with `nak`.
pub(crate) async fn offered_with(
    handoff: &Handoff,
    want: &Want<'_>,
    mut ask: Ask<'_>,
) -> Result<Option<Offered>, HandoffError> {
    let request = if ask.mount.is_some() {
        Request::Mount
    } else {
        Request::Ready
    };
    offer_of(handoff, want, (request, &mut ask), Knobs::real()).await
}

/// The whole exchange with the running instance: `request`, and what `ask` adds to it; `knobs`
/// make the copies of [`Inherited`] and name the user the running instance must be.
async fn exchange(
    handoff: &Handoff,
    want: &Want<'_>,
    (request, ask): (Request, &mut Ask<'_>),
    knobs: Knobs,
) -> Result<Option<Inherited>, HandoffError> {
    // The wait of the whole exchange is the caller's.
    match offer_of(handoff, want, (request, ask), knobs).await? {
        Some(offered) => offered.answer().await.map(Some),
        None => Ok(None),
    }
}

/// The exchange up to the taker's answer: `request`, and what `ask` adds to it, sent; the offer
/// read and checked whole; `nak` said to an offer found wrong. `Ok(None)` where nothing answers.
async fn offer_of(
    handoff: &Handoff,
    want: &Want<'_>,
    (request, ask): (Request, &mut Ask<'_>),
    knobs: Knobs,
) -> Result<Option<Offered>, HandoffError> {
    let io = HandoffError::io;
    let asking = async {
        let stream = match UnixStream::connect(&handoff.socket).await {
            Ok(stream) => stream,
            Err(why)
                if matches!(
                    why.kind(),
                    ErrorKind::NotFound | ErrorKind::ConnectionRefused
                ) =>
            {
                return Ok(None);
            }
            Err(why) => return Err(io("connecting to the control socket")(why)),
        };
        let peer = stream
            .peer_cred()
            .map_err(io("asking the running instance's user"))?;
        if peer.uid() != knobs.uid {
            let why = format!("the running instance is the user {}", peer.uid());
            let stranger = std::io::Error::new(ErrorKind::PermissionDenied, why);
            return Err(io("checking the running instance's user")(stranger));
        }
        wire::send(&stream, &request.lines(), &[])
            .await
            .map_err(io("asking for the sockets"))?;
        let offer = wire::recv(&stream).await.map_err(io("reading the offer"))?;
        Ok(Some((stream, offer)))
    };
    let asked = tokio::time::timeout(handoff.wait, asking).await;
    let Some((stream, offer)) =
        asked.map_err(|_| HandoffError::Timeout(handoff.socket.clone()))??
    else {
        return Ok(None);
    };
    let Frame { lines, fds } = offer;
    let head = lines.first().map_or("", String::as_str);
    let rest = lines.get(1..).unwrap_or_default();
    if head == "refused" {
        if let Some(again) = request.fallback(rest) {
            // A release, or an instance, that does not take the step: ask as it understands.
            drop(stream);
            return Box::pin(offer_of(handoff, want, (again, ask), knobs)).await;
        }
        return Err(HandoffError::Refused(rest.join(" ")));
    }
    if head != "offer" {
        return Err(HandoffError::Protocol("a frame that is not an offer"));
    }
    let mut parts = Parts::of(rest, fds)?;
    if let Err(mismatch) = parts.cover(want, rest) {
        return nak(&stream, mismatch).await;
    }
    let offered_mount = match check_mount(&mut parts, request, ask) {
        Ok(offered) => offered,
        Err(why) => return nak(&stream, why).await,
    };
    let lock = match take_lock(&mut parts, ask.lock) {
        Ok(lock) => lock,
        // Its descriptor is closed: the lock stays with the running instance.
        Err(HandoffError::Mismatch { .. }) if ask.leaves_another_lock => None,
        Err(why) => return nak(&stream, why).await,
    };
    let mut inherited = match adopt(parts.sockets.into_iter(), knobs.copy) {
        Ok(inherited) => inherited,
        // The running instance goes on as if nothing had been asked.
        Err(why) => return nak(&stream, io("taking over a socket")(why)).await,
    };
    // Last before `ack`, as it puts the device in the taker's store.
    inherited.mount = match prepare(offered_mount, ask) {
        Ok(taken) => taken,
        Err(why) => return nak(&stream, why).await,
    };
    inherited.lock = lock;
    // The running instance quiesces the mount between `ack` and `done`.
    let quiesce = (ask.mount.as_ref())
        .filter(|_| inherited.mount.is_some())
        .map_or(Duration::ZERO, |mount| mount.wait);
    Ok(Some(Offered {
        stream,
        inherited,
        request,
        socket: handoff.socket.clone(),
        wait: handoff.wait.saturating_add(quiesce),
    }))
}

/// The mount of an offer, checked against `ask.mount`, when it was asked for: the device and the
/// control socket.
///
/// # Errors
/// An offer of the mount that was not asked for, or none when it was; other paths than the
/// taker's; a descriptor not of its kind.
fn check_mount(
    parts: &mut Parts,
    request: Request,
    ask: &Ask<'_>,
) -> Result<Option<(OwnedFd, OwnedFd)>, HandoffError> {
    let offered = (parts.take(FUSE), parts.take(CONTROL));
    let want = ask.mount.as_ref().filter(|_| request == Request::Mount);
    let (want, (fuse, fuse_fd), (control, control_fd)) = match (want, offered) {
        (None, (None, None)) => return Ok(None),
        (Some(want), (Some(fuse), Some(control))) => (want, fuse, control),
        (None, _) => {
            return Err(HandoffError::Protocol(
                "an offer of a mount that was not asked for",
            ));
        }
        (Some(_), _) => {
            return Err(HandoffError::Protocol(
                "an offer without the mount it was asked for",
            ));
        }
    };
    let wanted = mount::lines(want.mountpoint, want.control);
    let offered = [format!("{FUSE} {fuse}"), format!("{CONTROL} {control}")];
    if offered != wanted {
        return Err(HandoffError::Mismatch {
            offered: offered.to_vec(),
            wanted: wanted.to_vec(),
        });
    }
    let kinds = HandoffError::io("checking the offered descriptors");
    (want.device)(fuse_fd.as_fd()).map_err(kinds)?;
    check_control(control_fd.as_fd())
        .map_err(HandoffError::io("checking the offered descriptors"))?;
    // The kernel hands a descriptor over open on exec; these are closed on it, as the listening
    // sockets are.
    for fd in [&fuse_fd, &control_fd] {
        let marked = fcntl_setfd(fd, FdFlags::CLOEXEC).map_err(std::io::Error::from);
        marked.map_err(HandoffError::io("marking a descriptor"))?;
    }
    Ok(Some((fuse_fd, control_fd)))
}

/// Runs `ask.mount`'s `prepare` with the device of `offered`, if the mount was offered.
///
/// # Errors
/// What `prepare` said.
fn prepare(
    offered: Option<(OwnedFd, OwnedFd)>,
    ask: &mut Ask<'_>,
) -> Result<Option<TakenMount>, HandoffError> {
    let (Some((device, control)), Some(want)) = (offered, ask.mount.as_mut()) else {
        return Ok(None);
    };
    if let Err(why) = (want.prepare)(&device) {
        return Err(HandoffError::io("preparing to take the mount")(
            std::io::Error::other(why),
        ));
    }
    let control = std::os::unix::net::UnixListener::from(control);
    Ok(Some(TakenMount { device, control }))
}

/// The cache lock of an offer, when it offers one and the taker has a cache: the descriptor must
/// be the file at the taker's lock path. A lock offered to a taker with no cache is closed, which
/// leaves it with the running instance.
///
/// # Errors
/// The offer names another lock file, or the descriptor is not the file at the path.
pub(super) fn take_lock(
    parts: &mut Parts,
    wanted: Option<&Path>,
) -> Result<Option<Received>, HandoffError> {
    let (Some((offered, fd)), Some(wanted)) = (parts.take(LOCK), wanted) else {
        return Ok(None);
    };
    let own = format!("{LOCK} {}", wanted.display());
    if format!("{LOCK} {offered}") != own {
        return Err(HandoffError::Mismatch {
            offered: vec![format!("{LOCK} {offered}")],
            wanted: vec![own],
        });
    }
    Received::of(fd, wanted)
        .map(Some)
        .map_err(HandoffError::io("checking the offered lock"))
}

/// The listeners of the descriptors that came on the control socket, by front, with their
/// addresses and a copy of each made by `copy`.
pub(super) fn adopt(
    offered: impl Iterator<Item = (Option<Front>, OwnedFd)>,
    copy: Copy,
) -> std::io::Result<Inherited> {
    let mut inherited = Inherited {
        s3: Vec::new(),
        oci: Vec::new(),
        s3_addrs: Vec::new(),
        oci_addrs: Vec::new(),
        nfs: Vec::new(),
        nfs_addrs: Vec::new(),
        copies: Vec::new(),
        ready: None,
        mount: None,
        lock: None,
    };
    for (front, fd) in offered {
        // A front this release does not know: the descriptor is closed.
        let Some(front) = front else { continue };
        let listener = listener_of(fd, front)?;
        let addr = listener.local_addr()?;
        inherited.copies.push(Socket::new(front, copy(&listener)?));
        let (listeners, addrs) = match front {
            Front::S3 => (&mut inherited.s3, &mut inherited.s3_addrs),
            Front::Oci => (&mut inherited.oci, &mut inherited.oci_addrs),
            Front::Nfs => (&mut inherited.nfs, &mut inherited.nfs_addrs),
        };
        listeners.push(listener);
        addrs.push(addr);
    }
    Ok(inherited)
}

/// The listener of a descriptor that came on the control socket for `front`, which must be a
/// listening socket. The kernel hands it over open on exec on macOS, and on Linux unless asked, so
/// close-on-exec is set here, as `listen::bind` does.
fn listener_of(fd: OwnedFd, front: Front) -> std::io::Result<TcpListener> {
    check_listening(fd.as_fd(), &format!("the {} socket", front.name()))?;
    fcntl_setfd(&fd, FdFlags::CLOEXEC)?;
    let listener = std::net::TcpListener::from(fd);
    listener.set_nonblocking(true)?;
    TcpListener::from_std(listener)
}

#[cfg(test)]
#[path = "take_lock_tests.rs"]
mod lock_tests;
#[cfg(test)]
#[path = "take_ready_tests.rs"]
mod ready_tests;
#[cfg(test)]
#[path = "take_service_tests.rs"]
mod service_tests;
#[cfg(test)]
#[path = "take_tests.rs"]
mod tests;
