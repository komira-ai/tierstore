//! The new instance's side: asking a running instance for its listening sockets.

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::os::fd::{AsFd as _, OwnedFd};
use std::path::Path;

use rustix::io::{FdFlags, fcntl_setfd};
use tokio::net::{TcpListener, UnixStream};

use super::offer::{BYE, NOT_UNDERSTOOD, READY, TAKE};
use super::wire::{self, Frame};
use super::{Front, Handoff, HandoffError, Socket, line};

/// The addresses a new instance's settings name, front by front, in the order of its interfaces.
pub(crate) struct Want<'a> {
    pub(crate) s3: &'a [SocketAddr],
    pub(crate) oci: &'a [SocketAddr],
    pub(crate) nfs: &'a [SocketAddr],
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
        let io = HandoffError::io("telling the running instance it can end");
        wire::send(&self.0, &[READY.to_owned()], &[])
            .await
            .map_err(HandoffError::io("telling the running instance it can end"))?;
        // The running instance ends only after it has read `ready` and answered `bye`; a
        // connection that closes instead means it gave up first and serves on.
        let answer = wire::recv(&self.0).await.map_err(io)?;
        if answer.lines == [BYE] {
            Ok(())
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
fn duplicate(listener: &TcpListener) -> std::io::Result<std::net::TcpListener> {
    Ok(listener.as_fd().try_clone_to_owned()?.into())
}

/// Takes the listening sockets of the instance that answers at `handoff.socket`.
///
/// `Ok(None)` when no instance answers there: there is no file, or a file that no one listens on
/// (an instance that was killed leaves it). The caller then binds its own sockets.
///
/// # Errors
/// The running instance refused (another taker is at it), listens on other addresses than `want`
/// names, did not end the hand-off within `handoff.wait`, or did not keep to the protocol.
pub(crate) async fn take(
    handoff: &Handoff,
    want: &Want<'_>,
) -> Result<Option<Inherited>, HandoffError> {
    take_as(handoff, want, false).await
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
    take_as(handoff, want, true).await
}

/// [`take`] and [`take_with_ready`]: `ready` is whether the step is asked for.
async fn take_as(
    handoff: &Handoff,
    want: &Want<'_>,
    ready: bool,
) -> Result<Option<Inherited>, HandoffError> {
    let exchanging = exchange(&handoff.socket, want, duplicate, ready);
    match tokio::time::timeout(handoff.wait, exchanging).await {
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

/// The whole exchange with the running instance; `copy` makes the copies of [`Inherited`], and
/// `ready` is whether the request asks for the `ready` step.
async fn exchange(
    path: &Path,
    want: &Want<'_>,
    copy: Copy,
    ready: bool,
) -> Result<Option<Inherited>, HandoffError> {
    let stream = match UnixStream::connect(path).await {
        Ok(stream) => stream,
        Err(why)
            if matches!(
                why.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None);
        }
        Err(why) => return Err(HandoffError::io("connecting to the control socket")(why)),
    };
    let io = HandoffError::io;
    let mut request = vec![TAKE.to_owned()];
    if ready {
        request.push(READY.to_owned());
    }
    wire::send(&stream, &request, &[])
        .await
        .map_err(io("asking for the sockets"))?;
    let offer = wire::recv(&stream).await.map_err(io("reading the offer"))?;
    let Frame { lines, fds } = offer;
    let head = lines.first().map_or("", String::as_str);
    let rest = lines.get(1..).unwrap_or_default();
    if head == "refused" {
        if ready && rest == [NOT_UNDERSTOOD] {
            // A release that has no `ready` step: ask as it understands.
            return Box::pin(exchange(path, want, copy, false)).await;
        }
        return Err(HandoffError::Refused(rest.join(" ")));
    }
    if head != "offer" {
        return Err(HandoffError::Protocol("a frame that is not an offer"));
    }
    if rest.len() != fds.len() {
        return Err(HandoffError::Protocol(
            "another number of sockets than lines",
        ));
    }
    let mut offered = Vec::new();
    for entry in rest {
        let (name, addr) = entry.split_once(' ').ok_or(HandoffError::Protocol(
            "a socket line that is not <front> <address>",
        ))?;
        // A front this release does not know is skipped; the descriptor is closed below.
        offered.push(match Front::named(name) {
            Some(front) => Some((
                front,
                addr.parse::<SocketAddr>().map_err(|_| {
                    HandoffError::Protocol("a socket line that is not <front> <address>")
                })?,
            )),
            None => None,
        });
    }
    let of = |front: Front| -> Vec<SocketAddr> {
        offered
            .iter()
            .flatten()
            .filter(|(f, _)| *f == front)
            .map(|(_, a)| *a)
            .collect()
    };
    if !(all_cover(want.s3, &of(Front::S3))
        && all_cover(want.oci, &of(Front::Oci))
        && all_cover(want.nfs, &of(Front::Nfs)))
    {
        wire::send(&stream, &["nak".to_owned()], &[])
            .await
            .map_err(io("refusing the offer"))?;
        let named = |front: Front, addrs: &[SocketAddr]| -> Vec<String> {
            addrs.iter().map(|addr| line(front, *addr)).collect()
        };
        return Err(HandoffError::Mismatch {
            offered: rest.to_vec(),
            wanted: [
                named(Front::S3, want.s3),
                named(Front::Oci, want.oci),
                named(Front::Nfs, want.nfs),
            ]
            .concat(),
        });
    }
    let fronts = offered.iter().map(|entry| entry.map(|(front, _)| front));
    let mut inherited = match adopt(fronts.zip(fds), copy) {
        Ok(inherited) => inherited,
        Err(why) => {
            // The running instance goes on as if nothing had been asked.
            wire::send(&stream, &["nak".to_owned()], &[])
                .await
                .map_err(io("refusing the offer"))?;
            return Err(io("taking over a socket")(why));
        }
    };
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
    inherited.ready = ready.then_some(Ready(stream));
    Ok(Some(inherited))
}

/// The listeners of the descriptors that came on the control socket, by front, with their
/// addresses and a copy of each made by `copy`.
fn adopt(
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
    };
    for (front, fd) in offered {
        // A front this release does not know: the descriptor is closed.
        let Some(front) = front else { continue };
        let listener = listener_of(fd)?;
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

/// The listener of a descriptor that came on the control socket. The kernel hands it over open on
/// exec on macOS, and on Linux unless asked, so close-on-exec is set here, as `listen::bind` does.
fn listener_of(fd: OwnedFd) -> std::io::Result<TcpListener> {
    fcntl_setfd(&fd, FdFlags::CLOEXEC)?;
    let listener = std::net::TcpListener::from(fd);
    listener.set_nonblocking(true)?;
    TcpListener::from_std(listener)
}

#[cfg(test)]
#[path = "take_ready_tests.rs"]
mod ready_tests;
#[cfg(test)]
#[path = "take_tests.rs"]
mod tests;
